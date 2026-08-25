//! Does running the GPU CONCURRENTLY with the CPU actually beat the CPU alone?
//!
//! The offload question is settled: the GPU is slower than the CPU fleet at this kernel, so
//! alternating between them loses. The hybrid only pays if the two OVERLAP — the CPU classifying
//! the next chunk while the GPU corrects the previous one. That makes dispatch and collect overhead,
//! not raw kernel speed, the thing that decides it, which is where hybrid designs usually die.
//!
//! This is a faithful stand-in for the worker's unit loop — same table lookups, same cross-pairs
//! decision, same bound — run three ways over the same units, on a real campaign graph:
//!
//!   * CPU-only         : corrections inline, as production does today
//!   * hybrid-sync      : collect corrections, dispatch, wait, finalise (no overlap)
//!   * hybrid-pipelined : dispatch chunk N, classify chunk N+1 while it runs, then collect N
//!
//! Every variant must produce identical results for every unit; that is asserted, not assumed.
//!
//!   cargo test --release --test gpu_hybrid_pipeline -- --ignored --nocapture

#![cfg(target_os = "macos")]

use ramsey_worker_rust::algorithm::count_cliques_through_vertex_set;
use ramsey_worker_rust::gpu::{CorrectionEngine, CorrectionRequest};
use ramsey_worker_rust::graph::Graph;
use ramsey_worker_rust::hoist::{cross_pairs, CrossPairs, HoistTables};

const FIXTURE: &str = include_str!("fixtures/campaign3-consecutive-bases.txt");
const V: usize = 282;
const K: usize = 8;
/// Corrections per GPU dispatch. Large enough that per-dispatch overhead is amortised, small
/// enough that the CPU has something to overlap with.
const CHUNK: usize = 32_768;

fn base() -> String {
    FIXTURE.lines().next().unwrap().trim().to_string()
}

/// The classify decision, mirroring `HoistTables::pair_created_bounded` exactly.
enum Decision {
    Resolved(i32),
    Rejected,
    Needs { base: i32, seeds: [u16; 4], n: u8, blue: bool },
}

fn classify(
    t: &mut HoistTables,
    g: &mut Graph,
    r: (usize, usize),
    b: (usize, usize),
    limit: i32,
) -> Decision {
    let c_b = t.single_created(g, K, b.0, b.1);
    let d_r = t.single_created(g, K, r.0, r.1);
    let base = c_b + d_r;
    match cross_pairs(&g.adjacency, r, b) {
        CrossPairs::Mixed => {
            if base > limit { Decision::Rejected } else { Decision::Resolved(base) }
        }
        CrossPairs::AllRed => {
            if d_r > limit { return Decision::Rejected; }
            let (seeds, n) = forced(r, b);
            Decision::Needs { base, seeds, n, blue: false }
        }
        CrossPairs::AllBlue => {
            if c_b > limit { return Decision::Rejected; }
            let (seeds, n) = forced(r, b);
            Decision::Needs { base, seeds, n, blue: true }
        }
    }
}

fn forced(r: (usize, usize), b: (usize, usize)) -> ([u16; 4], u8) {
    let mut s = [0u16; 4];
    let mut n = 0usize;
    for w in [r.0, r.1, b.0, b.1] {
        if !s[..n].contains(&(w as u16)) { s[n] = w as u16; n += 1; }
    }
    (s, n as u8)
}

fn finish(base: i32, correction: i32, limit: i32) -> Option<i32> {
    let created = base - correction;
    if created > limit { None } else { Some(created) }
}

