//! GPU correction engine vs the CPU primitive it replaces.
//!
//! The engine exists to compute exactly what `count_cliques_through_vertex_set` computes, faster.
//! "Looks equivalent" is not evidence for a reimplementation in another language on another
//! processor, so this checks it over real campaign graphs, in both colours, for both seed shapes
//! (3 distinct forced vertices when the two edges share one, 4 when they do not).
//!
//! Ignored by default: needs a Metal device.
//!   cargo test --release --test gpu_correction -- --ignored

#![cfg(target_os = "macos")]

use ramsey_worker_rust::algorithm::count_cliques_through_vertex_set;
use ramsey_worker_rust::gpu::{CorrectionEngine, CorrectionRequest};
use ramsey_worker_rust::graph::Graph;

const FIXTURE: &str = include_str!("fixtures/campaign3-consecutive-bases.txt");
const V: usize = 282;
const K: usize = 8;

fn bases() -> Vec<String> {
    FIXTURE
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The forced vertex set of a pair move: the distinct endpoints of the two edges.
fn forced(r: (usize, usize), b: (usize, usize)) -> ([u16; 4], u8) {
    let mut s = [0u16; 4];
    let mut n = 0usize;
    for w in [r.0, r.1, b.0, b.1] {
        if !s[..n].contains(&(w as u16)) {
            s[n] = w as u16;
            n += 1;
        }
    }
    (s, n as u8)
}

fn check_graph(bits: &str, label: &str) {
    let graph = Graph::from_bitstring(bits, V);
    let mut engine = match CorrectionEngine::new(&graph, V, K) {
        Some(e) => e,
        None => {
            eprintln!("no Metal device — skipping {label}");
            return;
        }
    };

    // Deterministic spread of pair moves, including shared-vertex ones so both seed shapes and
    // therefore both shader paths (need = 4 and need = 5) are exercised.
    let mut rng: u64 = 0x9E3779B97F4A7C15;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };

    let mut reqs = Vec::new();
    let mut want = Vec::new();
    let mut shared = 0usize;
    while reqs.len() < 20_000 {
        let a = (next() % V as u64) as usize;
        let b1 = (next() % V as u64) as usize;
        if a == b1 {
            continue;
        }
        // Half the cases deliberately share a vertex with the first edge.
        let (c, d) = if reqs.len() % 2 == 0 {
            let d = (next() % V as u64) as usize;
            if d == a {
                continue;
            }
            (a, d)
        } else {
            let c = (next() % V as u64) as usize;
            let d = (next() % V as u64) as usize;
            if c == d {
                continue;
            }
            (c, d)
        };
        let (seeds, n) = forced((a, b1), (c, d));
        if n < 3 {
            continue;
        }
        if n == 3 {
            shared += 1;
        }
        for blue in [false, true] {
            let adj = if blue {
                &graph.complement_adjacency
            } else {
                &graph.adjacency
            };
            let s: Vec<usize> = seeds[..n as usize].iter().map(|&x| x as usize).collect();
            want.push(count_cliques_through_vertex_set(adj, &s, K));
            reqs.push(CorrectionRequest { seeds, n, blue });
        }
    }
    assert!(shared > 1_000, "{label}: too few shared-vertex seeds to exercise need=5");

    let got = engine.run(&reqs).expect("engine rejected a request shape");
    assert_eq!(got.len(), want.len(), "{label}: result count");

    let mut mismatches = 0;
    for (i, (g, w)) in got.iter().zip(want.iter()).enumerate() {
        if g != w {
            if mismatches < 5 {
                eprintln!(
                    "{label}: req {i} seeds={:?} n={} blue={} gpu={g} cpu={w}",
                    reqs[i].seeds, reqs[i].n, reqs[i].blue
                );
            }
            mismatches += 1;
        }
    }
    assert_eq!(
        mismatches, 0,
        "{label}: {mismatches} of {} corrections disagree with the CPU primitive",
        want.len()
    );
}

#[test]
#[ignore]
fn gpu_matches_cpu_on_a_real_campaign_graph() {
    check_graph(&bases()[0], "graph 993915");
}

/// A second graph, because agreeing on one is agreeing on one set of candidate-set sizes.
#[test]
#[ignore]
fn gpu_matches_cpu_on_a_second_campaign_graph() {
    check_graph(&bases()[2], "graph 993917");
}

/// Throughput of the engine against the CPU primitive it would replace.
///
/// Reported for `n = 4` (both edges disjoint), which dominates production: a red and a blue edge
/// drawn from ~19,800 each rarely share a vertex. The `n = 3` case recurses one level deeper and is
/// far more expensive, so a 50/50 mix would understate the achievable rate — it is measured
/// separately rather than blended in.
#[test]
#[ignore]
fn gpu_correction_throughput_against_cpu() {
    let graph = Graph::from_bitstring(&bases()[0], V);
    let mut engine = match CorrectionEngine::new(&graph, V, K) {
        Some(e) => e,
        None => { eprintln!("no Metal device"); return; }
    };

    let mut rng: u64 = 0xDEADBEEFCAFEF00D;
    let mut next = move || { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; rng };

    for (label, want_n) in [("n=4 (production-dominant)", 4u8), ("n=3 (shared vertex)", 3u8)] {
        let mut reqs = Vec::new();
        while reqs.len() < 200_000 {
            let a = (next() % V as u64) as usize;
            let b1 = (next() % V as u64) as usize;
            let c = if want_n == 3 { a } else { (next() % V as u64) as usize };
            let d = (next() % V as u64) as usize;
            if a == b1 || c == d { continue; }
            let (seeds, n) = forced((a, b1), (c, d));
            if n != want_n { continue; }
            reqs.push(CorrectionRequest { seeds, n, blue: reqs.len() % 2 == 0 });
        }

        let t0 = std::time::Instant::now();
        let got = engine.run(&reqs).unwrap().to_vec();
        let gpu = t0.elapsed().as_secs_f64();

        let t1 = std::time::Instant::now();
        let mut sum: i64 = 0;
        for r in &reqs {
            let adj = if r.blue { &graph.complement_adjacency } else { &graph.adjacency };
            let s: Vec<usize> = r.seeds[..r.n as usize].iter().map(|&x| x as usize).collect();
            sum += count_cliques_through_vertex_set(adj, &s, K) as i64;
        }
        let cpu = t1.elapsed().as_secs_f64();

        let gsum: i64 = got.iter().map(|&x| x as i64).sum();
        assert_eq!(gsum, sum, "{label}: GPU and CPU totals differ");

        eprintln!(
            "{label}: {} reqs | GPU {:.2} M/s | CPU 1 core {:.2} M/s | ratio {:.1}x (1 core), {:.1}x (16 cores)",
            reqs.len(),
            reqs.len() as f64 / gpu / 1e6,
            reqs.len() as f64 / cpu / 1e6,
            gpu.recip() / cpu.recip(),
            (gpu.recip() / cpu.recip()) / 16.0
        );
    }
}
