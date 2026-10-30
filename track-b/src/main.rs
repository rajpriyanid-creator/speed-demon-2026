use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

struct MmapSlice {
    ptr: *mut u8,
    len: usize,
}

impl MmapSlice {
    fn open(path: &str) -> Option<Self> {
        let file = File::open(path).ok()?;
        let len = file.metadata().ok()?.len() as usize;
        if len == 0 {
            return Some(Self { ptr: std::ptr::null_mut(), len: 0 });
        }
        let fd = file.as_raw_fd();
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, 1, 2, fd, 0) };
        if ptr == usize::MAX as *mut u8 || ptr.is_null() {
            return None;
        }
        Some(Self { ptr, len })
    }

    fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

#[derive(Clone, Copy)]
struct DictEntry {
    hash: u64,
    term_id: u32,
    len: u8,
    bytes: [u8; 32],
}

struct Dict {
    table: Vec<DictEntry>,
    mask: usize,
}

impl Dict {
    fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two();
        Self {
            table: vec![DictEntry { hash: 0, term_id: 0, len: 0, bytes: [0; 32] }; cap],
            mask: cap - 1,
        }
    }

    #[inline(always)]
    fn hash_bytes(b: &[u8]) -> u64 {
        let mut h: u64 = 0xcbf29ce484222325;
        for &byte in b {
            h ^= byte as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        h
    }

    fn insert(&mut self, term: &[u8], term_id: u32) {
        if term.len() > 32 { return; }
        let h = Self::hash_bytes(term);
        let mut idx = (h as usize) & self.mask;
        loop {
            let e = &mut self.table[idx];
            if e.len == 0 {
                e.hash = h;
                e.term_id = term_id;
                e.len = term.len() as u8;
                e.bytes[..term.len()].copy_from_slice(term);
                return;
            }
            if e.hash == h && e.len as usize == term.len() && &e.bytes[..term.len()] == term {
                return;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn get(&self, term: &[u8]) -> Option<u32> {
        if term.len() > 32 { return None; }
        let h = Self::hash_bytes(term);
        let mut idx = (h as usize) & self.mask;
        loop {
            let e = unsafe { self.table.get_unchecked(idx) };
            if e.len == 0 {
                return None;
            }
            if e.hash == h && e.len as usize == term.len() && &e.bytes[..term.len()] == term {
                return Some(e.term_id);
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

#[derive(Clone, Copy)]
enum Op {
    And,
    Or,
    Not,
}

enum ParsedQuery {
    Bool {
        first_term: Option<u32>,
        steps: Vec<(Op, Option<u32>)>,
    },
    Phrase {
        terms: Vec<Option<u32>>,
    },
}

#[inline(always)]
fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        let va = a[i];
        let vb = b[j];
        if va == vb {
            out.push(va);
            i += 1;
            j += 1;
        } else if va < vb {
            i += 1;
        } else {
            j += 1;
        }
    }
}

#[inline(always)]
fn union(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        let va = a[i];
        let vb = b[j];
        if va == vb {
            out.push(va);
            i += 1;
            j += 1;
        } else if va < vb {
            out.push(va);
            i += 1;
        } else {
            out.push(vb);
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
    out.extend_from_slice(&b[j..]);
}

#[inline(always)]
fn difference(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        let va = a[i];
        let vb = b[j];
        if va == vb {
            i += 1;
            j += 1;
        } else if va < vb {
            out.push(va);
            i += 1;
        } else {
            j += 1;
        }
    }
    out.extend_from_slice(&a[i..]);
}

fn write_result(out: &mut Vec<u8>, ids: &[u32]) {
    if ids.is_empty() {
        out.extend_from_slice(b"0 0\n");
        return;
    }
    let count = ids.len();
    let sum: u64 = ids.iter().map(|&x| x as u64).sum();
    let take = count.min(10);

    let _ = write!(out, "{} {}", count, sum);
    for &id in &ids[..take] {
        let _ = write!(out, " {}", id);
    }
    out.push(b'\n');
}

fn main() {
    let mut data_dir = "/data".to_string();
    let mut out_dir = "/out".to_string();
    let args: Vec<String> = std::env::args().collect();
    let mut arg_i = 1;
    while arg_i < args.len() {
        if args[arg_i] == "--data" && arg_i + 1 < args.len() {
            data_dir = args[arg_i + 1].clone();
            arg_i += 2;
        } else if args[arg_i] == "--out" && arg_i + 1 < args.len() {
            out_dir = args[arg_i + 1].clone();
            arg_i += 2;
        } else {
            arg_i += 1;
        }
    }

    let queries_path = format!("{}/queries.txt", data_dir);
    let docs_path = format!("{}/docs.txt", data_dir);
    let results_path = format!("{}/results.txt", out_dir);

    // Mmap queries.txt
    let q_mmap = MmapSlice::open(&queries_path).expect("failed to open queries.txt");
    let q_data = q_mmap.as_slice();

    let mut dict = Dict::new(262_144);
    let mut num_terms = 0u32;

    // Pass 1 on queries: collect all unique query terms
    for line in q_data.split(|&b| b == b'\n') {
        let line = if line.ends_with(b"\r") { &line[..line.len() - 1] } else { line };
        if line.is_empty() { continue; }
        if line.starts_with(b"BOOL ") {
            let mut parts = line[5..].split(|&b| b == b' ');
            while let Some(part) = parts.next() {
                if part == b"AND" || part == b"OR" || part == b"NOT" || part.is_empty() {
                    continue;
                }
                if dict.get(part).is_none() {
                    dict.insert(part, num_terms);
                    num_terms += 1;
                }
            }
        } else if line.starts_with(b"PHRASE \"") {
            let inner = &line[8..line.len().saturating_sub(1)];
            for part in inner.split(|&b| b == b' ') {
                if part.is_empty() { continue; }
                if dict.get(part).is_none() {
                    dict.insert(part, num_terms);
                    num_terms += 1;
                }
            }
        }
    }

    // Pass 2 on queries: parse into ParsedQuery structs
    let mut parsed_queries = Vec::with_capacity(50_000);
    for line in q_data.split(|&b| b == b'\n') {
        let line = if line.ends_with(b"\r") { &line[..line.len() - 1] } else { line };
        if line.is_empty() { continue; }
        if line.starts_with(b"BOOL ") {
            let mut parts = line[5..].split(|&b| b == b' ');
            let first = parts.next().unwrap_or(b"");
            let first_id = dict.get(first);
            let mut steps = Vec::new();
            while let Some(op_bytes) = parts.next() {
                let op = match op_bytes {
                    b"AND" => Op::And,
                    b"OR" => Op::Or,
                    b"NOT" => Op::Not,
                    _ => continue,
                };
                let term_bytes = parts.next().unwrap_or(b"");
                let term_id = dict.get(term_bytes);
                steps.push((op, term_id));
            }
            parsed_queries.push(ParsedQuery::Bool { first_term: first_id, steps });
        } else if line.starts_with(b"PHRASE \"") {
            let inner = &line[8..line.len().saturating_sub(1)];
            let mut terms = Vec::new();
            for part in inner.split(|&b| b == b' ') {
                if !part.is_empty() {
                    terms.push(dict.get(part));
                }
            }
            parsed_queries.push(ParsedQuery::Phrase { terms });
        }
    }

    // Prepare token lookup table
    let mut is_token = [false; 256];
    for b in b'a'..=b'z' { is_token[b as usize] = true; }
    for b in b'A'..=b'Z' { is_token[b as usize] = true; }
    for b in b'0'..=b'9' { is_token[b as usize] = true; }

    // Mmap docs.txt
    let docs_mmap = MmapSlice::open(&docs_path).expect("failed to open docs.txt");
    let docs_data = docs_mmap.as_slice();
    let n = docs_data.len();

    let num_index_threads = std::thread::available_parallelism()
        .map(|num| num.get())
        .unwrap_or(6)
        .min(12);

    let mut doc_chunks = Vec::with_capacity(num_index_threads);
    {
        let mut cur_byte = 0;
        let mut cur_doc = 0u32;
        for t in 0..num_index_threads {
            let target_byte = ((n * (t + 1)) / num_index_threads).min(n);
            let mut end_byte = target_byte;
            while end_byte < n && docs_data[end_byte] != b'\n' {
                end_byte += 1;
            }
            if end_byte < n {
                end_byte += 1;
            }
            doc_chunks.push((cur_byte, end_byte, cur_doc));
            let chunk_slice = &docs_data[cur_byte..end_byte];
            let doc_count = chunk_slice.iter().filter(|&&b| b == b'\n').count() as u32;
            cur_doc += doc_count;
            cur_byte = end_byte;
        }
    }

    // Pass 1 on docs.txt in parallel: count term occurrences and record doc slices
    let pass1_results: Vec<(Vec<u32>, Vec<u32>, Vec<u32>)> = std::thread::scope(|s| {
        let handles: Vec<_> = doc_chunks.iter().map(|&(start_byte, end_byte, start_doc)| {
            let dict_ref = &dict;
            let is_token_ref = &is_token;
            let chunk = &docs_data[start_byte..end_byte];

            s.spawn(move || {
                let mut local_counts = vec![0u32; num_terms as usize];
                let mut last_seen = vec![u32::MAX; num_terms as usize];
                let est_docs = ((end_byte - start_byte) / 1000 + 1024).max(10_000);
                let mut local_starts = Vec::with_capacity(est_docs);
                let mut local_lens = Vec::with_capacity(est_docs);

                let mut i = 0;
                let clen = chunk.len();
                let mut doc_id = start_doc;

                while i < clen {
                    while i < clen && chunk[i] != b'\t' {
                        i += 1;
                    }
                    if i >= clen { break; }
                    i += 1; // skip TAB
                    let text_start_rel = i;
                    while i < clen && chunk[i] != b'\n' {
                        i += 1;
                    }
                    let text_end_rel = i;
                    i += 1; // skip LF

                    let text_len = text_end_rel - text_start_rel;
                    let global_text_start = (start_byte + text_start_rel) as u32;
                    local_starts.push(global_text_start);
                    local_lens.push(text_len as u32);

                    let text = &chunk[text_start_rel..text_end_rel];
                    let mut p = 0;
                    let tlen = text.len();
                    while p < tlen {
                        while p < tlen && !is_token_ref[text[p] as usize] {
                            p += 1;
                        }
                        if p >= tlen { break; }
                        let tok_start = p;
                        while p < tlen && is_token_ref[text[p] as usize] {
                            p += 1;
                        }
                        let tok_len = p - tok_start;
                        if tok_len <= 32 {
                            let mut buf = [0u8; 32];
                            for k in 0..tok_len {
                                buf[k] = text[tok_start + k].to_ascii_lowercase();
                            }
                            if let Some(term_id) = dict_ref.get(&buf[..tok_len]) {
                                let tid = term_id as usize;
                                if last_seen[tid] != doc_id {
                                    last_seen[tid] = doc_id;
                                    local_counts[tid] += 1;
                                }
                            }
                        }
                    }
                    doc_id += 1;
                }

                (local_counts, local_starts, local_lens)
            })
        }).collect();

        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Assemble global doc_starts and doc_lens
    let total_docs_est = pass1_results.iter().map(|r| r.1.len()).sum::<usize>();
    let mut doc_starts: Vec<u32> = Vec::with_capacity(total_docs_est);
    let mut doc_lens: Vec<u32> = Vec::with_capacity(total_docs_est);
    for r in &pass1_results {
        doc_starts.extend_from_slice(&r.1);
        doc_lens.extend_from_slice(&r.2);
    }

    // Prefix sums to build contiguous postings CSR with per-thread write bases
    let mut offsets = vec![0u32; (num_terms as usize) + 1];
    let mut thread_write_bases: Vec<Vec<u32>> = (0..num_index_threads)
        .map(|_| vec![0u32; num_terms as usize])
        .collect();

    let mut total_postings = 0u32;
    for term in 0..(num_terms as usize) {
        offsets[term] = total_postings;
        let mut cur = total_postings;
        for t in 0..num_index_threads {
            thread_write_bases[t][term] = cur;
            cur += pass1_results[t].0[term];
        }
        total_postings = cur;
    }
    offsets[num_terms as usize] = total_postings;

    let mut all_postings = vec![0u32; total_postings as usize];

    let base_addr = all_postings.as_mut_ptr() as usize;

    // Pass 2 on docs.txt in parallel: populate contiguous all_postings with zero line scanning
    std::thread::scope(|s| {
        for (t, r) in pass1_results.into_iter().enumerate() {
            let dict_ref = &dict;
            let is_token_ref = &is_token;
            let docs_data_ref = docs_data;
            let starts = r.1;
            let lens = r.2;
            let start_doc = doc_chunks[t].2;
            let mut write_pos = thread_write_bases[t].clone();

            s.spawn(move || {
                let raw_ptr = base_addr as *mut u32;
                let mut last_seen = vec![u32::MAX; num_terms as usize];
                for (local_idx, (&start, &len)) in starts.iter().zip(lens.iter()).enumerate() {
                    let d_id = start_doc + local_idx as u32;
                    let text = &docs_data_ref[start as usize..(start + len) as usize];
                    let mut p = 0;
                    let tlen = text.len();
                    while p < tlen {
                        while p < tlen && !is_token_ref[text[p] as usize] {
                            p += 1;
                        }
                        if p >= tlen { break; }
                        let tok_start = p;
                        while p < tlen && is_token_ref[text[p] as usize] {
                            p += 1;
                        }
                        let tok_len = p - tok_start;
                        if tok_len <= 32 {
                            let mut buf = [0u8; 32];
                            for k in 0..tok_len {
                                buf[k] = text[tok_start + k].to_ascii_lowercase();
                            }
                            if let Some(term_id) = dict_ref.get(&buf[..tok_len]) {
                                let tid = term_id as usize;
                                if last_seen[tid] != d_id {
                                    last_seen[tid] = d_id;
                                    let slot = write_pos[tid] as usize;
                                    write_pos[tid] += 1;
                                    unsafe {
                                        *raw_ptr.add(slot) = d_id;
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }
    });

    // Parallel query execution
    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .min(12);

    let chunk_size = (parsed_queries.len() + num_threads - 1) / num_threads;

    let thread_outputs: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = parsed_queries.chunks(chunk_size).map(|chunk| {
            let offsets_ref = &offsets;
            let all_postings_ref = &all_postings;
            let dict_ref = &dict;
            let doc_starts_ref = &doc_starts;
            let doc_lens_ref = &doc_lens;
            let docs_data_ref = docs_data;

            s.spawn(move || {
                let mut thread_out = Vec::with_capacity(chunk.len() * 32);
                let mut buf_a = Vec::with_capacity(250_000);
                let mut buf_b = Vec::with_capacity(250_000);
                let mut doc_tokens = Vec::with_capacity(1024);
                let mut matches = Vec::new();

                let get_postings = |term_id: u32| -> &[u32] {
                    let tid = term_id as usize;
                    let start = offsets_ref[tid] as usize;
                    let end = offsets_ref[tid + 1] as usize;
                    &all_postings_ref[start..end]
                };

                for query in chunk {
                    match query {
                        ParsedQuery::Bool { first_term, steps } => {
                            let r_slice: &[u32] = match *first_term {
                                Some(t) => get_postings(t),
                                None => &[],
                            };
                            if steps.is_empty() {
                                write_result(&mut thread_out, r_slice);
                            } else {
                                buf_a.clear();
                                buf_a.extend_from_slice(r_slice);
                                for &(op, maybe_term) in steps {
                                    let b: &[u32] = match maybe_term {
                                        Some(t) => get_postings(t),
                                        None => &[],
                                    };
                                    match op {
                                        Op::And => intersect(&buf_a, b, &mut buf_b),
                                        Op::Or => union(&buf_a, b, &mut buf_b),
                                        Op::Not => difference(&buf_a, b, &mut buf_b),
                                    }
                                    std::mem::swap(&mut buf_a, &mut buf_b);
                                }
                                write_result(&mut thread_out, &buf_a);
                            }
                        }
                        ParsedQuery::Phrase { terms } => {
                            if terms.iter().any(|t| t.is_none()) {
                                thread_out.extend_from_slice(b"0 0\n");
                            } else {
                                let term_ids: Vec<u32> = terms.iter().map(|t| t.unwrap()).collect();
                                let mut min_idx = 0;
                                let mut min_len = usize::MAX;
                                for (idx, &tid) in term_ids.iter().enumerate() {
                                    let plen = (offsets_ref[tid as usize + 1] - offsets_ref[tid as usize]) as usize;
                                    if plen < min_len {
                                        min_len = plen;
                                        min_idx = idx;
                                    }
                                }

                                if min_len == 0 {
                                    thread_out.extend_from_slice(b"0 0\n");
                                } else {
                                    buf_a.clear();
                                    buf_a.extend_from_slice(get_postings(term_ids[min_idx]));

                                    for (idx, &tid) in term_ids.iter().enumerate() {
                                        if idx == min_idx { continue; }
                                        intersect(&buf_a, get_postings(tid), &mut buf_b);
                                        std::mem::swap(&mut buf_a, &mut buf_b);
                                        if buf_a.is_empty() {
                                            break;
                                        }
                                    }

                                    if buf_a.is_empty() {
                                        thread_out.extend_from_slice(b"0 0\n");
                                    } else {
                                        matches.clear();
                                        let k = term_ids.len();
                                        for &cand_id in &buf_a {
                                            let start = doc_starts_ref[cand_id as usize] as usize;
                                            let len = doc_lens_ref[cand_id as usize] as usize;
                                            let doc_text = &docs_data_ref[start..start + len];

                                            doc_tokens.clear();
                                            let mut p = 0;
                                            let tlen = doc_text.len();
                                            while p < tlen {
                                                while p < tlen && !is_token[doc_text[p] as usize] {
                                                    p += 1;
                                                }
                                                if p >= tlen { break; }
                                                let tok_start = p;
                                                while p < tlen && is_token[doc_text[p] as usize] {
                                                    p += 1;
                                                }
                                                let tok_len = p - tok_start;
                                                if tok_len <= 32 {
                                                    let mut buf = [0u8; 32];
                                                    for m in 0..tok_len {
                                                        buf[m] = doc_text[tok_start + m].to_ascii_lowercase();
                                                    }
                                                    let tid = dict_ref.get(&buf[..tok_len]).unwrap_or(u32::MAX);
                                                    doc_tokens.push(tid);
                                                } else {
                                                    doc_tokens.push(u32::MAX);
                                                }
                                            }

                                            if doc_tokens.windows(k).any(|w| w == term_ids.as_slice()) {
                                                matches.push(cand_id);
                                            }
                                        }
                                        write_result(&mut thread_out, &matches);
                                    }
                                }
                            }
                        }
                    }
                }
                thread_out
            })
        }).collect();

        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Write output
    std::fs::create_dir_all(&out_dir).expect("failed to create out directory");
    let mut out_file = File::create(&results_path).expect("failed to create results.txt");
    for chunk_out in thread_outputs {
        out_file.write_all(&chunk_out).expect("failed to write output chunk");
    }
    out_file.flush().expect("failed to flush results.txt");

    // Munmap at end
    if q_mmap.len > 0 {
        unsafe { munmap(q_mmap.ptr, q_mmap.len); }
    }
    if docs_mmap.len > 0 {
        unsafe { munmap(docs_mmap.ptr, docs_mmap.len); }
    }
}
