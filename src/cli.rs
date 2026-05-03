//! CLI for parallel directory traversal optimized for ML workloads.

use clap::{Parser, ValueEnum};
use std::path::PathBuf;

/// Fast parallel file finder optimized for ML workloads.
///
/// Quickly find models, datasets, checkpoints, and configs across
/// large directory trees with automatic filtering of common junk.
#[derive(Parser, Debug)]
#[command(name = "pfind")]
#[command(author, version, about, long_about = None)]
pub struct Cli {
  /// Root directory to search (defaults to current directory)
  #[arg(default_value = ".")]
  pub root: PathBuf,

  /// File type preset to search for
  #[arg(short = 't', long, value_enum)]
  pub file_type: Option<FileType>,

  /// Custom extensions to match (comma-separated, e.g. "py,ipynb")
  #[arg(short, long, value_delimiter = ',')]
  pub ext: Vec<String>,

  /// Minimum file size (e.g. "100M", "1G", "500K")
  #[arg(long)]
  pub min_size: Option<String>,

  /// Maximum file size (e.g. "100M", "1G", "500K")
  #[arg(long)]
  pub max_size: Option<String>,

  /// Additional directories to skip (comma-separated)
  #[arg(long, value_delimiter = ',')]
  pub skip: Vec<String>,

  /// Don't skip common junk directories (.git, __pycache__, etc.)
  #[arg(long)]
  pub no_skip_defaults: bool,

  /// Only count matching files, don't print paths
  #[arg(short, long)]
  pub count: bool,

  /// Show total size of matching files
  #[arg(short = 's', long)]
  pub sum_size: bool,

  /// Show file sizes alongside paths
  #[arg(short = 'l', long)]
  pub long: bool,

  /// Number of worker threads (defaults to CPU count)
  #[arg(short = 'j', long)]
  pub threads: Option<usize>,

  /// Use depth-first traversal (better for deep trees)
  #[arg(long, conflicts_with = "breadth_first")]
  pub depth_first: bool,

  /// Use breadth-first traversal (better for wide trees)
  #[arg(long, conflicts_with = "depth_first")]
  pub breadth_first: bool,

  /// Use LIFO stack for work queue (unbounded)
  #[arg(long, conflicts_with = "queue")]
  pub stack: bool,

  /// Use FIFO queue for work queue (bounded)
  #[arg(long, conflicts_with = "stack")]
  pub queue: bool,

  /// Print matching directories instead of files
  #[arg(short = 'd', long)]
  pub dirs: bool,

  /// Match files by name pattern (glob-style)
  #[arg(short = 'n', long)]
  pub name: Option<String>,
}

/// Preset file type categories optimized for ML workflows.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum FileType {
  /// Model files: .pt, .pth, .ckpt, .safetensors, .onnx, .h5, .pb, .tflite, .bin
  Models,

  /// Checkpoints: .ckpt, .pt, .pth, .safetensors (same as models but semantic)
  Checkpoints,

  /// Dataset files: .csv, .parquet, .arrow, .tfrecord, .jsonl, .tsv, .npy, .npz
  Data,

  /// Image files: .jpg, .jpeg, .png, .webp, .bmp, .tiff, .gif
  Images,

  /// Audio files: .wav, .mp3, .flac, .ogg, .m4a
  Audio,

  /// Video files: .mp4, .avi, .mkv, .mov, .webm
  Video,

  /// Config files: .yaml, .yml, .json, .toml, .cfg, .ini
  Configs,

  /// Python files: .py, .pyi, .ipynb
  Python,

  /// Text/docs: .txt, .md, .rst, .tex
  Text,

  /// Logs: .log, .out, .err
  Logs,
}

impl FileType {
  /// Get the file extensions for this preset.
  pub fn extensions(&self) -> &'static [&'static str] {
    match self {
      FileType::Models => &[
        "pt", "pth", "ckpt", "safetensors", "onnx", "h5", "hdf5", "pb", "tflite", "bin",
        "model", "weights",
      ],
      FileType::Checkpoints => &["ckpt", "pt", "pth", "safetensors"],
      FileType::Data => &[
        "csv", "parquet", "arrow", "tfrecord", "jsonl", "json", "tsv", "npy", "npz",
        "pkl", "pickle", "feather", "hdf5", "h5",
      ],
      FileType::Images => &[
        "jpg", "jpeg", "png", "webp", "bmp", "tiff", "tif", "gif", "ico",
      ],
      FileType::Audio => &["wav", "mp3", "flac", "ogg", "m4a", "aac", "wma"],
      FileType::Video => &["mp4", "avi", "mkv", "mov", "webm", "flv", "wmv", "m4v"],
      FileType::Configs => &["yaml", "yml", "json", "toml", "cfg", "ini", "conf"],
      FileType::Python => &["py", "pyi", "ipynb", "pyx", "pxd"],
      FileType::Text => &["txt", "md", "rst", "tex", "rtf"],
      FileType::Logs => &["log", "out", "err"],
    }
  }
}

// =============================================================================
// Helper structs to package CLI concerns for the scanner
// =============================================================================

/// Filter criteria extracted from CLI for use by the scanner.
#[derive(Debug, Clone, Default)]
pub struct FilterConfig {
  /// Extensions to match (lowercase, no leading dot).
  pub extensions: Vec<String>,
  /// Directories to skip.
  pub skip_dirs: Vec<String>,
  /// Minimum file size in bytes.
  pub min_size: Option<u64>,
  /// Maximum file size in bytes.
  pub max_size: Option<u64>,
  /// Glob pattern for file names.
  pub name_pattern: Option<String>,
  /// Whether to match directories instead of files.
  pub match_dirs: bool,
}

