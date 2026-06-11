//! Regression suite: mathematical ground-truth anchors from Paley graphs.
//!
//! Paley colorings have clique structure known from the literature, entirely
//! independent of this codebase: Paley(5) witnesses R(3,3) > 5, Paley(17)
//! witnesses R(4,4) > 17, and Paley(281) witnesses R(8,8) ≥ 282 — the exact
//! bound this project starts from. Triangle counts follow from the
//! strongly-regular parameters (p, (p-1)/2, (p-5)/4, (p-1)/4) and, for
//! p = 281, match the values recorded in
//! ramsey-mw/docs/investigations/paley-graph-investigation.md.

mod common;

use common::{
    brute_force_mono_clique_count, paley_bitstring, paley_mono_triangle_count, red_count,
};
use ramsey_worker_rust::algorithm::get_cliques_comprehensive;
use ramsey_worker_rust::graph::Graph;

#[test]
fn paley_graphs_are_exactly_balanced() {
    // Self-complementarity: exactly half of the C(p,2) edges are red.
    for p in [5, 13, 17] {
        let bits = paley_bitstring(p);
        assert_eq!(red_count(&bits) * 2, bits.len(), "p={p}");
    }
}

#[test]
fn paley_5_has_no_monochromatic_triangle() {
    // C5 plus its complement C5: the classic R(3,3) > 5 witness.
    let mut g = Graph::from_bitstring(&paley_bitstring(5), 5);
    assert_eq!(get_cliques_comprehensive(&mut g, 3), 0);
    assert_eq!(brute_force_mono_clique_count(&g, 3), 0);
}

#[test]
fn paley_13_triangle_count_matches_strongly_regular_formula() {
    // 2 * 13*12*8/48 = 52 monochromatic triangles.
    let mut g = Graph::from_bitstring(&paley_bitstring(13), 13);
    let expected = paley_mono_triangle_count(13);
    assert_eq!(expected, 52);
    assert_eq!(get_cliques_comprehensive(&mut g, 3), expected);
    assert_eq!(brute_force_mono_clique_count(&g, 3), expected);
}

#[test]
fn paley_17_has_no_monochromatic_k4() {
    // The unique R(4,4) = 18 lower-bound witness.
    let mut g = Graph::from_bitstring(&paley_bitstring(17), 17);
    assert_eq!(get_cliques_comprehensive(&mut g, 4), 0);
    assert_eq!(brute_force_mono_clique_count(&g, 4), 0);
    // Its triangle ladder is still nonzero and known: 2 * 17*16*12/48 = 136.
    assert_eq!(get_cliques_comprehensive(&mut g, 3), 136);
}

#[test]
fn paley_281_has_no_monochromatic_k8() {
    // Production-scale ground truth: the R(8,8) ≥ 282 witness graph. The
    // campaign's entire premise rests on this count being zero, and it
    // exercises Bron-Kerbosch at the exact (n ≈ 282, k = 8) operating point
    // of the production system.
    let mut g = Graph::from_bitstring(&paley_bitstring(281), 281);
    assert_eq!(get_cliques_comprehensive(&mut g, 8), 0);
}

#[test]
fn paley_281_triangle_count_matches_documented_value() {
    // 2 * 281*280*276/48 = 904,820 — also recorded (as 452,410 per color) in
    // docs/investigations/paley-graph-investigation.md in ramsey-mw.
    let mut g = Graph::from_bitstring(&paley_bitstring(281), 281);
    let expected = paley_mono_triangle_count(281);
    assert_eq!(expected, 904_820);
    assert_eq!(get_cliques_comprehensive(&mut g, 3), expected);
}
