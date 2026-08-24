//! Production-scale accuracy check for the hoist table's stage-to-stage carry.
//!
//! `HoistTables::carry_forward` keeps every entry whose `cross_pairs` relationship to the flipped
//! edges is Mixed in both graphs, and rebuilds the rest. If that predicate is even slightly too
//! permissive, a stale `created` is served to every unit of the stage AND published to peers, and
//! nothing downstream notices — the fleet just quietly searches on wrong numbers.
//!
//! The unit tests in `hoist.rs` cover this exhaustively but at n=9/10, k=4/5. The predicate is
//! structural and so should be size-independent, but "should be" is exactly the assumption worth
//! checking at the size that actually runs: these are three consecutive campaign-3 base graphs,
//! 282 vertices, k=8, covering both transition shapes the queue manager produces — a single-edge
//! advance (993915 -> 993916) and a pair-edge advance (993916 -> 993917).
//!
//! Ignored by default: each case builds two full 39,621-entry tables at k=8, a few tens of seconds.
//! Run with `cargo test --release --test carry_forward_production_scale -- --ignored`.

use ramsey_worker_rust::graph::Graph;
use ramsey_worker_rust::hoist::HoistTables;

const FIXTURE: &str = include_str!("fixtures/campaign3-consecutive-bases.txt");
const VERTEX_COUNT: usize = 282;
const CLIQUE_SIZE: usize = 8;

fn bases() -> Vec<String> {
    FIXTURE
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Build the whole table for `g`, entry by entry.
fn full_table(g: &mut Graph) -> HoistTables {
    let mut t = HoistTables::new(VERTEX_COUNT);
    for u in 0..VERTEX_COUNT {
        for v in (u + 1)..VERTEX_COUNT {
            t.single_created(g, CLIQUE_SIZE, u, v);
        }
    }
    t
}

fn assert_carry_matches_rebuild(before_bits: &str, after_bits: &str, label: &str) {
    let mut before = Graph::from_bitstring(before_bits, VERTEX_COUNT);
    let mut after = Graph::from_bitstring(after_bits, VERTEX_COUNT);

    let flipped = (0..VERTEX_COUNT)
        .flat_map(|u| ((u + 1)..VERTEX_COUNT).map(move |v| (u, v)))
        .filter(|&(u, v)| before.adjacency[u].get(v) != after.adjacency[u].get(v))
        .count();
    assert!(flipped > 0, "{label}: fixtures are identical, nothing is being tested");

    let mut carried = full_table(&mut before);
    let (moved, invalidated) = carried.carry_forward(&before, &after, CLIQUE_SIZE);
    // Entries are now DERIVED rather than discarded: only the flipped edges themselves cannot be,
    // so invalidation should equal the number of flips and everything else is corrected in place.
    assert_eq!(
        invalidated, flipped,
        "{label}: only the flipped edges themselves should be invalidated"
    );
    assert!(
        moved > 0 && moved < VERTEX_COUNT * (VERTEX_COUNT - 1) / 2,
        "{label}: carry moved {moved} entries — all or nothing means the delta is not doing its job"
    );

    let mut fresh = HoistTables::new(VERTEX_COUNT);
    let mut mismatches = 0;
    for u in 0..VERTEX_COUNT {
        for v in (u + 1)..VERTEX_COUNT {
            let want = fresh.single_created(&mut after, CLIQUE_SIZE, u, v);
            let got = carried.single_created(&mut after, CLIQUE_SIZE, u, v);
            if want != got {
                if mismatches < 5 {
                    eprintln!("{label}: edge ({u},{v}) carried={got} rebuilt={want}");
                }
                mismatches += 1;
            }
        }
    }
    assert_eq!(
        mismatches, 0,
        "{label}: {mismatches} carried entries disagree with a fresh rebuild ({flipped} edges \
         flipped, {moved} entries derived)"
    );
}

#[test]
#[ignore]
fn carry_survives_a_real_single_edge_stage_advance() {
    let g = bases();
    assert_carry_matches_rebuild(&g[0], &g[1], "993915 -> 993916 (single)");
}

#[test]
#[ignore]
fn carry_survives_a_real_pair_edge_stage_advance() {
    let g = bases();
    assert_carry_matches_rebuild(&g[1], &g[2], "993916 -> 993917 (pair)");
}

/// The queue manager only ever advances one stage at a time, but a worker that misses an
/// announcement can carry across two advances at once. The predicate must hold for that too.
#[test]
#[ignore]
fn carry_survives_two_stage_advances_at_once() {
    let g = bases();
    assert_carry_matches_rebuild(&g[0], &g[2], "993915 -> 993917 (skipped a stage)");
}
