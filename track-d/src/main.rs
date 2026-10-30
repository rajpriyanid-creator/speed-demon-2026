use std::fs::{self, File};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::thread;

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

#[derive(Copy, Clone)]
struct State {
    cost: u64,
    node: u32,
}

struct MinHeap {
    data: Vec<State>,
}

impl MinHeap {
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
    fn push(&mut self, s: State) {
        let mut i = self.data.len();
        self.data.push(s);
        while i > 0 {
            let p = (i - 1) >> 1;
            if unsafe { self.data.get_unchecked(i).cost < self.data.get_unchecked(p).cost } {
                self.data.swap(i, p);
                i = p;
            } else {
                break;
            }
        }
    }

    #[inline(always)]
    fn pop(&mut self) -> Option<State> {
        if self.data.is_empty() {
            return None;
        }
        let res = self.data[0];
        let last = self.data.pop().unwrap();
        if !self.data.is_empty() {
            self.data[0] = last;
            let mut i = 0;
            let len = self.data.len();
            loop {
                let mut smallest = i;
                let left = 2 * i + 1;
                let right = 2 * i + 2;
                if left < len && unsafe { self.data.get_unchecked(left).cost < self.data.get_unchecked(smallest).cost } {
                    smallest = left;
                }
                if right < len && unsafe { self.data.get_unchecked(right).cost < self.data.get_unchecked(smallest).cost } {
                    smallest = right;
                }
                if smallest != i {
                    self.data.swap(i, smallest);
                    i = smallest;
                } else {
                    break;
                }
            }
        }
        Some(res)
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
    let data_dir = "/data";
    let out_dir = "/out";

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

    // Pass 1: count out-degrees
    let mut out_degree = vec![0u32; n];
    {
        let mut pos = edges_start;
        for _ in 0..m {
            let u = parse_u32(g_data, &mut pos);
            while pos < g_len && g_data[pos] != b' ' { pos += 1; }
            if pos < g_len { pos += 1; }
            while pos < g_len && g_data[pos] != b' ' { pos += 1; }
            if pos < g_len { pos += 1; }
            while pos < g_len && g_data[pos] != b'\n' { pos += 1; }
            if pos < g_len { pos += 1; }
            if (u as usize) < n {
                out_degree[u as usize] += 1;
            }
        }
    }

    // CSR head array
    let mut head = vec![0u32; n + 1];
    let mut cur = 0u32;
    for i in 0..n {
        head[i] = cur;
        cur += out_degree[i];
    }
    head[n] = cur;
    drop(out_degree);

    let mut write_pos = head.clone();
    let mut dest = vec![0u32; m];
    let mut weight = vec![0u32; m];

    // Pass 2: populate CSR
    {
        let mut pos = edges_start;
        for _ in 0..m {
            let u = parse_u32(g_data, &mut pos);
            if pos < g_len { pos += 1; }
            let v = parse_u32(g_data, &mut pos);
            if pos < g_len { pos += 1; }
            let w = parse_u32(g_data, &mut pos);
            if pos < g_len && g_data[pos] == b'\r' { pos += 1; }
            if pos < g_len && g_data[pos] == b'\n' { pos += 1; }

            let u_idx = u as usize;
            if u_idx < n {
                let slot = write_pos[u_idx] as usize;
                write_pos[u_idx] += 1;
                dest[slot] = v;
                weight[slot] = w;
            }
        }
    }
    drop(write_pos);
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
    let q_ptr = unsafe {
        mmap(std::ptr::null_mut(), q_len, PROT_READ, MAP_SHARED, q_file.as_raw_fd(), 0)
    };
    if q_ptr == usize::MAX as *mut u8 {
        return;
    }
    let q_data = unsafe { std::slice::from_raw_parts(q_ptr, q_len) };

    let mut queries = Vec::with_capacity(2048);
    {
        let mut pos = 0;
        while pos < q_len {
            if q_data[pos] == b'\r' || q_data[pos] == b'\n' {
                pos += 1;
                continue;
            }
            let s = parse_u32(q_data, &mut pos);
            while pos < q_len && q_data[pos] == b' ' { pos += 1; }
            let t = parse_u32(q_data, &mut pos);
            while pos < q_len && q_data[pos] != b'\n' { pos += 1; }
            if pos < q_len { pos += 1; }
            queries.push((s, t));
        }
    }
    unsafe { munmap(q_ptr, q_len); }

    let num_queries = queries.len();
    let mut answers = vec![-1i64; num_queries];

    let num_threads = thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(6);
    let chunk_size = (num_queries + num_threads - 1) / num_threads;

    let head_ref = &head;
    let dest_ref = &dest;
    let weight_ref = &weight;
    let queries_ref = &queries;

    thread::scope(|s| {
        let thread_chunks: Vec<&mut [i64]> = answers.chunks_mut(chunk_size).collect();
        for (th_idx, out_chunk) in thread_chunks.into_iter().enumerate() {
            let start_q = th_idx * chunk_size;
            s.spawn(move || {
                let mut dist = vec![u64::MAX; n];
                let mut visited_gen = vec![0u16; n];
                let mut cur_gen = 0u16;
                let mut heap = MinHeap::new(4096);

                for (offset, ans_slot) in out_chunk.iter_mut().enumerate() {
                    let q_idx = start_q + offset;
                    let (src, tgt) = queries_ref[q_idx];

                    if src == tgt {
                        *ans_slot = 0;
                        continue;
                    }

                    cur_gen += 1;
                    heap.clear();

                    let s_idx = src as usize;
                    visited_gen[s_idx] = cur_gen;
                    dist[s_idx] = 0;
                    heap.push(State { cost: 0, node: src });

                    let mut found = -1i64;
                    while let Some(State { cost, node }) = heap.pop() {
                        let u = node as usize;
                        if cost > dist[u] {
                            continue;
                        }
                        if node == tgt {
                            found = cost as i64;
                            break;
                        }

                        let start_edge = head_ref[u] as usize;
                        let end_edge = head_ref[u + 1] as usize;
                        for e in start_edge..end_edge {
                            let v = dest_ref[e] as usize;
                            let w = weight_ref[e] as u64;
                            let next_d = cost + w;

                            if visited_gen[v] != cur_gen {
                                visited_gen[v] = cur_gen;
                                dist[v] = next_d;
                                heap.push(State { cost: next_d, node: v as u32 });
                            } else if next_d < dist[v] {
                                dist[v] = next_d;
                                heap.push(State { cost: next_d, node: v as u32 });
                            }
                        }
                    }

                    *ans_slot = found;
                }
            });
        }
    });

    // Write output
    let _ = fs::create_dir_all(out_dir);
    let out_file = match File::create(format!("{}/distances.txt", out_dir)) {
        Ok(f) => f,
        Err(_) => return,
    };
    let mut writer = std::io::BufWriter::with_capacity(64 * 1024, out_file);
    for ans in answers {
        let _ = writeln!(writer, "{}", ans);
    }
    let _ = writer.flush();
}
