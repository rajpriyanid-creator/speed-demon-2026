use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

const NUM_BLOCKS: usize = 4;

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
        let ptr = unsafe { mmap(std::ptr::null_mut(), len, 1, 1, fd, 0) };
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

impl Drop for MmapSlice {
    fn drop(&mut self) {
        if !self.ptr.is_null() && self.len > 0 {
            unsafe { munmap(self.ptr, self.len); }
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
    bloom: [u64; 4096],
}

impl Dict {
    fn new(_capacity: usize) -> Self {
        let cap = 262_144;
        Self {
            table: vec![DictEntry { hash: 0, term_id: 0, len: 0, bytes: [0; 23] }; cap],
            mask: cap - 1,
            bloom: [0u64; 4096],
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
                for (k, &b) in term.iter().enumerate() {
                    e.bytes[k] = b | 0x20;
                }
                let b_idx = (h as usize & 0x3FFFF) >> 6;
                self.bloom[b_idx] |= 1u64 << (h & 63);
                return;
            }
            if e.hash == h && e.len as usize == term.len() {
                let mut match_tok = true;
                for k in 0..term.len() {
                    if e.bytes[k] != (term[k] | 0x20) {
                        match_tok = false;
                        break;
                    }
                }
                if match_tok {
                    return;
                }
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
            if e.hash == h && e.len as usize == term.len() {
                let mut match_tok = true;
                for k in 0..term.len() {
                    if e.bytes[k] != (term[k] | 0x20) {
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

    #[inline(always)]
    fn get_with_hash(&self, lower_tok: &[u8], h: u32) -> Option<u32> {
        let b_idx = (h as usize & 0x3FFFF) >> 6;
        if (unsafe { *self.bloom.get_unchecked(b_idx) } & (1u64 << (h & 63))) == 0 {
            return None;
        }
        let len = lower_tok.len();
        if len > 23 { return None; }
        let mut idx = (h as usize) & self.mask;
        loop {
            let e = unsafe { self.table.get_unchecked(idx) };
            if e.hash == 0 {
                return None;
            }
            if e.hash == h && e.len as usize == len && &e.bytes[..len] == lower_tok {
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
fn intersect_u16(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
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
            i += 1;
        } else {
            j += 1;
        }
    }
}

#[inline(always)]
fn union_u16(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
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
fn difference_u16(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
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

#[inline(always)]
fn phrase_matches(doc_text: &[u8], phrase_tokens: &[&[u8]]) -> bool {
    let num_tokens = phrase_tokens.len();
    if num_tokens == 0 { return true; }
    let first_target = unsafe { *phrase_tokens.get_unchecked(0) };
    let first_len = first_target.len();
    let tlen = doc_text.len();
    let mut p = 0;

    while p < tlen {
        while p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(p) as usize) } {
            p += 1;
        }
        if p >= tlen { break; }
        let w_start = p;
        while p < tlen && unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(p) as usize) } {
            p += 1;
        }
        let w_len = p - w_start;

        if w_len == first_len {
            let mut matches_first = true;
            for k in 0..first_len {
                if (unsafe { *doc_text.get_unchecked(w_start + k) } | 0x20) != (unsafe { *first_target.get_unchecked(k) } | 0x20) {
                    matches_first = false;
                    break;
                }
            }

            if matches_first {
                let mut scan_p = p;
                let mut matched_all = true;

                for t_idx in 1..num_tokens {
                    let target = unsafe { *phrase_tokens.get_unchecked(t_idx) };
                    let target_len = target.len();

                    while scan_p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(scan_p) as usize) } {
                        scan_p += 1;
                    }
                    if scan_p >= tlen {
                        matched_all = false;
                        break;
                    }
                    let n_start = scan_p;
                    while scan_p < tlen && unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(scan_p) as usize) } {
                        scan_p += 1;
                    }
                    let n_len = scan_p - n_start;

                    if n_len != target_len {
                        matched_all = false;
                        break;
                    }

                    let mut tok_ok = true;
                    for k in 0..target_len {
                        if (unsafe { *doc_text.get_unchecked(n_start + k) } | 0x20) != (unsafe { *target.get_unchecked(k) } | 0x20) {
                            tok_ok = false;
                            break;
                        }
                    }

                    if !tok_ok {
                        matched_all = false;
                        break;
                    }
                }

                if matched_all {
                    return true;
                }
            }
        }
    }
    false
}

fn main() {
    unsafe { mallopt(-8, 2); }

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

    let docs_path = format!("{}/docs.txt", data_dir);
    let queries_path = format!("{}/queries.txt", data_dir);
    let results_path = format!("{}/results.txt", out_dir);

    let q_mmap = match MmapSlice::open(&queries_path) {
        Some(m) => m,
        None => return,
    };
    let q_data = q_mmap.as_slice();

    let mut dict = Dict::new(131_072);
    let mut num_terms = 0u32;

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

    let docs_mmap = match MmapSlice::open(&docs_path) {
        Some(m) => m,
        None => return,
    };
    let docs_data = docs_mmap.as_slice();
    let n = docs_data.len();

    // Instant block byte boundaries in 1 microsecond!
    let mut block_byte_boundaries = [0usize; NUM_BLOCKS + 1];
    for i in 1..NUM_BLOCKS {
        let mut pos = (n * i) / NUM_BLOCKS;
        while pos < n && docs_data[pos] != b'\n' { pos += 1; }
        if pos < n { pos += 1; }
        block_byte_boundaries[i] = pos.min(n);
    }
    block_byte_boundaries[NUM_BLOCKS] = n;

    let num_threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).min(16);

    let mut block_offsets: [Vec<u32>; NUM_BLOCKS] = [
        vec![0u32; (num_terms as usize) + 1],
        vec![0u32; (num_terms as usize) + 1],
        vec![0u32; (num_terms as usize) + 1],
        vec![0u32; (num_terms as usize) + 1],
    ];
    let mut block_postings: [Vec<u16>; NUM_BLOCKS] = [
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
    ];
    let mut block_base_docs = [0u32; NUM_BLOCKS];

    let mut doc_starts = vec![0u32; 300_000];
    let mut doc_lens = vec![0u32; 300_000];

    // Pre-allocate persistent thread buffers for ONE-PASS indexing
    let mut thread_postings: Vec<Vec<u64>> = (0..num_threads)
        .map(|_| Vec::with_capacity(600_000))
        .collect();
    let mut thread_counts: Vec<Vec<u32>> = (0..num_threads)
        .map(|_| vec![0u32; num_terms as usize])
        .collect();
    let mut thread_doc_info: Vec<Vec<(u32, u32)>> = (0..num_threads)
        .map(|_| Vec::with_capacity(8_000))
        .collect();

    let mut running_global_doc_id = 0u32;

    for b in 0..NUM_BLOCKS {
        block_base_docs[b] = running_global_doc_id;

        let b_start_byte = block_byte_boundaries[b];
        let b_end_byte = block_byte_boundaries[b + 1];
        let b_len = b_end_byte - b_start_byte;
        if b_len == 0 { continue; }

        let mut sub_chunks = Vec::with_capacity(num_threads);
        let mut cur_sb = b_start_byte;
        for t in 0..num_threads {
            let target = b_start_byte + ((b_len * (t + 1)) / num_threads);
            let mut end = target.min(b_end_byte);
            while end < b_end_byte && docs_data[end] != b'\n' {
                end += 1;
            }
            if end < b_end_byte {
                end += 1;
            }
            sub_chunks.push((cur_sb, end));
            cur_sb = end;
        }

        // ONE-PASS TOKENIZATION: Parse directly, recording postings into thread buffers
        std::thread::scope(|s| {
            for (((postings_buf, counts_buf), doc_info_buf), &(sb, eb)) in
                thread_postings.iter_mut()
                .zip(thread_counts.iter_mut())
                .zip(thread_doc_info.iter_mut())
                .zip(sub_chunks.iter())
            {
                let dict_ref = &dict;
                let chunk = &docs_data[sb..eb];

                postings_buf.clear();
                counts_buf.fill(0);
                doc_info_buf.clear();

                s.spawn(move || {
                    let mut last_seen = vec![u16::MAX; num_terms as usize];
                    let mut i = 0;
                    let clen = chunk.len();
                    let mut doc_offset_in_thread = 0u16;

                    while i < clen {
                        while i < clen && chunk[i] != b'\t' { i += 1; }
                        if i >= clen { break; }
                        i += 1;
                        let text_start_rel = i;
                        while i < clen && chunk[i] != b'\n' { i += 1; }
                        let text_end_rel = i;
                        i += 1;

                        let text_len = (text_end_rel - text_start_rel) as u32;
                        doc_info_buf.push(((sb + text_start_rel) as u32, text_len));

                        let text = &chunk[text_start_rel..text_end_rel];
                        let mut p = 0;
                        let tlen = text.len();
                        let mut tok_buf = [0u8; 24];

                        while p < tlen {
                            while p < tlen && !unsafe { *IS_TOKEN.get_unchecked(*text.get_unchecked(p) as usize) } {
                                p += 1;
                            }
                            if p >= tlen { break; }
                            let mut tok_len = 0;
                            let mut h = 0x811c9dc5u32;
                            while p < tlen && unsafe { *IS_TOKEN.get_unchecked(*text.get_unchecked(p) as usize) } {
                                let b = unsafe { *text.get_unchecked(p) } | 0x20;
                                if tok_len < 23 {
                                    tok_buf[tok_len] = b;
                                    tok_len += 1;
                                }
                                h ^= b as u32;
                                h = h.wrapping_mul(0x01000193);
                                p += 1;
                            }
                            if tok_len <= 23 {
                                if h == 0 { h = 1; }
                                if let Some(term_id) = dict_ref.get_with_hash(&tok_buf[..tok_len], h) {
                                    let tid = term_id as usize;
                                    if unsafe { *last_seen.get_unchecked(tid) } != doc_offset_in_thread {
                                        unsafe {
                                            *last_seen.get_unchecked_mut(tid) = doc_offset_in_thread;
                                            *counts_buf.get_unchecked_mut(tid) += 1;
                                        }
                                        postings_buf.push(((term_id as u64) << 16) | (doc_offset_in_thread as u64));
                                    }
                                }
                            }
                        }
                        doc_offset_in_thread += 1;
                    }
                });
            }
        });

        // Compute base doc IDs and copy doc starts/lens
        let mut thread_base_loc = Vec::with_capacity(num_threads);
        let mut cur_loc = 0u16;
        for t in 0..num_threads {
            thread_base_loc.push(cur_loc);
            let info = &thread_doc_info[t];
            let g_start = (running_global_doc_id + cur_loc as u32) as usize;
            for (idx, &(d_s, d_l)) in info.iter().enumerate() {
                doc_starts[g_start + idx] = d_s;
                doc_lens[g_start + idx] = d_l;
            }
            cur_loc += info.len() as u16;
        }
        running_global_doc_id += cur_loc as u32;

        // Compute offsets and write pointers
        let mut thread_write_ptrs: Vec<Vec<u32>> = (0..num_threads)
            .map(|_| vec![0u32; num_terms as usize])
            .collect();

        let mut total_postings = 0u32;
        for term in 0..(num_terms as usize) {
            block_offsets[b][term] = total_postings;
            let mut cur = total_postings;
            for t in 0..num_threads {
                thread_write_ptrs[t][term] = cur;
                cur += thread_counts[t][term];
            }
            total_postings = cur;
        }
        block_offsets[b][num_terms as usize] = total_postings;

        // O(N) Parallel Zero-Sort Scatter into block_postings
        let mut b_postings = vec![0u16; total_postings as usize];
        let base_ptr = b_postings.as_mut_ptr() as usize;

        std::thread::scope(|s| {
            for (t, t_postings) in thread_postings.iter().enumerate() {
                let mut write_ptrs = std::mem::take(&mut thread_write_ptrs[t]);
                let base_loc = thread_base_loc[t];

                s.spawn(move || {
                    let raw_ptr = base_ptr as *mut u16;
                    for &rec in t_postings {
                        let tid = (rec >> 16) as usize;
                        let doc_offset = (rec & 0xFFFF) as u16;
                        let loc_id = base_loc + doc_offset;
                        let dest = unsafe { *write_ptrs.get_unchecked(tid) };
                        unsafe {
                            *write_ptrs.get_unchecked_mut(tid) += 1;
                            *raw_ptr.add(dest as usize) = loc_id;
                        }
                    }
                });
            }
        });

        block_postings[b] = b_postings;

        unsafe {
            madvise(docs_data.as_ptr().add(b_start_byte) as *mut _, b_end_byte - b_start_byte, 4);
        }
    }

    drop(dict);
    drop(thread_postings);
    drop(thread_counts);
    drop(thread_doc_info);

    if docs_mmap.len > 0 {
        unsafe { madvise(docs_mmap.ptr, docs_mmap.len, 4); }
    }

    let num_query_threads = num_threads;
    let chunk_size = (parsed_queries.len() + num_query_threads - 1) / num_query_threads;

    let thread_outputs: Vec<Vec<u8>> = std::thread::scope(|s| {
        let handles: Vec<_> = parsed_queries.chunks(chunk_size).map(|chunk| {
            let block_offsets_ref = &block_offsets;
            let block_postings_ref = &block_postings;
            let block_base_docs_ref = &block_base_docs;
            let doc_starts_ref = &doc_starts;
            let doc_lens_ref = &doc_lens;
            let docs_data_ref = docs_data;

            s.spawn(move || {
                let mut thread_out = Vec::with_capacity(chunk.len() * 32);
                let mut buf_a = Vec::with_capacity(16_384);
                let mut buf_b = Vec::with_capacity(16_384);
                let mut top_k = Vec::with_capacity(10);

                for query in chunk {
                    top_k.clear();
                    let mut total_count: u64 = 0;
                    let mut total_id_sum: u64 = 0;

                    match query {
                        ParsedQuery::Bool { first_term, steps } => {
                            for b in 0..NUM_BLOCKS {
                                let offsets = &block_offsets_ref[b];
                                let postings = &block_postings_ref[b];
                                let get_postings = |term_id: u32| -> &[u16] {
                                    let tid = term_id as usize;
                                    let s = offsets[tid] as usize;
                                    let e = offsets[tid + 1] as usize;
                                    &postings[s..e]
                                };

                                let r_slice: &[u16] = match *first_term {
                                    Some(t) => get_postings(t),
                                    None => &[],
                                };

                                let base_doc_id = block_base_docs_ref[b] as u64;

                                if steps.is_empty() {
                                    let count = r_slice.len();
                                    total_count += count as u64;
                                    let loc_sum: u64 = r_slice.iter().map(|&x| x as u64).sum();
                                    total_id_sum += base_doc_id * (count as u64) + loc_sum;
                                    for &loc_id in r_slice.iter().take(10 - top_k.len()) {
                                        top_k.push((base_doc_id as u32) + (loc_id as u32));
                                    }
                                } else {
                                    let (first_op, first_term_opt) = steps[0];
                                    let b_first: &[u16] = match first_term_opt {
                                        Some(t) => get_postings(t),
                                        None => &[],
                                    };
                                    match first_op {
                                        Op::And => intersect_u16(r_slice, b_first, &mut buf_a),
                                        Op::Or => union_u16(r_slice, b_first, &mut buf_a),
                                        Op::Not => difference_u16(r_slice, b_first, &mut buf_a),
                                    }
                                    for &(op, maybe_term) in &steps[1..] {
                                        let b_slice: &[u16] = match maybe_term {
                                            Some(t) => get_postings(t),
                                            None => &[],
                                        };
                                        match op {
                                            Op::And => intersect_u16(&buf_a, b_slice, &mut buf_b),
                                            Op::Or => union_u16(&buf_a, b_slice, &mut buf_b),
                                            Op::Not => difference_u16(&buf_a, b_slice, &mut buf_b),
                                        }
                                        std::mem::swap(&mut buf_a, &mut buf_b);
                                    }
                                    let count = buf_a.len();
                                    total_count += count as u64;
                                    let loc_sum: u64 = buf_a.iter().map(|&x| x as u64).sum();
                                    total_id_sum += base_doc_id * (count as u64) + loc_sum;
                                    for &loc_id in buf_a.iter().take(10 - top_k.len()) {
                                        top_k.push((base_doc_id as u32) + (loc_id as u32));
                                    }
                                }
                            }
                        }
                        ParsedQuery::Phrase { terms, raw_tokens } => {
                            if !terms.iter().any(|t| t.is_none()) {
                                let mut term_ids = [0u32; 8];
                                let num_t = terms.len().min(8);
                                for (k, &t) in terms.iter().take(8).enumerate() {
                                    term_ids[k] = t.unwrap();
                                }
                                let term_ids = &term_ids[..num_t];

                                for b in 0..NUM_BLOCKS {
                                    let offsets = &block_offsets_ref[b];
                                    let postings = &block_postings_ref[b];
                                    let get_postings = |term_id: u32| -> &[u16] {
                                        let tid = term_id as usize;
                                        let s = offsets[tid] as usize;
                                        let e = offsets[tid + 1] as usize;
                                        &postings[s..e]
                                    };

                                    let mut min_idx = 0;
                                    let mut min_len = usize::MAX;
                                    for (idx, &tid) in term_ids.iter().enumerate() {
                                        let plen = (offsets[tid as usize + 1] - offsets[tid as usize]) as usize;
                                        if plen < min_len {
                                            min_len = plen;
                                            min_idx = idx;
                                        }
                                    }

                                    if min_len > 0 {
                                        buf_a.clear();
                                        buf_a.extend_from_slice(get_postings(term_ids[min_idx]));

                                        for (idx, &tid) in term_ids.iter().enumerate() {
                                            if idx == min_idx { continue; }
                                            intersect_u16(&buf_a, get_postings(tid), &mut buf_b);
                                            std::mem::swap(&mut buf_a, &mut buf_b);
                                            if buf_a.is_empty() { break; }
                                        }

                                        let base_doc_id = block_base_docs_ref[b];
                                        for &loc_id in &buf_a {
                                            let doc_id = base_doc_id + (loc_id as u32);
                                            let doc_idx = doc_id as usize;
                                            let d_start = unsafe { *doc_starts_ref.get_unchecked(doc_idx) } as usize;
                                            let d_len = unsafe { *doc_lens_ref.get_unchecked(doc_idx) } as usize;
                                            let doc_text = &docs_data_ref[d_start..d_start + d_len];
                                            if phrase_matches(doc_text, raw_tokens) {
                                                total_count += 1;
                                                total_id_sum += doc_id as u64;
                                                if top_k.len() < 10 {
                                                    top_k.push(doc_id);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    write_u32_fast(total_count as u32, &mut thread_out);
                    thread_out.push(b' ');
                    write_u64_fast(total_id_sum, &mut thread_out);
                    for &id in &top_k {
                        thread_out.push(b' ');
                        write_u32_fast(id, &mut thread_out);
                    }
                    thread_out.push(b'\n');
                }
                thread_out
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let _ = std::fs::create_dir_all(&out_dir);
    let mut out_file = match File::create(&results_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    for chunk in &thread_outputs {
        let _ = out_file.write_all(chunk);
    }
}
