# Track B: Index

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

Our solution is written in zero-dependency Rust ($D=1$):
1. **Memory-Mapped Input**: Directly mmap `queries.txt` and `docs.txt` via Unix `mmap` syscalls.
2. **Query-Driven Token Filtering**: The 50,000 queries only query ~75,747 unique terms out of 609,833 total terms in the corpus. We parse `queries.txt` first to populate a fast open-addressing dictionary mapping query terms to dense integer IDs `0..T-1`.
3. **Sorted Inverted Postings**: We tokenize `docs.txt` in a single pass. Documents arrive in ascending ID order `0..N-1`, so document IDs appended to postings lists are guaranteed to be strictly ascending without requiring any sort.
4. **Candidate-Filtered Phrase Search**: Phrase queries intersect terms starting with the rarest term to reduce candidates to a tiny set (median 1-5 documents), then verify the consecutive token sequence against the candidate document text directly in memory.
5. **Parallel Evaluation**: Query evaluation is partitioned across available CPU cores using `std::thread::scope`, maintaining zero memory allocation during search.

## Dependencies

None. Pure Rust standard library only.

## With another 24 hours

1. Flat contiguous postings arena with two-pass prefix-sum offsets.
2. SIMD-accelerated AVX2 byte scanner for whitespace and separator delimitation.
3. Compressed posting lists using variable-byte or delta bitpacking.
