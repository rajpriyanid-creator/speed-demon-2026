# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | Multi-threaded Rust with mmap and select_nth_unstable (BTreeMap baseline) | run-c90c25d39a4e | 0.291 | 67.8 | 52.34 |
| 2 | Open-addressing EndpointCatalog, single-pass parallel record packing, and contiguous counting sort | run-e7cdfb3f6a83 | 0.1466 | 57.7 | 58.57 |

### Step 2 Optimization Details
- Replaced per-record `BTreeMap` lookups with thread-local open-addressing `LocalCatalog` using linear probing in L1/L2 cache.
- Partitioned 238 MB log across 12 threads with zero mutex contention or thread synchronization during parse.
- Flat record structure packed into 12 bytes (`(local_group_id, latency, is_error)`).
- After threads join, unified local catalogs into canonical sorted ranks and scattered latencies into a single flat buffer using two-pass counting sort.
- Computed percentiles via `select_nth_unstable` directly on contiguous group slices.
- Achieved **0.1466 s (218x faster than Python)** and **58.57 points** on the live Falkenstein board.
