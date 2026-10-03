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

/// A possibly reordered and padded GPU correction batch.
///
/// The worker finalizes results in original enumeration order because that is where its local
/// top-N threshold tightens. The GPU does not need that ordering, so a dispatch can group the two
/// correction shapes (three or four forced vertices) and later scatter answers back exactly.
#[derive(Clone, Debug)]
pub struct CorrectionDispatchPlan {
    requests: Vec<CorrectionRequest>,
    dispatch_to_original: Vec<Option<usize>>,
    original_len: usize,
}

impl CorrectionDispatchPlan {
    /// Preserve the current one-request-in, one-result-out dispatch behavior.
    pub fn identity(requests: &[CorrectionRequest]) -> Self {
        CorrectionDispatchPlan {
            requests: requests.to_vec(),
            dispatch_to_original: (0..requests.len()).map(Some).collect(),
            original_len: requests.len(),
        }
    }

    /// Group only by correction seed shape, padding each bucket to the Metal SIMD width.
    ///
    /// This is intentionally cheaper than candidate-size bucketing: it avoids a CPU
    /// common-neighborhood prepass, preserving the overlap window that makes the hybrid path pay.
    pub fn by_shape(requests: &[CorrectionRequest], thread_execution_width: usize) -> Self {
        let width = thread_execution_width.max(1);
        if requests.is_empty() || width == 1 {
            return Self::identity(requests);
        }

        let mut shapes: Vec<u8> = Vec::new();
        let mut buckets: Vec<Vec<usize>> = Vec::new();
        for (original, request) in requests.iter().enumerate() {
            let bucket = match shapes.iter().position(|known| *known == request.n) {
                Some(index) => index,
                None => {
                    shapes.push(request.n);
                    buckets.push(Vec::new());
                    buckets.len() - 1
                }
            };
            buckets[bucket].push(original);
        }
        Self::from_buckets(requests, buckets, width)
    }

    fn from_buckets(
        requests: &[CorrectionRequest],
        buckets: Vec<Vec<usize>>,
        thread_execution_width: usize,
    ) -> Self {
        let width = thread_execution_width.max(1);
        let mut dispatch_requests = Vec::with_capacity(requests.len());
        let mut dispatch_to_original = Vec::with_capacity(requests.len());
        for bucket in buckets {
            for &original in &bucket {
                dispatch_requests.push(requests[original]);
                dispatch_to_original.push(Some(original));
            }
            let padding = (width - (bucket.len() % width)) % width;
            for offset in 0..padding {
                let duplicate = bucket[offset % bucket.len()];
                dispatch_requests.push(requests[duplicate]);
                dispatch_to_original.push(None);
            }
        }

        CorrectionDispatchPlan {
            requests: dispatch_requests,
            dispatch_to_original,
            original_len: requests.len(),
        }
    }

    pub fn requests(&self) -> &[CorrectionRequest] {
        &self.requests
    }

