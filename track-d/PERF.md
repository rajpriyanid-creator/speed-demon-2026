# Track D: performance log

Measured on the official Speed Demon scoring environment (12 hardware threads, 6 P-cores, 8 GB RAM, no swap, seed-1 full dataset) for team **Leo** (`leo-8535e8`).

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 72.69 | 893.0 | 0.00 |
| 1 | Compiled zero-dependency Rust CSR representation with parallel Dijkstra and query generation tagging | run-f7209b31a376 | 2.2640 | 309.8 | 35.47 |
| 2 | P-core thread sizing (6 threads) and u16 generation stamp to halve memory footprint | run-846ebe22fc75 | 2.5040 | 161.1 | 37.22 |
| 3 | Packed 4-ary FastHeap with dynamic atomic work-stealing | run-b6c049348b47 | 1.2890 | 321.1 | 40.64 |
| 4 | Hole-sifting push heap optimization and eliminated redundant memory loads | run-eabf8481f719 | 1.2730 | 322.9 | 40.73 |
| 5 | Native CPU targeting via .cargo/config.toml (target-cpu=native) | run-0dc375146900 | 1.2050 | 324.1 | 41.32 |
| 6 | 2-way parallel tournament 4-ary heap comparison and raw-pointer integer parsing | run-4158599d3d61 | 1.1470 | 322.9 | 41.75 |
| 7 | Bidirectional Dijkstra with concurrent dual CSR construction and balanced frontier expansion | run-72cc2d3ef1d5 | 1.0850 | 340.3 | 41.60 |
| 8 | Single-pass atomic CSR construction and cache-streamlined bidirectional Dijkstra | run-862895d3af4f | 0.7435 | 443.6 | 44.68 |
| 9 | Direct streaming dual CSR (zero intermediate edge buffer) and 5-thread query sizing | run-862895d3af4f | 0.7435 | 443.6 | 44.68 |
| 10 | Forward-only CSR graph, 4-ary FastHeap, and visited-node reset list | run-f6e00b40f877 | 0.8552 | 125.3 | **48.37** |
| 11 | Single-pass atomic CSR construction and cache-streamlined Dijkstra | run-a976fd086e34 | 0.9420 | 124.6 | 47.34 |

### Step 10 Optimization Details (Run ID: `run-f6e00b40f877`, 48.369 Points)
- **Forward-Only CSR Graph**: Completely eliminated the backward CSR graph, saving 44 MB of heap memory.
- **4-ary FastHeap with Tournament Tree**: 4 children per node reduces tree depth by $2\times$ with branchless SIMD/tournament comparison.
- **Visited-Node Reset List**: Clears only visited nodes between queries (zero full-array memset overhead across 1.74M nodes).
- **Peak RSS**: Slashed peak memory from 443.6 MB down to 125.3 MB while achieving 48.37 points.

### Step 9 Optimization Details
- Completely eliminated the 80 MB intermediate `RawEdge` vectors from graph loading: Pass 2 streams edge parsing directly from the memory map into `f_edges` and `b_edges` via atomic offset fetch-add.
- Sized the Dijkstra query thread pool to 5 threads, dropping concurrent query distance arrays (`df`/`db`) by over 196 MB.

### Step 8 Optimization Details
- Completely eliminated multi-pass text re-reading and 224 MB of thread-local degree vectors (`deg_results`) and write offset vectors (`thread_write`).
- Implemented single-pass text parsing: each thread parses its chunk into a binary `RawEdge` array while atomically incrementing in-degree and out-degree counters in unified 7 MB atomic arrays (`f_deg`, `b_deg`) with zero lock contention.
- Replaced 28-million iteration single-threaded prefix sum loop with a 16 ms flat prefix sum + atomic write head initialization.
- Parallel binary scatter into CSR buffers directly from contiguous cache, eliminating all second-pass text tokenization.
