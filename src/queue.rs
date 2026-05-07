//! Per-worker work-stealing pool with parking.
//!
//! Each worker owns a `crossbeam::deque::Worker<T>` (LIFO). Children of
//! the directory currently being walked are pushed onto the local deque
//! — no atomic on the hot path. When the local deque is empty, the
//! worker tries to steal from random peer `Stealer`s. If still empty
//! and `dir_pending > 0`, the worker parks (`crossbeam::sync::Parker`)
//! until another worker pushes work and unparks it. When all workers
//! are parked AND `dir_pending == 0`, the last to observe quiescence
//! sets `shutdown` and unparks everyone for a clean exit.
//!
//! `dir_pending` is held by:
//!   - any directory enqueued in any deque (waiting to be popped), and
//!   - any directory currently being walked by a worker.
//! Producers `add_dirs(n)` BEFORE pushing children onto the local
//! deque. Consumers `sub_dirs(1)` AFTER finishing a dir's walk. The
//! ordering matters: if a child became visible to stealers before the
//! producer's add, the stealer's sub could underflow the counter.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crossbeam::deque::{Steal, Stealer, Worker};
use crossbeam::sync::{Parker, Unparker};

pub struct Pool<T: Send> {
  stealers: Vec<Stealer<T>>,
  unparkers: Vec<Unparker>,
  parked_count: AtomicUsize,
  dir_pending: AtomicUsize,
  shutdown: AtomicBool,
}

impl<T: Send> Pool<T> {
  /// Construct a pool with `num_workers` per-thread deques and parkers.
  /// Returns the pool plus the per-worker `Worker<T>` and `Parker`
  /// handles, each of which must be moved into exactly one worker
  /// thread.
  pub fn new(num_workers: usize) -> (Self, Vec<Worker<T>>, Vec<Parker>) {
    let n = num_workers.max(1);
    let mut workers = Vec::with_capacity(n);
    for _ in 0..n {
      workers.push(Worker::<T>::new_lifo());
    }
    let stealers: Vec<Stealer<T>> = workers.iter().map(|w| w.stealer()).collect();

    let mut parkers = Vec::with_capacity(n);
    let mut unparkers = Vec::with_capacity(n);
    for _ in 0..n {
      let p = Parker::new();
      unparkers.push(p.unparker().clone());
      parkers.push(p);
    }

    let pool = Pool {
      stealers,
      unparkers,
      parked_count: AtomicUsize::new(0),
      dir_pending: AtomicUsize::new(0),
      shutdown: AtomicBool::new(false),
    };
    (pool, workers, parkers)
  }

  #[inline]
  pub fn add_dirs(&self, n: usize) {
    if n > 0 {
      self.dir_pending.fetch_add(n, Ordering::Release);
    }
  }

  #[inline]
  pub fn sub_dirs(&self, n: usize) {
    if n > 0 {
      self.dir_pending.fetch_sub(n, Ordering::AcqRel);
    }
  }

  /// Wake parked workers if any exist. Cheap fast-path when nobody is
  /// parked (steady-state during the bulk of a walk). Broadcasts to
  /// every unparker; an attempted round-robin selective wake regressed
  /// `wide` by 1.17× because the cursor sometimes landed on an active
  /// worker (a no-op unpark) and missed a parked one. At the j=4
  /// default the broadcast is 4 cheap unparks per push and parker
  /// permits coalesce wakes that happen during steady-state walks.
  #[inline]
  pub fn maybe_unpark(&self) {
    if self.parked_count.load(Ordering::Acquire) > 0 {
      for u in &self.unparkers {
        u.unpark();
      }
    }
  }

  /// Try stealing one item from peer deques, starting at `my_id + 1`
  /// and wrapping. Returns the stolen item or None if all peers empty.
  #[inline]
  pub fn try_steal(&self, my_id: usize) -> Option<T> {
    let n = self.stealers.len();
    for k in 1..n {
      let idx = (my_id + k) % n;
      loop {
        match self.stealers[idx].steal() {
          Steal::Success(t) => return Some(t),
          Steal::Empty => break,
          Steal::Retry => continue,
        }
      }
    }
    None
  }

