//! The single-edge fill path must agree with the bitset recursion it replaces, exactly.
//!
//! `single_created` — the per-edge hoist table fill — still ran the full-width 5-word recursion
//! after the pair correction was compressed. Profiling a GPU-assisted worker put it and its
//! descendants at ~42% of CPU time, the largest remaining item once the correction moved to the GPU.
//!
//! Seeding on ONE edge leaves |P| ~ 70, too wide for a u64, so the compression can only engage a
//! level deeper — which makes the equivalence check matter more, not less.
//!
//!   cargo test --release --test compressed_single -- --ignored

use ramsey_worker_rust::algorithm::{get_new_cliques_with_limit, get_new_cliques_with_limit_reference};
use ramsey_worker_rust::graph::{Graph, WorkUnitEdge};

const FIXTURE: &str = include_str!("fixtures/campaign3-consecutive-bases.txt");
const V: usize = 282;
const K: usize = 8;

fn bases() -> Vec<String> {
    FIXTURE.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
}

fn check(bits: &str, label: &str) {
    let mut g = Graph::from_bitstring(bits, V);
    let mut checked = 0usize;
    let mut nonzero = 0usize;
    let mut limited = 0usize;

    for u in 0..V {
        for v in (u + 1)..V {
            if (u * V + v) % 37 != 0 {
                continue; // a spread across the whole edge set without running all 39,621
            }
            let e = [WorkUnitEdge { vertex_one: u as u16, vertex_two: v as u16 }];
            // Unlimited: what single_created actually uses.
            let (got, ge) = get_new_cliques_with_limit(&mut g, K, &e, i32::MAX);
            let (want, we) = get_new_cliques_with_limit_reference(&mut g, K, &e, i32::MAX);
            assert_eq!((got, ge), (want, we), "{label}: edge ({u},{v}) unlimited");
            if want != 0 { nonzero += 1; }

            // And with a live limit, where the early-exit semantics have to match too — including
            // the `exceeded` flag, not just the count.
            for lim in [0i32, want / 2, want.saturating_sub(1), want] {
                let (got, ge) = get_new_cliques_with_limit(&mut g, K, &e, lim);
                let (want2, we) = get_new_cliques_with_limit_reference(&mut g, K, &e, lim);
                assert_eq!(ge, we, "{label}: edge ({u},{v}) limit={lim}: exceeded flag differs");
                if !ge {
                    assert_eq!(got, want2, "{label}: edge ({u},{v}) limit={lim}: count differs");
                }
                if ge { limited += 1; }
            }
            checked += 1;
        }
    }
    assert!(checked > 800, "{label}: only {checked} edges checked");
    assert!(nonzero > 400, "{label}: {nonzero} non-zero results — mostly trivial");
    assert!(limited > 400, "{label}: {limited} limit-exceeded cases — the early exit is untested");
}

#[test]
#[ignore]
fn compressed_single_matches_the_bitset_path() {
    check(&bases()[0], "graph 993915");
}

#[test]
#[ignore]
fn compressed_single_matches_on_a_second_graph() {
    check(&bases()[2], "graph 993917");
}
