//! Single-stage directory walker with per-thread work-stealing deque.
//!
//! Each worker pops a directory (locally or stolen), reads its entries,
//! and inline-applies the filter to files (counter or output buffer).
//! Subdirectories are pushed onto the worker's own `Worker<PathBuf>`
//! deque, which costs no atomic; idle peers steal from it. There is
//! no shared MPMC queue and no per-loop yield.

use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam::deque::Worker;

use crate::cli::{FilterConfig, OutputConfig};
use crate::config::Config;
use crate::process::{glob_match, name_matches_extension};
use crate::queue::Pool;

/// Per-worker scratch state. Lives for the entire worker lifetime.
pub struct WorkerState {
  pub out: Vec<u8>,
  pub local_count: u64,
  pub local_size: u64,
  pub child_dirs: Vec<PathBuf>,
}

impl WorkerState {
  pub fn new(out_cap: usize, dir_batch: usize) -> Self {
    Self {
      out: Vec::with_capacity(out_cap),
      local_count: 0,
      local_size: 0,
      child_dirs: Vec::with_capacity(dir_batch),
    }
  }
}

/// Threshold at which the worker flushes its output buffer to stdout.
const OUT_FLUSH_BYTES: usize = 256 * 1024;

/// Walk one already-popped directory. Pushes any subdirs found onto
/// the caller's local deque and decrements `dir_pending` once done.
pub fn walk_one(
  dir: PathBuf,
  _config: &Config,
  filter: &FilterConfig,
  output: &OutputConfig,
  pool: &Pool<PathBuf>,
  local: &Worker<PathBuf>,
  state: &mut WorkerState,
) {
  let read_dir = match fs::read_dir(&dir) {
    Ok(rd) => rd,
    Err(_) => {
      pool.sub_dirs(1);
      return;
    }
  };

  let need_size = output.sum_size
    || output.long_format
    || filter.min_size.is_some()
    || filter.max_size.is_some();
  let count_only = output.count_only;
  let print_paths = !count_only;
  let dirs_match = filter.match_dirs;
  let exts = &filter.extensions;
  let name_pat = filter.name_pattern.as_deref();
  let need_file_name = print_paths || !exts.is_empty() || name_pat.is_some() || need_size;

  let dir_bytes = dir.as_os_str().as_bytes();
  let dir_needs_sep = !dir_bytes.is_empty() && *dir_bytes.last().unwrap() != b'/';

  state.child_dirs.clear();

  for entry in read_dir {
    let entry = match entry {
      Ok(e) => e,
      Err(_) => continue,
    };
    let ft = match entry.file_type() {
      Ok(t) => t,
      Err(_) => continue,
    };

    if ft.is_file() && !dirs_match {
      // Hot path for `count_only` with no filter: skip the OsString
      // alloc that `entry.file_name()` would do — we only need the
      // dirent type, which we already have.
      if !need_file_name {
        state.local_count += 1;
        continue;
      }
    }

    let name = entry.file_name();
    let name_bytes = name.as_bytes();

    if ft.is_dir() {
      if should_skip_dir_bytes(name_bytes, filter) {
        continue;
      }
      let mut child = PathBuf::with_capacity(dir_bytes.len() + 1 + name_bytes.len());
      child.push(&dir);
      child.push(&name);

      if dirs_match && name_matches(name_bytes, exts, name_pat) {
        write_match(
          state,
          dir_bytes,
          dir_needs_sep,
          name_bytes,
          &child,
          need_size,
          print_paths,
          output,
        );
      }

      state.child_dirs.push(child);
    } else if ft.is_file() && !dirs_match {
      if !name_matches(name_bytes, exts, name_pat) {
        continue;
      }

      let size = if need_size {
        match entry.metadata() {
          Ok(m) => Some(m.len()),
          Err(_) => continue,
        }
      } else {
        None
      };

      if let Some(min) = filter.min_size {
        if size.map_or(true, |s| s < min) {
          continue;
        }
      }
      if let Some(max) = filter.max_size {
        if size.map_or(true, |s| s > max) {
          continue;
        }
      }

      state.local_count += 1;
      if let Some(s) = size {
        state.local_size += s;
      }

      if print_paths {
        if output.long_format {
          let _ = write!(&mut state.out, "{:>12}  ", size.unwrap_or(0));
          state.out.extend_from_slice(dir_bytes);
          if dir_needs_sep {
            state.out.push(b'/');
          }
          state.out.extend_from_slice(name_bytes);
          state.out.push(b'\n');
        } else {
          state.out.extend_from_slice(dir_bytes);
          if dir_needs_sep {
            state.out.push(b'/');
          }
          state.out.extend_from_slice(name_bytes);
          state.out.push(b'\n');
        }
        if state.out.len() >= OUT_FLUSH_BYTES {
          flush_out(&mut state.out);
        }
      }
    }
  }

  // Publish discovered subdirs to the local deque. add_dirs MUST happen
  // before any push so a stealer's later sub_dirs can't underflow.
  let n = state.child_dirs.len();
  if n > 0 {
    pool.add_dirs(n);
    for child in state.child_dirs.drain(..) {
      local.push(child);
    }
    pool.maybe_unpark();
  }

  pool.sub_dirs(1);
}

