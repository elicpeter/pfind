//! Directory traversal (Stage A).
//!
//! Responsible for:
//!   - Pulling directories from `DirQueue`
//!   - Reading entries
//!   - Partitioning into subdirectories and files
//!   - Pushing into `DirQueue` and `FileQueue` in batches

use std::fs;
use std::path::PathBuf;

use crate::config::Config;
use crate::queue::{DirQueue, FileQueue};

/// Traverse a directory (or batch of directories) and enqueue its contents.
///
/// This function is intended to be called from worker threads, as part of
/// Stage A. It should be relatively small and non-blocking except for the
/// actual `read_dir` calls.
///
/// For now, this is just an outline.
pub fn walk_dirs(
  dir_queue: &DirQueue<PathBuf>,
  file_queue: &FileQueue<PathBuf>,
  config: &Config,
) -> bool {
  let dir = match dir_queue.try_pop() {
    Some(d) => d,
    None => return false,
  };

  let mut local_dirs = Vec::with_capacity(config.dir_batch_size);
  let mut local_files = Vec::with_capacity(config.file_batch_size);

  let read_dir = match fs::read_dir(&dir) {
    Ok(rd) => rd,
    Err(_) => return true, // treat as "handled", just skip on error
  };

  for entry in read_dir {
    let entry = match entry {
      Ok(e) => e,
      Err(_) => continue,
    };

    let path = entry.path();
    let file_type = match entry.file_type() {
      Ok(ft) => ft,
      Err(_) => continue,
    };

    if file_type.is_dir() {
      local_dirs.push(path);
      if local_dirs.len() >= config.dir_batch_size {
        dir_queue.try_push_batch(local_dirs.drain(..));
      }
    } else if file_type.is_file() {
      local_files.push(path);
      if local_files.len() >= config.file_batch_size {
        file_queue.try_push_batch(local_files.drain(..));
      }
    }
  }


  // flush any leftovers
  if !local_dirs.is_empty() {
    dir_queue.try_push_batch(local_dirs.drain(..));
  }
  if !local_files.is_empty() {
    file_queue.try_push_batch(local_files.drain(..));
  }

  true
}
