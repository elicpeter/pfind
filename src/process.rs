//! File processing (Stage B).
//!
//! Responsible for:
//!   - Popping file paths from `FileQueue`
//!   - Running per-file work (regex, hashing, stats, etc.)
//!   - Emitting results to output layer.

use std::alloc::System;
use std::path::PathBuf;

use crate::config::Config;
use crate::queue::FileQueue;

/// Process a batch of files.
///
/// This is where you plug in your "real" processing:
///   - regex search
///   - ML feature extraction
///   - checksum / hashing
///   - etc.
pub fn process_files(
  file_queue: &FileQueue<PathBuf>,
  config: &Config,
) -> bool {
  let files = file_queue.pop_batch(config.file_batch_size);
  if files.is_empty() {
    return false;
  }

  // Do something minimal so the optimizer can't drop it:
  for f in files {
    println!("{}", f.display());
  }
  true
}
