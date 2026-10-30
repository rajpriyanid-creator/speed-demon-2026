# Track D: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 72.69 | 893 | 0 |
| 1 | Compiled zero-dependency Rust CSR representation with parallel Dijkstra and query generation tagging | (pending run) | ~0.10 | ~50 | ~80+ |

### Step 1 Architecture Notes
- Pure Rust 2021 with zero runtime external crates (`Cargo.toml` dependencies empty, $D=1$).
- Memory-mapped `graph.txt` and `queries.txt` via `MAP_SHARED` zero-copy I/O.
- Compact Compressed Sparse Row (CSR) graph format: 1.74M nodes (7 MB `head`) and 5M edges (20 MB `dest`, 20 MB `weight`), total graph resident in ~47 MB (down from 893 MB in Python reference).
- Parallel query evaluation across 12 hardware threads using `std::thread::scope`.
- Thread-local query generation counter `visited_gen: Vec<u32>` and persistent distance buffer `dist: Vec<u64>`: eliminates clearing/zeroing 1.74M node arrays between queries ($O(1)$ query reset).
- Custom branchless binary min-heap reused across queries.
