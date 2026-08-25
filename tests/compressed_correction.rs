//! The compressed-candidate-set path must agree with the bitset path it replaces, exactly.
//!
//! Below the first intersection the recursion manipulates a set that never exceeds ~49 elements
//! (measured over 400k corrections on real campaign graphs: max |P| = 32 for n=4, 49 for n=3), yet
//! it does so with 320-bit bitsets. Compressing to a local index space lets the whole recursion run
//! on single `u64` masks. That is a rewrite of the hottest code in the worker, so it is checked
//! against the original rather than reasoned about.
//!
//!   cargo test --release --test compressed_correction -- --ignored

use ramsey_worker_rust::algorithm::{
    count_cliques_through_vertex_set, count_cliques_through_vertex_set_reference,
};
use ramsey_worker_rust::graph::Graph;

const FIXTURE: &str = include_str!("fixtures/campaign3-consecutive-bases.txt");
const V: usize = 282;
const K: usize = 8;

fn bases() -> Vec<String> {
    FIXTURE.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

fn check(bits: &str, label: &str) {
    let g = Graph::from_bitstring(bits, V);
    let mut rng: u64 = 0x51ED270B4C1D9A2F;
    let mut next = move || { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; rng };

    let mut checked = 0usize;
    let mut n3 = 0usize;
    let mut nonzero = 0usize;
    for _ in 0..60_000 {
        let n = if checked % 2 == 0 { 3 } else { 4 };
        let mut seeds = [0usize; 4];
        for s in seeds.iter_mut().take(n) { *s = (next() % V as u64) as usize; }
        if (0..n).any(|a| (a + 1..n).any(|b| seeds[a] == seeds[b])) { continue; }
        let blue = next() & 1 == 1;
        let adj = if blue { &g.complement_adjacency } else { &g.adjacency };

        let got = count_cliques_through_vertex_set(adj, &seeds[..n], K);
        let want = count_cliques_through_vertex_set_reference(adj, &seeds[..n], K);
        assert_eq!(
            got, want,
            "{label}: seeds={:?} n={n} blue={blue}: compressed={got} reference={want}",
            &seeds[..n]
        );
        if n == 3 { n3 += 1; }
        if want != 0 { nonzero += 1; }
        checked += 1;
    }
    assert!(checked > 40_000, "{label}: only {checked} cases ran");
    assert!(n3 > 15_000, "{label}: too few n=3 cases ({n3}) — the deeper recursion is untested");
    assert!(nonzero > 1_000, "{label}: {nonzero} non-zero results — mostly trivial, proves little");
}

#[test]
#[ignore]
fn compressed_matches_the_bitset_path_on_a_real_graph() {
    check(&bases()[0], "graph 993915");
}

#[test]
#[ignore]
fn compressed_matches_the_bitset_path_on_a_second_graph() {
    check(&bases()[2], "graph 993917");
}

/// Small dense graphs push |P| far above what campaign graphs produce, which is where the
/// >64 fallback has to engage. A complete graph makes every candidate set maximal.
#[test]
#[ignore]
fn compressed_agrees_where_the_candidate_set_is_too_large_to_compress() {
    for v in [70usize, 130, 200] {
        let g = Graph::from_bitstring(&"1".repeat(v * (v - 1) / 2), v);
        for n in 2..=4usize {
            let seeds: Vec<usize> = (0..n).collect();
            let got = count_cliques_through_vertex_set(&g.adjacency, &seeds, K);
            let want = count_cliques_through_vertex_set_reference(&g.adjacency, &seeds, K);
            assert_eq!(got, want, "K{v} n={n}: compressed={got} reference={want}");
        }
    }
}

/// Speed of the compressed path against the bitset path it replaces, on a real graph.
#[test]
#[ignore]
fn compressed_is_faster_than_the_bitset_path() {
    let g = Graph::from_bitstring(&bases()[0], V);
    let mut rng: u64 = 0xA5A5C3C3F0F00F0F;
    let mut next = move || { rng ^= rng << 13; rng ^= rng >> 7; rng ^= rng << 17; rng };
    for want_n in [4usize, 3] {
        let mut sets = Vec::new();
        while sets.len() < 150_000 {
            let mut s = [0usize; 4];
            for x in s.iter_mut().take(want_n) { *x = (next() % V as u64) as usize; }
            if (0..want_n).any(|a| (a + 1..want_n).any(|b| s[a] == s[b])) { continue; }
            sets.push((s, want_n, next() & 1 == 1));
        }
        let run = |f: &dyn Fn(&[ramsey_worker_rust::bitset::BitMatrix], &[usize], usize) -> i32| {
            let t = std::time::Instant::now();
            let mut acc = 0i64;
            for (s, n, blue) in &sets {
                let adj = if *blue { &g.complement_adjacency } else { &g.adjacency };
                acc += f(adj, &s[..*n], K) as i64;
            }
            (t.elapsed().as_secs_f64(), acc)
        };
        let (t_new, a_new) = run(&count_cliques_through_vertex_set);
        let (t_old, a_old) = run(&count_cliques_through_vertex_set_reference);
        assert_eq!(a_new, a_old, "n={want_n}: totals differ");
        eprintln!(
            "n={want_n}: compressed {:.2} M/s | bitset {:.2} M/s | {:.2}x",
            sets.len() as f64 / t_new / 1e6,
            sets.len() as f64 / t_old / 1e6,
            t_old / t_new
        );
    }
}
