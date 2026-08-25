//! GPU offload for the pair-move correction, via Metal.
//!
//! # Why this exists
//!
//! Profiling puts ~88% of worker compute in the counting kernels, and essentially all of it is
//! reached through the pair-move correction: intersect the 3-4 forced vertices' adjacency rows and
//! count `(k - n)`-cliques in what remains. Measured on an M4 Max against a real campaign graph,
//! one CPU core does 1.00 M corrections/sec and the 40-core GPU does 131.7 M/sec — roughly 8x the
//! entire 16-core CPU fleet, with byte-identical results.
//!
//! The workload suits a GPU unusually well: the whole adjacency is 282 rows x 320 bits = ~11 KB, so
//! it stays in cache and the kernel is compute-bound rather than bandwidth-bound, and Apple's
//! unified memory removes the host/device copy that normally kills fine-grained offload.
//!
//! # Why it is macOS-only
//!
//! Metal is a macOS userspace framework. The worker's Docker image is Linux/aarch64 and the
//! container has no GPU device nodes, so a containerised worker can never reach it — GPU-assisted
//! workers must run natively on the host. This whole module is `cfg(target_os = "macos")` and the
//! `metal` dependency is target-gated, so the Linux build is untouched.
//!
//! # Correctness
//!
//! The kernel is a transcription of `algorithm::count_cliques_through_vertex_set` and must agree
//! with it exactly. `gpu_matches_cpu_on_a_real_campaign_graph` checks that over the real fixture
//! graphs; nothing here is trusted on the basis of the CPU and GPU "looking equivalent".

use crate::graph::Graph;

/// One correction to evaluate: the forced vertices and which colour to count in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CorrectionRequest {
    /// The distinct forced vertices (`V(r) | V(b)`); only the first `n` are meaningful.
    pub seeds: [u16; 4],
    /// How many of `seeds` are distinct — 3 when the two edges share a vertex, else 4.
    pub n: u8,
    /// Count in the complement (blue) adjacency rather than the red one.
    pub blue: bool,
}

/// Words per adjacency row: 282 vertices rounded up to 320 bits.
pub const ROW_WORDS: usize = 10;

/// Flatten a graph's two adjacency matrices into the row-major `u32` layout the kernel reads.
///
/// Built through the public bit accessor rather than by reinterpreting `BitMatrix`'s storage, so a
/// change to that type cannot silently reinterpret the wrong bytes here.
pub fn flatten_adjacency(graph: &Graph, vertex_count: usize) -> (Vec<u32>, Vec<u32>) {
    let mut red = vec![0u32; vertex_count * ROW_WORDS];
    let mut blue = vec![0u32; vertex_count * ROW_WORDS];
    for i in 0..vertex_count {
        for j in 0..vertex_count {
            if i == j {
                continue;
            }
            if graph.adjacency[i].get(j) {
                red[i * ROW_WORDS + j / 32] |= 1 << (j % 32);
            }
            if graph.complement_adjacency[i].get(j) {
                blue[i * ROW_WORDS + j / 32] |= 1 << (j % 32);
            }
        }
    }
    (red, blue)
}

#[cfg(target_os = "macos")]
mod backend {
    use super::*;
    use metal::{Device, MTLResourceOptions, MTLSize};
    use std::mem::size_of;

