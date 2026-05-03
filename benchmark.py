#!/usr/bin/env python3
"""
Advanced benchmark suite for pfind performance testing.

Tests the C implementation of pfind against find, fd, and Python alternatives.
"""

import argparse
import json
import os
import random
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional


# ============================================================================
# Configuration
# ============================================================================

@dataclass
class DatasetConfig:
    name: str
    num_dirs: int
    files_per_dir: int
    depth: int = 1

    @property
    def total_files(self) -> int:
        return self.num_dirs * self.files_per_dir


DATASET_SIZES = {
    "tiny":   DatasetConfig("tiny",   10,    100),       # 1K files
    "small":  DatasetConfig("small",  100,   100),       # 10K files
    "medium": DatasetConfig("medium", 100,   1000),      # 100K files
    "large":  DatasetConfig("large",  1000,  1000),      # 1M files
    "deep":   DatasetConfig("deep",   100,   100, depth=5),  # 10K, nested
}

EXTENSIONS = ["jpg", "png", "txt", "json", "py", "bin"]
EXTENSION_WEIGHTS = [0.4, 0.3, 0.1, 0.1, 0.05, 0.05]


@dataclass
class BenchmarkResult:
    tool: str
    scenario: str
    dataset: str
    runs: list = field(default_factory=list)
    files_found: int = 0

    @property
    def mean(self) -> float:
        return statistics.mean(self.runs) if self.runs else 0

    @property
    def stddev(self) -> float:
        return statistics.stdev(self.runs) if len(self.runs) > 1 else 0

    @property
    def min(self) -> float:
        return min(self.runs) if self.runs else 0

    @property
    def max(self) -> float:
        return max(self.runs) if self.runs else 0

    @property
    def files_per_sec(self) -> float:
        return self.files_found / self.mean if self.mean > 0 else 0


# ============================================================================
# Dataset Generation
# ============================================================================

