//! Runtime configuration for the parallel directory traversal scanner.

#[derive(Debug, Clone)]
pub struct Config {
    /// Number of worker threads in the pool.
    ///
    /// If `None`, defaults to `2` on macOS (the APFS volume lock makes
    /// more workers worse) and `num_cpus::get()` on other platforms.
    pub num_threads: Option<usize>,

    /// Initial capacity hint for the per-worker child-dir scratch Vec.
    /// Sized to match typical fan-out so one read_dir doesn't trigger a
    /// reallocation.
    pub dir_batch_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            num_threads: None,
            dir_batch_size: 4,
        }
    }
}

impl Config {
    pub fn from_cli(cli: &crate::cli::Cli) -> Self {
        let mut cfg = Self::default();
        if let Some(threads) = cli.threads {
            cfg.num_threads = Some(threads);
        }
        cfg
    }

    pub fn effective_threads(&self) -> usize {
        // Sweep on macOS/APFS (M2 Max, APFS, idle system) across every
        // scenario × size cell shows j=4 wins by 1.21-1.55× over j=2 and
        // by 1.13× over j=3. j=5+ regresses sharply as the APFS volume
        // lock saturates and excess workers stack on park/unpark. Linux
        // per-inode locks scale wider — fall back to logical CPU count
        // there.
        self.num_threads.unwrap_or_else(|| {
            if cfg!(target_os = "macos") {
                4
            } else {
                num_cpus::get()
            }
        })
    }
}
