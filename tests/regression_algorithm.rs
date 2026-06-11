//! Regression suite: core clique-counting algorithms against an independent
//! brute-force reference, plus the incremental-delta identities every worker
//! mode (exhaustive, SA, VDS, tabu) depends on.

mod common;

use common::{brute_force_mono_clique_count, random_bitstring};
use ramsey_worker_rust::algorithm::{
    get_all_cliques, get_cliques_comprehensive, get_new_cliques, get_new_cliques_with_limit,
};
use ramsey_worker_rust::clique_collection::CliqueCollection;
use ramsey_worker_rust::graph::{Graph, WorkUnitEdge};
use std::collections::HashSet;

const SEEDS: [u64; 6] = [1, 2, 3, 42, 1337, 8319];

#[test]
fn comprehensive_count_matches_brute_force_on_random_graphs() {
    for n in [7, 9, 10] {
        for k in [3, 4, 5] {
            for seed in SEEDS {
                let mut g = Graph::from_bitstring(&random_bitstring(seed, n), n);
                let expected = brute_force_mono_clique_count(&g, k);
                let actual = get_cliques_comprehensive(&mut g, k);
                assert_eq!(actual, expected, "n={n} k={k} seed={seed}");
            }
        }
    }
}

#[test]
fn get_all_cliques_returns_valid_unique_monochromatic_cliques() {
    for seed in SEEDS {
        let n = 10;
        let k = 4;
        let mut g = Graph::from_bitstring(&random_bitstring(seed, n), n);
        let cliques = get_all_cliques(&mut g, k);

        // Count agrees with the counting variant and the brute-force reference.
        assert_eq!(
            cliques.len() as i32,
            get_cliques_comprehensive(&mut g, k),
            "collect/count mismatch seed={seed}"
        );
        assert_eq!(
            cliques.len() as i32,
            brute_force_mono_clique_count(&g, k),
            "collect/brute mismatch seed={seed}"
        );

        // Every clique is the right size, monochromatic, and unique.
        let mut seen: HashSet<Vec<usize>> = HashSet::new();
        for clique in &cliques {
            assert_eq!(clique.len(), k, "wrong clique size seed={seed}");
            let color = g.adjacency[clique[0]].get(clique[1]);
            for i in 0..k {
                for j in (i + 1)..k {
                    assert_eq!(
                        g.adjacency[clique[i]].get(clique[j]),
                        color,
                        "non-monochromatic clique {clique:?} seed={seed}"
                    );
                }
            }
            let mut sorted = clique.clone();
            sorted.sort_unstable();
            assert!(
                seen.insert(sorted),
                "duplicate clique {clique:?} seed={seed}"
            );
        }
    }
}

#[test]
fn single_edge_delta_matches_full_recount_on_every_edge() {
    // The identity behind every incremental evaluation in the system:
    // new_total = base - destroyed + created, where destroyed/created come
    // from edge-seeded Bron-Kerbosch before/after the flip.
    for seed in SEEDS {
        let n = 8;
        let k = 4;
        let mut g = Graph::from_bitstring(&random_bitstring(seed, n), n);
        let base = get_cliques_comprehensive(&mut g, k);
        for i in 0..n as u16 {
            for j in (i + 1)..n as u16 {
                let edge = WorkUnitEdge {
                    vertex_one: i,
                    vertex_two: j,
                };
                let edges = std::slice::from_ref(&edge);
                let destroyed = get_new_cliques(&mut g, k, edges);
                g.flip_edges(edges);
                let created = get_new_cliques(&mut g, k, edges);
                let recount = get_cliques_comprehensive(&mut g, k);
                g.flip_edges(edges);
                assert_eq!(
                    base - destroyed + created,
                    recount,
                    "edge=({i},{j}) seed={seed}"
                );
            }
        }
    }
}

#[test]
fn balanced_pair_delta_matches_full_recount_for_every_red_blue_pair() {
    // The exhaustive engine's work-unit shape: flip one red and one blue edge
    // simultaneously. Opposite colors guarantee the two seeded searches count
    // disjoint clique sets, so the summed delta is exact.
    let n = 8;
    let k = 4;
    let mut g = Graph::from_bitstring(&random_bitstring(42, n), n);
    let base = get_cliques_comprehensive(&mut g, k);

    let mut red = Vec::new();
    let mut blue = Vec::new();
    for i in 0..n as u16 {
        for j in (i + 1)..n as u16 {
            let edge = WorkUnitEdge {
                vertex_one: i,
                vertex_two: j,
            };
            if g.adjacency[i as usize].get(j as usize) {
                red.push(edge);
            } else {
                blue.push(edge);
            }
        }
    }

    for r in &red {
        for b in &blue {
            let pair = [r.clone(), b.clone()];
            let destroyed = get_new_cliques(&mut g, k, &pair);
            g.flip_edges(&pair);
            let created = get_new_cliques(&mut g, k, &pair);
            let recount = get_cliques_comprehensive(&mut g, k);
            g.flip_edges(&pair);
            assert_eq!(
                base - destroyed + created,
                recount,
                "pair=({},{})x({},{})",
                r.vertex_one,
                r.vertex_two,
                b.vertex_one,
                b.vertex_two
            );
        }
    }
}

