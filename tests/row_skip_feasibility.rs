//! Can a whole red row be rejected without evaluating it?
//!
//! A unit is worth evaluating only if `created <= early_limit`, where
//! `early_limit = (threshold - 1) - base_total + broken`. For Mixed and AllRed pairs
//! `created >= D_r`, and `broken(r,b) <= brk(r) + brk(b)`, so the entire row of ~19,800 units is
//! rejectable when
//!
//!     D_r > (threshold - 1) - base_total + brk(r) + max_blue_brk
//!
//! That bound is conservative — `max_blue_brk` is a global maximum — so whether it ever fires is an
//! empirical question, and the answer decides whether the optimisation is worth building at all.
//!
//!   cargo test --release --test row_skip_feasibility -- --ignored --nocapture

use ramsey_worker_rust::algorithm::get_all_cliques;
use ramsey_worker_rust::clique_collection::CliqueCollection;
use ramsey_worker_rust::graph::{Graph, WorkUnitEdge};
use ramsey_worker_rust::hoist::HoistTables;

const FIXTURE: &str = include_str!("fixtures/campaign3-consecutive-bases.txt");
const V: usize = 282;
const K: usize = 8;

#[test]
#[ignore]
fn how_many_red_rows_are_skippable() {
    // Prefer a LIVE campaign graph if one has been exported to /tmp/live_graph.txt: its
    // clique_count sits at the threshold, which is the regime that matters. The fixture is from an
    // older era and its ~750-clique head start makes the bound far more generous than production.
    let live = std::fs::read_to_string("/tmp/live_graph.txt").ok();
    let bits: &str = match live.as_deref() {
        Some(b) if b.trim().len() == V * (V - 1) / 2 => { eprintln!("using LIVE campaign graph"); b.trim() }
        _ => { eprintln!("using fixture graph"); FIXTURE.lines().next().unwrap().trim() }
    };
    let mut g = Graph::from_bitstring(bits, V);

    let t0 = std::time::Instant::now();
    let cliques = get_all_cliques(&mut g, K);
    let mut cc = CliqueCollection::new(V);
    cc.set_cliques(cliques, V);
    let base_total = cc.total() as i32;
    eprintln!("base_total = {base_total} ({:.1}s to enumerate)", t0.elapsed().as_secs_f64());

    let mut reds = Vec::new();
    let mut blues = Vec::new();
    for i in 0..V {
        for j in (i + 1)..V {
            if g.adjacency[i].get(j) { reds.push((i, j)) } else { blues.push((i, j)) }
        }
    }

    let brk = |e: (usize, usize)| {
        cc.get_count_of_cliques_containing_edges(&[WorkUnitEdge {
            vertex_one: e.0 as u16,
            vertex_two: e.1 as u16,
        }])
    };

    let blue_brks: Vec<i32> = blues.iter().map(|&b| brk(b)).collect();
    let max_blue_brk = *blue_brks.iter().max().unwrap();
    let mean_blue_brk = blue_brks.iter().map(|&x| x as f64).sum::<f64>() / blue_brks.len() as f64;
    eprintln!("blue brk: mean {mean_blue_brk:.0}, max {max_blue_brk}");

    // The live threshold is the campaign's best; base_total here is this graph's own count.
    let threshold: i32 = std::fs::read_to_string("/tmp/live_threshold.txt")
        .ok().and_then(|v| v.trim().parse().ok()).unwrap_or(743_716);
    eprintln!("threshold {threshold}, base_total {base_total} -> slack {}", threshold - 1 - base_total);

    let mut tables = HoistTables::new(V);
    let mut skippable = 0usize;
    let mut margins: Vec<i32> = Vec::new();
    for &r in reds.iter() {
        let d_r = tables.single_created(&mut g, K, r.0, r.1);
        let bound = (threshold - 1) - base_total + brk(r) + max_blue_brk;
        margins.push(d_r - bound);
        if d_r > bound { skippable += 1; }
    }
    margins.sort_unstable();
    let pct = 100.0 * skippable as f64 / reds.len() as f64;
    eprintln!("\nSKIPPABLE ROWS: {skippable} of {} ({pct:.1}%)", reds.len());
    eprintln!("  margin (D_r - bound) percentiles:");
    for (label, q) in [("p10", 0.10), ("p50", 0.50), ("p90", 0.90), ("max", 0.999)] {
        eprintln!("    {label}: {}", margins[(q * (margins.len() - 1) as f64) as usize]);
    }
    eprintln!("\n  (positive margin = row provably rejectable without evaluating it)");
}
