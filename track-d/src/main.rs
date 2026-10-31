use std::fs::File;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering::Relaxed};

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn mallopt(param: i32, value: i32) -> i32;
}

struct FastHeap {
    data: Vec<u64>,
}

impl FastHeap {
    #[inline(always)]
    fn new(cap: usize) -> Self { Self { data: Vec::with_capacity(cap) } }
    #[inline(always)]
    fn clear(&mut self) { self.data.clear(); }
    #[inline(always)]
    fn push(&mut self, cost: u64, node: u32) {
        let val = (cost << 21) | (node as u64);
        let mut i = self.data.len();
        self.data.push(val);
        while i > 0 {
            let p = (i - 1) >> 2;
            let parent_val = unsafe { *self.data.get_unchecked(p) };
            if val < parent_val {
                unsafe { *self.data.get_unchecked_mut(i) = parent_val; }
                i = p;
            } else { break; }
        }
        unsafe { *self.data.get_unchecked_mut(i) = val; }
    }
    #[inline(always)]
    fn pop(&mut self) -> Option<(u64, u32)> {
        if self.data.is_empty() { return None; }
        let top = self.data[0];
        let last = self.data.pop().unwrap();
        if !self.data.is_empty() {
            let len = self.data.len();
            let mut i = 0;
            loop {
                let first_child = (i << 2) + 1;
                if first_child >= len { break; }
                let (best_val, best_child) = if first_child + 3 < len {
                    let c1 = first_child;
                    let c2 = first_child + 1;
                    let c3 = first_child + 2;
                    let c4 = first_child + 3;
                    let v1 = unsafe { *self.data.get_unchecked(c1) };
                    let v2 = unsafe { *self.data.get_unchecked(c2) };
                    let v3 = unsafe { *self.data.get_unchecked(c3) };
                    let v4 = unsafe { *self.data.get_unchecked(c4) };
                    let (b12_val, b12_idx) = if v2 < v1 { (v2, c2) } else { (v1, c1) };
                    let (b34_val, b34_idx) = if v4 < v3 { (v4, c4) } else { (v3, c3) };
                    if b34_val < b12_val { (b34_val, b34_idx) } else { (b12_val, b12_idx) }
                } else {
                    let mut b_child = first_child;
                    let mut b_val = unsafe { *self.data.get_unchecked(first_child) };
                    for child_idx in (first_child + 1)..len {
                        let c_val = unsafe { *self.data.get_unchecked(child_idx) };
                        if c_val < b_val {
                            b_val = c_val;
                            b_child = child_idx;
                        }
                    }
                    (b_val, b_child)
                };
                if best_val < last {
                    unsafe { *self.data.get_unchecked_mut(i) = best_val; }
                    i = best_child;
                } else { break; }
            }
            unsafe { *self.data.get_unchecked_mut(i) = last; }
        }
        Some((top >> 21, (top & 0x1F_FFFF) as u32))
    }
}