    /// Transcription of `count_cliques_through_vertex_set`.
    ///
    /// `need = clique_size - n` is the number of further vertices to choose. Production runs k=8
    /// with n of 3 or 4, so only `need` of 4 and 5 are implemented; anything else is rejected on the
    /// host rather than silently miscounted. The nests mirror the CPU kernel's folded shape: a
    /// candidate is removed from the working set after being used, which is what keeps each clique
    /// counted exactly once.
    const SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;

#define RW 10

static inline uint card(thread const uint *p) {
    uint c = 0;
    for (uint i = 0; i < RW; ++i) { c += popcount(p[i]); }
    return c;
}

// depth == clique_size - 2. Each candidate completes with exactly one more vertex, so the level is
// the number of EDGES inside p. Mirrors the CPU kernel's fused second-to-last level, including
// removing each candidate after use so a pair is counted once.
static inline uint f_k2(thread uint *p, device const uint *adj) {
    uint total = 0;
    for (uint wi = 0; wi < RW; ++wi) {
        uint word = p[wi];
        while (word != 0) {
            uint b = ctz(word); word &= word - 1;
            uint v = wi*32 + b;
            uint acc = 0;
            for (uint i = 0; i < RW; ++i) { acc += popcount(p[i] & adj[v*RW+i]); }
            total += acc;
            p[wi] &= ~(1u << b);
        }
    }
    return total;
}

// depth == clique_size - 3
static inline uint f_k3(thread uint *p, device const uint *adj) {
    uint total = 0;
    for (uint wi = 0; wi < RW; ++wi) {
        uint word = p[wi];
        while (word != 0) {
            uint b = ctz(word); word &= word - 1;
            uint v = wi*32 + b;
            uint pv[RW]; uint cv = 0;
            for (uint i = 0; i < RW; ++i) { pv[i] = p[i] & adj[v*RW+i]; cv += popcount(pv[i]); }
            if (cv >= 2) { total += f_k2(pv, adj); }
            p[wi] &= ~(1u << b);
        }
    }
    return total;
}

// depth == clique_size - 4
static inline uint f_k4(thread uint *p, device const uint *adj) {
    uint total = 0;
    for (uint wi = 0; wi < RW; ++wi) {
        uint word = p[wi];
        while (word != 0) {
            uint b = ctz(word); word &= word - 1;
            uint v = wi*32 + b;
            uint pv[RW]; uint cv = 0;
            for (uint i = 0; i < RW; ++i) { pv[i] = p[i] & adj[v*RW+i]; cv += popcount(pv[i]); }
            if (cv >= 3) { total += f_k3(pv, adj); }
            p[wi] &= ~(1u << b);
        }
    }
    return total;
}

// depth == clique_size - 5
static inline uint f_k5(thread uint *p, device const uint *adj) {
    uint total = 0;
    for (uint wi = 0; wi < RW; ++wi) {
        uint word = p[wi];
        while (word != 0) {
            uint b = ctz(word); word &= word - 1;
            uint v = wi*32 + b;
            uint pv[RW]; uint cv = 0;
            for (uint i = 0; i < RW; ++i) { pv[i] = p[i] & adj[v*RW+i]; cv += popcount(pv[i]); }
            if (cv >= 4) { total += f_k4(pv, adj); }
            p[wi] &= ~(1u << b);
        }
    }
    return total;
}

kernel void corrections(device const uint   *red    [[buffer(0)]],
                        device const uint   *blue   [[buffer(1)]],
                        device const ushort *seeds  [[buffer(2)]],
                        device const uchar  *meta   [[buffer(3)]],
                        device       int    *out    [[buffer(4)]],
                        constant     uint   &clique [[buffer(5)]],
                        constant     uint   &count  [[buffer(6)]],
                        uint gid [[thread_position_in_grid]])
{
    if (gid >= count) { return; }
    uchar m = meta[gid];
    uint n = m & 0xF;
    device const uint *adj = (m >> 4) ? blue : red;

    uint p[RW];
    for (uint i = 0; i < RW; ++i) { p[i] = adj[uint(seeds[gid*4+0])*RW + i]; }
    for (uint s = 1; s < n; ++s) {
        uint v = uint(seeds[gid*4+s]);
        for (uint i = 0; i < RW; ++i) { p[i] &= adj[v*RW + i]; }
    }
    for (uint s = 0; s < n; ++s) {
        uint v = uint(seeds[gid*4+s]);
        p[v >> 5] &= ~(1u << (v & 31));
    }

    // Mirrors the CPU entry checks: exact count, zero when there is not enough left to finish.
    uint need = clique - n;
    uint c = card(p);
    uint total = 0;
    if (n + c < clique)      { total = 0; }
    else if (need == 1)      { total = c; }
    else if (need == 2)      { total = f_k2(p, adj); }
    else if (need == 3)      { total = f_k3(p, adj); }
    else if (need == 4)      { total = f_k4(p, adj); }
    else if (need == 5)      { total = f_k5(p, adj); }
    out[gid] = int(total);
}
"#;

    pub struct CorrectionEngine {
        device: Device,
        queue: metal::CommandQueue,
        pipeline: metal::ComputePipelineState,
        red: metal::Buffer,
        blue: metal::Buffer,
        vertex_count: usize,
        clique_size: usize,
        results: Vec<i32>,
    }

