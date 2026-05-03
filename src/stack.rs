//! Lock-free Treiber stack implementation.
//!
//! A classic LIFO stack using compare-and-swap for thread safety.
//! Includes an approximate length counter for heuristics.
//! https://en.wikipedia.org/wiki/Treiber_stack

use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

/// A node in the Treiber stack.
struct Node<T> {
  value: T,
  next: *mut Node<T>,
}

/// A lock-free LIFO stack (Treiber stack).
///
/// This implementation uses atomic operations for thread safety and
/// maintains an approximate length counter for use in load-balancing
/// heuristics.
pub struct TreiberStack<T> {
  head: AtomicPtr<Node<T>>,
  /// Approximate length - may be slightly off under heavy concurrency,
  /// but good enough for heuristics and soft caps.
  len: AtomicUsize,
}

// Safety: The stack is safe to send/share across threads as long as T is Send.
unsafe impl<T: Send> Send for TreiberStack<T> {}
unsafe impl<T: Send> Sync for TreiberStack<T> {}

impl<T> TreiberStack<T> {
  /// Create a new empty stack.
  pub fn new() -> Self {
    TreiberStack {
      head: AtomicPtr::new(ptr::null_mut()),
      len: AtomicUsize::new(0),
    }
  }

  /// Push a value onto the stack.
  ///
  /// This operation is lock-free and will always succeed.
  pub fn push(&self, value: T) {
    let new_node = Box::into_raw(Box::new(Node {
      value,
      next: ptr::null_mut(),
    }));

    loop {
      let old_head = self.head.load(Ordering::Acquire);

      // Safety: new_node is valid and we own it
      unsafe {
        (*new_node).next = old_head;
      }

      if self
        .head
        .compare_exchange_weak(old_head, new_node, Ordering::Release, Ordering::Relaxed)
        .is_ok()
      {
        self.len.fetch_add(1, Ordering::Relaxed);
        return;
      }
    }
  }

  /// Pop a value from the stack.
  ///
  /// Returns `None` if the stack is empty.
  pub fn pop(&self) -> Option<T> {
    loop {
      let old_head = self.head.load(Ordering::Acquire);

      if old_head.is_null() {
        return None;
      }

      // Safety: old_head is not null and was previously allocated by push()
      let next = unsafe { (*old_head).next };

      if self
        .head
        .compare_exchange_weak(old_head, next, Ordering::Release, Ordering::Relaxed)
        .is_ok()
      {
        self.len.fetch_sub(1, Ordering::Relaxed);

        // Safety: we have exclusive access to old_head after successful CAS
        let node = unsafe { Box::from_raw(old_head) };
        return Some(node.value);
      }
    }
  }

  /// Returns the approximate length of the stack.
  ///
  /// NOTE: This is only approximate under concurrent access, as items
  /// may be pushed/popped between reading the counter and using the value.
  /// Fine for heuristics and load-balancing decisions.
  #[inline]
  pub fn len(&self) -> usize {
    self.len.load(Ordering::Relaxed)
  }

  /// Returns true if the stack is (observationally) empty.
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.head.load(Ordering::Acquire).is_null()
  }
}

impl<T> Default for TreiberStack<T> {
  fn default() -> Self {
    Self::new()
  }
}

impl<T> Drop for TreiberStack<T> {
  fn drop(&mut self) {
    while self.pop().is_some() {}
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_push_pop() {
    let stack = TreiberStack::new();

    stack.push(1);
    stack.push(2);
    stack.push(3);

    assert_eq!(stack.len(), 3);
    assert_eq!(stack.pop(), Some(3)); // LIFO
    assert_eq!(stack.pop(), Some(2));
    assert_eq!(stack.pop(), Some(1));
    assert_eq!(stack.pop(), None);
    assert!(stack.is_empty());
  }

  #[test]
  fn test_empty_stack() {
    let stack: TreiberStack<i32> = TreiberStack::new();
    assert!(stack.is_empty());
    assert_eq!(stack.len(), 0);
    assert_eq!(stack.pop(), None);
  }
}