#[inline(always)]
unsafe fn parse_u32_fast(p: *const u8, pos: &mut usize) -> u32 {
    let mut val = 0u32;
    loop {
        let b = *p.add(*pos);
        if b >= b'0' && b <= b'9' {
            val = val * 10 + (b - b'0') as u32;
            *pos += 1;
        } else { break; }
    }
    val
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

    let graph_path = format!("{}/graph.txt", data_dir);
    let queries_path = format!("{}/queries.txt", data_dir);

    let g_file = match File::open(&graph_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let g_len = match g_file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    if g_len == 0 { return; }
    let g_fd = g_file.as_raw_fd();
    let g_ptr = unsafe { mmap(std::ptr::null_mut(), g_len, PROT_READ, MAP_SHARED, g_fd, 0) };
    if g_ptr == usize::MAX as *mut u8 {
        return;
    }

    let mut g_pos = 0;
    let n = unsafe { parse_u32_fast(g_ptr, &mut g_pos) } as usize;
    while g_pos < g_len && unsafe { *g_ptr.add(g_pos) } == b' ' { g_pos += 1; }
    let m = unsafe { parse_u32_fast(g_ptr, &mut g_pos) } as usize;
    while g_pos < g_len && unsafe { *g_ptr.add(g_pos) } != b'\n' { g_pos += 1; }
    if g_pos < g_len { g_pos += 1; }
    let edges_start = g_pos;

    let num_load_threads = 8;
    let mut edge_chunks = Vec::with_capacity(num_load_threads);
    let total_edge_bytes = g_len - edges_start;
    let mut cur = edges_start;
    for t in 0..num_load_threads {
        let target = edges_start + ((total_edge_bytes * (t + 1)) / num_load_threads);
        let mut end = target.min(g_len);
        while end < g_len && unsafe { *g_ptr.add(end) } != b'\n' { end += 1; }
        if end < g_len { end += 1; }
        edge_chunks.push((cur, end));
        cur = end;
    }

    let g_addr = g_ptr as usize;

    // Pass 1: Atomic degree count (only 4 MB memory!)
    let f_deg: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();

    std::thread::scope(|s| {
        for &(start, end) in &edge_chunks {
            let f_deg_ref = &f_deg;
            s.spawn(move || {
                let g_ptr = g_addr as *const u8;
                let mut pos = start;
                while pos < end {
                    let u = unsafe { parse_u32_fast(g_ptr, &mut pos) } as usize;
                    pos += 1;
                    let _v = unsafe { parse_u32_fast(g_ptr, &mut pos) };
                    while pos < end && unsafe { *g_ptr.add(pos) } != b'\n' { pos += 1; }
                    if pos < end { pos += 1; }

                    if u < n {
                        f_deg_ref[u].fetch_add(1, Relaxed);
                    }
                }
            });
        }
    });

    let mut f_head = vec![0u32; n + 1];
    let f_write: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();

    let mut cur_f = 0u32;
    for i in 0..n {
        f_head[i] = cur_f;
        f_write[i].store(cur_f, Relaxed);
        cur_f += f_deg[i].load(Relaxed);
    }
    f_head[n] = cur_f;
    drop(f_deg);

    // Pass 2: Atomic CSR scatter directly into f_edges (40 MB)
    let mut f_edges = vec![0u64; m];
    let f_edges_addr = f_edges.as_mut_ptr() as usize;

    std::thread::scope(|s| {
        for &(start, end) in &edge_chunks {
            let f_write_ref = &f_write;
            s.spawn(move || {
                let g_ptr = g_addr as *const u8;
                let f_raw = f_edges_addr as *mut u64;
                let mut pos = start;
                while pos < end {
                    let u = unsafe { parse_u32_fast(g_ptr, &mut pos) } as usize;
                    pos += 1;
                    let v = unsafe { parse_u32_fast(g_ptr, &mut pos) } as usize;
                    pos += 1;
                    let w = unsafe { parse_u32_fast(g_ptr, &mut pos) } as u64;
                    if pos < end && unsafe { *g_ptr.add(pos) } == b'\r' { pos += 1; }
                    if pos < end && unsafe { *g_ptr.add(pos) } == b'\n' { pos += 1; }

                    let packed_f = (v as u64) | (w << 32);

                    if u < n {
                        let idx = f_write_ref[u].fetch_add(1, Relaxed);
                        unsafe { *f_raw.add(idx as usize) = packed_f; }
                    }
                }
            });
        }
    });

    drop(f_write);
    unsafe { munmap(g_ptr, g_len); }

    // 2. Parse queries
    let q_file = match File::open(&queries_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let q_len = match q_file.metadata() {
        Ok(m) => m.len() as usize,
        Err(_) => return,
    };
    if q_len == 0 { return; }
    let q_fd = q_file.as_raw_fd();
    let q_ptr = unsafe { mmap(std::ptr::null_mut(), q_len, PROT_READ, MAP_SHARED, q_fd, 0) };
    if q_ptr == usize::MAX as *mut u8 {
        return;
    }

    let mut queries: Vec<(u32, u32)> = Vec::with_capacity(10_000);
    let mut q_pos = 0;
    while q_pos < q_len {
        let b = unsafe { *q_ptr.add(q_pos) };
        if b == b'\r' || b == b'\n' { q_pos += 1; continue; }
        let s = unsafe { parse_u32_fast(q_ptr, &mut q_pos) };
        if q_pos < q_len && unsafe { *q_ptr.add(q_pos) } == b' ' { q_pos += 1; }
        let t = unsafe { parse_u32_fast(q_ptr, &mut q_pos) };
        while q_pos < q_len && unsafe { *q_ptr.add(q_pos) } != b'\n' { q_pos += 1; }
        if q_pos < q_len { q_pos += 1; }
        queries.push((s, t));
    }
    unsafe { munmap(q_ptr, q_len); }

    let num_queries = queries.len();
    let mut results: Vec<i64> = vec![-1; num_queries];
    let query_cursor = AtomicUsize::new(0);
    let results_ptr = results.as_mut_ptr() as usize;

    let q_threads = 10;
    std::thread::scope(|s| {
        let f_head_ref = &f_head;
        let f_edges_ref = &f_edges;
        let query_cursor_ref = &query_cursor;
        let queries_ref = &queries;

        for _ in 0..q_threads {
            s.spawn(move || {
                let mut q = FastHeap::new(4096);
                let mut d: Vec<u64> = vec![u64::MAX; n];
                let mut visited: Vec<u32> = Vec::with_capacity(4096);
                let res_raw = results_ptr as *mut i64;

                const BATCH_SIZE: usize = 16;
                loop {
                    let start_idx = query_cursor_ref.fetch_add(BATCH_SIZE, Relaxed);
                    if start_idx >= num_queries { break; }
                    let end_idx = (start_idx + BATCH_SIZE).min(num_queries);

                    for q_idx in start_idx..end_idx {
                        let (src, dst) = unsafe { *queries_ref.get_unchecked(q_idx) };
                        if src == dst { unsafe { *res_raw.add(q_idx) = 0; } continue; }
                        let s_idx = src as usize;
                        let t_idx = dst as usize;
                        if s_idx >= n || t_idx >= n { unsafe { *res_raw.add(q_idx) = -1; } continue; }

                        for &node in &visited { unsafe { *d.get_unchecked_mut(node as usize) = u64::MAX; } }
                        visited.clear();
                        q.clear();

                        unsafe { *d.get_unchecked_mut(s_idx) = 0; }
                        visited.push(src);
                        q.push(0, src);

                        let mut found_dist = -1i64;

                        while let Some((curr_cost, curr_node)) = q.pop() {
                            if curr_node == dst {
                                found_dist = curr_cost as i64;
                                break;
                            }
                            if curr_cost > unsafe { *d.get_unchecked(curr_node as usize) } {
                                continue;
                            }
                            let u = curr_node as usize;
                            let e_start = unsafe { *f_head_ref.get_unchecked(u) } as usize;
                            let e_end = unsafe { *f_head_ref.get_unchecked(u + 1) } as usize;
                            for edge_idx in e_start..e_end {
                                let packed = unsafe { *f_edges_ref.get_unchecked(edge_idx) };
                                let v = (packed & 0xFFFF_FFFF) as u32;
                                let w = packed >> 32;
                                let new_dist = curr_cost + w;
                                let v_idx = v as usize;
                                let old_dist = unsafe { *d.get_unchecked(v_idx) };
                                if new_dist < old_dist {
                                    if old_dist == u64::MAX { visited.push(v); }
                                    unsafe { *d.get_unchecked_mut(v_idx) = new_dist; }
                                    q.push(new_dist, v);
                                }
                            }
                        }

                        unsafe { *res_raw.add(q_idx) = found_dist; }
                    }
                }
            });
        }
    });

    let _ = std::fs::create_dir_all(&out_dir);
    let mut out_file = match File::create(format!("{}/distances.txt", out_dir)) {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut out_buf: Vec<u8> = Vec::with_capacity(32 * 1024);
    for dist in results {
        let mut d = dist;
        if d == -1 {
            out_buf.extend_from_slice(b"-1\n");
        } else if d == 0 {
            out_buf.extend_from_slice(b"0\n");
        } else {
            let mut num_buf = [0u8; 20];
            let mut i = 0;
            while d > 0 {
                num_buf[i] = b'0' + (d % 10) as u8;
                d /= 10;
                i += 1;
            }
            while i > 0 {
                i -= 1;
                out_buf.push(num_buf[i]);
            }
            out_buf.push(b'\n');
        }
        if out_buf.len() >= 32 * 1024 {
            let _ = out_file.write_all(&out_buf);
            out_buf.clear();
        }
    }
    if !out_buf.is_empty() {
        let _ = out_file.write_all(&out_buf);
    }
}
