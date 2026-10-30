# Track D: Traverse

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Our solution is implemented in pure Rust 2021 with zero runtime external crates ($D=1$).

1. **Memory-Mapped I/O**: `graph.txt` and `queries.txt` are mapped into virtual address space via direct `mmap(MAP_SHARED)` syscalls, bypassing kernel buffer copies.
2. **Compact CSR Graph**: The 1.74M node, 5M edge graph is indexed into a two-level Compressed Sparse Row (CSR) structure:
   - `head: Vec<u32>` of length $n + 1$ (6.96 MB).
   - Flat contiguous edge destinations `dest: Vec<u32>` (20 MB) and weights `weight: Vec<u32>` (20 MB).
   - Entire graph data structure fits inside ~47 MB of L3/RAM (vs 893 MB in the reference).
3. **Parallel Query Answering**: The 2,000 queries are partitioned across 12 hardware threads using `std::thread::scope`.
4. **Zero-Allocation Query Evaluation**:
   - Each thread maintains a query generation counter `visited_gen: Vec<u32>` and reusable `dist: Vec<u64>` buffer.
   - Nodes are stamped with `visited_gen[v] = cur_gen` upon visit, eliminating the $O(N)$ overhead of clearing the distance array between queries.
   - Reusable binary min-heap with custom inlined operations.

## Dependencies

None ($D=1$). Standard library only.

## With another 24 hours

We would implement bidirectional Dijkstra with landmark heuristics (ALT algorithm) or contraction hierarchies for road network queries.