def create_dataset(root: Path, config: DatasetConfig, verbose: bool = True) -> dict:
    """Create a test dataset with realistic file distribution."""
    if verbose:
        print(f"  Creating {config.name} dataset: {config.total_files:,} files...")

    start = time.time()
    extension_counts = {ext: 0 for ext in EXTENSIONS}
    file_count = 0

    random.seed(42)

    def create_files_in_dir(dir_path: Path, num_files: int):
        nonlocal file_count
        dir_path.mkdir(parents=True, exist_ok=True)

        for j in range(num_files):
            ext = random.choices(EXTENSIONS, weights=EXTENSION_WEIGHTS)[0]
            file_name = f"file_{j:06d}.{ext}"
            file_path = dir_path / file_name
            file_path.write_bytes(b"x" * 100)
            extension_counts[ext] += 1
            file_count += 1

    if config.depth == 1:
        # Flat structure
        for i in range(config.num_dirs):
            dir_path = root / f"dir_{i:04d}"
            create_files_in_dir(dir_path, config.files_per_dir)
    else:
        # Nested structure
        dirs_per_level = config.num_dirs // config.depth

        def create_level(base: Path, level: int):
            if level >= config.depth:
                return

            for i in range(dirs_per_level):
                dir_path = base / f"level{level}_dir{i:04d}"
                create_files_in_dir(dir_path, config.files_per_dir // config.depth)
                create_level(dir_path, level + 1)

        create_level(root, 0)

    elapsed = time.time() - start

    stats = {
        "total_files": file_count,
        "extension_counts": extension_counts,
        "creation_time": elapsed,
    }

    if verbose:
        print(f"    Created {stats['total_files']:,} files in {elapsed:.2f}s")
        ext_summary = ', '.join(f'{k}={v:,}' for k, v in extension_counts.items())
        print(f"    Extensions: {ext_summary}")

    return stats


# ============================================================================
# Benchmark Runners
# ============================================================================

def check_tool(name: str, version_arg: str = "--version") -> Optional[str]:
    """Check if tool exists and return version string."""
    try:
        result = subprocess.run(
            [name, version_arg],
            capture_output=True,
            text=True,
            timeout=5
        )
        if result.returncode == 0:
            version = result.stdout.strip().split('\n')[0]
            return version[:60]
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    return None


def run_benchmark(
        cmd: list,
        warmup: int = 1,
        runs: int = 5,
        count_lines: bool = True,
) -> tuple:
    """Run a benchmark command multiple times."""

    # Warmup
    for _ in range(warmup):
        subprocess.run(cmd, capture_output=True, check=False)

    times = []
    lines = 0

    for _ in range(runs):
        start = time.perf_counter()
        result = subprocess.run(cmd, capture_output=True, check=False)
        elapsed = time.perf_counter() - start
        times.append(elapsed)

        if count_lines and result.returncode == 0:
            lines = result.stdout.count(b'\n')

    return times, lines


def benchmark_oswalk(root: Path, extension: Optional[str] = None, runs: int = 5) -> tuple:
    """Benchmark Python os.walk."""
    times = []
    count = 0

    # Warmup
    for dirpath, _, filenames in os.walk(root):
        for f in filenames:
            if extension is None or f.endswith(f".{extension}"):
                pass

    for _ in range(runs):
        start = time.perf_counter()
        count = 0
        for dirpath, _, filenames in os.walk(root):
            for f in filenames:
                if extension is None or f.endswith(f".{extension}"):
                    count += 1
        elapsed = time.perf_counter() - start
        times.append(elapsed)

    return times, count


def benchmark_pathlib(root: Path, pattern: str = "**/*", runs: int = 5) -> tuple:
    """Benchmark pathlib.glob."""
    times = []
    count = 0

    # Warmup
    list(root.glob(pattern))

    for _ in range(runs):
        start = time.perf_counter()
        files = list(root.glob(pattern))
        count = len(files)
        elapsed = time.perf_counter() - start
        times.append(elapsed)

    return times, count


# ============================================================================
# Test Scenarios
# ============================================================================

def run_scenario(
        name: str,
        root: Path,
        dataset_config: DatasetConfig,
        pfind_path: Path,
        runs: int = 5,
        warmup: int = 1,
) -> list:
    """Run a single test scenario across all tools."""

    results = []
    dataset_name = dataset_config.name
    root_str = str(root)

    # Base pfind command (Rust/C++ style CLI):
    #   pfind find [OPTIONS] [PATH...]

    # Define commands for each scenario
    if name == "all_files":
        cmds = {
            # All regular files – no extra filters
            "pfind": [str(pfind_path), root_str],
            "find": ["find", root_str, "-type", "f"],
            "fd": ["fd", "-t", "f", ".", root_str],
        }
        oswalk_ext = None
        pathlib_pattern = "**/*"

    elif name == "extension_jpg":
        cmds = {
            # Extensions in pfind: --extensions EXT1,EXT2,...
            "pfind": [str(pfind_path), "--extensions", "jpg", root_str],
            "find": ["find", root_str, "-name", "*.jpg"],
            "fd": ["fd", "-e", "jpg", ".", root_str],
        }
        oswalk_ext = "jpg"
        pathlib_pattern = "**/*.jpg"

    elif name == "extension_multi":
        cmds = {
            # Multiple extensions: comma-separated
            "pfind": [str(pfind_path), "--extensions", "jpg,png", root_str],
            "find": ["find", root_str, "(", "-name", "*.jpg", "-o", "-name", "*.png", ")"],
            "fd": ["fd", "-e", "jpg", "-e", "png", ".", root_str],
        }
        oswalk_ext = None
        pathlib_pattern = None

    elif name == "regex_pattern":
        cmds = {
            # Regex in pfind: -e / --expr. By default it matches the file name,
            # which matches what fd does here.
            "pfind": [str(pfind_path), "-e", r"file_00[0-9]{4}\.jpg", root_str],
            "find": ["find", root_str, "-regex", r".*/file_00[0-9][0-9][0-9][0-9]\.jpg"],
            "fd": ["fd", r"file_00[0-9]{4}\.jpg", root_str],
        }
        oswalk_ext = None
        pathlib_pattern = None

    elif name == "quiet_exists":
        cmds = {
            # Quiet existence check:
            #   --extensions jpg     only jpg
            #   --max-results 1      stop after first match
            #   --quiet / -q         suppress path output (just like fd -q)
            "pfind": [str(pfind_path), "--extensions", "jpg", "--max-results", "1", "--quiet", root_str ],
            "find": ["find", root_str, "-name", "*.jpg", "-print", "-quit"],
            "fd": ["fd", "-e", "jpg", "-q", ".", root_str],
        }
        oswalk_ext = None
        pathlib_pattern = None

    else:
        return results

    # os.walk benchmark
    if oswalk_ext is not None or name == "all_files":
        times, count = benchmark_oswalk(root, oswalk_ext, runs)
        result = BenchmarkResult("os.walk", name, dataset_name)
        result.runs = times
        result.files_found = count
        results.append(result)

    # pathlib benchmark (skip for large datasets)
    if pathlib_pattern and dataset_config.total_files <= 100_000:
        times, count = benchmark_pathlib(root, pathlib_pattern, runs)
        result = BenchmarkResult("pathlib", name, dataset_name)
        result.runs = times
        result.files_found = count
        results.append(result)

    # Command-line tools
    for tool_name, cmd in cmds.items():
        if tool_name == "pfind":
            if not pfind_path.exists():
                continue
        elif tool_name == "fd":
            if not check_tool("fd"):
                continue
        elif tool_name == "find":
            pass

        times, count = run_benchmark(cmd, warmup=warmup, runs=runs)
        result = BenchmarkResult(tool_name, name, dataset_name)
        result.runs = times
        result.files_found = count
        results.append(result)

    return results


# ============================================================================
# Output Formatting
# ============================================================================

def format_time(seconds: float) -> str:
    if seconds < 0.001:
        return f"{seconds * 1_000_000:.0f}µs"
    elif seconds < 1:
        return f"{seconds * 1000:.1f}ms"
    else:
        return f"{seconds:.3f}s"


def format_rate(files_per_sec: float) -> str:
    if files_per_sec >= 1_000_000:
        return f"{files_per_sec / 1_000_000:.2f}M/s"
    elif files_per_sec >= 1_000:
        return f"{files_per_sec / 1_000:.1f}K/s"
    else:
        return f"{files_per_sec:.0f}/s"


def print_comparison_matrix(all_results: list):
    """Print a comparison matrix showing speedups."""
    by_scenario = {}
    for r in all_results:
        key = (r.scenario, r.dataset)
        if key not in by_scenario:
            by_scenario[key] = {}
        by_scenario[key][r.tool] = r

    all_tools = sorted(set(r.tool for r in all_results))

    print(f"\n{'═' * 80}")
    print(" SPEEDUP MATRIX (vs slowest tool)")
    print(f"{'═' * 80}")

    header = f"{'Scenario':<25}"
    for tool in all_tools:
        header += f" {tool:>10}"
    print(header)
    print("─" * len(header))

    for (scenario, dataset), tools in sorted(by_scenario.items()):
        if not tools:
            continue

        slowest = max(r.mean for r in tools.values() if r.mean > 0)

        row = f"{scenario[:20]:<20} ({dataset:<3})"
        for tool in all_tools:
            if tool in tools and tools[tool].mean > 0:
                speedup = slowest / tools[tool].mean
                row += f" {speedup:>9.1f}x"
            else:
                row += f" {'N/A':>10}"
        print(row)

    print()


def print_summary(all_results: list):
    """Print overall summary statistics."""
    print(f"\n{'═' * 80}")
    print(" SUMMARY")
    print(f"{'═' * 80}")

    by_tool = {}
    for r in all_results:
        if r.tool not in by_tool:
            by_tool[r.tool] = []
        by_tool[r.tool].append(r)

    print("\nAverage performance by tool:")
    print(f"{'Tool':<12} {'Avg Time':>12} {'Avg Rate':>15} {'Total Files':>15}")
    print("─" * 55)

    for tool in sorted(by_tool.keys()):
        results = by_tool[tool]
        avg_time = statistics.mean(r.mean for r in results)
        total_files = sum(r.files_found for r in results)
        total_time = sum(r.mean * len(r.runs) for r in results)
        avg_rate = total_files / total_time if total_time > 0 else 0

        print(f"{tool:<12} {format_time(avg_time):>12} {format_rate(avg_rate):>15} {total_files:>15,}")


# ============================================================================
# Main
# ============================================================================

def main():
    parser = argparse.ArgumentParser(
        description="Advanced benchmark suite for pfind",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )

    parser.add_argument(
        "--sizes",
        nargs="+",
        choices=list(DATASET_SIZES.keys()),
        default=["small", "medium"],
        help="Dataset sizes to test"
    )

    parser.add_argument(
        "--scenarios",
        nargs="+",
        choices=["all_files", "extension_jpg", "extension_multi", "regex_pattern", "quiet_exists"],
        default=["all_files", "extension_jpg", "regex_pattern"],
        help="Scenarios to test"
    )

    parser.add_argument("--runs", type=int, default=5, help="Runs per test")
    parser.add_argument("--warmup", type=int, default=1, help="Warmup runs")
    parser.add_argument("--pfind", type=Path, default=Path("./pfind"), help="Path to pfind")
    parser.add_argument("--output", type=Path, help="Save results to JSON")
    parser.add_argument("--keep-dataset", action="store_true", help="Don't delete dataset")
    parser.add_argument("-v", "--verbose", action="store_true")

    args = parser.parse_args()

    args.pfind = args.pfind.expanduser().resolve()

    print("╔════════════════════════════════════════════════════════════════════════════╗")
    print("║                    PFIND ADVANCED BENCHMARK SUITE                          ║")
    print("╚════════════════════════════════════════════════════════════════════════════╝")

    # Check tools
    print("\n▶ Checking available tools...")
    tools = {
        "find": check_tool("find", "--version") or "found",
        "fd": check_tool("fd", "--version"),
        "pfind": str(args.pfind) if args.pfind.exists() else None,
    }

    for name, version in tools.items():
        status = f"✓ {version}" if version else "✗ not found"
        print(f"  {name:<10} {status}")

    if not tools["pfind"]:
        print(f"\n⚠ pfind not found at {args.pfind}")
        print("  Build with: make")

    all_results = []

    tmpdir = tempfile.mkdtemp(prefix="pfind_bench_")
    base_dir = Path(tmpdir)

    try:
        for size_name in args.sizes:
            config = DATASET_SIZES[size_name]

            print(f"\n{'▶'} Dataset: {size_name} ({config.total_files:,} files)")
            print("─" * 60)

            dataset_path = base_dir / f"dataset_{size_name}"
            dataset_path.mkdir(parents=True, exist_ok=True)
            create_dataset(dataset_path, config, verbose=True)

            for scenario in args.scenarios:
                print(f"\n  ▷ Scenario: {scenario}")

                results = run_scenario(
                    scenario,
                    dataset_path,
                    config,
                    args.pfind,
                    runs=args.runs,
                    warmup=args.warmup,
                )

                all_results.extend(results)

                if results:
                    fastest = min(r.mean for r in results if r.mean > 0)
                    for r in sorted(results, key=lambda x: x.mean if x.mean > 0 else float('inf')):
                        is_fastest = r.mean == fastest and r.mean > 0
                        indicator = "★" if is_fastest else " "
                        print(
                            f"    {indicator} {r.tool:<12} "
                            f"{format_time(r.mean):>10} ± {format_time(r.stddev):<10} "
                            f"({r.files_found:,} files, {format_rate(r.files_per_sec)})"
                        )

        print_comparison_matrix(all_results)
        print_summary(all_results)

        if args.output:
            output_data = {
                "metadata": {
                    "timestamp": time.strftime("%Y-%m-%d %H:%M:%S"),
                    "runs_per_test": args.runs,
                    "sizes_tested": args.sizes,
                    "scenarios_tested": args.scenarios,
                },
                "tools": tools,
                "results": [
                    {
                        "tool": r.tool,
                        "scenario": r.scenario,
                        "dataset": r.dataset,
                        "mean": r.mean,
                        "stddev": r.stddev,
                        "min": r.min,
                        "max": r.max,
                        "files_found": r.files_found,
                        "files_per_sec": r.files_per_sec,
                        "runs": r.runs,
                    }
                    for r in all_results
                ],
            }

            with open(args.output, "w") as f:
                json.dump(output_data, f, indent=2)
            print(f"\n✓ Results saved to {args.output}")

    finally:
        if not args.keep_dataset:
            print(f"\n▶ Cleaning up temporary dataset...")
            shutil.rmtree(tmpdir, ignore_errors=True)
        else:
            print(f"\n▶ Dataset kept at: {tmpdir}")

    print("\n✓ Benchmark complete!")
    return 0


if __name__ == "__main__":
    sys.exit(main())