  fn signal_shutdown(&self) {
    self.shutdown.store(true, Ordering::Release);
    for u in &self.unparkers {
      u.unpark();
    }
  }
}

/// Worker steady-state loop. Pops local; steals; parks. Returns when
/// the pool detects global quiescence.
pub fn worker_loop<T, F>(
  id: usize,
  local: &Worker<T>,
  pool: &Pool<T>,
  parker: &Parker,
  mut walk: F,
) where
  T: Send,
  F: FnMut(T, &Worker<T>),
{
  'outer: loop {
    // 1. Local pop (zero atomics on the producer side).
    if let Some(t) = local.pop() {
      walk(t, local);
      continue;
    }

    // 2. Steal sweep. `try_steal` already iterates every peer once
    //    and resolves Steal::Retry internally, so a single call
    //    suffices.
    if let Some(t) = pool.try_steal(id) {
      walk(t, local);
      continue 'outer;
    }

    // 3. About to park: announce, then re-check work. The SeqCst
    //    fence here pairs with the producer's push+unpark sequence so
    //    that a producer who pushed before our re-check is guaranteed
    //    visible to our steal.
    pool.parked_count.fetch_add(1, Ordering::SeqCst);

    if let Some(t) = pool.try_steal(id) {
      pool.parked_count.fetch_sub(1, Ordering::Relaxed);
      walk(t, local);
      continue;
    }

    // 4. Quiescence: if pending == 0 we're done; broadcast and exit.
    if pool.dir_pending.load(Ordering::Acquire) == 0 {
      pool.parked_count.fetch_sub(1, Ordering::Relaxed);
      pool.signal_shutdown();
      return;
    }

    // 5. Park until producer unparks (or shutdown broadcast).
    parker.park();
    pool.parked_count.fetch_sub(1, Ordering::Relaxed);

    if pool.shutdown.load(Ordering::Acquire) {
      return;
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;
  use std::thread;

  #[test]
  fn pool_drains_with_one_worker() {
    let (pool, mut workers, parkers) = Pool::<u32>::new(1);
    let pool = Arc::new(pool);
    pool.add_dirs(3);
    workers[0].push(1);
    workers[0].push(2);
    workers[0].push(3);

    let mut workers = workers.into_iter();
    let mut parkers = parkers.into_iter();
    let local = workers.next().unwrap();
    let parker = parkers.next().unwrap();

    let counter = Arc::new(AtomicUsize::new(0));
    let pool_c = Arc::clone(&pool);
    let counter_c = Arc::clone(&counter);
    thread::spawn(move || {
      worker_loop(0, &local, &pool_c, &parker, |_v, _w| {
        counter_c.fetch_add(1, Ordering::Relaxed);
        pool_c.sub_dirs(1);
      });
    })
    .join()
    .unwrap();

    assert_eq!(counter.load(Ordering::Relaxed), 3);
  }

  #[test]
  fn pool_drains_with_stealing() {
    let (pool, workers, parkers) = Pool::<u32>::new(4);
    let pool = Arc::new(pool);
    let mut workers = workers;
    pool.add_dirs(40);
    for v in 0..40 {
      workers[0].push(v);
    }

    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for (id, (local, parker)) in workers.into_iter().zip(parkers.into_iter()).enumerate() {
      let pool_c = Arc::clone(&pool);
      let counter_c = Arc::clone(&counter);
      handles.push(thread::spawn(move || {
        worker_loop(id, &local, &pool_c, &parker, |_v, _w| {
          counter_c.fetch_add(1, Ordering::Relaxed);
          pool_c.sub_dirs(1);
        });
      }));
    }
    for h in handles {
      h.join().unwrap();
    }
    assert_eq!(counter.load(Ordering::Relaxed), 40);
  }
}