/// Output options extracted from CLI.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutputConfig {
  /// Only count, don't print paths.
  pub count_only: bool,
  /// Show total size summary.
  pub sum_size: bool,
  /// Show size alongside each path.
  pub long_format: bool,
}

// =============================================================================
// Cli implementation
// =============================================================================

impl Cli {
  /// Build filter configuration from CLI args.
  pub fn filter_config(&self) -> FilterConfig {
    FilterConfig {
      extensions: self.all_extensions(),
      skip_dirs: self.skip_dirs().into_iter().map(String::from).collect(),
      min_size: self.min_size_bytes(),
      max_size: self.max_size_bytes(),
      name_pattern: self.name.clone(),
      match_dirs: self.dirs,
    }
  }

  /// Build output configuration from CLI args.
  pub fn output_config(&self) -> OutputConfig {
    OutputConfig {
      count_only: self.count,
      sum_size: self.sum_size,
      long_format: self.long,
    }
  }

  /// Get all extensions to match (combining preset + custom).
  pub fn all_extensions(&self) -> Vec<String> {
    let mut exts: Vec<String> = self
      .ext
      .iter()
      .map(|e| e.trim_start_matches('.').to_lowercase())
      .collect();

    if let Some(ft) = &self.file_type {
      for ext in ft.extensions() {
        let e = ext.to_string();
        if !exts.contains(&e) {
          exts.push(e);
        }
      }
    }

    exts
  }

  /// Get directories to skip.
  pub fn skip_dirs(&self) -> Vec<&str> {
    let mut dirs: Vec<&str> = self.skip.iter().map(|s| s.as_str()).collect();

    if !self.no_skip_defaults {
      // Common junk directories in ML projects
      const DEFAULTS: &[&str] = &[
        ".git",
        ".hg",
        ".svn",
        "__pycache__",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        "node_modules",
        ".venv",
        "venv",
        ".env",
        "env",
        ".tox",
        ".nox",
        ".eggs",
        "*.egg-info",
        "build",
        "dist",
        ".ipynb_checkpoints",
        "wandb",          // W&B logs
        "mlruns",         // MLflow
        "lightning_logs", // PyTorch Lightning
        "outputs",        // Hydra default
        ".cache",
        "__MACOSX",
        ".DS_Store",
        "Thumbs.db",
      ];
      for d in DEFAULTS {
        if !dirs.contains(d) {
          dirs.push(d);
        }
      }
    }

    dirs
  }

  /// Parse a size string like "100M" or "1G" into bytes.
  pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim().to_uppercase();
    let (num, mult): (&str, u64) = if s.ends_with("G") || s.ends_with("GB") {
      (
        s.trim_end_matches("GB").trim_end_matches("G"),
        1024 * 1024 * 1024,
      )
    } else if s.ends_with("M") || s.ends_with("MB") {
      (
        s.trim_end_matches("MB").trim_end_matches("M"),
        1024 * 1024,
      )
    } else if s.ends_with("K") || s.ends_with("KB") {
      (s.trim_end_matches("KB").trim_end_matches("K"), 1024)
    } else if s.ends_with("B") {
      (s.trim_end_matches("B"), 1)
    } else {
      (&s, 1)
    };

    num.trim()
      .parse::<f64>()
      .map(|n| (n * mult as f64) as u64)
      .map_err(|_| format!("Invalid size: {}", s))
  }

  /// Get min size in bytes.
  pub fn min_size_bytes(&self) -> Option<u64> {
    self.min_size.as_ref().and_then(|s| Self::parse_size(s).ok())
  }

  /// Get max size in bytes.
  pub fn max_size_bytes(&self) -> Option<u64> {
    self.max_size.as_ref().and_then(|s| Self::parse_size(s).ok())
  }
}

/// Format bytes as human-readable size.
pub fn format_size(bytes: u64) -> String {
  const KB: u64 = 1024;
  const MB: u64 = KB * 1024;
  const GB: u64 = MB * 1024;
  const TB: u64 = GB * 1024;

  if bytes >= TB {
    format!("{:.2} TB", bytes as f64 / TB as f64)
  } else if bytes >= GB {
    format!("{:.2} GB", bytes as f64 / GB as f64)
  } else if bytes >= MB {
    format!("{:.2} MB", bytes as f64 / MB as f64)
  } else if bytes >= KB {
    format!("{:.2} KB", bytes as f64 / KB as f64)
  } else {
    format!("{} B", bytes)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_parse_size() {
    assert_eq!(Cli::parse_size("100").unwrap(), 100);
    assert_eq!(Cli::parse_size("100B").unwrap(), 100);
    assert_eq!(Cli::parse_size("1K").unwrap(), 1024);
    assert_eq!(Cli::parse_size("1KB").unwrap(), 1024);
    assert_eq!(Cli::parse_size("1M").unwrap(), 1024 * 1024);
    assert_eq!(
      Cli::parse_size("1.5G").unwrap(),
      (1.5 * 1024.0 * 1024.0 * 1024.0) as u64
    );
  }

  #[test]
  fn test_format_size() {
    assert_eq!(format_size(500), "500 B");
    assert_eq!(format_size(1024), "1.00 KB");
    assert_eq!(format_size(1024 * 1024), "1.00 MB");
    assert_eq!(format_size(1024 * 1024 * 1024), "1.00 GB");
  }

  #[test]
  fn test_file_type_extensions() {
    let exts = FileType::Models.extensions();
    assert!(exts.contains(&"pt"));
    assert!(exts.contains(&"safetensors"));
    assert!(exts.contains(&"onnx"));
  }
}