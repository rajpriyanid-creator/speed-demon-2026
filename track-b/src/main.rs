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

const IS_TOKEN: [bool; 256] = {
    let mut table = [false; 256];
    let mut b = 0u8;
    loop {
        if (b >= b'a' && b <= b'z') || (b >= b'A' && b <= b'Z') || (b >= b'0' && b <= b'9') {
            table[b as usize] = true;
        }
        if b == 255 { break; }
        b += 1;
    }
    table
};

#[derive(Clone, Copy)]
#[repr(C, align(32))]
struct DictEntry {
    hash: u32,
    term_id: u32,
    len: u8,
    bytes: [u8; 23],
}

struct Dict {
    table: Vec<DictEntry>,
    mask: usize,
}

impl Dict {
    fn new(capacity: usize) -> Self {
        let cap = capacity.next_power_of_two();
        Self {
            table: vec![DictEntry { hash: 0, term_id: 0, len: 0, bytes: [0; 23] }; cap],
            mask: cap - 1,
        }
    }

    #[inline(always)]
    fn hash_bytes(b: &[u8]) -> u32 {
        let mut h: u32 = 0x811c9dc5;
        for &byte in b {
            h ^= (byte | 0x20) as u32;
            h = h.wrapping_mul(0x01000193);
        }
        if h == 0 { 1 } else { h }
    }

    fn insert(&mut self, term: &[u8], term_id: u32) {
        if term.len() > 23 { return; }
        let h = Self::hash_bytes(term);
        let mut idx = (h as usize) & self.mask;
        loop {
            let e = &mut self.table[idx];
            if e.hash == 0 {
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
        if term.len() > 23 { return None; }
        let h = Self::hash_bytes(term);
        let mut idx = (h as usize) & self.mask;
        loop {
            let e = unsafe { self.table.get_unchecked(idx) };
            if e.hash == 0 {
                return None;
            }
            if e.hash == h && e.len as usize == term.len() && &e.bytes[..term.len()] == term {
                return Some(e.term_id);
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn get_with_hash(&self, raw: &[u8], h: u32) -> Option<u32> {
        let len = raw.len();
        if len > 23 { return None; }
        let mut idx = (h as usize) & self.mask;
        loop {
            let e = unsafe { self.table.get_unchecked(idx) };
            if e.hash == 0 {
                return None;
            }
            if e.hash == h && e.len as usize == len {
                let mut match_tok = true;
                for k in 0..len {
                    if unsafe { *e.bytes.get_unchecked(k) != (*raw.get_unchecked(k) | 0x20) } {
                        match_tok = false;
                        break;
                    }
                }
                if match_tok {
                    return Some(e.term_id);
                }
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

enum ParsedQuery<'a> {
    Bool {
        first_term: Option<u32>,
        steps: Vec<(Op, Option<u32>)>,
    },
    Phrase {
        terms: Vec<Option<u32>>,
        raw_tokens: Vec<&'a [u8]>,
    },
}

#[inline(always)]
fn phrase_matches(doc_text: &[u8], phrase: &[&[u8]]) -> bool {
    if phrase.is_empty() { return true; }
    let t0 = phrase[0];
    let l0 = t0.len();
    let tlen = doc_text.len();
    let mut p = 0;
    while p < tlen {
        while p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(p) as usize) } {
            p += 1;
        }
        if p >= tlen { break; }
        let tok_start = p;
        while p < tlen && unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(p) as usize) } {
            p += 1;
        }
        let tok_len = p - tok_start;
        if tok_len == l0 {
            let mut matches_t0 = true;
            for m in 0..l0 {
                if unsafe { (*doc_text.get_unchecked(tok_start + m) | 0x20) != *t0.get_unchecked(m) } {
                    matches_t0 = false;
                    break;
                }
            }
            if matches_t0 {
                let mut curr_p = p;
                let mut all_match = true;
                for &next_tok in &phrase[1..] {
                    while curr_p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(curr_p) as usize) } {
                        curr_p += 1;
                    }
                    if curr_p >= tlen {
                        all_match = false;
                        break;
                    }
                    let nt_start = curr_p;
                    while curr_p < tlen && unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(curr_p) as usize) } {
                        curr_p += 1;
                    }
                    let nt_len = curr_p - nt_start;
                    if nt_len != next_tok.len() {
                        all_match = false;
                        break;
                    }
                    for m in 0..nt_len {
                        if unsafe { (*doc_text.get_unchecked(nt_start + m) | 0x20) != *next_tok.get_unchecked(m) } {
                            all_match = false;
                            break;
                        }
                    }
                    if !all_match {
                        break;
                    }
                }
                if all_match {
                    return true;
                }
            }
        }
    }
    false
}