/// Drain `state.out` and `state.local_count` into globals. Called at worker
/// exit.
pub fn flush_worker_final(
  state: &mut WorkerState,
  matched: &AtomicU64,
  total_size: &AtomicU64,
) {
  flush_out(&mut state.out);
  if state.local_count > 0 {
    matched.fetch_add(state.local_count, Ordering::Relaxed);
    state.local_count = 0;
  }
  if state.local_size > 0 {
    total_size.fetch_add(state.local_size, Ordering::Relaxed);
    state.local_size = 0;
  }
}

#[allow(clippy::too_many_arguments)]
fn write_match(
  state: &mut WorkerState,
  dir_bytes: &[u8],
  dir_needs_sep: bool,
  name_bytes: &[u8],
  full_path: &Path,
  need_size: bool,
  print_paths: bool,
  output: &OutputConfig,
) {
  let size = if need_size {
    fs::metadata(full_path).map(|m| m.len()).ok()
  } else {
    None
  };
  state.local_count += 1;
  if let Some(s) = size {
    state.local_size += s;
  }
  if print_paths {
    if output.long_format {
      let _ = write!(&mut state.out, "{:>12}  ", size.unwrap_or(0));
    }
    state.out.extend_from_slice(dir_bytes);
    if dir_needs_sep {
      state.out.push(b'/');
    }
    state.out.extend_from_slice(name_bytes);
    state.out.push(b'\n');
    if state.out.len() >= OUT_FLUSH_BYTES {
      flush_out(&mut state.out);
    }
  }
}

#[inline]
fn name_matches(name: &[u8], exts: &[String], pat: Option<&str>) -> bool {
  if !exts.is_empty() && !any_extension_matches(name, exts) {
    return false;
  }
  if let Some(p) = pat {
    if !glob_match(p.as_bytes(), name) {
      return false;
    }
  }
  true
}

#[inline]
fn any_extension_matches(name: &[u8], exts: &[String]) -> bool {
  for e in exts {
    if name_matches_extension(name, e.as_bytes()) {
      return true;
    }
  }
  false
}

fn flush_out(buf: &mut Vec<u8>) {
  if buf.is_empty() {
    return;
  }
  let stdout = std::io::stdout();
  let mut h = stdout.lock();
  match h.write_all(buf) {
    Ok(()) => buf.clear(),
    // Downstream pipe closed (e.g. `pfind ... | head`). Stop walking
    // immediately rather than burning syscalls producing output that
    // nobody will read. Match fd/rg/bfs behaviour.
    Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
      std::process::exit(0);
    }
    Err(e) => {
      eprintln!("pfind: write error: {e}");
      std::process::exit(1);
    }
  }
}

fn should_skip_dir_bytes(name: &[u8], filter: &FilterConfig) -> bool {
  if filter.skip_dirs.is_empty() {
    return false;
  }
  for skip in &filter.skip_dirs {
    let s = skip.as_bytes();
    if s == name {
      return true;
    }
    if let Some(suffix) = s.strip_prefix(b"*") {
      if name.len() >= suffix.len() && &name[name.len() - suffix.len()..] == suffix {
        return true;
      }
    }
  }
  false
}
