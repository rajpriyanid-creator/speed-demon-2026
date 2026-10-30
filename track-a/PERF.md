# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | Multi-threaded Rust with mmap and select_nth_unstable (BTreeMap baseline) | run-c90c25d39a4e | 0.291 | 67.8 | 52.34 |
| 2 | Flat array indexing, zero-allocation endpoint catalog, O(N) bucket/counting sort | (pending run) | ~0.04 | ~20 | ~75+ |

### Optimization Journey
- **Step 1 (run-c90c25d39a4e)**:
  - Switched from naive Python (32.05 s) to compiled Rust 2021 with zero runtime dependencies ($D=1$).
  - Memory-mapped `/data/requests.log` with `MAP_SHARED` to utilize host page-cache directly.
  - Partitioned work across 12 logical threads using `std::thread::scope`.
  - Achieved **0.291 s (110x speedup)** and scored **52.34 points** on the live Falkenstein board.
- **Step 2**:
  - Replaced tree-based `BTreeMap` and `Box<[u8]>` heap allocations with a pre-indexed `EndpointCatalog` in L1/L2 cache.
  - Mapped group keys to dense integers: `group_id = hour_id * num_endpoints + sorted_ep_id`.
  - Replaced hash table lookups with single-pass counting sort into a flat contiguous latency buffer.
  - Eliminated per-record heap churn and reduced cache misses.