#[inline(always)]
fn intersect(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let mut i = 0;
    let mut j = 0;
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = unsafe { *a.get_unchecked(i) };
        let vb = unsafe { *b.get_unchecked(j) };
        if va == vb {
            out.push(va);
            i += 1;
            j += 1;
        } else if va < vb {
            if i + 8 < na && unsafe { *a.get_unchecked(i + 8) } < vb {
                i += 8;
                while i + 8 < na && unsafe { *a.get_unchecked(i + 8) } < vb {
                    i += 8;
                }
            }
            i += 1;
        } else {
            if j + 8 < nb && unsafe { *b.get_unchecked(j + 8) } < va {
                j += 8;
                while j + 8 < nb && unsafe { *b.get_unchecked(j + 8) } < va {
                    j += 8;
                }
            }
            j += 1;
        }
    }
}

#[inline(always)]
fn union(a: &[u32], b: &[u32], out: &mut Vec<u32>) {
    out.clear();
    let mut i = 0;
    let mut j = 0;
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = unsafe { *a.get_unchecked(i) };
        let vb = unsafe { *b.get_unchecked(j) };
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
    let na = a.len();
    let nb = b.len();
    while i < na && j < nb {
        let va = unsafe { *a.get_unchecked(i) };
        let vb = unsafe { *b.get_unchecked(j) };
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

#[inline(always)]
fn write_u64_fast(mut n: u64, out: &mut Vec<u8>) {
    if n == 0 {
        out.push(b'0');
        return;
    }
    let mut buf = [0u8; 20];
    let mut i = 0;
    while n > 0 {
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    while i > 0 {
        i -= 1;
        out.push(buf[i]);
    }
}

#[inline(always)]
fn write_u32_fast(n: u32, out: &mut Vec<u8>) {
    write_u64_fast(n as u64, out);
}

fn write_result(out: &mut Vec<u8>, ids: &[u32]) {
    if ids.is_empty() {
        out.extend_from_slice(b"0 0\n");
        return;
    }
    let count = ids.len();
    let sum: u64 = ids.iter().map(|&x| x as u64).sum();
    let take = count.min(10);

    write_u32_fast(count as u32, out);
    out.push(b' ');
    write_u64_fast(sum, out);
    for &id in &ids[..take] {
        out.push(b' ');
        write_u32_fast(id, out);
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

    let mut dict = Dict::new(131_072);
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
            let mut raw_tokens = Vec::new();
            for part in inner.split(|&b| b == b' ') {
                if !part.is_empty() {
                    terms.push(dict.get(part));
                    raw_tokens.push(part);
                }
            }
            parsed_queries.push(ParsedQuery::Phrase { terms, raw_tokens });
        }
    }

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
        for t in 0..num_index_threads {
            let target_byte = ((n * (t + 1)) / num_index_threads).min(n);
            let mut end_byte = target_byte;
            while end_byte < n && docs_data[end_byte] != b'\n' {
                end_byte += 1;
            }
            if end_byte < n {
                end_byte += 1;
            }
            doc_chunks.push((cur_byte, end_byte));
            cur_byte = end_byte;
        }
    }

    // Pass 1 on docs.txt in parallel: count term occurrences and record doc slices
    let pass1_results: Vec<(Vec<u32>, Vec<u32>, Vec<u32>)> = std::thread::scope(|s| {
        let handles: Vec<_> = doc_chunks.iter().map(|&(start_byte, end_byte)| {
            let dict_ref = &dict;
            let chunk = &docs_data[start_byte..end_byte];

            s.spawn(move || {
                let mut local_counts = vec![0u32; num_terms as usize];
                let mut last_seen = vec![u32::MAX; num_terms as usize];
                let est_docs = ((end_byte - start_byte) / 1000 + 1024).max(10_000);
                let mut local_starts = Vec::with_capacity(est_docs);
                let mut local_lens = Vec::with_capacity(est_docs);

                let mut i = 0;
                let clen = chunk.len();
                let mut local_doc_id = 0u32;

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
                        while p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*text.get_unchecked(p) as usize) } {
                            p += 1;
                        }
                        if p >= tlen { break; }
                        let tok_start = p;
                        let mut h = 0x811c9dc5u32;
                        while p < tlen && unsafe { *IS_TOKEN.get_unchecked(*text.get_unchecked(p) as usize) } {
                            let b = unsafe { *text.get_unchecked(p) };
                            h ^= (b | 0x20) as u32;
                            h = h.wrapping_mul(0x01000193);
                            p += 1;
                        }
                        let tok_len = p - tok_start;
                        if tok_len <= 23 {
                            if h == 0 { h = 1; }
                            if let Some(term_id) = dict_ref.get_with_hash(&text[tok_start..p], h) {
                                let tid = term_id as usize;
                                if unsafe { *last_seen.get_unchecked(tid) } != local_doc_id {
                                    unsafe { *last_seen.get_unchecked_mut(tid) = local_doc_id; }
                                    unsafe { *local_counts.get_unchecked_mut(tid) += 1; }
                                }
                            }
                        }
                    }
                    local_doc_id += 1;
                }

                (local_counts, local_starts, local_lens)
            })
        }).collect();

        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut thread_start_docs = Vec::with_capacity(num_index_threads);
    let mut cur_global_doc = 0u32;
    for r in &pass1_results {
        thread_start_docs.push(cur_global_doc);
        cur_global_doc += r.1.len() as u32;
    }

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
            let docs_data_ref = docs_data;
            let starts = r.1;
            let lens = r.2;
            let start_doc = thread_start_docs[t];
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
                        while p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*text.get_unchecked(p) as usize) } {
                            p += 1;
                        }
                        if p >= tlen { break; }
                        let tok_start = p;
                        let mut h = 0x811c9dc5u32;
                        while p < tlen && unsafe { *IS_TOKEN.get_unchecked(*text.get_unchecked(p) as usize) } {
                            let b = unsafe { *text.get_unchecked(p) };
                            h ^= (b | 0x20) as u32;
                            h = h.wrapping_mul(0x01000193);
                            p += 1;
                        }
                        let tok_len = p - tok_start;
                        if tok_len <= 23 {
                            if h == 0 { h = 1; }
                            if let Some(term_id) = dict_ref.get_with_hash(&text[tok_start..p], h) {
                                let tid = term_id as usize;
                                if unsafe { *last_seen.get_unchecked(tid) } != d_id {
                                    unsafe { *last_seen.get_unchecked_mut(tid) = d_id; }
                                    let slot = unsafe { *write_pos.get_unchecked(tid) } as usize;
                                    unsafe {
                                        *write_pos.get_unchecked_mut(tid) += 1;
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
            let doc_starts_ref = &doc_starts;
            let doc_lens_ref = &doc_lens;
            let docs_data_ref = docs_data;

            s.spawn(move || {
                let mut thread_out = Vec::with_capacity(chunk.len() * 32);
                let mut buf_a = Vec::with_capacity(32_768);
                let mut buf_b = Vec::with_capacity(32_768);
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
                                let (first_op, first_term_opt) = steps[0];
                                let b_first: &[u32] = match first_term_opt {
                                    Some(t) => get_postings(t),
                                    None => &[],
                                };
                                match first_op {
                                    Op::And => intersect(r_slice, b_first, &mut buf_a),
                                    Op::Or => union(r_slice, b_first, &mut buf_a),
                                    Op::Not => difference(r_slice, b_first, &mut buf_a),
                                }
                                for &(op, maybe_term) in &steps[1..] {
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
                        ParsedQuery::Phrase { terms, raw_tokens } => {
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
                                        for &cand_id in &buf_a {
                                            let start = doc_starts_ref[cand_id as usize] as usize;
                                            let len = doc_lens_ref[cand_id as usize] as usize;
                                            let doc_text = &docs_data_ref[start..start + len];
                                            if phrase_matches(doc_text, raw_tokens) {
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
