#!/usr/bin/env python3
"""
pfind benchmark suite — baseline edition.

Goals:
  * Reproducible datasets (seeded).
  * Accurate timing via hyperfine when available; fallback to in-process timer.
  * Sweep pfind's own knobs (threads, backend, traversal order) so future
    optimizations produce comparable curves.
  * Honest scenario tagging: filter scenarios are marked [walker-only] until
    process.rs actually filters.
  * Compare against fd / find / rg / os.walk.
  * Capture system + tool versions for paper appendix.
  * Emit stdout summary + JSON + Markdown report.

Usage:
    python3 benchmark.py --build --sizes small medium --output bench.json
    python3 benchmark.py --quick                  # tiny dataset, smoke test
    python3 benchmark.py --sweeps threads backend # pfind-internal sweeps
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import random
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Optional


# ============================================================================
# Dataset configs
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


DATASET_SIZES: dict[str, DatasetConfig] = {
    "tiny":   DatasetConfig("tiny",   10,    100),                  # 1K
    "small":  DatasetConfig("small",  100,   100),                  # 10K
    "medium": DatasetConfig("medium", 100,   1000),                 # 100K
    "large":  DatasetConfig("large",  1000,  1000),                 # 1M
    "deep":   DatasetConfig("deep",   100,   100, depth=5),         # nested
    "wide":   DatasetConfig("wide",   2000,  50),                   # very flat
}

EXTENSIONS = ["jpg", "png", "txt", "json", "py", "bin"]
EXTENSION_WEIGHTS = [0.4, 0.3, 0.1, 0.1, 0.05, 0.05]


# ============================================================================
# Result types
# ============================================================================

@dataclass
class BenchmarkResult:
    tool: str                       # "pfind", "fd", "find", "rg", "os.walk"
    variant: str                    # tool-specific variant tag (e.g. "stack-bf-j8")
    scenario: str                   # "all_files", "extension_jpg", ...
    dataset: str                    # dataset name
    runs: list[float] = field(default_factory=list)
    files_found: int = 0
    timer: str = "perf_counter"     # "hyperfine" or "perf_counter"
    note: str = ""                  # e.g. "[walker-only]"

    @property
    def mean(self) -> float: return statistics.mean(self.runs) if self.runs else 0.0
    @property
    def stddev(self) -> float: return statistics.stdev(self.runs) if len(self.runs) > 1 else 0.0
    @property
    def min(self) -> float: return min(self.runs) if self.runs else 0.0
    @property
    def max(self) -> float: return max(self.runs) if self.runs else 0.0
    @property
    def files_per_sec(self) -> float: return self.files_found / self.mean if self.mean > 0 else 0.0

    def to_dict(self) -> dict:
        d = asdict(self)
        d["mean"] = self.mean
        d["stddev"] = self.stddev
        d["min"] = self.min
        d["max"] = self.max
        d["files_per_sec"] = self.files_per_sec
        return d


# ============================================================================
# Dataset generation
# ============================================================================

def create_dataset(root: Path, config: DatasetConfig, verbose: bool = True) -> dict:
    if verbose:
        print(f"  Generating {config.name}: target ~{config.total_files:,} files...")

    start = time.time()
    extension_counts = {ext: 0 for ext in EXTENSIONS}
    file_count = [0]

    rnd = random.Random(42)

    def make_files(dir_path: Path, n: int) -> None:
        dir_path.mkdir(parents=True, exist_ok=True)
        for j in range(n):
            ext = rnd.choices(EXTENSIONS, weights=EXTENSION_WEIGHTS)[0]
            (dir_path / f"file_{j:06d}.{ext}").write_bytes(b"x" * 100)
            extension_counts[ext] += 1
            file_count[0] += 1

    if config.depth == 1:
        for i in range(config.num_dirs):
            make_files(root / f"dir_{i:04d}", config.files_per_dir)
    else:
        dirs_per_level = max(1, config.num_dirs // config.depth)
        files_per_level = max(1, config.files_per_dir // config.depth)

        def recurse(base: Path, level: int) -> None:
            if level >= config.depth:
                return
            for i in range(dirs_per_level):
                d = base / f"L{level}_d{i:04d}"
                make_files(d, files_per_level)
                recurse(d, level + 1)

        recurse(root, 0)

    elapsed = time.time() - start
    stats = {
        "total_files": file_count[0],
        "extension_counts": extension_counts,
        "creation_time": elapsed,
    }
    if verbose:
        print(f"    Created {stats['total_files']:,} files in {elapsed:.2f}s")
    return stats


# ============================================================================
# Tool detection
# ============================================================================

def check_tool(name: str, version_arg: str = "--version") -> Optional[str]:
    if shutil.which(name) is None:
        return None
    for arg in (version_arg, "-V", "-v"):
        try:
            r = subprocess.run([name, arg], capture_output=True, text=True, timeout=5)
            out = (r.stdout or r.stderr or "").strip()
            if out:
                return out.split("\n")[0][:80]
        except (FileNotFoundError, subprocess.TimeoutExpired):
            return None
    # Tool exists but no version output — still report presence.
    return f"{name} (version unknown)"


def have(name: str) -> bool:
    return shutil.which(name) is not None


# ============================================================================
# Build pfind
# ============================================================================

def build_pfind(repo_root: Path, force: bool = False) -> Path:
    target = repo_root / "target" / "release" / "pfind"
    if force or not target.exists():
        print("▶ cargo build --release ...")
        r = subprocess.run(
            ["cargo", "build", "--release"],
            cwd=repo_root,
            capture_output=True,
            text=True,
        )
        if r.returncode != 0:
            print(r.stdout)
            print(r.stderr, file=sys.stderr)
            sys.exit(f"cargo build failed (exit {r.returncode})")
    if not target.exists():
        sys.exit(f"build succeeded but binary missing at {target}")
    return target


# ============================================================================
# Runners
# ============================================================================

def count_results(stdout: bytes) -> int:
    """Count results from a tool's stdout.

    Most tools print one match per line, so we just count newlines. But pfind
    `-c` (and similar tools) print a single integer summary; detect that and
    return the integer. A trailing newline is allowed.
    """
    if not stdout:
        return 0
    s = stdout.strip()
    if s.isdigit():
        try:
            return int(s)
        except ValueError:
            pass
    return stdout.count(b"\n")


def run_with_hyperfine(
        cmd: list[str],
        warmup: int,
        runs: int,
) -> tuple[list[float], int]:
    """Run command via hyperfine. Returns (per-run seconds, lines from one final run)."""
    with tempfile.NamedTemporaryFile(mode="r", suffix=".json", delete=False) as f:
        json_path = f.name
    try:
        # Hyperfine treats each `--` arg as a separate benchmark, so the
        # full command must be passed as ONE string. `-N` disables shell
        # wrapping; hyperfine then splits the string itself (shlex-style).
        hf_cmd = [
            "hyperfine",
            "-N",
            "--warmup", str(max(warmup, 0)),
            "--runs", str(max(runs, 2)),
            "--export-json", json_path,
            "--",
            shlex.join(cmd),
        ]
        r = subprocess.run(hf_cmd, capture_output=True, text=True)
        if r.returncode != 0:
            return [], 0
        with open(json_path) as f:
            data = json.load(f)
        times = data["results"][0]["times"]

        # Run once more to capture line count (hyperfine discards stdout).
        out = subprocess.run(cmd, capture_output=True)
        lines = count_results(out.stdout) if out.returncode == 0 else 0
        return times, lines
    finally:
        try:
            os.unlink(json_path)
        except OSError:
            pass


def run_with_python_timer(
        cmd: list[str],
        warmup: int,
        runs: int,
) -> tuple[list[float], int]:
    for _ in range(warmup):
        subprocess.run(cmd, capture_output=True, check=False)
    times: list[float] = []
    lines = 0
    for _ in range(runs):
        start = time.perf_counter()
        r = subprocess.run(cmd, capture_output=True, check=False)
        times.append(time.perf_counter() - start)
        if r.returncode == 0:
            lines = count_results(r.stdout)
    return times, lines


def run_cmd(
        cmd: list[str],
        warmup: int,
        runs: int,
        use_hyperfine: bool,
) -> tuple[list[float], int, str]:
    if use_hyperfine and have("hyperfine"):
        times, lines = run_with_hyperfine(cmd, warmup, runs)
        if times:
            return times, lines, "hyperfine"
    times, lines = run_with_python_timer(cmd, warmup, runs)
    return times, lines, "perf_counter"


def benchmark_oswalk(root: Path, ext: Optional[str], runs: int) -> tuple[list[float], int]:
    # Warmup
    for _ in os.walk(root):
        pass
    times = []
    count = 0
    for _ in range(runs):
        start = time.perf_counter()
        count = 0
        for _, _, files in os.walk(root):
            if ext is None:
                count += len(files)
            else:
                suf = f".{ext}"
                count += sum(1 for f in files if f.endswith(suf))
        times.append(time.perf_counter() - start)
    return times, count


# ============================================================================
# Scenarios
# ============================================================================

# Each scenario yields per-tool command builders.
# pfind variant string captures backend/order/threads.

WALKER_ONLY_NOTE = "[walker-only: filter not wired in process.rs yet]"


def pfind_variants(pfind: Path, root: str, base_args: list[str], sweeps: set[str], max_threads: int) -> list[tuple[str, list[str]]]:
    """Return list of (variant_label, cmd) for pfind based on enabled sweeps."""
    variants: list[tuple[str, list[str]]] = []

    def cmd(extra: list[str]) -> list[str]:
        return [str(pfind), root, *base_args, *extra]

    # Default (no sweep flags): single representative run.
    variants.append(("default", cmd([])))

    if "backend" in sweeps:
        variants.append(("queue", cmd(["--queue"])))
        variants.append(("stack", cmd(["--stack"])))

    if "order" in sweeps:
        variants.append(("breadth-first", cmd(["--breadth-first"])))
        variants.append(("depth-first", cmd(["--depth-first"])))

    if "threads" in sweeps:
        thread_counts = sorted({1, 2, 4, 8, max_threads})
        for j in thread_counts:
            variants.append((f"j{j}", cmd(["-j", str(j)])))

    return variants


SCENARIOS = {
    "all_files": {
        "fair": True,
        "pfind_args": [],
        "fd": lambda root: ["fd", "-t", "f", ".", root],
        "find": lambda root: ["find", root, "-type", "f"],
        "rg": lambda root: ["rg", "--files", root],
        "oswalk_ext": None,
    },
    "extension_jpg": {
        "fair": True,
        "pfind_args": ["-e", "jpg"],
        "fd": lambda root: ["fd", "-e", "jpg", ".", root],
        "find": lambda root: ["find", root, "-name", "*.jpg"],
        "rg": lambda root: ["rg", "--files", "-g", "*.jpg", root],
        "oswalk_ext": "jpg",
    },
    "extension_multi": {
        "fair": True,
        "pfind_args": ["-e", "jpg,png"],
        "fd": lambda root: ["fd", "-e", "jpg", "-e", "png", ".", root],
        "find": lambda root: ["find", root, "(", "-name", "*.jpg", "-o", "-name", "*.png", ")"],
        "rg": lambda root: ["rg", "--files", "-g", "*.jpg", "-g", "*.png", root],
        "oswalk_ext": None,
    },
    "name_glob": {
        "fair": True,
        "pfind_args": ["-n", "file_00*"],
        "fd": lambda root: ["fd", "-g", "file_00*", root],
        "find": lambda root: ["find", root, "-name", "file_00*"],
        "rg": lambda root: ["rg", "--files", "-g", "file_00*", root],
        "oswalk_ext": None,
    },
    "count_only": {
        "fair": True,
        "pfind_args": ["-c"],
        "fd": lambda root: ["fd", "-t", "f", ".", root],     # no native count
        "find": lambda root: ["find", root, "-type", "f"],
        "rg": lambda root: ["rg", "--files", root],
        "oswalk_ext": None,
    },
}


# ============================================================================
# Driver
# ============================================================================

def run_scenario(
        scenario: str,
        spec: dict,
        root: Path,
        dataset: DatasetConfig,
        pfind: Path,
        runs: int,
        warmup: int,
        sweeps: set[str],
        use_hyperfine: bool,
        max_threads: int,
        skip_oswalk: bool,
) -> list[BenchmarkResult]:
    out: list[BenchmarkResult] = []
    root_s = str(root)
    note = "" if spec["fair"] else WALKER_ONLY_NOTE

    # pfind variants
    if pfind.exists():
        for variant, cmd in pfind_variants(pfind, root_s, spec["pfind_args"], sweeps, max_threads):
            times, lines, timer = run_cmd(cmd, warmup, runs, use_hyperfine)
            out.append(BenchmarkResult(
                tool="pfind", variant=variant, scenario=scenario,
                dataset=dataset.name, runs=times, files_found=lines,
                timer=timer, note=note,
            ))

    # External tools
    for tool in ("fd", "find", "rg"):
        if not have(tool):
            continue
        cmd = spec[tool](root_s)
        times, lines, timer = run_cmd(cmd, warmup, runs, use_hyperfine)
        out.append(BenchmarkResult(
            tool=tool, variant="default", scenario=scenario,
            dataset=dataset.name, runs=times, files_found=lines,
            timer=timer, note="",
        ))

    # os.walk (skip on huge datasets unless explicitly enabled)
    if not skip_oswalk and dataset.total_files <= 100_000:
        times, count = benchmark_oswalk(root, spec["oswalk_ext"], runs)
        out.append(BenchmarkResult(
            tool="os.walk", variant="default", scenario=scenario,
            dataset=dataset.name, runs=times, files_found=count,
            timer="perf_counter", note="",
        ))

    return out


# ============================================================================
# Reporting
# ============================================================================

def fmt_time(s: float) -> str:
    if s < 1e-3: return f"{s * 1e6:.0f}µs"
    if s < 1.0: return f"{s * 1e3:.1f}ms"
    return f"{s:.3f}s"


def fmt_rate(r: float) -> str:
    if r >= 1e6: return f"{r/1e6:.2f}M/s"
    if r >= 1e3: return f"{r/1e3:.1f}K/s"
    return f"{r:.0f}/s"


def print_scenario_block(results: list[BenchmarkResult]) -> None:
    if not results:
        return
    fastest = min((r.mean for r in results if r.mean > 0), default=0.0)
    for r in sorted(results, key=lambda x: x.mean if x.mean > 0 else float("inf")):
        star = "★" if r.mean == fastest and r.mean > 0 else " "
        label = f"{r.tool}/{r.variant}"
        print(
            f"    {star} {label:<26} {fmt_time(r.mean):>10} ± {fmt_time(r.stddev):<8} "
            f"({r.files_found:>9,} files, {fmt_rate(r.files_per_sec):>10}) "
            f"[{r.timer}] {r.note}"
        )


def print_correctness(results: list[BenchmarkResult]) -> None:
    """Compare file counts vs fd baseline for fair scenarios."""
    by_key: dict[tuple[str, str], list[BenchmarkResult]] = {}
    for r in results:
        by_key.setdefault((r.scenario, r.dataset), []).append(r)

    print("\n" + "═" * 80)
    print(" CORRECTNESS (file-count parity vs fd)")
    print("═" * 80)
    print(f"{'scenario':<18} {'dataset':<8} {'tool/variant':<28} {'count':>10} {'delta':>10}")
    print("─" * 80)
    for (scenario, dataset), rs in sorted(by_key.items()):
        baseline = next((x.files_found for x in rs if x.tool == "fd"), None)
        for r in rs:
            delta = "—" if baseline is None else f"{r.files_found - baseline:+d}"
            print(f"{scenario:<18} {dataset:<8} {r.tool + '/' + r.variant:<28} {r.files_found:>10,} {delta:>10}")


def print_speedup_matrix(results: list[BenchmarkResult]) -> None:
    by_scenario: dict[tuple[str, str], dict[str, BenchmarkResult]] = {}
    for r in results:
        key = (r.scenario, r.dataset)
        by_scenario.setdefault(key, {})[f"{r.tool}/{r.variant}"] = r

    all_tools = sorted({f"{r.tool}/{r.variant}" for r in results})
    print("\n" + "═" * 80)
    print(" SPEEDUP MATRIX (vs slowest in row)")
    print("═" * 80)
    header = f"{'scenario/dataset':<28}" + "".join(f" {t[:14]:>14}" for t in all_tools)
    print(header)
    print("─" * len(header))

    for (scenario, dataset), tools in sorted(by_scenario.items()):
        slowest = max((r.mean for r in tools.values() if r.mean > 0), default=0.0)
        row = f"{(scenario + '/' + dataset)[:26]:<28}"
        for t in all_tools:
            r = tools.get(t)
            if r and r.mean > 0:
                row += f" {slowest / r.mean:>13.2f}x"
            else:
                row += f" {'—':>14}"
        print(row)


def write_markdown(path: Path, results: list[BenchmarkResult], meta: dict) -> None:
    with open(path, "w") as f:
        f.write("# pfind benchmark report\n\n")
        f.write("## metadata\n\n")
        for k, v in meta.items():
            f.write(f"- **{k}**: `{v}`\n")
        f.write("\n## results\n\n")
        f.write("| scenario | dataset | tool | variant | mean | stddev | files | rate | timer | note |\n")
        f.write("|---|---|---|---|---:|---:|---:|---:|---|---|\n")
        for r in sorted(results, key=lambda x: (x.scenario, x.dataset, x.tool, x.variant)):
            f.write(
                f"| {r.scenario} | {r.dataset} | {r.tool} | {r.variant} "
                f"| {fmt_time(r.mean)} | {fmt_time(r.stddev)} | {r.files_found:,} "
                f"| {fmt_rate(r.files_per_sec)} | {r.timer} | {r.note} |\n"
            )


# ============================================================================
# System info
# ============================================================================

def gather_system_info(repo_root: Path) -> dict:
    info: dict = {
        "timestamp": time.strftime("%Y-%m-%d %H:%M:%S"),
        "platform": platform.platform(),
        "machine": platform.machine(),
        "python": sys.version.split()[0],
        "cpu_count_logical": os.cpu_count(),
    }

    # CPU model
    try:
        if sys.platform == "darwin":
            r = subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True)
            if r.returncode == 0:
                info["cpu_model"] = r.stdout.strip()
        elif sys.platform.startswith("linux"):
            with open("/proc/cpuinfo") as f:
                for line in f:
                    if line.startswith("model name"):
                        info["cpu_model"] = line.split(":", 1)[1].strip()
                        break
    except Exception:
        pass

    # git rev
    try:
        r = subprocess.run(
            ["git", "rev-parse", "--short", "HEAD"],
            cwd=repo_root, capture_output=True, text=True,
        )
        if r.returncode == 0:
            info["git_rev"] = r.stdout.strip()
        r = subprocess.run(
            ["git", "status", "--porcelain"],
            cwd=repo_root, capture_output=True, text=True,
        )
        if r.returncode == 0:
            info["git_dirty"] = bool(r.stdout.strip())
    except Exception:
        pass

    # tool versions
    info["versions"] = {
        "pfind": "release-build",
        "fd": check_tool("fd"),
        "find": check_tool("find"),
        "rg": check_tool("rg"),
        "hyperfine": check_tool("hyperfine"),
        "cargo": check_tool("cargo"),
    }
    return info


# ============================================================================
# Main
# ============================================================================

def main() -> int:
    p = argparse.ArgumentParser(
        description="pfind benchmark suite",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    p.add_argument("--sizes", nargs="+", choices=list(DATASET_SIZES), default=["small", "medium"])
    p.add_argument("--scenarios", nargs="+", choices=list(SCENARIOS), default=list(SCENARIOS))
    p.add_argument("--sweeps", nargs="*", choices=["threads", "backend", "order"], default=[],
                   help="pfind-internal sweeps to run (in addition to default)")
    p.add_argument("--runs", type=int, default=5)
    p.add_argument("--warmup", type=int, default=1)
    p.add_argument("--pfind", type=Path, help="path to pfind binary; default: cargo build target")
    p.add_argument("--build", action="store_true", help="cargo build --release before benchmarking")
    p.add_argument("--rebuild", action="store_true", help="force rebuild even if binary exists")
    p.add_argument("--no-hyperfine", action="store_true", help="never use hyperfine even if installed")
    p.add_argument("--skip-oswalk", action="store_true")
    p.add_argument("--output", type=Path, help="JSON output path")
    p.add_argument("--markdown", type=Path, help="markdown report path")
    p.add_argument("--keep-dataset", action="store_true")
    p.add_argument("--quick", action="store_true", help="tiny dataset, 2 runs, smoke test")
    args = p.parse_args()

    repo_root = Path(__file__).resolve().parent

    if args.quick:
        args.sizes = ["tiny"]
        args.runs = 2
        args.warmup = 0

    # Build / locate binary
    if args.pfind:
        pfind = args.pfind.expanduser().resolve()
        if not pfind.exists():
            sys.exit(f"pfind not found at {pfind}")
    else:
        pfind = build_pfind(repo_root, force=args.rebuild) if (args.build or args.rebuild) \
                else (repo_root / "target" / "release" / "pfind")
        if not pfind.exists():
            print(f"⚠ {pfind} missing — running cargo build --release ...")
            pfind = build_pfind(repo_root, force=False)

    use_hyperfine = (not args.no_hyperfine) and have("hyperfine")
    max_threads = os.cpu_count() or 4

    # Header
    print("=" * 80)
    print(" pfind benchmark suite")
    print("=" * 80)
    sysinfo = gather_system_info(repo_root)
    for k, v in sysinfo.items():
        if k == "versions": continue
        print(f"  {k:<20} {v}")
    print(f"  pfind binary         {pfind}")
    print(f"  hyperfine            {'on' if use_hyperfine else 'off'}")
    print(f"  sweeps               {args.sweeps or 'none (default variant only)'}")
    print()
    print("  tools:")
    for k, v in sysinfo["versions"].items():
        print(f"    {k:<10} {v or 'not found'}")

    # Run
    sweeps = set(args.sweeps)
    all_results: list[BenchmarkResult] = []
    tmpdir = Path(tempfile.mkdtemp(prefix="pfind_bench_"))

    try:
        for size in args.sizes:
            cfg = DATASET_SIZES[size]
            print(f"\n▶ dataset {size} ({cfg.total_files:,} files)")
            ds_root = tmpdir / f"ds_{size}"
            ds_root.mkdir(parents=True, exist_ok=True)
            create_dataset(ds_root, cfg, verbose=True)

            for scenario in args.scenarios:
                spec = SCENARIOS[scenario]
                print(f"\n  ▷ scenario {scenario}{' ' + WALKER_ONLY_NOTE if not spec['fair'] else ''}")
                rs = run_scenario(
                    scenario, spec, ds_root, cfg, pfind,
                    runs=args.runs, warmup=args.warmup, sweeps=sweeps,
                    use_hyperfine=use_hyperfine,
                    max_threads=max_threads,
                    skip_oswalk=args.skip_oswalk,
                )
                all_results.extend(rs)
                print_scenario_block(rs)

        print_speedup_matrix(all_results)
        print_correctness(all_results)

        if args.output:
            with open(args.output, "w") as f:
                json.dump({
                    "metadata": sysinfo | {
                        "runs": args.runs,
                        "warmup": args.warmup,
                        "sizes": args.sizes,
                        "scenarios": args.scenarios,
                        "sweeps": sorted(sweeps),
                    },
                    "results": [r.to_dict() for r in all_results],
                }, f, indent=2)
            print(f"\n✓ JSON  → {args.output}")

        if args.markdown:
            meta = {**sysinfo, "runs": args.runs, "warmup": args.warmup,
                    "sweeps": sorted(sweeps)}
            write_markdown(args.markdown, all_results, meta)
            print(f"✓ MD    → {args.markdown}")

    finally:
        if args.keep_dataset:
            print(f"\n▶ dataset kept at {tmpdir}")
        else:
            shutil.rmtree(tmpdir, ignore_errors=True)

    print("\n✓ done.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
