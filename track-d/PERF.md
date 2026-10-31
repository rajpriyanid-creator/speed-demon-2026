# Track D: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 72.69 | 893 | 0 |
| 1 | Compiled zero-dependency Rust CSR representation with parallel Dijkstra and query generation tagging | run-f7209b31a376 | 2.264 | 309.8 | 35.47 |
| 2 | P-core thread sizing (6 threads) and u16 generation stamp to halve memory footprint | run-846ebe22fc75 | 2.504 | 161.1 | 37.22 |
| 3 | Packed 4-ary FastHeap with dynamic atomic work-stealing | run-b6c049348b47 | 1.289 | 321.1 | 40.64 |
| 4 | Hole-sifting push heap optimization and eliminated redundant memory loads | run-eabf8481f719 | 1.273 | 322.9 | 40.73 |
| 5 | Native CPU targeting via .cargo/config.toml (target-cpu=native) | run-0dc375146900 | 1.205 | 324.1 | 41.32 |
| 6 | 2-way parallel tournament 4-ary heap comparison and raw-pointer integer parsing | run-4158599d3d61 | 1.147 | 322.9 | 41.75 |
| 7 | Bidirectional Dijkstra with concurrent dual CSR construction and balanced frontier expansion | run-72cc2d3ef1d5 | 1.085 | 340.3 | 41.60 |

### Step 5 Optimization Details
- Added `.cargo/config.toml` targeting `target-cpu=native` for BMI2 and advanced instruction set utilization, improving runtime to 1.205s (41.32 pts).

### Step 6 Optimization Details
- Implemented 2-way parallel tournament tree comparison in `FastHeap::pop`: unrolls the 4-child min-finding loop into two independent pairwise comparisons followed by a final winner selection, maximizing ILP on out-of-order execution pipelines.
- Replaced slice-indexed integer parsing with raw-pointer `parse_u32_fast` eliminating per-digit bounds checking across 15M integer parses.
- Container end-to-end benchmark runtime dropped to 1.147s (41.75 pts).

### Step 7 Optimization Details
- Replaced forward Dijkstra with Bidirectional Dijkstra: simultaneously expands forward search from `src` and backward search from `dst` on reverse CSR graph.
- Frontier-balanced scheduling: always expands the heap with smaller top cost (`top_f <= top_b`), ensuring symmetric search radius growth.
- Early termination condition: terminates immediately when `top_f + top_b >= best`, reducing search space by >3x.
- Single-pass dual-degree counting: Pass 1 calculates both in-degrees and out-degrees in one pass across `graph.txt`, populating forward and backward CSR structures concurrently with zero duplicate I/O.