#[test]
#[ignore]
fn hybrid_pipeline_vs_cpu_only() {
    let bits = base();
    let mut g = Graph::from_bitstring(&bits, V);

    let mut reds = Vec::new();
    let mut blues = Vec::new();
    for i in 0..V {
        for j in (i + 1)..V {
            if g.adjacency[i].get(j) { reds.push((i, j)) } else { blues.push((i, j)) }
        }
    }
    eprintln!("graph: {} red edges, {} blue edges", reds.len(), blues.len());

    // Warm the per-edge table once, as a worker does at stage engage.
    let mut tables = HoistTables::new(V);
    let t_warm = std::time::Instant::now();
    for u in 0..V { for v in (u + 1)..V { tables.single_created(&mut g, K, u, v); } }
    eprintln!("hoist table warmed in {:.1}s", t_warm.elapsed().as_secs_f64());

    // A realistic slice of the work space: a contiguous run of (red, blue) pairs.
    const UNITS: usize = 3_000_000;
    let limit = 744_600i32;
    let mut units = Vec::with_capacity(UNITS);
    'outer: for &r in reds.iter() {
        for &b in blues.iter() {
            units.push((r, b));
            if units.len() == UNITS { break 'outer; }
        }
    }

    // ---- CPU-only ----
    let t0 = std::time::Instant::now();
    let mut cpu_out: Vec<Option<i32>> = Vec::with_capacity(UNITS);
    let mut needed = 0usize;
    for &(r, b) in &units {
        match classify(&mut tables, &mut g, r, b, limit) {
            Decision::Resolved(x) => cpu_out.push(Some(x)),
            Decision::Rejected => cpu_out.push(None),
            Decision::Needs { base, seeds, n, blue } => {
                needed += 1;
                let adj = if blue { &g.complement_adjacency } else { &g.adjacency };
                let s: Vec<usize> = seeds[..n as usize].iter().map(|&x| x as usize).collect();
                let c = count_cliques_through_vertex_set(adj, &s, K);
                cpu_out.push(finish(base, c, limit));
            }
        }
    }
    let cpu_secs = t0.elapsed().as_secs_f64();
    eprintln!(
        "CPU-only        : {:.2} M units/sec   ({} corrections, {:.2}% of units)",
        UNITS as f64 / cpu_secs / 1e6, needed, 100.0 * needed as f64 / UNITS as f64
    );

    let mut engine = match CorrectionEngine::new(&g, V, K) {
        Some(e) => e,
        None => { eprintln!("no Metal device"); return; }
    };

    // ---- hybrid, synchronous (no overlap) ----
    let t1 = std::time::Instant::now();
    let mut sync_out: Vec<Option<i32>> = vec![None; UNITS];
    {
        let mut pend: Vec<(usize, i32)> = Vec::new();
        let mut reqs: Vec<CorrectionRequest> = Vec::new();
        for (i, &(r, b)) in units.iter().enumerate() {
            match classify(&mut tables, &mut g, r, b, limit) {
                Decision::Resolved(x) => sync_out[i] = Some(x),
                Decision::Rejected => {}
                Decision::Needs { base, seeds, n, blue } => {
                    pend.push((i, base));
                    reqs.push(CorrectionRequest { seeds, n, blue });
                    if reqs.len() == CHUNK {
                        let res = engine.run(&reqs).unwrap().to_vec();
                        for ((idx, bse), c) in pend.iter().zip(res.iter()) {
                            sync_out[*idx] = finish(*bse, *c, limit);
                        }
                        pend.clear(); reqs.clear();
                    }
                }
            }
        }
        if !reqs.is_empty() {
            let res = engine.run(&reqs).unwrap().to_vec();
            for ((idx, bse), c) in pend.iter().zip(res.iter()) {
                sync_out[*idx] = finish(*bse, *c, limit);
            }
        }
    }
    let sync_secs = t1.elapsed().as_secs_f64();

    // ---- hybrid, pipelined (CPU classifies while the GPU corrects) ----
    let t2 = std::time::Instant::now();
    let mut pipe_out: Vec<Option<i32>> = vec![None; UNITS];
    {
        let mut pend: Vec<(usize, i32)> = Vec::new();
        let mut reqs: Vec<CorrectionRequest> = Vec::new();
        let mut inflight: Option<(ramsey_worker_rust::gpu::Pending, Vec<(usize, i32)>)> = None;
        for (i, &(r, b)) in units.iter().enumerate() {
            match classify(&mut tables, &mut g, r, b, limit) {
                Decision::Resolved(x) => pipe_out[i] = Some(x),
                Decision::Rejected => {}
                Decision::Needs { base, seeds, n, blue } => {
                    pend.push((i, base));
                    reqs.push(CorrectionRequest { seeds, n, blue });
                    if reqs.len() == CHUNK {
                        // Collect the PREVIOUS batch only now — it ran while we classified.
                        if let Some((p, owed)) = inflight.take() {
                            let res = engine.collect(p).to_vec();
                            for ((idx, bse), c) in owed.iter().zip(res.iter()) {
                                pipe_out[*idx] = finish(*bse, *c, limit);
                            }
                        }
                        let p = engine.dispatch(&reqs).unwrap();
                        inflight = Some((p, std::mem::take(&mut pend)));
                        reqs.clear();
                    }
                }
            }
        }
        if let Some((p, owed)) = inflight.take() {
            let res = engine.collect(p).to_vec();
            for ((idx, bse), c) in owed.iter().zip(res.iter()) {
                pipe_out[*idx] = finish(*bse, *c, limit);
            }
        }
        if !reqs.is_empty() {
            let res = engine.run(&reqs).unwrap().to_vec();
            for ((idx, bse), c) in pend.iter().zip(res.iter()) {
                pipe_out[*idx] = finish(*bse, *c, limit);
            }
        }
    }
    let pipe_secs = t2.elapsed().as_secs_f64();

    eprintln!(
        "hybrid-sync     : {:.2} M units/sec   ({:.2}x CPU-only)",
        UNITS as f64 / sync_secs / 1e6, cpu_secs / sync_secs
    );
    eprintln!(
        "hybrid-pipelined: {:.2} M units/sec   ({:.2}x CPU-only)",
        UNITS as f64 / pipe_secs / 1e6, cpu_secs / pipe_secs
    );

    // ACCURACY: every variant must agree with CPU-only on every single unit.
    let mut bad_sync = 0usize;
    let mut bad_pipe = 0usize;
    for i in 0..UNITS {
        if sync_out[i] != cpu_out[i] { if bad_sync < 3 { eprintln!("sync mismatch at {i}: {:?} vs {:?}", sync_out[i], cpu_out[i]); } bad_sync += 1; }
        if pipe_out[i] != cpu_out[i] { if bad_pipe < 3 { eprintln!("pipe mismatch at {i}: {:?} vs {:?}", pipe_out[i], cpu_out[i]); } bad_pipe += 1; }
    }
    assert_eq!(bad_sync, 0, "hybrid-sync disagreed on {bad_sync} of {UNITS} units");
    assert_eq!(bad_pipe, 0, "hybrid-pipelined disagreed on {bad_pipe} of {UNITS} units");
    eprintln!("accuracy: all {UNITS} units identical across all three paths");
}