#[test]
fn same_color_seed_pairs_count_shared_cliques_twice() {
    // Contract documentation: get_new_cliques sums one seeded search per edge
    // and does NOT de-duplicate cliques containing several seed edges. For the
    // production red+blue pair shape this cannot happen (a monochromatic
    // clique cannot contain both colors), which is why deltas are exact there.
    // Any future same-color enumeration must handle this multiplicity.
    let mut g = Graph::from_bitstring(&"1".repeat(10), 5); // K5, one red 5-clique
    let pair = [
        WorkUnitEdge {
            vertex_one: 0,
            vertex_two: 1,
        },
        WorkUnitEdge {
            vertex_one: 2,
            vertex_two: 3,
        },
    ];
    // Both seed edges are red and both lie in the single 5-clique: it is
    // counted once per seed edge.
    assert_eq!(get_new_cliques(&mut g, 5, &pair), 2);
}

#[test]
fn clique_collection_broken_count_matches_seeded_bron_kerbosch() {
    // The exhaustive engine looks up "broken" counts in CliqueCollection while
    // SA/VDS/tabu derive the same quantity from edge-seeded Bron-Kerbosch.
    // Both paths must agree on every edge.
    for seed in SEEDS {
        let n = 10;
        let k = 4;
        let mut g = Graph::from_bitstring(&random_bitstring(seed, n), n);
        let cliques = get_all_cliques(&mut g, k);
        let mut cc = CliqueCollection::new(n);
        cc.set_cliques(cliques, n);

        for i in 0..n as u16 {
            for j in (i + 1)..n as u16 {
                let edge = WorkUnitEdge {
                    vertex_one: i,
                    vertex_two: j,
                };
                let from_collection =
                    cc.get_count_of_cliques_containing_edges(std::slice::from_ref(&edge));
                let from_seeded_bk = get_new_cliques(&mut g, k, std::slice::from_ref(&edge));
                assert_eq!(
                    from_collection, from_seeded_bk,
                    "edge=({i},{j}) seed={seed}"
                );
            }
        }
    }
}

#[test]
fn clique_collection_participation_sums_to_pairs_per_clique_times_total() {
    // Every k-clique contributes C(k,2) edge participations.
    let n = 10;
    let k = 4;
    let mut g = Graph::from_bitstring(&random_bitstring(1337, n), n);
    let cliques = get_all_cliques(&mut g, k);
    let total = cliques.len() as i64;
    let mut cc = CliqueCollection::new(n);
    cc.set_cliques(cliques, n);

    let mut sum: i64 = 0;
    for i in 0..n as u16 {
        for j in (i + 1)..n as u16 {
            let edge = WorkUnitEdge {
                vertex_one: i,
                vertex_two: j,
            };
            sum += cc.get_count_of_cliques_containing_edges(std::slice::from_ref(&edge)) as i64;
        }
    }
    let pairs_per_clique = (k * (k - 1) / 2) as i64;
    assert_eq!(sum, pairs_per_clique * total);
}

#[test]
fn early_termination_limit_semantics_are_exact_at_the_boundary() {
    // K7 all red at k=5: the seeded search on edge (0,1) finds C(5,3) = 10
    // cliques containing that edge.
    let mut g = Graph::from_bitstring(&"1".repeat(21), 7);
    let edge = [WorkUnitEdge {
        vertex_one: 0,
        vertex_two: 1,
    }];

    let (exact, exceeded) = get_new_cliques_with_limit(&mut g, 5, &edge, i32::MAX);
    assert_eq!(exact, 10);
    assert!(!exceeded);

    // Limit equal to the true count: exact result, no early exit.
    let (at_limit, exceeded_at_limit) = get_new_cliques_with_limit(&mut g, 5, &edge, 10);
    assert_eq!(at_limit, 10);
    assert!(!exceeded_at_limit);

    // Limit below the true count: must flag exceeded, and the partial count
    // must already be above the limit (the caller treats it as "worse than
    // threshold", never as an exact value).
    let (partial, exceeded_below) = get_new_cliques_with_limit(&mut g, 5, &edge, 9);
    assert!(exceeded_below);
    assert!(partial > 9, "partial count {partial} must exceed the limit");

    // Graph state must be restored after every call.
    assert_eq!(get_cliques_comprehensive(&mut g, 5), 21); // C(7,5) red 5-cliques
}
