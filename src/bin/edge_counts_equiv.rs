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

    // Optional 4th arg: the NEXT stage's bitstring. Consecutive stages differ by one flip, so the
    // counts can be updated incrementally instead of rebuilt. Prove that lands on exactly the same
    // numbers as a full rebuild of the next graph, and show what it costs.
    if let Some(next_path) = args.get(4) {
        let next_bits = std::fs::read_to_string(next_path).expect("read next bitstring").trim().to_string();
        let diff: Vec<usize> = bits
            .chars()
            .zip(next_bits.chars())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect();
        println!("\nnext graph differs in {} bit(s): {:?}", diff.len(), &diff[..diff.len().min(4)]);

        // bit index -> (u,v) in the same row-major i<j order Graph::from_bitstring uses
        let to_pair = |index: usize| -> (usize, usize) {
            let mut idx = 0;
            for i in 0..vertex_count {
                for j in (i + 1)..vertex_count {
                    if idx == index {
                        return (i, j);
                    }
                    idx += 1;
                }
            }
            panic!("bit index {index} out of range");
        };

        let mut inc_graph = Graph::from_bitstring(&bits, vertex_count);
        let mut inc = CliqueCollection::new(vertex_count);
        inc.build_counts_only(&mut inc_graph, clique_size);

        let t2 = Instant::now();
        for &bit in &diff {
            let (u, v) = to_pair(bit);
            inc.apply_edge_flip(&mut inc_graph, clique_size, u, v);
        }
        let t_incremental = t2.elapsed();

        let mut full_graph = Graph::from_bitstring(&next_bits, vertex_count);
        let mut full = CliqueCollection::new(vertex_count);
        let t3 = Instant::now();
        full.build_counts_only(&mut full_graph, clique_size);
        let t_full = t3.elapsed();

        println!("INCREMENTAL update = {:?}   FULL rebuild = {:?}", t_incremental, t_full);
        println!(
            "speedup: {:.0}x",
            t_full.as_secs_f64() / t_incremental.as_secs_f64().max(1e-9)
        );
        assert_eq!(inc.total(), full.total(), "incremental total mismatch");
        assert_eq!(inc.edge_counts(), full.edge_counts(), "incremental per-edge counts mismatch");
        assert_eq!(inc_graph.to_bitstring(), next_bits, "graph not advanced to the next state");
        println!("EQUIVALENT: incremental == full rebuild (total={})", inc.total());
    }
}
