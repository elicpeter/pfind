//! Work-queue abstraction for directory traversal and file processing.
//!
//! Two implementations:
//!   1. Bounded FIFO queues backed by `crossbeam_queue::ArrayQueue`.
//!   2. LIFO stacks are backed by our custom `TreiberStack`.
//!
//! The `Config.work_queue_backend` decides which backend to use
//! at runtime, but the rest of the pipeline sees a single `WorkQueue<T>`
//! API exposing `try_push`, `try_pop`, and `pop_batch`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crossbeam_queue::ArrayQueue;

use crate::config::{Config, WorkQueueBackend};
use crate::stack::TreiberStack;

/// Generic work-queue abstraction (either FIFO queue or LIFO stack).
pub struct WorkQueue<T> {
  inner: Inner<T>,
}

enum Inner<T> {
  /// Version 1: bounded MPMC FIFO queue (ArrayQueue).
  ArrayQueue(Arc<ArrayQueue<T>>),

  /// Version 2: unbounded LIFO stack (TreiberStack).
  TreiberStack(Arc<TreiberStack<T>>),
}

// Manual Clone implementation since Inner contains Arc (which is Clone)
impl<T> Clone for WorkQueue<T> {
  fn clone(&self) -> Self {
    WorkQueue {
      inner: self.inner.clone(),
    }
  }
}

impl<T> Clone for Inner<T> {
  fn clone(&self) -> Self {
    match self {
      Inner::ArrayQueue(q) => Inner::ArrayQueue(Arc::clone(q)),
      Inner::TreiberStack(s) => Inner::TreiberStack(Arc::clone(s)),
    }
  }
}

impl<T> WorkQueue<T> {
  /// Construct a new queue using the given backend and capacity.
  ///
  /// `capacity` is only used for `ArrayQueue`; for `TreiberStack`
  /// it is ignored (but must be > 0).
  pub fn new(backend: WorkQueueBackend, capacity: usize) -> Self {
    let cap = capacity.max(1);

    match backend {
      WorkQueueBackend::BoundedMpmcQueue => {
        let q = Arc::new(ArrayQueue::new(cap));
        WorkQueue {
          inner: Inner::ArrayQueue(q),
        }
      }
      WorkQueueBackend::TreiberStack => {
        let stack = Arc::new(TreiberStack::new());
        WorkQueue {
          inner: Inner::TreiberStack(stack),
        }
      }
    }
  }

  /// Push a single item into the queue / stack.
  ///
  /// For the bounded queue backend, this returns `Err(item)` if the
  /// queue is full. For the TreiberStack backend, this always returns `Ok(())`.
  #[inline]
  pub fn try_push(&self, value: T) -> Result<(), T> {
    match &self.inner {
      Inner::ArrayQueue(q) => q.push(value),
      Inner::TreiberStack(s) => {
        s.push(value);
        Ok(())
      }
    }
  }

  /// Push a batch of items into the queue / stack.
  ///
  /// For the bounded queue backend, this returns a `Vec<T>` containing
  /// any items that could *not* be enqueued because the queue was full.
  /// For the TreiberStack backend, all items are pushed and the returned
  /// `Vec` is always empty.
  #[inline]
  pub fn try_push_batch<I>(&self, iter: I) -> Vec<T>
  where
    I: IntoIterator<Item = T>,
  {
    match &self.inner {
      Inner::ArrayQueue(q) => {
        let mut leftover = Vec::new();

        for item in iter {
          if let Err(item) = q.push(item) {
            // Queue is full; keep track of items we couldn't enqueue.
            leftover.push(item);
          }
        }

        leftover
      }
      Inner::TreiberStack(s) => {
        for item in iter {
          s.push(item);
        }
        Vec::new()
      }
    }
  }

  /// Pop a single item from the queue / stack, if any.
  ///
  /// FIFO semantics when backed by ArrayQueue,
  /// LIFO semantics when backed by TreiberStack.
  #[inline]
  pub fn try_pop(&self) -> Option<T> {
    match &self.inner {
      Inner::ArrayQueue(q) => q.pop(),
      Inner::TreiberStack(s) => s.pop(),
    }
  }

  /// Pop up to `max` items as a batch.
  ///
  /// Used to implement queue-aware batching in Stage A/B.
  #[inline]
  pub fn pop_batch(&self, max: usize) -> Vec<T> {
    let max = max.max(1);
    let mut out = Vec::with_capacity(max);

    for _ in 0..max {
      if let Some(v) = self.try_pop() {
        out.push(v);
      } else {
        break;
      }
    }

    out
  }

  /// Approximate length of the queue / stack.
  ///
  /// NOTE: For lock-free structures, `len()` is only approximate
  /// under heavy concurrency, which is fine for heuristics / soft caps.
  #[inline]
  pub fn len_approx(&self) -> usize {
    match &self.inner {
      Inner::ArrayQueue(q) => q.len(),
      Inner::TreiberStack(s) => s.len(),
    }
  }

