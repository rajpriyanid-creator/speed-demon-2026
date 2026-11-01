use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;

#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::{_mm_crc32_u64, _mm_crc32_u8};

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

#[inline(always)]
unsafe fn hash_bytes_fast(tok: *const u8, len: usize) -> u32 {
    let mut crc = 0u64;
    let mut i = 0;
    while i + 8 <= len {
        let val = std::ptr::read_unaligned(tok.add(i) as *const u64) | 0x2020202020202020;
        crc = _mm_crc32_u64(crc, val);
        i += 8;
    }
    let mut c32 = crc as u32;
    while i < len {
        c32 = _mm_crc32_u8(c32, *tok.add(i) | 0x20);
        i += 1;
    }
    if c32 == 0 { 1 } else { c32 }
}

#[inline(always)]
unsafe fn make_h64(tok: *const u8, len: usize) -> u64 {
    let h32 = hash_bytes_fast(tok, len);
    ((len as u64) << 32) | (h32 as u64)
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
#[repr(C)]
struct DictEntry {
    hash: u64,
    term_id: u32,
    _pad: u32,
}

struct FastDict {
    table: Vec<DictEntry>,
    mask: usize,
    bloom: Vec<u64>,
    bloom_mask: usize,
}

impl FastDict {
    fn new(cap: usize) -> Self {
        let size = cap.next_power_of_two();
        let bloom_words = 65536; // 64 KB Bloom filter
        Self {
            table: vec![DictEntry { hash: 0, term_id: 0, _pad: 0 }; size],
            mask: size - 1,
            bloom: vec![0u64; bloom_words],
            bloom_mask: (bloom_words * 64) - 1,
        }
    }

    #[inline(always)]
    fn insert(&mut self, h: u64, term_id: u32) {
        let bit = (h as usize) & self.bloom_mask;
        self.bloom[bit / 64] |= 1u64 << (bit % 64);

        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { self.table.get_unchecked_mut(idx) };
            if slot.hash == 0 || slot.hash == h {
                slot.hash = h;
                slot.term_id = term_id;
                return;
            }
            idx = (idx + 1) & self.mask;
        }
    }

    #[inline(always)]
    fn get(&self, h: u64) -> Option<u32> {
        let bit = (h as usize) & self.bloom_mask;
        let word = unsafe { *self.bloom.get_unchecked(bit / 64) };
        if (word & (1u64 << (bit % 64))) == 0 {
            return None;
        }

        let mut idx = (h as usize) & self.mask;
        loop {
            let slot = unsafe { *self.table.get_unchecked(idx) };
            if slot.hash == h {
                return Some(slot.term_id);
            }
            if slot.hash == 0 {
                return None;
            }
            idx = (idx + 1) & self.mask;
        }
    }
}

#[derive(Clone, Copy)]
enum Op { And, Or, Not }

enum ParsedQuery<'a> {
    Bool {
        first_term: Option<u32>,
        steps: Vec<(Op, Option<u32>)>,
    },
    Phrase {
        term_ids: Vec<u32>,
        valid: bool,
        raw_tokens: Vec<&'a [u8]>,
    },
}