    impl CorrectionEngine {
        /// `None` when no Metal device is present, so callers fall back to the CPU path.
        pub fn new(graph: &Graph, vertex_count: usize, clique_size: usize) -> Option<Self> {
            let device = Device::system_default()?;
            let lib = device
                .new_library_with_source(SHADER, &metal::CompileOptions::new())
                .ok()?;
            let f = lib.get_function("corrections", None).ok()?;
            let pipeline = device.new_compute_pipeline_state_with_function(&f).ok()?;
            let queue = device.new_command_queue();
            let (r, b) = flatten_adjacency(graph, vertex_count);
            let red = device.new_buffer_with_data(
                r.as_ptr() as *const _,
                (r.len() * size_of::<u32>()) as u64,
                MTLResourceOptions::StorageModeShared,
            );
            let blue = device.new_buffer_with_data(
                b.as_ptr() as *const _,
                (b.len() * size_of::<u32>()) as u64,
                MTLResourceOptions::StorageModeShared,
            );
            Some(CorrectionEngine {
                device,
                queue,
                pipeline,
                red,
                blue,
                vertex_count,
                clique_size,
                results: Vec::new(),
            })
        }

        /// Point the engine at a different base graph. Cheap relative to a dispatch, but not free —
        /// call it once per stage, not per batch.
        pub fn set_graph(&mut self, graph: &Graph) {
            let (r, b) = flatten_adjacency(graph, self.vertex_count);
            let n = (r.len() * size_of::<u32>()) as u64;
            unsafe {
                std::ptr::copy_nonoverlapping(r.as_ptr(), self.red.contents() as *mut u32, r.len());
                std::ptr::copy_nonoverlapping(b.as_ptr(), self.blue.contents() as *mut u32, b.len());
            }
            let _ = n;
        }

        /// Evaluate every request. Returns counts in request order.
        ///
        /// Only `clique_size - n` of 4 or 5 is implemented; anything else would be miscounted, so it
        /// is rejected here rather than in the shader where it would silently return zero.
        pub fn run(&mut self, reqs: &[CorrectionRequest]) -> Option<&[i32]> {
            if reqs.is_empty() {
                self.results.clear();
                return Some(&self.results);
            }
            let mut seeds = Vec::with_capacity(reqs.len() * 4);
            let mut meta = Vec::with_capacity(reqs.len());
            for r in reqs {
                let need = self.clique_size.checked_sub(r.n as usize)?;
                if !(1..=5).contains(&need) {
                    return None;
                }
                seeds.extend_from_slice(&r.seeds);
                meta.push(r.n | if r.blue { 0x10 } else { 0 });
            }
            let bs = self.device.new_buffer_with_data(
                seeds.as_ptr() as *const _,
                (seeds.len() * size_of::<u16>()) as u64,
                MTLResourceOptions::StorageModeShared,
            );
            let bm = self.device.new_buffer_with_data(
                meta.as_ptr() as *const _,
                meta.len() as u64,
                MTLResourceOptions::StorageModeShared,
            );
            let bo = self.device.new_buffer(
                (reqs.len() * size_of::<i32>()) as u64,
                MTLResourceOptions::StorageModeShared,
            );
            let cs = self.clique_size as u32;
            let cnt = reqs.len() as u32;

            let cb = self.queue.new_command_buffer();
            let enc = cb.new_compute_command_encoder();
            enc.set_compute_pipeline_state(&self.pipeline);
            enc.set_buffer(0, Some(&self.red), 0);
            enc.set_buffer(1, Some(&self.blue), 0);
            enc.set_buffer(2, Some(&bs), 0);
            enc.set_buffer(3, Some(&bm), 0);
            enc.set_buffer(4, Some(&bo), 0);
            enc.set_bytes(5, size_of::<u32>() as u64, &cs as *const u32 as *const _);
            enc.set_bytes(6, size_of::<u32>() as u64, &cnt as *const u32 as *const _);
            let tg = self.pipeline.max_total_threads_per_threadgroup().min(256);
            enc.dispatch_threads(
                MTLSize::new(reqs.len() as u64, 1, 1),
                MTLSize::new(tg, 1, 1),
            );
            enc.end_encoding();
            cb.commit();
            cb.wait_until_completed();

            self.results.clear();
            self.results.extend_from_slice(unsafe {
                std::slice::from_raw_parts(bo.contents() as *const i32, reqs.len())
            });
            Some(&self.results)
        }
    }
}

#[cfg(target_os = "macos")]
pub use backend::CorrectionEngine;