  /// Returns true if the queue / stack is (observationally) empty.
  #[inline]
  pub fn is_empty(&self) -> bool {
    match &self.inner {
      Inner::ArrayQueue(q) => q.is_empty(),
      Inner::TreiberStack(s) => s.is_empty(),
    }
  }
}

/// Type alias for the directory-work queue.
///
/// You can choose whatever payload you like here:
///   - `PathBuf` for single-directory tasks
///   - custom `DirTask` struct for batched subtrees, etc.
pub type DirQueue<T> = WorkQueue<T>;

/// Type alias for the file-work queue.
///
/// Similarly, you might use `PathBuf`, or a small `Vec<PathBuf>`,
/// or some `FileBatch` wrapper.
pub type FileQueue<T> = WorkQueue<T>;

/// Helper to construct the pair of Dir/File queues from a `PipelineConfig`.
pub fn make_dir_file_queues<DirItem, FileItem>(
  cfg: &Config,
) -> (DirQueue<DirItem>, FileQueue<FileItem>) {
  let backend = cfg.work_queue_backend;

  let dir_q = WorkQueue::new(backend, cfg.dir_queue_capacity);
  let file_q = WorkQueue::new(backend, cfg.file_queue_capacity);

  (dir_q, file_q)
}

/// Count of dirs in flight: enqueued but not yet popped + popped but not yet
/// finished walking. Producers `add_dirs` BEFORE pushing; consumers
/// `sub_dirs` AFTER they finish walking the popped directory. The single
/// counter is enough for quiescence because the underlying queue ops
/// synchronise-with each other (release/acquire), so when all_quiet
/// returns true, no thread can still hold a reference to in-flight work.
pub struct WorkCounters {
  pub dir_pending: AtomicUsize,
}

impl WorkCounters {
  pub fn new() -> Self {
    Self {
      dir_pending: AtomicUsize::new(0),
    }
  }

  #[inline]
  pub fn add_dirs(&self, n: usize) {
    if n > 0 {
      self.dir_pending.fetch_add(n, Ordering::Relaxed);
    }
  }

  #[inline]
  pub fn sub_dirs(&self, n: usize) {
    if n > 0 {
      self.dir_pending.fetch_sub(n, Ordering::Relaxed);
    }
  }

  #[inline]
  pub fn all_quiet(&self) -> bool {
    self.dir_pending.load(Ordering::Acquire) == 0
  }
}

impl Default for WorkCounters {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  // Mock config types for testing
  #[derive(Clone, Copy)]
  pub enum WorkQueueBackend {
    BoundedMpmcQueue,
    TreiberStack,
  }

  #[test]
  fn test_array_queue_backend() {
    let q: WorkQueue<i32> = WorkQueue {
      inner: Inner::ArrayQueue(Arc::new(ArrayQueue::new(10))),
    };

    assert!(q.is_empty());
    assert!(q.try_push(1).is_ok());
    assert!(q.try_push(2).is_ok());
    assert_eq!(q.len_approx(), 2);

    // FIFO: first in, first out
    assert_eq!(q.try_pop(), Some(1));
    assert_eq!(q.try_pop(), Some(2));
    assert_eq!(q.try_pop(), None);
  }

  #[test]
  fn test_treiber_stack_backend() {
    let q: WorkQueue<i32> = WorkQueue {
      inner: Inner::TreiberStack(Arc::new(TreiberStack::new())),
    };

    assert!(q.is_empty());
    assert!(q.try_push(1).is_ok());
    assert!(q.try_push(2).is_ok());
    assert_eq!(q.len_approx(), 2);

    // LIFO: last in, first out
    assert_eq!(q.try_pop(), Some(2));
    assert_eq!(q.try_pop(), Some(1));
    assert_eq!(q.try_pop(), None);
  }

  #[test]
  fn test_pop_batch() {
    let q: WorkQueue<i32> = WorkQueue {
      inner: Inner::ArrayQueue(Arc::new(ArrayQueue::new(10))),
    };

    for i in 1..=5 {
      q.try_push(i).unwrap();
    }

    let batch = q.pop_batch(3);
    assert_eq!(batch, vec![1, 2, 3]);
    assert_eq!(q.len_approx(), 2);

    // Pop more than available
    let batch = q.pop_batch(10);
    assert_eq!(batch, vec![4, 5]);
    assert!(q.is_empty());
  }

  #[test]
  fn test_clone() {
    let q1: WorkQueue<i32> = WorkQueue {
      inner: Inner::ArrayQueue(Arc::new(ArrayQueue::new(10))),
    };

    q1.try_push(42).unwrap();

    let q2 = q1.clone();

    // Both should see the same item (shared Arc)
    assert_eq!(q2.try_pop(), Some(42));
    assert!(q1.is_empty()); // q1 is also empty now
  }
}