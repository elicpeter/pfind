//! Configuration for the parallel directory traversal scanner.
//!
//! This module defines all tunable parameters for the scanner:
//!   - thread pool size
//!   - batching for directories and files
//!   - queue depth limits
//!   - work-stealing behavior
//!   - idle backoff / logging intervals
//!   - queue backend (FIFO ArrayQueue vs. LIFO TreiberStack)

use std::time::Duration;

/// Which lock-free data structure backs our work queues.
///
/// This is about the concurrent data structure, NOT traversal order.
/// - ArrayQueue: bounded, better memory predictability
/// - TreiberStack: unbounded, naturally LIFO which can help locality
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WorkQueueBackend {
  /// Bounded multi-producer/multi-consumer FIFO queue (`crossbeam_queue::ArrayQueue`).
  #[default]
  BoundedMpmcQueue,

  /// Unbounded LIFO work stack (`crossbeam::sync::TreiberStack`).
  TreiberStack,
}

/// Top-level configuration for the scanner.
#[derive(Debug, Clone)]
pub struct Config {
  /// Number of worker threads in the pool.
  ///
  /// If `None`, the scanner may choose a default (e.g., num_cpus).
  pub num_threads: Option<usize>,

  /// Percentage of workers the scanner will bias toward processing work.
  pub worker_split_ratio: f64, // todo: implement this

  /// Number of directories to pop from the directory queue at once
  /// (batch size).
  ///
  /// Larger batches = fewer queue operations but coarser granularity.
  pub dir_batch_size: usize,

  /// Number of files to pop from the file queue at once (batch size).
  ///
  /// Larger batches = fewer queue operations but coarser granularity.
  pub file_batch_size: usize,

  /// Optional soft cap on total entries in the directory queue.
  ///
  /// If `Some(n)`, producers may throttle when the queue length exceeds `n`.
  pub max_dir_queue_depth: Option<usize>,

  /// Optional soft cap on total entries in the file queue.
  ///
  /// If `Some(n)`, producers may throttle when the queue length exceeds `n`.
  pub max_file_queue_depth: Option<usize>,

  /// Whether workers that are idle on the file side are allowed to
  /// steal directory work (Stage A).
  pub allow_steal_dirs_when_idle: bool,

  /// Whether workers that are idle on the directory side are allowed to
  /// steal file work (Stage B).
  pub allow_steal_files_when_idle: bool,

  /// Minimum duration a worker will sleep when it finds no work in either
  /// queue before retrying.
  pub idle_backoff_min: Duration,

  /// Maximum duration to which the idle backoff may grow (if you implement
  /// exponential backoff).
  pub idle_backoff_max: Duration,

  /// How often the scanner logs high-level stats (queue sizes,
  /// throughput, worker utilization).
  pub log_interval: Duration,

  /// If true, stop the whole pipeline on the first hard error
  /// (e.g., unrecoverable I/O).
  pub stop_on_first_error: bool,

  /// Backend used for `DirQueue` / `FileQueue`.
  pub work_queue_backend: WorkQueueBackend,

  /// Hard capacity for the bounded directory queue when using
  /// `WorkQueueBackend::BoundedMpmcQueue`.
  ///
  /// For `TreiberStack`, this is ignored.
  pub dir_queue_capacity: usize,

  /// Hard capacity for the bounded file queue when using
  /// `WorkQueueBackend::BoundedMpmcQueue`.
  ///
  /// For `TreiberStack`, this is ignored.
  pub file_queue_capacity: usize,

  /// Use depth-first traversal order.
  ///
  /// When true, process deeper directories before shallower ones.
  /// Better for deep trees and cache locality.
  pub depth_first: bool,
}

impl Default for Config {
  fn default() -> Self {
    Self {
      num_threads: None,
      worker_split_ratio: 0.8,
      dir_batch_size: 4,
      file_batch_size: 128,
      max_dir_queue_depth: None,
      max_file_queue_depth: None,
      allow_steal_dirs_when_idle: true,
      allow_steal_files_when_idle: true,
      idle_backoff_min: Duration::from_millis(1),
      idle_backoff_max: Duration::from_millis(50),
      log_interval: Duration::from_secs(2),
      stop_on_first_error: false,
      work_queue_backend: WorkQueueBackend::BoundedMpmcQueue,
      dir_queue_capacity: 16_384,
      file_queue_capacity: 65_536,
      depth_first: false,
    }
  }
}

