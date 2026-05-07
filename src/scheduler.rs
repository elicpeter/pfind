use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use crate::cli::Cli;
use crate::config::Config;
use crate::queue::{Pool, worker_loop};
use crate::walker::{WorkerState, flush_worker_final, walk_one};

pub struct Scheduler;

impl Scheduler {
    pub(crate) fn run(config: &Config, cli: &Cli) {
        let total = config.effective_threads().max(1);

        let (pool, workers, parkers) = Pool::<PathBuf>::new(total);
        let pool = Arc::new(pool);

        // Seed the root dir on worker 0's local deque. add_dirs first so any
        // stealing peer that grabs it can't underflow on sub_dirs.
        pool.add_dirs(1);
        workers[0].push(cli.root.clone());

        let matched = AtomicU64::new(0);
        let total_size = AtomicU64::new(0);

        let filter = cli.filter_config();
        let output = cli.output_config();

        thread::scope(|s| {
            for (id, (worker, parker)) in workers.into_iter().zip(parkers.into_iter()).enumerate() {
                let pool = Arc::clone(&pool);
                let cfg = config.clone();
                let filter_ref = &filter;
                let output_ref = &output;
                let matched_ref = &matched;
                let size_ref = &total_size;

                s.spawn(move || {
                    // Tiny initial out cap — Vec doubles on push, and most small
                    // walks never need more than a few KB. Big workloads will grow
                    // it to fit; the cost is amortised.
                    let mut state = WorkerState::new(4 * 1024, cfg.dir_batch_size);
                    worker_loop(id, &worker, &pool, &parker, |dir, local| {
                        walk_one(dir, &cfg, filter_ref, output_ref, &pool, local, &mut state);
                    });
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
