# Track B: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 56.35 | 2290 | 0 |
| 1 | Zero-dep Rust query-driven inverted index with parallel query evaluation | run-4c828af558fc | 3.388 | 159.4 | 36.13 |
| 2 | Contiguous CSR postings arena eliminating 300k dynamic vector reallocations | pending | | | |

### Step 2 Architecture
- Replaced 75,747 dynamic `Vec<Vec<u32>>` heap allocations with a single flat contiguous `all_postings: Vec<u32>` arena (22.24M entries, 89 MB).
- Pass 1 counts term occurrences and records exact `(start, len)` slices of documents.
- Computed prefix sums into `offsets: Vec<u32>` and populated postings in Pass 2 with zero text line parsing overhead.
- All term postings are contiguous L1/L2 cache-friendly slices `&all_postings[offsets[tid]..offsets[tid+1]]`.
