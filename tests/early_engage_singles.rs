//! Early hoist engagement evaluates a stage's singles from a table CARRIED from the parent graph,
//! so every carried (or lazily recomputed) single must equal a fresh count on the child graph.
//!
//! The reference flips the edge in a copy of the child and counts the monochromatic K_k through
//! both endpoints with the full-width recursion, without touching the hoist tables. Use two
//! consecutive stage base graphs (1-2 flips apart), as production carries:
//!
//!   PARENT_GRAPH=$(cat parent.txt) CHILD_GRAPH=$(cat child.txt) \
//!     cargo test --release --test early_engage_singles -- --ignored --nocapture

use ramsey_worker_rust::algorithm::count_cliques_through_vertex_set_reference;
use ramsey_worker_rust::graph::{Graph, WorkUnitEdge};
use ramsey_worker_rust::hoist::HoistTables;

const V: usize = 282;
const K: usize = 8;

fn graph_from_env(name: &str) -> Graph {
    let bits = std::env::var(name).unwrap_or_else(|_| panic!("set {name} to a 282-vertex edge string"));
    assert_eq!(bits.trim().len(), V * (V - 1) / 2, "{name} has the wrong length");
    Graph::from_bitstring(bits.trim(), V)
}

#[test]
#[ignore]
fn carried_singles_match_a_fresh_count_on_the_child_graph() {
    let mut parent = graph_from_env("PARENT_GRAPH");
    let mut child = graph_from_env("CHILD_GRAPH");
    let differing = (0..V)
        .flat_map(|u| ((u + 1)..V).map(move |v| (u, v)))
        .filter(|&(u, v)| parent.adjacency[u].get(v) != child.adjacency[u].get(v))
        .count();
    assert!(differing > 0, "parent and child are identical");

    let mut tables = HoistTables::new(V);
    tables.fill_slice(&mut parent, K, 0, 1);
    assert!(tables.is_complete(), "parent table must be complete before carrying");
    let (derived, invalidated) = tables.carry_forward(&parent, &child, K);
    let carried = tables.filled();

    let mut checked = 0usize;
    for u in 0..V {
        for v in (u + 1)..V {
            let got = tables.single_created(&mut child, K, u, v);
            let mut flipped = child.clone();
            flipped.flip_edges(&[WorkUnitEdge {
                vertex_one: u as u16,
                vertex_two: v as u16,
            }]);
            let adjacency = if flipped.adjacency[u].get(v) {
                &flipped.adjacency
            } else {
                &flipped.complement_adjacency
            };
            let want = count_cliques_through_vertex_set_reference(adjacency, &[u, v], K);
            assert_eq!(got, want, "single ({u},{v}): carried table {got} != fresh count {want}");
            checked += 1;
        }
    }
    eprintln!(
        "{differing} edge(s) apart: carried {carried} entries ({derived} derived, {invalidated} invalidated); \
         all {checked} singles match a fresh full-width count on the child graph"
    );
}
