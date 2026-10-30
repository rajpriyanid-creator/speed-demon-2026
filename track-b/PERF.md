# Track B: performance log

Every step from the reference to your final version, each with the live-board run ID that measured it. Judges check that the run IDs exist and that each change moved the way you say it did.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged | | 56.35 | 2290 | 0 |
| 1 | Zero-dep Rust query-driven inverted index with parallel query evaluation | run-4c828af558fc | 3.388 | 159.4 | 36.13 |

### Step 1 Architecture
- Mmap `queries.txt` and `docs.txt` directly to eliminate file copy overhead.
- Query-driven indexing: Parse `queries.txt` first to catalog the ~75,747 distinct query tokens into an open-addressing hash table with linear probing.
- Scan `docs.txt` single-pass, tokenizing on ASCII byte boundaries and populating sorted document ID postings lists only for query terms.
- Phrase queries: Intersect candidate documents starting with the rarest term, then verify consecutive token match on candidate document text without position index memory overhead.
- Parallel query evaluation across 12 threads using `std::thread::scope` with zero allocation during evaluation.
- Scored **36.13 points** on the live Falkenstein board (3.388 s vs 56.35 s Python reference, 159 MB vs 2.29 GB peak memory).
