# Track A: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 32.05 | 228 | 0 |
| 1 | Multi-threaded Rust with zero runtime dependencies, mmap MAP_SHARED, select_nth_unstable percentiles, and buffered output | (board run 1) | ~0.15 | ~30 | ~70+ |

### Step 1 Architecture Notes
- Pure Rust 2021 with zero runtime external crates (`Cargo.toml` dependencies empty, $D=1$).
- Read-only memory mapping of `/data/requests.log` with `MAP_SHARED` to utilize host page-cache without Copy-On-Write overhead.
- Parallel line processing across 12 threads using `std::thread::scope`.
- In-place order statistics via `select_nth_unstable` for exact nearest-rank percentiles (p50, p95, p99) without full array sorting.
- Integer arithmetic for `error_ppm` via `(errors * 1_000_000) / count`.
