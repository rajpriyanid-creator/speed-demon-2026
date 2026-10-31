# Track D: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 72.69 | 893 | 0 |
| 1 | Compiled zero-dependency Rust CSR representation with parallel Dijkstra and query generation tagging | run-f7209b31a376 | 2.264 | 309.8 | 35.47 |
| 2 | P-core thread sizing (6 threads) and u16 generation stamp to halve memory footprint | run-846ebe22fc75 | 2.504 | 161.1 | 37.22 |
| 3 | Packed 4-ary FastHeap with dynamic atomic work-stealing | run-b6c049348b47 | 1.289 | 321.1 | 40.64 |
| 4 | Hole-sifting push heap optimization and eliminated redundant memory loads | run-eabf8481f719 | 1.273 | 322.9 | 40.73 |
| 5 | Native CPU targeting via .cargo/config.toml (target-cpu=native) | pending | | | |

### Step 4 Optimization Details
- Implemented single-write hole sifting in `FastHeap::push`: sifts parent values down without writing candidate values to intermediate slots, halving the memory write count during heap insertions.
- Cached `old_dist` in local register during edge relaxation, eliminating redundant 64-bit memory reads from the distance array.

### Step 5 Optimization Details
- Added `.cargo/config.toml` targeting `target-cpu=native` for BMI2 and advanced instruction set utilization.
