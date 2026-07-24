//! Equivalence + timing harness for the counts-only per-edge clique cardinality build.
//!
//! The counter-based worker path only reads the per-edge counts and the clique total, so the
//! build was changed from "collect every clique, then index it" to a streaming counts-only
//! traversal (and the result is shared between workers via Redis). Those counts drive
//! enumeration ORDER, so they must be bit-identical to the old path or replays diverge.
//!
//! This replays both builds over a real production bitstring and asserts equality, printing
//! the wall-clock cost of each (the "new graph tax" every worker pays per stage).
//!
//! Usage: edge_counts_equiv <bitstring-file> [vertex_count] [clique_size]

use ramsey_worker_rust::algorithm::get_all_cliques;
use ramsey_worker_rust::clique_collection::CliqueCollection;
use ramsey_worker_rust::graph::Graph;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = args.get(1).expect("usage: edge_counts_equiv <bitstring-file> [vc] [k]");
    let vertex_count: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(282);
    let clique_size: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(8);

    let bits = std::fs::read_to_string(path).expect("read bitstring").trim().to_string();
    println!(
        "graph: {} bits, vertex_count={}, clique_size={}",
        bits.len(),
        vertex_count,
        clique_size
    );

    // OLD path: materialize every clique, then build the edge->cliques index.
    let mut g_old = Graph::from_bitstring(&bits, vertex_count);
    let t0 = Instant::now();
    let all = get_all_cliques(&mut g_old, clique_size);
    let collected = all.len();
    let t_collect = t0.elapsed();
    let mut old = CliqueCollection::new(vertex_count);
    old.set_cliques(all, vertex_count);
    let t_old = t0.elapsed();

    // NEW path: streaming counts-only.
    let mut g_new = Graph::from_bitstring(&bits, vertex_count);
    let t1 = Instant::now();
    let mut new = CliqueCollection::new(vertex_count);
    new.build_counts_only(&mut g_new, clique_size);
    let t_new = t1.elapsed();

    println!("cliques: {collected}");
    println!(
        "OLD  get_all_cliques {:?} + set_cliques {:?} = {:?}",
        t_collect,
        t_old - t_collect,
        t_old
    );
    println!("NEW  build_counts_only = {:?}", t_new);
    println!(
        "speedup: {:.2}x",
        t_old.as_secs_f64() / t_new.as_secs_f64().max(1e-9)
    );

    assert_eq!(new.total(), old.total(), "clique total mismatch");
    assert_eq!(new.edge_counts(), old.edge_counts(), "per-edge counts mismatch");
    println!("EQUIVALENT: total={} and all {} edge counts identical", new.total(), new.edge_counts().len());
}
