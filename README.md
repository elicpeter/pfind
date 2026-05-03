# Parallel Directory‑Traversal & File‑Processing Pipeline

A multi‑threaded pipeline (in Rust) for walking a directory tree in parallel, queueing file paths, and then processing them concurrently — using a dynamic, work‑stealing + batching + dual‑queue architecture for robust load balancing and throughput.

## 🎯 Goals & Motivation

* Traverse very large or irregular directory hierarchies efficiently in parallel.
* Decouple traversal (I/O + metadata) from file processing (regex, stats, etc.) for better concurrency.
* Dynamically balance work between directory‑walking and file‑processing stages, to avoid idle workers.
* Minimize scheduling overhead (queue operations, locking) via batching.
* Adapt to skewed / unbalanced directory trees (deep trees, directories with many children, widely varying file counts).

## 🧩 Architecture Overview

```
┌──────────┐        ┌────────────┐         ┌──────────────┐
│ DirQueue │─ pop ─▶│ Stage A    │─ push ─▶│ DirQueue     │  (subdirectories)
│ (dirs)   │        │ (walker)   │         └──────────────┘  (batch or single)
│          │        └────────────┘         ┌──────────────┐        ┌─────────────┐
│          │                └────── push ─▶│ FileQueue    │─ pop ─▶│ Stage B     │─▶ output
│          │                               └──────────────┘        │ (processor) │
└──────────┘                                                       └─────────────┘                                                 
```

* **Two shared queues**:

    * `DirQueue`: directories/sub‑trees yet to be explored.
    * `FileQueue`: file paths found during traversal, ready for processing.
* **Worker threads (pool)**: a fixed number (e.g. N threads).
* Each worker repeatedly:

    1. Attempts to pop from `FileQueue` — if non‑empty → process file (Stage B).
    2. Else, attempts to pop from `DirQueue` — if non‑empty → traverse directory (Stage A), discovering sub‑directories and files.
    3. If both queues empty: attempt *work‑stealing* (steal sub‑trees / directory tasks from other threads), or terminate when global work done.
* **Batching (tunable)**: when exploring a directory, instead of enqueuing every file or subdirectory individually, the walker may enqueue them in batches (configurable batch size). Reduces per‑item queue overhead.
* **Dynamic worker allocation**: no rigid "5 threads for walk, 11 for process" — threads adapt based on queue emptiness / fullness. Idle threads steal work as needed.

## 🔄 Work-stealing & Load Balancing

We leverage a **work‑stealing scheduler**: if a thread’s local work is exhausted (queues empty), it can steal tasks (directories/sub‑trees) from other threads’ queues. This helps handle irregular directory structures, uneven subtree sizes, and load imbalance — similar to classic parallel scheduling strategies.

This ensures good utilization even when the directory tree is "unbalanced" (some branches very deep or wide, others shallow).

## ⚙️ Configurable / Tunable Parameters

* **Batch size** for directory and file enqueueing — e.g. number of subdirectories or files per batch.
* **Number of worker threads** (pool size).
* **Stealing thresholds** / heuristics — e.g. minimum batch size before enqueuing, when to push vs. process immediately.
* **Queue data structures** — global shared queue vs per‑thread local deque with steal logic (depending on implementation).

## 📝 When to Use This Design / Trade‑offs

### ✅ Good For

* Large directory trees, deep or with uneven branching.
* Workloads where file processing is decoupled from traversal (e.g. regex search, metadata stats).
* Multi‑core/multi‑thread environments where you want high throughput and full CPU utilization.

### ⚠️ Consider Carefully When

* Per‑file processing is trivially cheap: queue overhead and locking might dominate. Batching helps, but there’s still overhead.
* Directory structure is extremely shallow / trivial — the overhead of concurrency may outweigh benefits.
* Order matters (you need deterministic traversals / file ordering) — since work‑stealing and batching break strict ordering.
