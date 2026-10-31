# Track C: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 14.44 | 367 | 0 |
| 1 | Compiled Rust with direct O(1) region lookup table, single-pass zero-copy CSV parsing, and per-country aggregation | run-1fc829be14e8 | 2.296 | 198.4 | 19.66 |
| 2 | Direct dense country index array lookup, elimination of 3.32M country clones, and pre-allocation check on distinct names | run-37ceee8694bb | 1.671 | 178.1 | 23.33 |
| 3 | 6-core CRLF-partitioned parallel CSV parsing and zero-dep FxHash distinct name set | run-709c2f97057d | 1.191 | 328.2 | 23.40 |
| 4 | Single-pass direct-mapped 17,576 ASCII country array, 8x faster regions.csv load | run-38150884beac | 0.0796 | 52.0 | 60.48 |
| 5 | Flat pre-allocated name hash array per thread, fast note scanning, zero heap reallocations, and parallel binary-slice dedupe | pending | | | |

### Step 5 Optimization Details
- Replaced 2,880 dynamic per-country heap vectors across threads with a single pre-allocated flat `Vec<u64>` per thread (`c_id << 48 | hash`), eliminating ~43,000 heap reallocations during records parsing.
- Fast note field scanning: scans directly for quotes and computes unescaped length via `note_len = (pos - n_start) - escaped_quotes` without per-byte branch/increment overhead.
- Each thread sorts its local flat buffer and deduplicates in parallel, computing per-country slice offsets via binary search partition points in $O(\log N)$.
- Parallel country-level deduplication merges only pre-sorted sub-slices per country across threads.
