// One-off analysis binary: drop-6-vertex sampling at N=282 from a 288v base graph.
//
// Usage: vreduce_analysis <source_graph_id> <edge_data_file> <output_sql_file> [<sample_count>]
//
// Reads the bitstring from <edge_data_file>, performs the greedy-then-sample
// vertex-removal strategy, computes clique counts for each 282v subgraph and its
// rebalanced variant, and writes INSERT statements for `graph_vreduce` to
// <output_sql_file>.

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;

use ramsey_worker_rust::algorithm::{get_all_cliques, get_cliques_comprehensive};
use ramsey_worker_rust::graph::{Graph, WorkUnitEdge};

const SRC_VERTEX_COUNT: usize = 288;
const SUB_VERTEX_COUNT: usize = 282;
const CLIQUE_SIZE: usize = 8;
const TOP_VERTEX_POOL: usize = 30;
const REMOVE_COUNT: usize = 6;

fn edge_index(i: usize, j: usize, n: usize) -> usize {
    debug_assert!(i < j && j < n);
    i * (n - 1) - i * (i - 1) / 2 + (j - i - 1)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "Usage: {} <source_graph_id> <edge_data_file> <output_sql_file> [sample_count]",
            args[0]
        );
        std::process::exit(1);
    }
    let source_id: i32 = args[1].parse().expect("arg 1 must be an integer graph id");
    let edge_file = &args[2];
    let out_file = &args[3];
    let sample_count: usize = args.get(4).map(|s| s.parse().unwrap()).unwrap_or(100);

    let edge_data = fs::read_to_string(edge_file)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", edge_file, e))
        .trim()
        .to_string();
    let expected_len = SRC_VERTEX_COUNT * (SRC_VERTEX_COUNT - 1) / 2;
    assert_eq!(
        edge_data.len(),
        expected_len,
        "expected edge_data length {} for {}v, got {}",
        expected_len,
        SRC_VERTEX_COUNT,
        edge_data.len()
    );

    let base_red = edge_data.chars().filter(|&c| c == '1').count();
    let base_blue = edge_data.len() - base_red;
    eprintln!(
        "Source graph: id={}, vertices={}, red={}, blue={}",
        source_id, SRC_VERTEX_COUNT, base_red, base_blue
    );

    let mut base_graph = Graph::from_bitstring(&edge_data, SRC_VERTEX_COUNT);

    eprintln!("Enumerating 8-cliques in source graph...");
    let t_base = std::time::Instant::now();
    let base_cliques = get_all_cliques(&mut base_graph, CLIQUE_SIZE);
    eprintln!(
        "  {} cliques in {:.1}s",
        base_cliques.len(),
        t_base.elapsed().as_secs_f64()
    );

    // Per-vertex 8-clique participation across both colors.
    let mut vertex_participation = vec![0u32; SRC_VERTEX_COUNT];
    for clique in &base_cliques {
        for &v in clique {
            vertex_participation[v] += 1;
        }
    }

    let mut indexed: Vec<(usize, u32)> = vertex_participation
        .iter()
        .enumerate()
        .map(|(i, &p)| (i, p))
        .collect();
    indexed.sort_by(|a, b| b.1.cmp(&a.1));

    eprintln!(
        "Top {} vertices by 8-clique participation:",
        TOP_VERTEX_POOL
    );
    for (rank, (v, p)) in indexed.iter().take(TOP_VERTEX_POOL).enumerate() {
        eprintln!("  {:2}: vertex={:3} participation={}", rank + 1, v, p);
    }

    let pool: Vec<usize> = indexed
        .iter()
        .take(TOP_VERTEX_POOL)
        .map(|(v, _)| *v)
        .collect();

    // Sample <sample_count> distinct 6-subsets from the pool.
    let mut samples_set: HashSet<Vec<usize>> = HashSet::new();
    while samples_set.len() < sample_count {
        let mut picked: HashSet<usize> = HashSet::new();
        while picked.len() < REMOVE_COUNT {
            picked.insert(rand::random::<u64>() as usize % pool.len());
        }
        let mut sample: Vec<usize> = picked.iter().map(|&i| pool[i]).collect();
        sample.sort();
        samples_set.insert(sample);
    }
    let samples: Vec<Vec<usize>> = samples_set.into_iter().collect();
    eprintln!(
        "Generated {} distinct {}-vertex subsets from top-{} pool.",
        samples.len(),
        REMOVE_COUNT,
        TOP_VERTEX_POOL
    );

    // Free up the large base clique list; we don't need it per-sample.
    drop(base_cliques);

    let mut out = fs::File::create(out_file).expect("create output sql file");
    writeln!(out, "USE `ramsey-dev`;").unwrap();
    writeln!(
        out,
        "-- vreduce_analysis: source_graph_id={}, samples={}, generated={}",
        source_id,
        samples.len(),
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    )
    .unwrap();

    let mut summary: Vec<(Vec<usize>, i32, i32, usize)> = Vec::with_capacity(samples.len());

    for (idx, removed) in samples.iter().enumerate() {
        let t_s = std::time::Instant::now();

        let removed_set: HashSet<usize> = removed.iter().copied().collect();
        let kept: Vec<usize> = (0..SRC_VERTEX_COUNT)
            .filter(|v| !removed_set.contains(v))
            .collect();
        assert_eq!(kept.len(), SUB_VERTEX_COUNT);

        // Build the induced 282v graph.
        let mut sub_graph = Graph::new(SUB_VERTEX_COUNT);
        for ni in 0..SUB_VERTEX_COUNT {
            let oi = kept[ni];
            for nj in (ni + 1)..SUB_VERTEX_COUNT {
                let oj = kept[nj];
                if base_graph.adjacency[oi].get(oj) {
                    sub_graph.adjacency[ni].set(nj);
                    sub_graph.adjacency[nj].set(ni);
                }
            }
        }
        // adjacency was mutated directly, so the complement must be resynced
        // before any clique counting (which inverts the graph).
        sub_graph.resync_complement();

        let raw_bitstring = sub_graph.to_bitstring();
        let raw_red = raw_bitstring.chars().filter(|&c| c == '1').count();
        let raw_blue = raw_bitstring.len() - raw_red;

        let raw_cc = get_cliques_comprehensive(&mut sub_graph, CLIQUE_SIZE);

        let removed_str = removed
            .iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(",");

        writeln!(out,
            "INSERT INTO `graph_vreduce` (clique_count, subgraph_size, vertex_count, identified_date, edge_data, source_graph_id, removed_vertices, rebalanced, red_edge_count, blue_edge_count, rebalance_flips) \
             VALUES ({cc}, 8, 282, NOW(6), '{ed}', {src}, '{rv}', 0, {r}, {b}, NULL);",
            cc = raw_cc, ed = raw_bitstring, src = source_id,
            rv = removed_str, r = raw_red, b = raw_blue
        ).unwrap();

        // Rebalance: enumerate cliques once in 282v graph, score every edge, flip lowest
        // majority-color edges until balance (or off-by-one for odd totals).
        let diff: i64 = raw_red as i64 - raw_blue as i64;
        let num_flips = (diff.abs() / 2) as usize;

        let (reb_cc, reb_bitstring, reb_red, reb_blue, flips) = if num_flips == 0 {
            (raw_cc, raw_bitstring.clone(), raw_red, raw_blue, Vec::new())
        } else {
            let sub_cliques = get_all_cliques(&mut sub_graph, CLIQUE_SIZE);
            let edge_slots = SUB_VERTEX_COUNT * (SUB_VERTEX_COUNT - 1) / 2;
            let mut edge_part = vec![0u32; edge_slots];
            for clique in &sub_cliques {
                // clique is sorted ascending (from BitMatrix::to_indices).
                for i in 0..clique.len() {
                    for j in (i + 1)..clique.len() {
                        edge_part[edge_index(clique[i], clique[j], SUB_VERTEX_COUNT)] += 1;
                    }
                }
            }
            drop(sub_cliques);

            let majority_red = diff > 0;
            let mut candidates: Vec<(usize, usize, u32)> =
                Vec::with_capacity(if majority_red { raw_red } else { raw_blue });
            for i in 0..SUB_VERTEX_COUNT {
                for j in (i + 1)..SUB_VERTEX_COUNT {
                    let is_red = sub_graph.adjacency[i].get(j);
                    if (majority_red && is_red) || (!majority_red && !is_red) {
                        let p = edge_part[edge_index(i, j, SUB_VERTEX_COUNT)];
                        candidates.push((i, j, p));
                    }
                }
            }
            candidates.sort_by_key(|&(_, _, p)| p);

            let flip_edges: Vec<(usize, usize)> = candidates
                .iter()
                .take(num_flips)
                .map(|&(i, j, _)| (i, j))
                .collect();

            let wu_edges: Vec<WorkUnitEdge> = flip_edges
                .iter()
                .map(|&(i, j)| WorkUnitEdge {
                    vertex_one: i as u16,
                    vertex_two: j as u16,
                })
                .collect();
            sub_graph.flip_edges(&wu_edges);

            let reb_cc = get_cliques_comprehensive(&mut sub_graph, CLIQUE_SIZE);
            let reb_bitstring = sub_graph.to_bitstring();
            let reb_red = reb_bitstring.chars().filter(|&c| c == '1').count();
            let reb_blue = reb_bitstring.len() - reb_red;

            (reb_cc, reb_bitstring, reb_red, reb_blue, flip_edges)
        };

        let flips_sql = if flips.is_empty() {
            "NULL".to_string()
        } else {
            let parts: Vec<String> = flips
                .iter()
                .map(|(i, j)| format!("({},{})", i, j))
                .collect();
            format!("'{}'", parts.join(";"))
        };

        writeln!(out,
            "INSERT INTO `graph_vreduce` (clique_count, subgraph_size, vertex_count, identified_date, edge_data, source_graph_id, removed_vertices, rebalanced, red_edge_count, blue_edge_count, rebalance_flips) \
             VALUES ({cc}, 8, 282, NOW(6), '{ed}', {src}, '{rv}', 1, {r}, {b}, {f});",
            cc = reb_cc, ed = reb_bitstring, src = source_id,
            rv = removed_str, r = reb_red, b = reb_blue, f = flips_sql
        ).unwrap();

        let dt = t_s.elapsed().as_secs_f64();
        eprintln!(
            "Sample {:3}/{}: removed={:?} raw_cc={} reb_cc={} (Δ={:+}) flips={} elapsed={:.1}s",
            idx + 1,
            samples.len(),
            removed,
            raw_cc,
            reb_cc,
            reb_cc - raw_cc,
            flips.len(),
            dt
        );

        summary.push((removed.clone(), raw_cc, reb_cc, flips.len()));
    }

    // Summary (by raw_cc ascending).
    summary.sort_by_key(|s| s.1);
    eprintln!("\n=== Summary (best 10 by raw_cc) ===");
    for (removed, raw_cc, reb_cc, nflips) in summary.iter().take(10) {
        eprintln!(
            "  removed={:?} raw_cc={} reb_cc={} flips={}",
            removed, raw_cc, reb_cc, nflips
        );
    }
    let raw_vals: Vec<i32> = summary.iter().map(|s| s.1).collect();
    let reb_vals: Vec<i32> = summary.iter().map(|s| s.2).collect();
    let min_raw = *raw_vals.iter().min().unwrap();
    let max_raw = *raw_vals.iter().max().unwrap();
    let avg_raw = raw_vals.iter().sum::<i32>() as f64 / raw_vals.len() as f64;
    let min_reb = *reb_vals.iter().min().unwrap();
    let max_reb = *reb_vals.iter().max().unwrap();
    let avg_reb = reb_vals.iter().sum::<i32>() as f64 / reb_vals.len() as f64;
    eprintln!(
        "\nRaw   cc: min={} avg={:.0} max={}",
        min_raw, avg_raw, max_raw
    );
    eprintln!(
        "Reb   cc: min={} avg={:.0} max={}",
        min_reb, avg_reb, max_reb
    );
    eprintln!("\nSQL written to {}", out_file);
}
