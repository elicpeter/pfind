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
    // Sweep on macOS/APFS shows j=2 is the sweet spot across small/
    // medium/large datasets: APFS serialises getdirentries per volume
    // so a 3rd+ worker mostly waits on the volume lock and pays
    // park/unpark overhead. Linux per-inode locks scale wider — fall
    // back to logical CPU count there.
    self.num_threads.unwrap_or_else(|| {
      if cfg!(target_os = "macos") {
        2
      } else {
        num_cpus::get()
      }
    })
  }
}
