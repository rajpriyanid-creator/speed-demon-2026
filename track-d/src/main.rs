use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

// 4-ary Min-Heap with packed u64: (cost << 21) | node
struct FastHeap {
    data: Vec<u64>,
}

impl FastHeap {
    #[inline(always)]
    fn new(cap: usize) -> Self {
        Self {
            data: Vec::with_capacity(cap),
        }
    }

    #[inline(always)]
    fn clear(&mut self) {
        self.data.clear();
    }

    #[inline(always)]
    fn push(&mut self, cost: u64, node: u32) {
        let val = (cost << 21) | (node as u64);
        let mut i = self.data.len();
        self.data.push(val);
        while i > 0 {
            let p = (i - 1) >> 2;
            let parent_val = unsafe { *self.data.get_unchecked(p) };
            if val < parent_val {
                unsafe {
                    *self.data.get_unchecked_mut(i) = parent_val;
                }
                i = p;
            } else {
                break;
            }
        }
        unsafe { *self.data.get_unchecked_mut(i) = val; }
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<(u64, u32)> {
        if self.data.is_empty() {
            return None;
        }
        let top = self.data[0];
        let last = self.data.pop().unwrap();
        if !self.data.is_empty() {
            let len = self.data.len();
            let mut i = 0;
            loop {
                let first_child = (i << 2) + 1;
                if first_child >= len {
                    break;
                }
                let mut best_child = first_child;
                let mut best_val = unsafe { *self.data.get_unchecked(first_child) };

                let num_children = (len - first_child).min(4);
                for c in 1..num_children {
                    let child_idx = first_child + c;
                    let child_val = unsafe { *self.data.get_unchecked(child_idx) };
                    if child_val < best_val {
                        best_val = child_val;
                        best_child = child_idx;
                    }
                }

                if best_val < last {
                    unsafe { *self.data.get_unchecked_mut(i) = best_val; }
                    i = best_child;
                } else {
                    break;
                }
            }
            unsafe { *self.data.get_unchecked_mut(i) = last; }
        }
        Some((top >> 21, (top & 0x1F_FFFF) as u32))
    }
}

#[inline(always)]
fn parse_u32(data: &[u8], pos: &mut usize) -> u32 {
    let mut val = 0u32;
    while *pos < data.len() {
        let b = unsafe { *data.get_unchecked(*pos) };
        if b >= b'0' && b <= b'9' {
            val = val * 10 + (b - b'0') as u32;
            *pos += 1;
        } else {
            break;
        }
    }
    val
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

    // 1. Mmap graph.txt
    let graph_path = format!("{}/graph.txt", data_dir);
    let graph_file = match File::open(&graph_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let meta = match graph_file.metadata() {
        Ok(m) => m,
        Err(_) => return,
    };
    let g_len = meta.len() as usize;
    if g_len == 0 {
        return;
    }

    let g_ptr = unsafe {
        mmap(std::ptr::null_mut(), g_len, PROT_READ, MAP_SHARED, graph_file.as_raw_fd(), 0)
    };
    if g_ptr == usize::MAX as *mut u8 {
        return;
    }
    let g_data = unsafe { std::slice::from_raw_parts(g_ptr, g_len) };

    let mut g_pos = 0;
    let n = parse_u32(g_data, &mut g_pos) as usize;
    while g_pos < g_len && g_data[g_pos] == b' ' { g_pos += 1; }
    let m = parse_u32(g_data, &mut g_pos) as usize;
    while g_pos < g_len && g_data[g_pos] != b'\n' { g_pos += 1; }
    if g_pos < g_len { g_pos += 1; }
    let edges_start = g_pos;

    // Pass 1: count forward degrees in parallel
    let num_load_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .min(12);

    let mut edge_chunks = Vec::with_capacity(num_load_threads);
    {
        let total_edge_bytes = g_len - edges_start;
        let mut cur = edges_start;
        for t in 0..num_load_threads {
            let target = edges_start + ((total_edge_bytes * (t + 1)) / num_load_threads);
            let mut end = target.min(g_len);
            while end < g_len && g_data[end] != b'\n' {
                end += 1;
            }
            if end < g_len {
                end += 1;
            }
            edge_chunks.push((cur, end));
            cur = end;
        }
    }

    let deg_results: Vec<Vec<u32>> = std::thread::scope(|s| {
        let handles: Vec<_> = edge_chunks.iter().map(|&(start, end)| {
            s.spawn(move || {
                let mut local_f_deg = vec![0u32; n];
                let mut pos = start;
                while pos < end {
                    let u = parse_u32(g_data, &mut pos) as usize;
                    while pos < end && unsafe { *g_data.get_unchecked(pos) } != b'\n' {
                        pos += 1;
                    }
                    if pos < end {
                        pos += 1;
                    }

                    if u < n {
                        unsafe { *local_f_deg.get_unchecked_mut(u) += 1; }
                    }
                }
                local_f_deg
            })
        }).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut f_head = vec![0u32; n + 1];
    let mut thread_f_write: Vec<Vec<u32>> = (0..num_load_threads).map(|_| vec![0u32; n]).collect();

    let mut cur_f = 0u32;
    for i in 0..n {
        f_head[i] = cur_f;
        let mut f_sum = cur_f;
        for t in 0..num_load_threads {
            thread_f_write[t][i] = f_sum;
            f_sum += deg_results[t][i];
        }
        cur_f = f_sum;
    }
    f_head[n] = cur_f;
    drop(deg_results);

    // Packed edges: lower 32 bits = dest node, upper 32 bits = weight
    let mut f_edges = vec![0u64; m];
    let f_edges_addr = f_edges.as_mut_ptr() as usize;

    // Pass 2: populate forward CSR in parallel
    std::thread::scope(|s| {
        for (t, &(start, end)) in edge_chunks.iter().enumerate() {
            let mut f_write = thread_f_write[t].clone();

            s.spawn(move || {
                let f_raw = f_edges_addr as *mut u64;
                let mut pos = start;
                while pos < end {
                    let u = parse_u32(g_data, &mut pos) as usize;
                    if pos < end { pos += 1; }
                    let v = parse_u32(g_data, &mut pos) as usize;
                    if pos < end { pos += 1; }
                    let w = parse_u32(g_data, &mut pos) as u64;
                    if pos < end && g_data[pos] == b'\r' { pos += 1; }
                    if pos < end && g_data[pos] == b'\n' { pos += 1; }

                    let packed_f = (v as u64) | (w << 32);

                    if u < n {
                        let slot = unsafe { *f_write.get_unchecked(u) } as usize;
                        unsafe {
                            *f_write.get_unchecked_mut(u) += 1;
                            *f_raw.add(slot) = packed_f;
                        }
                    }
                }
            });
        }
    });
    drop(thread_f_write);
    unsafe { munmap(g_ptr, g_len); }

    // 2. Mmap queries.txt
    let q_path = format!("{}/queries.txt", data_dir);
    let q_file = match File::open(&q_path) {
        Ok(f) => f,
        Err(_) => return,
    };
    let q_meta = match q_file.metadata() {
        Ok(m) => m,
        Err(_) => return,
    };
    let q_len = q_meta.len() as usize;
    if q_len == 0 {
        return;
    }

    let q_ptr = unsafe {
        mmap(std::ptr::null_mut(), q_len, PROT_READ, MAP_SHARED, q_file.as_raw_fd(), 0)
    };
    if q_ptr == usize::MAX as *mut u8 {
        return;
    }
    let q_data = unsafe { std::slice::from_raw_parts(q_ptr, q_len) };

    let mut queries: Vec<(u32, u32)> = Vec::with_capacity(10_000);
    let mut q_pos = 0;
    while q_pos < q_len {
        if q_data[q_pos] == b'\r' || q_data[q_pos] == b'\n' {
            q_pos += 1;
            continue;
        }
        let s = parse_u32(q_data, &mut q_pos);
        if q_pos < q_len && q_data[q_pos] == b' ' { q_pos += 1; }
        let t = parse_u32(q_data, &mut q_pos);
        while q_pos < q_len && q_data[q_pos] != b'\n' { q_pos += 1; }
        if q_pos < q_len { q_pos += 1; }
        queries.push((s, t));
    }
    unsafe { munmap(q_ptr, q_len); }

    let num_queries = queries.len();
    let num_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(8)
        .min(12);

    let mut results: Vec<i64> = vec![-1; num_queries];
    let query_cursor = std::sync::atomic::AtomicUsize::new(0);
    let results_ptr = results.as_mut_ptr() as usize;

    std::thread::scope(|s| {
        let f_head_ref = &f_head;
        let f_edges_ref = &f_edges;
        let query_cursor_ref = &query_cursor;
        let queries_ref = &queries;

        for _ in 0..num_threads {
            s.spawn(move || {
                let mut q = FastHeap::new(4096);
                let mut dist: Vec<u64> = vec![u64::MAX; n];
                let mut visited: Vec<u32> = Vec::with_capacity(4096);
                let res_raw = results_ptr as *mut i64;

                const BATCH_SIZE: usize = 16;
                loop {
                    let start_idx = query_cursor_ref.fetch_add(BATCH_SIZE, std::sync::atomic::Ordering::Relaxed);
                    if start_idx >= num_queries {
                        break;
                    }
                    let end_idx = (start_idx + BATCH_SIZE).min(num_queries);

                    for q_idx in start_idx..end_idx {
                        let (src, dst) = unsafe { *queries_ref.get_unchecked(q_idx) };
                        if src == dst {
                            unsafe { *res_raw.add(q_idx) = 0; }
                            continue;
                        }
                        let s_idx = src as usize;
                        let t_idx = dst as usize;
                        if s_idx >= n || t_idx >= n {
                            unsafe { *res_raw.add(q_idx) = -1; }
                            continue;
                        }

                        // Reset visited nodes from previous query
                        for &node in &visited {
                            unsafe { *dist.get_unchecked_mut(node as usize) = u64::MAX; }
                        }
                        visited.clear();
                        q.clear();

                        unsafe { *dist.get_unchecked_mut(s_idx) = 0; }
                        visited.push(src);
                        q.push(0, src);

                        let mut found = -1i64;
                        while let Some((curr_cost, curr_node)) = q.pop() {
                            if curr_cost > unsafe { *dist.get_unchecked(curr_node as usize) } {
                                continue;
                            }
                            if curr_node == dst {
                                found = curr_cost as i64;
                                break;
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

                                let old_dist = unsafe { *dist.get_unchecked(v_idx) };
                                if new_dist < old_dist {
                                    if old_dist == u64::MAX {
                                        visited.push(v);
                                    }
                                    unsafe { *dist.get_unchecked_mut(v_idx) = new_dist; }
                                    q.push(new_dist, v);
                                }
                            }
                        }
                        unsafe { *res_raw.add(q_idx) = found; }
                    }
                }
            });
        }
    });

    // 3. Write output
    fs::create_dir_all(&out_dir).unwrap();
    let out_file_path = format!("{}/distances.txt", out_dir);
    let mut out_file = File::create(&out_file_path).unwrap();

    let mut out_buf = Vec::with_capacity(32 * 1024);
    for d in results {
        if d == -1 {
            out_buf.extend_from_slice(b"-1\n");
        } else {
            let mut n = d as u64;
            if n == 0 {
                out_buf.push(b'0');
            } else {
                let mut digits = [0u8; 20];
                let mut dig_i = 0;
                while n > 0 {
                    digits[dig_i] = b'0' + (n % 10) as u8;
                    n /= 10;
                    dig_i += 1;
                }
                while dig_i > 0 {
                    dig_i -= 1;
                    out_buf.push(digits[dig_i]);
                }
            }
            out_buf.push(b'\n');
        }
        if out_buf.len() > 24 * 1024 {
            out_file.write_all(&out_buf).unwrap();
            out_buf.clear();
        }
    }
    if !out_buf.is_empty() {
        out_file.write_all(&out_buf).unwrap();
    }
}
