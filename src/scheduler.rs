use std::path::PathBuf;
use std::thread;
use std::thread::JoinHandle;
use crate::cli::Cli;
use crate::config::Config;
use crate::queue::{make_dir_file_queues, DirQueue, FileQueue};
use crate::walker::walk_dirs;
use crate::process::process_files;

pub struct Scheduler;

enum Role {
  Walker,
  Processor,
}

impl Scheduler {
  pub(crate) fn run(config: &Config, cli: &Cli) {
    // How many worker threads to run.
    let num_threads = config
      .num_threads
      .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));


    // Create shared dir/file queues for PathBuf tasks.
    let (dir_q, file_q): (DirQueue<PathBuf>, FileQueue<PathBuf>) = make_dir_file_queues(config);

    let _ = dir_q.try_push(cli.root.clone());

    // Spawn N workers in a scoped thread environment so we can borrow `&Config`.
    let total = num_threads;
    let num_stage_b = ((total as f64) * config.worker_split_ratio).round() as usize;
    let num_stage_b = num_stage_b.clamp(1, total - 1);
    let num_stage_a = total - num_stage_b;

    // Stage A biased workers
    for _ in 0..num_stage_a {
      spawn_worker(dir_q.clone(), file_q.clone(), config.clone(), Role::Walker);
    }

    // Stage B biased workers
    for _ in 0..num_stage_b {
      spawn_worker(dir_q.clone(), file_q.clone(), config.clone(), Role::Processor);
    }
  }
}

fn spawn_worker(
  dir_q: DirQueue<PathBuf>,
  file_q: FileQueue<PathBuf>,
  config: Config,
  role: Role,
) -> JoinHandle<()> {
  thread::spawn(move || {
    loop {
      match role {
        Role::Processor => {
          // Prefer files
          if process_files(&file_q, &config) {
            continue;
          }
          // Optionally steal dir work when idle
          if config.allow_steal_dirs_when_idle && walk_dirs(&dir_q, &file_q, &config) {
            continue;
          }
        }
        Role::Walker => {
          // Prefer dirs
          if walk_dirs(&dir_q, &file_q, &config) {
            continue;
          }
          // Optionally steal file work when idle
          if config.allow_steal_files_when_idle && process_files(&file_q, &config) {
            continue;
          }
        }
      }

      // if dir_q.is_empty() && file_q.is_empty() {
      //   break;
      // }

      // thread::yield_now();
    }
  })
}