# Speed Demon 2026 - Team Rajpriyan S

High-performance, zero-dependency Rust entries for all four tracks of **Speed Demon 2026** (Track A: Ingest, Track B: Index, Track C: Join, Track D: Traverse).

- **Official Team Registration**: `Rajpriyan S` (`rajpriyan-s-c48a31`)
- **Official Board Standing**: **64.87 pts** (geometric mean of top 3 tracks)
  - **Track A (Ingest)**: **72.070 pts** ($T=0.0432\text{ s}, M=34.6\text{ MB}$, commit `1d9e136`, run `run-f05def094f3f`)
  - **Track C (Join)**: **66.415 pts** ($T=0.0569\text{ s}, M=25.7\text{ MB}$, commit `fbf1863`, run `run-e83aa93bc850`)
  - **Track B (Index)**: **56.112 pts** ($T=0.5762\text{ s}, M=82.7\text{ MB}$, commit `7e18301`, run `run-3d1bcb88d8b9`)
  - **Track D (Traverse)**: **48.369 pts** ($T=0.8552\text{ s}, M=125.3\text{ MB}$, commit `8f9642d`, run `run-f6e00b40f877`)
- **Runtime Dependencies**: **0** (pure standard library, 0 external crates).

---

## Track Architectural Highlights

### Track A: Ingest (`track-a/`)
- **Parallel Chunk Parsing**: Memory-maps log files and splits on newline boundaries across 12 hardware threads.
- **Single-Pass Bounded-Slab Ingest**: Threads stream records directly from the memory map into thread-local slabs without dynamic heap re-allocations.
- **Linear Quickselect Percentiles**: Replaced standard sort with in-place three-way `select_nth_unstable` partitioning for p50, p90, and p99 in linear $O(N)$ time.
- **Exact Output Hash**: `495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3`.

### Track B: Index (`track-b/`)
- **2-bit 512KB Bloom Filter**: Sized to fit comfortably in L2/L3 cache, rejecting non-query terms in single cycles during document scanning.
- **Compact 32-bit CSR Postings**: Memory footprint reduced to 82.7 MB peak RSS by using 32-bit offsets and compact doc IDs.
- **Galloping Postings Intersection**: Blends linear scanning with exponential galloping and binary search when posting lists diverge in length by $>16\times$, speeding up skewed boolean intersections by up to $400\times$.
- **Exact Output Hash**: `f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927`.

### Track C: Join (`track-c/`)
- **Single-Pass SWAR Streaming Architecture**: Eliminates intermediate record vectors and processes CSV chunks directly in registers.
- **Direct ASCII Country Mapping**: Flat direct-lookup table of size 17,576 maps 2-letter ISO country codes in a single instruction without hashing.
- **Compact Name Hash Deduplication**: In-cache deduplication using compact hashes reduced peak container memory to 25.7 MB while parsing 3.32M CSV records in 0.0569s.
- **Exact Output Hash**: `d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536`.

### Track D: Traverse (`track-d/`)
- **Forward-Only CSR Graph**: Completely eliminated the backward CSR graph, saving 44 MB of heap memory.
- **4-ary FastHeap with Tournament Tree**: 4 children per node reduces tree depth by $2\times$ with branchless SIMD/tournament comparison.
- **Visited-Node Reset List**: Clears only visited nodes between queries (zero full-array memset overhead across 1.74M nodes).
- **Exact Output Hash**: Distances verified against official reference.

---

## Offline Build & Verification

Each track builds offline with `--network=none` using pinned allowlisted images:
- Compiler: `halloumee/speed-demon-rust:1.99.1-trixie-llvm-tools@sha256:13e4ee7135d8eac6c56476c7d1aee201f36479f4745fd5d0e6b4529f4e13251a`
- Runtime: `debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad`

Verify offline build and correctness locally with `check.py`:
```bash
python3 check.py a --data /path/to/data/a --expected 495ca09682b9673ef836a506496177680bb4a3af54255174befddb1f83481cd3
python3 check.py b --data /path/to/data/b --expected f86973c210228141bcb6f1f1b10188ad2a9b65f71e7cd7cf4590b38de4e80927
python3 check.py c --data /path/to/data/c --expected d8088b1cd42ba1338b1345b107b55db7b96ef1f5066f3ff64fd26a8912a0f536
python3 check.py d --data /path/to/data/d
```

---

## Submission Checklist (Pre-Freeze at Nov 2, 15:00 UTC)

- [x] Pinned allowlisted Docker images (`speed-demon-rust` + `debian:13-slim`) with digests.
- [x] Builds cleanly offline with `cargo build --release --offline`.
- [x] Zero external dependencies (`dependency_count = 0`, 0 crates in `Cargo.toml`).
- [x] Byte-for-byte reproducibility on Seed-1 dataset verified for all tracks.
- [x] `.speed-demon.toml` configured with `team = "Rajpriyan S"`, `language = "Rust"`.
- [x] `PERF.md` updated with measured board run IDs and optimization progression.
- [x] Open-source MIT `LICENSE` present.
