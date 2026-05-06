use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crate::cli::Cli;
use crate::config::Config;
use crate::queue::{DirQueue, WorkCounters, WorkQueue};
use crate::walker::{flush_worker_final, walk_one, WorkerState};

pub struct Scheduler;

impl Scheduler {
  pub(crate) fn run(config: &Config, cli: &Cli) {
    let total = config.effective_threads().max(1);
    let dir_q: DirQueue<PathBuf> =
      WorkQueue::new(config.work_queue_backend, config.dir_queue_capacity);

    let counters = WorkCounters::new();
    let matched = AtomicU64::new(0);
    let total_size = AtomicU64::new(0);

    let filter = cli.filter_config();
    let output = cli.output_config();

    counters.add_dirs(1);
    let mut root = cli.root.clone();
    loop {
      match dir_q.try_push(root) {
        Ok(()) => break,
        Err(r) => {
          root = r;
          thread::yield_now();
        }
      }
    }

    thread::scope(|s| {
      for _ in 0..total {
        let dir_q = dir_q.clone();
        let cfg = config.clone();
        let filter_ref = &filter;
        let output_ref = &output;
        let counters_ref = &counters;
        let matched_ref = &matched;
        let size_ref = &total_size;

        s.spawn(move || {
          let mut state = WorkerState::new(64 * 1024, cfg.dir_batch_size);
          worker_loop(
            &dir_q,
            &cfg,
            filter_ref,
            output_ref,
            counters_ref,
            matched_ref,
            size_ref,
            &mut state,
          );
          flush_worker_final(&mut state, matched_ref, size_ref);
        });
      }
    });

    if output.count_only {
      println!("{}", matched.load(Ordering::Relaxed));
    }
    if output.sum_size {
      let n = total_size.load(Ordering::Relaxed);
      println!("total: {} bytes", n);
    }
  }
}

#[allow(clippy::too_many_arguments)]
fn worker_loop(
  dir_q: &DirQueue<PathBuf>,
  config: &Config,
  filter: &crate::cli::FilterConfig,
  output: &crate::cli::OutputConfig,
  counters: &WorkCounters,
  matched: &AtomicU64,
  total_size: &AtomicU64,
  state: &mut WorkerState,
) {
  let mut idle_spins = 0u32;
  loop {
    if walk_one(dir_q, config, filter, output, counters, matched, total_size, state) {
      idle_spins = 0;
      continue;
    }

    // No work: check quiescence then back off. Yield immediately — spin
    // wasted cycles on small datasets where many workers idle once initial
    // dirs are consumed.
    if counters.all_quiet() {
      return;
    }
    idle_spins = idle_spins.saturating_add(1);
    if idle_spins < 1024 {
      thread::yield_now();
    } else {
      thread::sleep(std::time::Duration::from_micros(50));
    }
  }
}