    /// Restore GPU answers to the original request order and discard padded duplicates.
    pub fn scatter(&self, dispatch_results: &[i32]) -> Vec<i32> {
        assert_eq!(
            dispatch_results.len(),
            self.dispatch_to_original.len(),
            "GPU result count must match the dispatch plan"
        );
        let mut original_results = vec![0; self.original_len];
        let mut filled = vec![false; self.original_len];
        for (&result, original) in dispatch_results.iter().zip(self.dispatch_to_original.iter()) {
            if let Some(original) = original {
                assert!(
                    !filled[*original],
                    "a real correction may have only one dispatch result"
                );
                original_results[*original] = result;
                filled[*original] = true;
            }
        }
        assert!(
            filled.iter().all(|was_filled| *was_filled),
            "every real correction must receive a dispatch result"
        );
        original_results
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request(n: u8, seed: u16) -> CorrectionRequest {
        CorrectionRequest {
            seeds: [seed, seed + 1, seed + 2, seed + 3],
            n,
            blue: false,
        }
    }

    #[test]
    fn shape_dispatch_groups_seed_shapes_and_restores_original_order() {
        let requests = [
            request(4, 0),
            request(3, 10),
            request(4, 20),
            request(3, 30),
            request(3, 40),
        ];
        let plan = CorrectionDispatchPlan::by_shape(&requests, 4);

        // n=4 contributes two real requests plus two pads; n=3 contributes three plus one pad.
        assert_eq!(plan.requests().len(), 8);
        let dispatch_results: Vec<i32> = (0..plan.requests().len())
            .map(|slot| 2000 + slot as i32)
            .collect();
        let restored = plan.scatter(&dispatch_results);
        assert_eq!(restored.len(), requests.len());
        assert!(restored.iter().all(|result| *result >= 2000));
        assert_ne!(restored, dispatch_results[..requests.len()]);
    }
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

// ---- compressed path -------------------------------------------------------------------------
// |P| never exceeds 32 for disjoint edge pairs on real campaign graphs (measured: max 32 for n=4,
// 49 for n=3), so the induced subgraph on P fits in 32 x uint = 128 bytes per thread. That is the
// difference between spilling and not: the full-width recursion carries a 40-byte candidate set per
// level, which is what wrecked occupancy in the first version of this kernel.

// Metal has no recursion, so the levels are an explicit chain: each calls only the level below.
// need = clique_size - n, which is 4 when the flipped edges are disjoint and 5 when they share a
// vertex, so the chain bottoms out at cd2 (the number of edges inside the candidate set).

static inline uint cd2(uint p, thread const uint *loc) {
    uint c = 0;
    while (p != 0) { uint v = ctz(p); p &= p - 1; c += popcount(p & loc[v]); }
    return c;
}

static inline uint cd3(uint p, thread const uint *loc) {
    uint c = 0; uint rem = popcount(p);
    while (p != 0) {
        if (rem < 3) { break; }
        rem -= 1;
        uint v = ctz(p); p &= p - 1;
        uint pv = p & loc[v];
        if (popcount(pv) >= 2) { c += cd2(pv, loc); }
    }
    return c;
}

static inline uint cd4(uint p, thread const uint *loc) {
    uint c = 0; uint rem = popcount(p);
    while (p != 0) {
        if (rem < 4) { break; }
        rem -= 1;
        uint v = ctz(p); p &= p - 1;
        uint pv = p & loc[v];
        if (popcount(pv) >= 3) { c += cd3(pv, loc); }
    }
    return c;
}

static inline uint cd5(uint p, thread const uint *loc) {
    uint c = 0; uint rem = popcount(p);
    while (p != 0) {
        if (rem < 5) { break; }
        rem -= 1;
        uint v = ctz(p); p &= p - 1;
        uint pv = p & loc[v];
        if (popcount(pv) >= 4) { c += cd4(pv, loc); }
    }
    return c;
}

static inline uint cdense(uint p, uint need, thread const uint *loc) {
    if (need == 0) { return 1; }
    if (need == 1) { return popcount(p); }
    if (need == 2) { return cd2(p, loc); }
    if (need == 3) { return cd3(p, loc); }
    if (need == 4) { return cd4(p, loc); }
    if (need == 5) { return cd5(p, loc); }
    return 0;   // unreachable: the host rejects need outside 1..=5
}

// n=3 candidates occasionally exceed the 32-bit compressed path, but none of the sampled
// production candidates exceeded 64 vertices. Keep this deliberately separate from the 32-bit
// path: it is opt-in at dispatch time so the benchmark can compare it with the established
// full-width fallback on identical requests.
static inline uint cd2_64(ulong p, thread const ulong *loc) {
    uint c = 0;
    while (p != 0) {
        uint v = uint(ctz(p));
        p &= p - 1;
        c += uint(popcount(p & loc[v]));
    }
    return c;
}

static inline uint cd3_64(ulong p, thread const ulong *loc) {
    uint c = 0;
    uint rem = uint(popcount(p));
    while (p != 0) {
        if (rem < 3) { break; }
        rem -= 1;
        uint v = uint(ctz(p));
        p &= p - 1;
        ulong pv = p & loc[v];
        if (popcount(pv) >= 2) { c += cd2_64(pv, loc); }
    }
    return c;
}

static inline uint cd4_64(ulong p, thread const ulong *loc) {
    uint c = 0;
    uint rem = uint(popcount(p));
    while (p != 0) {
        if (rem < 4) { break; }
        rem -= 1;
        uint v = uint(ctz(p));
        p &= p - 1;
        ulong pv = p & loc[v];
        if (popcount(pv) >= 3) { c += cd3_64(pv, loc); }
    }
    return c;
}

static inline uint cd5_64(ulong p, thread const ulong *loc) {
    uint c = 0;
    uint rem = uint(popcount(p));
    while (p != 0) {
        if (rem < 5) { break; }
        rem -= 1;
        uint v = uint(ctz(p));
        p &= p - 1;
        ulong pv = p & loc[v];
        if (popcount(pv) >= 4) { c += cd4_64(pv, loc); }
    }
    return c;
}

static inline uint cdense64(ulong p, uint need, thread const ulong *loc) {
    if (need == 0) { return 1; }
    if (need == 1) { return uint(popcount(p)); }
    if (need == 2) { return cd2_64(p, loc); }
    if (need == 3) { return cd3_64(p, loc); }
    if (need == 4) { return cd4_64(p, loc); }
    if (need == 5) { return cd5_64(p, loc); }
    return 0;
}

kernel void corrections(device const uint   *red    [[buffer(0)]],
                        device const uint   *blue   [[buffer(1)]],
                        device const ushort *seeds  [[buffer(2)]],
                        device const uchar  *meta   [[buffer(3)]],
                        device       int    *out    [[buffer(4)]],
                        constant     uint   &clique [[buffer(5)]],
                        constant     uint   &count  [[buffer(6)]],
                        constant     uint   &dense64_enabled [[buffer(7)]],
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
    else if (c <= 32) {
        // Relabel P into a dense index space and finish on 32-bit masks.
        uint verts[32]; uint m = 0;
        for (uint wi = 0; wi < RW && m < c; ++wi) {
            uint word = p[wi];
            while (word != 0) { uint b = ctz(word); word &= word - 1; verts[m++] = wi*32 + b; }
        }
        uint loc[32];
        for (uint a = 0; a < m; ++a) {
            device const uint *row = adj + verts[a]*RW;
            uint mask = 0;
            for (uint b = 0; b < m; ++b) {
                uint vb = verts[b];
                if ((row[vb >> 5] >> (vb & 31)) & 1u) { mask |= (1u << b); }
            }
            loc[a] = mask;
        }
        uint full = (m == 32) ? 0xFFFFFFFFu : ((1u << m) - 1u);
        total = cdense(full, need, loc);
    }
    else if (n == 3 && c <= 64 && dense64_enabled != 0) {
        // n=3 only: preserve the dense32 branch above, and only replace the proven generic
        // fallback when P has 33--64 vertices.
        ushort verts[64]; uint m = 0;
        for (uint wi = 0; wi < RW && m < c; ++wi) {
            uint word = p[wi];
            while (word != 0) {
                uint b = ctz(word);
                word &= word - 1;
                verts[m++] = ushort(wi*32 + b);
            }
        }
        ulong loc[64];
        for (uint a = 0; a < m; ++a) {
            device const uint *row = adj + uint(verts[a])*RW;
            ulong mask = 0;
            for (uint b = 0; b < m; ++b) {
                uint vb = uint(verts[b]);
                if ((row[vb >> 5] >> (vb & 31)) & 1u) { mask |= (1ul << b); }
            }
            loc[a] = mask;
        }
        ulong full = (m == 64) ? ~0ul : ((1ul << m) - 1ul);
        total = cdense64(full, need, loc);
    }
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
        dense64_enabled: u32,
        results: Vec<i32>,
    }

    impl CorrectionEngine {
        /// `None` when no Metal device is present, so callers fall back to the CPU path.
        pub fn new(graph: &Graph, vertex_count: usize, clique_size: usize) -> Option<Self> {
            Self::new_with_dense64(graph, vertex_count, clique_size, true)
        }

        /// Construct an engine with the n=3, 33--64 compressed path explicitly selected.
        ///
        /// Direct constructor users get the enabled default; the worker passes its startup flag.
        /// The switch also lets the GPU integration test compare this kernel against the established
        /// generic fallback on the same input.
        pub fn new_with_dense64(
            graph: &Graph,
            vertex_count: usize,
            clique_size: usize,
            dense64_enabled: bool,
        ) -> Option<Self> {
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
                dense64_enabled: u32::from(dense64_enabled),
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

        /// Native SIMD width for this exact pipeline. Dispatch packing must use this rather than
        /// assuming a particular Apple GPU generation's width.
        pub fn thread_execution_width(&self) -> usize {
            self.pipeline.thread_execution_width() as usize
        }

        /// Submit work without waiting, so the CPU can keep classifying while the GPU runs.
        ///
        /// A synchronous dispatch cannot help here: the GPU is slower than the CPU fleet at this
        /// kernel, so alternating between them is strictly worse than the CPU alone. The hybrid only
        /// pays if the two overlap, which means committing the command buffer and returning.
        pub fn dispatch(&mut self, reqs: &[CorrectionRequest]) -> Option<Pending> {
            if reqs.is_empty() {
                return Some(Pending { cb: None, out: None, len: 0 });
            }
            let (bs, bm, bo) = self.encode(reqs)?;
            let cb = self.queue.new_command_buffer();
            self.encode_pass(&cb, &bs, &bm, &bo, reqs.len());
            cb.commit();
            Some(Pending { cb: Some(cb.to_owned()), out: Some(bo), len: reqs.len() })
        }

        /// Block until a dispatched batch is done and read its results.
        pub fn collect(&mut self, p: Pending) -> &[i32] {
            self.results.clear();
            if let (Some(cb), Some(bo)) = (p.cb, p.out) {
                cb.wait_until_completed();
                self.results.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(bo.contents() as *const i32, p.len)
                });
            }
            &self.results
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
            let (bs, bm, bo) = self.encode(reqs)?;
            let cb = self.queue.new_command_buffer();
            self.encode_pass(&cb, &bs, &bm, &bo, reqs.len());
            cb.commit();
            cb.wait_until_completed();

            self.results.clear();
            self.results.extend_from_slice(unsafe {
                std::slice::from_raw_parts(bo.contents() as *const i32, reqs.len())
            });
            Some(&self.results)
        }

        fn encode(
            &self,
            reqs: &[CorrectionRequest],
        ) -> Option<(metal::Buffer, metal::Buffer, metal::Buffer)> {
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
            Some((
                self.device.new_buffer_with_data(
                    seeds.as_ptr() as *const _,
                    (seeds.len() * size_of::<u16>()) as u64,
                    MTLResourceOptions::StorageModeShared,
                ),
                self.device.new_buffer_with_data(
                    meta.as_ptr() as *const _,
                    meta.len() as u64,
                    MTLResourceOptions::StorageModeShared,
                ),
                self.device.new_buffer(
                    (reqs.len() * size_of::<i32>()) as u64,
                    MTLResourceOptions::StorageModeShared,
                ),
            ))
        }

        fn encode_pass(
            &self,
            cb: &metal::CommandBufferRef,
            bs: &metal::Buffer,
            bm: &metal::Buffer,
            bo: &metal::Buffer,
            n: usize,
        ) {
            let cs = self.clique_size as u32;
            let cnt = n as u32;
            let dense64_enabled = self.dense64_enabled;
            let enc = cb.new_compute_command_encoder();
            enc.set_compute_pipeline_state(&self.pipeline);
            enc.set_buffer(0, Some(&self.red), 0);
            enc.set_buffer(1, Some(&self.blue), 0);
            enc.set_buffer(2, Some(bs), 0);
            enc.set_buffer(3, Some(bm), 0);
            enc.set_buffer(4, Some(bo), 0);
            enc.set_bytes(5, size_of::<u32>() as u64, &cs as *const u32 as *const _);
            enc.set_bytes(6, size_of::<u32>() as u64, &cnt as *const u32 as *const _);
            enc.set_bytes(
                7,
                size_of::<u32>() as u64,
                &dense64_enabled as *const u32 as *const _,
            );
            let tg = self.pipeline.max_total_threads_per_threadgroup().min(256);
            enc.dispatch_threads(MTLSize::new(n as u64, 1, 1), MTLSize::new(tg, 1, 1));
            enc.end_encoding();
        }
    }

    /// A dispatched batch that has not been collected yet.
    pub struct Pending {
        cb: Option<metal::CommandBuffer>,
        out: Option<metal::Buffer>,
        len: usize,
    }
}

#[cfg(target_os = "macos")]
pub use backend::{CorrectionEngine, Pending};