#[inline(always)]
fn phrase_matches(doc_text: &[u8], tokens: &[&[u8]]) -> bool {
    let mut p = 0;
    let dlen = doc_text.len();
    let first = tokens[0];
    let flen = first.len();

    while p < dlen {
        while p < dlen && !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(p) as usize) } { p += 1; }
        if p >= dlen { break; }
        let tok_start = p;
        while p < dlen && unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(p) as usize) } { p += 1; }
        let tok_len = p - tok_start;

        if tok_len == flen {
            let mut matches_first = true;
            for m in 0..flen {
                if unsafe { (*doc_text.get_unchecked(tok_start + m) | 0x20) != (*first.get_unchecked(m) | 0x20) } {
                    matches_first = false;
                    break;
                }
            }

            if matches_first {
                let mut cur_p = p;
                let mut all_match = true;
                for &next_tok in &tokens[1..] {
                    while cur_p < dlen && !unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(cur_p) as usize) } { cur_p += 1; }
                    if cur_p >= dlen {
                        all_match = false;
                        break;
                    }
                    let nt_start = cur_p;
                    while cur_p < dlen && unsafe { *IS_TOKEN.get_unchecked(*doc_text.get_unchecked(cur_p) as usize) } { cur_p += 1; }
                    let nt_len = cur_p - nt_start;
                    if nt_len != next_tok.len() {
                        all_match = false;
                        break;
                    }
                    for m in 0..nt_len {
                        if unsafe { (*doc_text.get_unchecked(nt_start + m) | 0x20) != (*next_tok.get_unchecked(m) | 0x20) } {
                            all_match = false;
                            break;
                        }
                    }
                    if !all_match { break; }
                }
                if all_match { return true; }
            }
        }
    }
    false
}