impl Config {
  /// Start from defaults.
  pub fn new() -> Self {
    Self::default()
  }

  /// Build config from CLI arguments.
  ///
  /// Maps user-facing CLI options to internal config fields.
  pub fn from_cli(cli: &crate::cli::Cli) -> Self {
    let mut cfg = Self::default();

    if let Some(threads) = cli.threads {
      cfg.num_threads = Some(threads);
    }

    // Traversal order (if neither specified, keep config default)
    if cli.depth_first {
      cfg.depth_first = true;
    } else if cli.breadth_first {
      cfg.depth_first = false;
    }

    // Backend data structure (if neither specified, keep config default)
    if cli.stack {
      cfg.work_queue_backend = WorkQueueBackend::TreiberStack;
    } else if cli.queue {
      cfg.work_queue_backend = WorkQueueBackend::BoundedMpmcQueue;
    }

    cfg
  }

  /// Get effective thread count (defaults to CPU count).
  pub fn effective_threads(&self) -> usize {
    self.num_threads.unwrap_or_else(num_cpus::get)
  }

  // -------------------------------------------------------------------------
  // Builder methods
  // -------------------------------------------------------------------------

  /// Set the number of worker threads.
  pub fn with_num_threads(mut self, num_threads: usize) -> Self {
    self.num_threads = Some(num_threads);
    self
  }

  /// Set the worker split ratio.
  pub fn with_worker_split_ratio(mut self, ratio: f64) -> Self {
    self.worker_split_ratio = ratio.clamp(0.0, 1.0);
    self
  }

  /// Set directory batch size.
  pub fn with_dir_batch_size(mut self, batch: usize) -> Self {
    self.dir_batch_size = batch.max(1);
    self
  }

  /// Set the file batch size.
  pub fn with_file_batch_size(mut self, batch: usize) -> Self {
    self.file_batch_size = batch.max(1);
    self
  }

  /// Set max directory queue depth (soft cap).
  pub fn with_max_dir_queue_depth(mut self, depth: Option<usize>) -> Self {
    self.max_dir_queue_depth = depth;
    self
  }

  /// Set max file queue depth (soft cap).
  pub fn with_max_file_queue_depth(mut self, depth: Option<usize>) -> Self {
    self.max_file_queue_depth = depth;
    self
  }

  /// Whether workers idle on file work may steal directory work.
  pub fn with_allow_steal_dirs_when_idle(mut self, allow: bool) -> Self {
    self.allow_steal_dirs_when_idle = allow;
    self
  }

  /// Whether workers idle on directory work may steal file work.
  pub fn with_allow_steal_files_when_idle(mut self, allow: bool) -> Self {
    self.allow_steal_files_when_idle = allow;
    self
  }

  /// Set idle backoff min/max in milliseconds.
  pub fn with_idle_backoff_range(mut self, min_ms: u64, max_ms: u64) -> Self {
    let min = Duration::from_millis(min_ms);
    let max = Duration::from_millis(max_ms.max(min_ms));
    self.idle_backoff_min = min;
    self.idle_backoff_max = max;
    self
  }

  /// Set the logging interval in seconds.
  pub fn with_log_interval_secs(mut self, secs: u64) -> Self {
    self.log_interval = Duration::from_secs(secs.max(1));
    self
  }

  /// Set whether to stop on the first unrecoverable error.
  pub fn with_stop_on_first_error(mut self, stop: bool) -> Self {
    self.stop_on_first_error = stop;
    self
  }

  /// Choose the backend for the work queues.
  pub fn with_work_queue_backend(mut self, backend: WorkQueueBackend) -> Self {
    self.work_queue_backend = backend;
    self
  }

  /// Set hard capacities for the bounded queues (used only when
  /// `WorkQueueBackend::BoundedMpmcQueue` is active).
  pub fn with_queue_capacities(mut self, dir_cap: usize, file_cap: usize) -> Self {
    self.dir_queue_capacity = dir_cap.max(1);
    self.file_queue_capacity = file_cap.max(1);
    self
  }

  /// Set depth-first traversal mode.
  pub fn with_depth_first(mut self, depth_first: bool) -> Self {
    self.depth_first = depth_first;
    self
  }
}