#[inline(always)]
fn intersect_u16(a: &[u16], b: &[u16], out: &mut Vec<u16>) {
    out.clear();
    let na = a.len();
    let nb = b.len();
    if na == 0 || nb == 0 { return; }

    if na * 16 < nb {
        let mut j = 0;
        for &val in a {
            let mut low = j;
            let mut high = nb;
            while low < high {
                let mid = (low + high) / 2;
                if unsafe { *b.get_unchecked(mid) } < val {
                    low = mid + 1;
                } else {
                    high = mid;
                }
            }
            if low < nb && unsafe { *b.get_unchecked(low) } == val {
                out.push(val);
                j = low + 1;
            } else {
                j = low;
            }
            if j >= nb { break; }
        }
        return;
    } else if nb * 16 < na {
        let mut i = 0;
        for &val in b {
            let mut low = i;
            let mut high = na;
            while low < high {
                let mid = (low + high) / 2;
                if unsafe { *a.get_unchecked(mid) } < val {
                    low = mid + 1;
                } else {
                    high = mid;
                }
            }
            if low < na && unsafe { *a.get_unchecked(low) } == val {
                out.push(val);
                i = low + 1;
            } else {
                i = low;
            }
            if i >= na { break; }
        }
        return;
    }

    let mut i = 0;
    let mut j = 0;
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

static DIGIT_PAIRS: &[u8; 200] = b"\
00010203040506070809\
10111213141516171819\
20212223242526272829\
30313233343536373839\
40414243444546474849\
50515253545556575859\
60616263646566676869\
70717273747576777879\
80818283848586878889\
90919293949596979899";

#[inline(always)]
unsafe fn write_u64_fast(mut n: u64, buf: *mut u8) -> usize {
    if n == 0 {
        *buf = b'0';
        return 1;
    }
    let mut temp = [0u8; 20];
    let mut i = 0;
    while n >= 100 {
        let rem = (n % 100) as usize * 2;
        n /= 100;
        temp[i] = DIGIT_PAIRS[rem + 1];
        temp[i + 1] = DIGIT_PAIRS[rem];
        i += 2;
    }
    if n >= 10 {
        let rem = (n as usize) * 2;
        temp[i] = DIGIT_PAIRS[rem + 1];
        temp[i + 1] = DIGIT_PAIRS[rem];
        i += 2;
    } else if n > 0 {
        temp[i] = b'0' + n as u8;
        i += 1;
    }
    for j in 0..i {
        *buf.add(j) = temp[i - 1 - j];
    }
    i
}

#[derive(Clone, Copy)]
struct QueryAccum {
    count: u32,
    id_sum: u64,
    top_k: [u32; 10],
    top_k_len: u8,
}

const NUM_SEGMENTS: usize = 4;

struct SegmentIndex {
    base_doc: u32,
    offsets: Vec<u32>,
    postings: Vec<u16>,
    doc_starts: Vec<u32>,
    doc_lens: Vec<u32>,
    data_start: usize,
}

#[derive(Clone, Copy, Default)]
#[repr(C, packed)]
struct PostingPair {
    term_id: u32,
    doc_id: u16,
}

fn main() {
    unsafe { mallopt(-8, 2); }

    let mut data_dir = "/data".to_string();
    let mut out_dir = "/out".to_string();
    let args: Vec<String> = std::env::args().collect();
    let mut positional = Vec::new();
    let mut arg_i = 1;
    while arg_i < args.len() {
        if args[arg_i] == "--data" && arg_i + 1 < args.len() {
            data_dir = args[arg_i + 1].clone();
            arg_i += 2;
        } else if args[arg_i] == "--out" && arg_i + 1 < args.len() {
            out_dir = args[arg_i + 1].clone();
            arg_i += 2;
        } else if !args[arg_i].starts_with("--") {
            positional.push(args[arg_i].clone());
            arg_i += 1;
        } else {
            arg_i += 1;
        }
    }
    if positional.len() >= 1 && data_dir == "/data" { data_dir = positional[0].clone(); }
    if positional.len() >= 2 && out_dir == "/out" { out_dir = positional[1].clone(); }

    let queries_path = format!("{}/queries.txt", data_dir);
    let docs_path = format!("{}/docs.txt", data_dir);
    let results_path = format!("{}/results.txt", out_dir);

    let q_file = match File::open(&queries_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let q_len = match q_file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    let q_ptr = unsafe { mmap(std::ptr::null_mut(), q_len, 1, 1, q_file.as_raw_fd(), 0) };
    if q_ptr == usize::MAX as *mut u8 || q_ptr.is_null() { return; }
    let q_data = unsafe { std::slice::from_raw_parts(q_ptr, q_len) };

    let mut dict = FastDict::new(131_072);
    let mut num_terms = 0u32;
    for line in q_data.split(|&b| b == b'\n') {
        let line = if line.ends_with(b"\r") { &line[..line.len() - 1] } else { line };
        if line.is_empty() { continue; }
        if line.starts_with(b"BOOL ") {
            for part in line[5..].split(|&b| b == b' ') {
                if part == b"AND" || part == b"OR" || part == b"NOT" || part.is_empty() { continue; }
                let h64 = unsafe { make_h64(part.as_ptr(), part.len()) };
                if dict.get(h64).is_none() {
                    dict.insert(h64, num_terms);
                    num_terms += 1;
                }
            }
        } else if line.starts_with(b"PHRASE \"") {
            let inner = &line[8..line.len().saturating_sub(1)];
            for part in inner.split(|&b| b == b' ') {
                if part.is_empty() { continue; }
                let h64 = unsafe { make_h64(part.as_ptr(), part.len()) };
                if dict.get(h64).is_none() {
                    dict.insert(h64, num_terms);
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
            let first_h = unsafe { make_h64(first.as_ptr(), first.len()) };
            let first_id = dict.get(first_h);
            let mut steps = Vec::new();
            while let Some(op_bytes) = parts.next() {
                let op = match op_bytes {
                    b"AND" => Op::And,
                    b"OR" => Op::Or,
                    b"NOT" => Op::Not,
                    _ => continue,
                };
                let term_bytes = parts.next().unwrap_or(b"");
                let th = unsafe { make_h64(term_bytes.as_ptr(), term_bytes.len()) };
                let term_id = dict.get(th);
                steps.push((op, term_id));
            }
            parsed_queries.push(ParsedQuery::Bool { first_term: first_id, steps });
        } else if line.starts_with(b"PHRASE \"") {
            let inner = &line[8..line.len().saturating_sub(1)];
            let mut term_ids = Vec::new();
            let mut raw_tokens = Vec::new();
            let mut valid = true;
            for part in inner.split(|&b| b == b' ') {
                if !part.is_empty() {
                    let th = unsafe { make_h64(part.as_ptr(), part.len()) };
                    match dict.get(th) {
                        Some(tid) => term_ids.push(tid),
                        None => { valid = false; }
                    }
                    raw_tokens.push(part);
                }
            }
            parsed_queries.push(ParsedQuery::Phrase { term_ids, valid, raw_tokens });
        }
    }

    let docs_file = match File::open(&docs_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let docs_len = match docs_file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    let docs_fd = docs_file.as_raw_fd();
    let docs_ptr = unsafe { mmap(std::ptr::null_mut(), docs_len, 1, 1, docs_fd, 0) };
    if docs_ptr == usize::MAX as *mut u8 || docs_ptr.is_null() { return; }
    let docs_slice = unsafe { std::slice::from_raw_parts(docs_ptr, docs_len) };

    let mut seg_byte_boundaries = [0usize; NUM_SEGMENTS + 1];
    seg_byte_boundaries[0] = 0;
    for i in 1..NUM_SEGMENTS {
        let target = (docs_len * i) / NUM_SEGMENTS;
        let mut p = target;
        while p < docs_len && docs_slice[p] != b'\n' { p += 1; }
        if p < docs_len { p += 1; }
        seg_byte_boundaries[i] = p;
    }
    seg_byte_boundaries[NUM_SEGMENTS] = docs_len;

    let num_threads = 12;
    let mut segments: Vec<SegmentIndex> = Vec::with_capacity(NUM_SEGMENTS);
    let mut running_global_doc_id = 0u32;

    for seg in 0..NUM_SEGMENTS {
        let seg_base_doc = running_global_doc_id;
        let seg_start = seg_byte_boundaries[seg];
        let seg_end = seg_byte_boundaries[seg + 1];
        let seg_len = seg_end - seg_start;
        let seg_slice = &docs_slice[seg_start..seg_end];

        let mut sub_chunks = Vec::with_capacity(num_threads);
        let mut cur_sb = 0usize;
        for t in 0..num_threads {
            let target = (seg_len * (t + 1)) / num_threads;
            let mut end = target.min(seg_len);
            while end < seg_len && seg_slice[end] != b'\n' { end += 1; }
            if end < seg_len { end += 1; }
            sub_chunks.push((cur_sb, end));
            cur_sb = end;
        }

        #[derive(Default)]
        struct SegThreadResult {
            counts: Vec<u32>,
            pairs: Vec<PostingPair>,
            doc_starts: Vec<u32>,
            doc_lens: Vec<u32>,
        }

        let mut t_results: Vec<SegThreadResult> = std::thread::scope(|s| {
            let handles: Vec<_> = sub_chunks.iter().map(|&(sb, eb)| {
                let dict_ref = &dict;
                let chunk = &seg_slice[sb..eb];
                s.spawn(move || {
                    let mut local_counts = vec![0u32; num_terms as usize];
                    let mut last_seen = vec![u16::MAX; num_terms as usize];
                    let est_docs = ((eb - sb) / 1000 + 512).max(4000);
                    let mut local_starts = Vec::with_capacity(est_docs);
                    let mut local_lens = Vec::with_capacity(est_docs);
                    let est_pairs = est_docs * 90;
                    let mut pairs = Vec::with_capacity(est_pairs);

                    let mut i = 0;
                    let clen = chunk.len();
                    let mut loc_doc: u16 = 0;
                    let base_ptr = chunk.as_ptr();

                    while i < clen {
                        while i < clen && chunk[i] != b'\t' { i += 1; }
                        if i >= clen { break; }
                        i += 1;
                        let text_start = i;
                        while i < clen && chunk[i] != b'\n' { i += 1; }
                        let text_end = i;
                        i += 1;
                        local_starts.push((sb + text_start) as u32);
                        local_lens.push((text_end - text_start) as u32);
                        let mut p = text_start;
                        while p < text_end {
                            while p < text_end && !unsafe { *IS_TOKEN.get_unchecked(*chunk.get_unchecked(p) as usize) } { p += 1; }
                            if p >= text_end { break; }
                            let tok_start = p;
                            while p < text_end && unsafe { *IS_TOKEN.get_unchecked(*chunk.get_unchecked(p) as usize) } { p += 1; }
                            let tok_len = p - tok_start;
                            if tok_len <= 23 {
                                let ptr_tok = unsafe { base_ptr.add(tok_start) };
                                let h64 = unsafe { make_h64(ptr_tok, tok_len) };
                                if let Some(term_id) = dict_ref.get(h64) {
                                    let tid = term_id as usize;
                                    if unsafe { *last_seen.get_unchecked(tid) } != loc_doc {
                                        unsafe {
                                            *last_seen.get_unchecked_mut(tid) = loc_doc;
                                            *local_counts.get_unchecked_mut(tid) += 1;
                                        }
                                        pairs.push(PostingPair { term_id, doc_id: loc_doc });
                                    }
                                }
                            }
                        }
                        loc_doc += 1;
                    }
                    SegThreadResult {
                        counts: local_counts,
                        pairs,
                        doc_starts: local_starts,
                        doc_lens: local_lens,
                    }
                })
            }).collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let mut t_start_doc = Vec::with_capacity(num_threads);
        let mut cur_d = 0u16;
        for r in &t_results {
            t_start_doc.push(cur_d);
            cur_d += r.doc_starts.len() as u16;
        }
        running_global_doc_id += cur_d as u32;

        let mut seg_doc_starts = Vec::with_capacity(cur_d as usize);
        let mut seg_doc_lens = Vec::with_capacity(cur_d as usize);
        for r in &t_results {
            seg_doc_starts.extend_from_slice(&r.doc_starts);
            seg_doc_lens.extend_from_slice(&r.doc_lens);
        }

        let mut offsets = vec![0u32; (num_terms as usize) + 1];
        let mut t_write_bases: Vec<Vec<u32>> = (0..num_threads).map(|_| vec![0u32; num_terms as usize]).collect();
        let mut total_postings = 0u32;
        for term in 0..(num_terms as usize) {
            offsets[term] = total_postings;
            let mut cur = total_postings;
            for t in 0..num_threads {
                t_write_bases[t][term] = cur;
                cur += t_results[t].counts[term];
            }
            total_postings = cur;
        }
        offsets[num_terms as usize] = total_postings;

        let mut all_postings: Vec<u16> = vec![0u16; total_postings as usize];
        let raw_postings_ptr = all_postings.as_mut_ptr() as usize;

        std::thread::scope(|s| {
            for t in 0..num_threads {
                let tr = std::mem::take(&mut t_results[t]);
                let mut write_pos = std::mem::take(&mut t_write_bases[t]);
                let base_doc_offset = t_start_doc[t];
                s.spawn(move || {
                    let raw_p = raw_postings_ptr as *mut u16;
                    for p in tr.pairs {
                        let tid = p.term_id as usize;
                        let global_loc_doc = p.doc_id + base_doc_offset;
                        let target = unsafe { *write_pos.get_unchecked(tid) };
                        unsafe {
                            *raw_p.add(target as usize) = global_loc_doc;
                            *write_pos.get_unchecked_mut(tid) += 1;
                        }
                    }
                });
            }
        });

        drop(t_results);
        drop(t_write_bases);

        segments.push(SegmentIndex {
            base_doc: seg_base_doc,
            offsets,
            postings: all_postings,
            doc_starts: seg_doc_starts,
            doc_lens: seg_doc_lens,
            data_start: seg_start,
        });
    }

    // Evaluate all 50,000 queries across the 4 segments in parallel
    let num_queries = parsed_queries.len();
    let mut query_accums: Vec<QueryAccum> = vec![
        QueryAccum { count: 0, id_sum: 0, top_k: [0; 10], top_k_len: 0 };
        num_queries
    ];

    let q_chunk_size = (num_queries + num_threads - 1) / num_threads;
    let seg_refs = &segments;
    let queries_ref = &parsed_queries;
    let out_accums_ptr = query_accums.as_mut_ptr() as usize;

    std::thread::scope(|s| {
        for t in 0..num_threads {
            let q_start = t * q_chunk_size;
            let q_end = (q_start + q_chunk_size).min(num_queries);
            if q_start >= q_end { continue; }

            s.spawn(move || {
                let acc_base = out_accums_ptr as *mut QueryAccum;
                let mut buf_a: Vec<u16> = Vec::with_capacity(8192);
                let mut buf_b: Vec<u16> = Vec::with_capacity(8192);
                let mut matches: Vec<u16> = Vec::with_capacity(8192);

                for q_idx in q_start..q_end {
                    let q = &queries_ref[q_idx];
                    let mut total_count = 0u32;
                    let mut total_sum = 0u64;
                    let mut top_k = [0u32; 10];
                    let mut top_k_len = 0u8;

                    for seg_idx in 0..NUM_SEGMENTS {
                        let seg = &seg_refs[seg_idx];
                        let offsets = &seg.offsets;
                        let postings = &seg.postings;
                        let base_doc = seg.base_doc;

                        let get_postings = |tid: u32| -> &[u16] {
                            let s = offsets[tid as usize] as usize;
                            let e = offsets[tid as usize + 1] as usize;
                            &postings[s..e]
                        };

                        let mut seg_cnt = 0u32;
                        let mut seg_sum = 0u64;

                        match q {
                            ParsedQuery::Bool { first_term, steps } => {
                                let r_slice: &[u16] = match first_term {
                                    Some(tid) => get_postings(*tid),
                                    None => &[],
                                };
                                if steps.is_empty() {
                                    seg_cnt = r_slice.len() as u32;
                                    let loc_sum: u64 = r_slice.iter().map(|&x| x as u64).sum();
                                    seg_sum = (base_doc as u64) * (seg_cnt as u64) + loc_sum;
                                    for &loc_id in r_slice.iter().take(10 - top_k_len as usize) {
                                        top_k[top_k_len as usize] = base_doc + (loc_id as u32);
                                        top_k_len += 1;
                                    }
                                } else {
                                    let (first_op, first_term_opt) = steps[0];
                                    let b_first: &[u16] = match first_term_opt {
                                        Some(tid) => get_postings(tid),
                                        None => &[],
                                    };
                                    match first_op {
                                        Op::And => intersect_u16(r_slice, b_first, &mut buf_a),
                                        Op::Or => union_u16(r_slice, b_first, &mut buf_a),
                                        Op::Not => difference_u16(r_slice, b_first, &mut buf_a),
                                    }
                                    for &(op, maybe_term) in &steps[1..] {
                                        let b: &[u16] = match maybe_term {
                                            Some(tid) => get_postings(tid),
                                            None => &[],
                                        };
                                        match op {
                                            Op::And => {
                                                intersect_u16(&buf_a, b, &mut buf_b);
                                                std::mem::swap(&mut buf_a, &mut buf_b);
                                            }
                                            Op::Or => {
                                                union_u16(&buf_a, b, &mut buf_b);
                                                std::mem::swap(&mut buf_a, &mut buf_b);
                                            }
                                            Op::Not => {
                                                difference_u16(&buf_a, b, &mut buf_b);
                                                std::mem::swap(&mut buf_a, &mut buf_b);
                                            }
                                        }
                                    }
                                    seg_cnt = buf_a.len() as u32;
                                    let loc_sum: u64 = buf_a.iter().map(|&x| x as u64).sum();
                                    seg_sum = (base_doc as u64) * (seg_cnt as u64) + loc_sum;
                                    for &loc_id in buf_a.iter().take(10 - top_k_len as usize) {
                                        top_k[top_k_len as usize] = base_doc + (loc_id as u32);
                                        top_k_len += 1;
                                    }
                                }
                            }
                            ParsedQuery::Phrase { term_ids, valid, raw_tokens } => {
                                if *valid {
                                    if term_ids.is_empty() {
                                    } else if term_ids.len() == 1 {
                                        let post = get_postings(term_ids[0]);
                                        seg_cnt = post.len() as u32;
                                        let loc_sum: u64 = post.iter().map(|&x| x as u64).sum();
                                        seg_sum = (base_doc as u64) * (seg_cnt as u64) + loc_sum;
                                        for &loc_id in post.iter().take(10 - top_k_len as usize) {
                                            top_k[top_k_len as usize] = base_doc + (loc_id as u32);
                                            top_k_len += 1;
                                        }
                                    } else {
                                        let p0 = get_postings(term_ids[0]);
                                        let p1 = get_postings(term_ids[1]);
                                        intersect_u16(p0, p1, &mut buf_a);
                                        for &tid in &term_ids[2..] {
                                            if buf_a.is_empty() { break; }
                                            let p_next = get_postings(tid);
                                            intersect_u16(&buf_a, p_next, &mut buf_b);
                                            std::mem::swap(&mut buf_a, &mut buf_b);
                                        }

                                        let cand_slice = &buf_a;
                                        if !cand_slice.is_empty() {
                                            matches.clear();
                                            for &cand_doc in cand_slice {
                                                let d_start = seg.doc_starts[cand_doc as usize] as usize;
                                                let d_len = seg.doc_lens[cand_doc as usize] as usize;
                                                let d_slice = &docs_slice[seg.data_start + d_start..seg.data_start + d_start + d_len];
                                                if phrase_matches(d_slice, &raw_tokens) {
                                                    matches.push(cand_doc);
                                                }
                                            }
                                            seg_cnt = matches.len() as u32;
                                            let loc_sum: u64 = matches.iter().map(|&x| x as u64).sum();
                                            seg_sum = (base_doc as u64) * (seg_cnt as u64) + loc_sum;
                                            for &loc_id in matches.iter().take(10 - top_k_len as usize) {
                                                top_k[top_k_len as usize] = base_doc + (loc_id as u32);
                                                top_k_len += 1;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        total_count += seg_cnt;
                        total_sum += seg_sum;
                    }

                    unsafe {
                        *acc_base.add(q_idx) = QueryAccum {
                            count: total_count,
                            id_sum: total_sum,
                            top_k,
                            top_k_len,
                        };
                    }
                }
            });
        }
    });

    let _ = std::fs::create_dir_all(&out_dir);
    if let Ok(mut out_file) = File::create(&results_path) {
        let mut out_buf = Vec::with_capacity(num_queries * 32);
        let mut num_buf = [0u8; 32];
        for acc in query_accums {
            if acc.count == 0 {
                out_buf.extend_from_slice(b"0 0\n");
            } else {
                let w = unsafe { write_u64_fast(acc.count as u64, num_buf.as_mut_ptr()) };
                out_buf.extend_from_slice(&num_buf[..w]);
                out_buf.push(b' ');
                let w = unsafe { write_u64_fast(acc.id_sum, num_buf.as_mut_ptr()) };
                out_buf.extend_from_slice(&num_buf[..w]);
                for i in 0..(acc.top_k_len as usize) {
                    out_buf.push(b' ');
                    let w = unsafe { write_u64_fast(acc.top_k[i] as u64, num_buf.as_mut_ptr()) };
                    out_buf.extend_from_slice(&num_buf[..w]);
                }
                out_buf.push(b'\n');
            }
            if out_buf.len() >= 65536 {
                let _ = out_file.write_all(&out_buf);
                out_buf.clear();
            }
        }
        if !out_buf.is_empty() {
            let _ = out_file.write_all(&out_buf);
        }
    }

    unsafe {
        munmap(docs_ptr, docs_len);
        munmap(q_ptr, q_len);
    }
}
