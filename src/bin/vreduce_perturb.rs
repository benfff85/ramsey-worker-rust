// One-off analysis binary: perturb a 282v base graph to explore the balanced
// neighborhood, looking for variants with lower 8-clique counts.
//
// Usage: vreduce_perturb <lineage_source_id> <lineage_removed_csv> <edge_data_file> <output_sql_file> [<count>]
//
// Reads the 282v bitstring from <edge_data_file>, enumerates 8-cliques, scores
// edges by participation and by common-red-neighbor count, then generates <count>
// balanced perturbations using five strategies:
//   1. RANDOM           - majority-color edges picked uniformly at random
//   2. MAX_DESTROY      - majority edges sampled from top-K by clique participation
//   3. MIN_CREATE       - majority edges sampled from bottom-K by common-neighbor count
//   4. NET_SCORE        - majority edges sampled from top-K by (part - common_nbrs)
//   5. NET_SCORE+SWAPS  - NET_SCORE + K additional red/blue pair-swaps
//
// Every variant rebalances to |red - blue| = 1 (closest achievable at 282v,
// since C(282,2) = 39621 is odd).

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;

use rand::{Rng, RngExt, SeedableRng};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;

use ramsey_worker_rust::algorithm::{get_all_cliques, get_cliques_comprehensive};
use ramsey_worker_rust::graph::{Graph, WorkUnitEdge};

const VERTEX_COUNT: usize = 282;
const CLIQUE_SIZE: usize = 8;
const TOP_K: usize = 200;

fn edge_index(i: usize, j: usize, n: usize) -> usize {
    debug_assert!(i < j && j < n);
    i * (n - 1) - i * (i - 1) / 2 + (j - i - 1)
}

fn common_red_neighbors(graph: &Graph, u: usize, v: usize) -> u32 {
    let mut inter = graph.adjacency[u];
    inter.and_assign(&graph.adjacency[v]);
    inter.cardinality() as u32
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 5 {
        eprintln!(
            "Usage: {} <lineage_source_id> <lineage_removed_csv> <edge_data_file> <output_sql_file> [count]",
            args[0]
        );
        std::process::exit(1);
    }
    let lineage_source_id: i32 = args[1].parse().expect("arg 1: source graph id");
    let lineage_removed_csv = args[2].clone();
    let edge_file = &args[3];
    let out_file = &args[4];
    let count: usize = args.get(5).map(|s| s.parse().unwrap()).unwrap_or(1000);
    let only_strategy: Option<usize> = args.get(6).and_then(|s| s.parse().ok());

    let edge_data = fs::read_to_string(edge_file)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", edge_file, e))
        .trim()
        .to_string();
    let expected_len = VERTEX_COUNT * (VERTEX_COUNT - 1) / 2;
    assert_eq!(
        edge_data.len(),
        expected_len,
        "expected {} chars for {}v, got {}",
        expected_len,
        VERTEX_COUNT,
        edge_data.len()
    );

    let base_red = edge_data.chars().filter(|&c| c == '1').count();
    let base_blue = edge_data.len() - base_red;
    eprintln!(
        "Source: lineage_src={}, removed=[{}], vertices={}, red={}, blue={}",
        lineage_source_id, lineage_removed_csv, VERTEX_COUNT, base_red, base_blue
    );

    let mut base_graph = Graph::from_bitstring(&edge_data, VERTEX_COUNT);

    eprintln!("Enumerating 8-cliques in source graph...");
    let t0 = std::time::Instant::now();
    let base_cliques = get_all_cliques(&mut base_graph, CLIQUE_SIZE);
    let base_cc = base_cliques.len() as i32;
    eprintln!("  {} cliques in {:.1}s", base_cc, t0.elapsed().as_secs_f64());

    // Per-edge participation (counts cliques of both colors).
    let edge_slots = VERTEX_COUNT * (VERTEX_COUNT - 1) / 2;
    let mut edge_part = vec![0u32; edge_slots];
    for clique in &base_cliques {
        for i in 0..clique.len() {
            for j in (i + 1)..clique.len() {
                edge_part[edge_index(clique[i], clique[j], VERTEX_COUNT)] += 1;
            }
        }
    }
    drop(base_cliques);

    // Partition edges into red (adjacent) and blue (non-adjacent) pools.
    // Each entry: (u, v, participation, common_red_neighbors).
    let mut red_edges: Vec<(usize, usize, u32, u32)> = Vec::new();
    let mut blue_edges: Vec<(usize, usize, u32, u32)> = Vec::new();
    for i in 0..VERTEX_COUNT {
        for j in (i + 1)..VERTEX_COUNT {
            let p = edge_part[edge_index(i, j, VERTEX_COUNT)];
            let cn = common_red_neighbors(&base_graph, i, j);
            if base_graph.adjacency[i].get(j) {
                red_edges.push((i, j, p, cn));
            } else {
                blue_edges.push((i, j, p, cn));
            }
        }
    }

    let diff: i64 = base_red as i64 - base_blue as i64;
    let majority_blue = diff < 0;
    // Rebalance to |new_diff| = 1 (best achievable for odd total).
    let num_rebalance = ((diff.abs() - 1) / 2) as usize;
    eprintln!(
        "Rebalance target: flip {} {} edges (base diff={}, target |diff|=1)",
        num_rebalance,
        if majority_blue { "blue->red" } else { "red->blue" },
        diff
    );

    let (majority_pool_full, minority_pool_full) = if majority_blue {
        (blue_edges, red_edges)
    } else {
        (red_edges, blue_edges)
    };

    // Pre-sorted pools for each strategy.
    let mut by_part_desc = majority_pool_full.clone();
    by_part_desc.sort_by(|a, b| b.2.cmp(&a.2));
    let mut by_common_asc = majority_pool_full.clone();
    by_common_asc.sort_by_key(|&(_, _, _, cn)| cn);
    let mut by_net_desc = majority_pool_full.clone();
    by_net_desc.sort_by(|a, b| {
        let na = a.2 as i64 - a.3 as i64;
        let nb = b.2 as i64 - b.3 as i64;
        nb.cmp(&na)
    });

    let mut minority_by_part_asc = minority_pool_full.clone();
    minority_by_part_asc.sort_by_key(|&(_, _, p, _)| p);

    eprintln!(
        "Pool sizes: majority={}, minority={}",
        majority_pool_full.len(),
        minority_pool_full.len()
    );

    let mut rng = StdRng::from_rng(&mut rand::rng());
    let mut seen: HashSet<Vec<(usize, usize)>> = HashSet::new();
    let mut out = fs::File::create(out_file).expect("create output sql file");
    writeln!(out, "USE `ramsey-dev`;").unwrap();
    writeln!(
        out,
        "-- vreduce_perturb: lineage_src={}, removed=[{}], base_cc={}, count={}, generated={}",
        lineage_source_id,
        lineage_removed_csv,
        base_cc,
        count,
        chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
    )
    .unwrap();

    let strategy_names = ["RANDOM", "MAX_DESTROY", "MIN_CREATE", "NET_SCORE", "NET_SCORE+SWAPS"];
    let strategies_to_run: Vec<usize> = match only_strategy {
        Some(s) if s < 5 => vec![s],
        _ => (0..5).collect(),
    };
    let per_strategy = count / strategies_to_run.len();
    eprintln!(
        "Strategies to run: {:?} ({} variants each)",
        strategies_to_run
            .iter()
            .map(|&s| strategy_names[s])
            .collect::<Vec<_>>(),
        per_strategy
    );

    let mut summary: Vec<(String, i32, usize)> = Vec::new();

    for &strategy in &strategies_to_run {
        eprintln!("\n--- Strategy {}: {} ---", strategy, strategy_names[strategy]);
        let t_strat = std::time::Instant::now();
        let mut generated = 0usize;
        let mut attempts = 0usize;

        while generated < per_strategy && attempts < per_strategy * 10 {
            attempts += 1;

            // Rebalance-phase flips.
            let rebalance: Vec<(usize, usize)> = match strategy {
                0 => {
                    let mut indices: Vec<usize> = (0..majority_pool_full.len()).collect();
                    indices.shuffle(&mut rng);
                    indices
                        .iter()
                        .take(num_rebalance)
                        .map(|&i| (majority_pool_full[i].0, majority_pool_full[i].1))
                        .collect()
                }
                1 => {
                    let k = TOP_K.min(by_part_desc.len());
                    let mut indices: Vec<usize> = (0..k).collect();
                    indices.shuffle(&mut rng);
                    indices
                        .iter()
                        .take(num_rebalance)
                        .map(|&i| (by_part_desc[i].0, by_part_desc[i].1))
                        .collect()
                }
                2 => {
                    let k = TOP_K.min(by_common_asc.len());
                    let mut indices: Vec<usize> = (0..k).collect();
                    indices.shuffle(&mut rng);
                    indices
                        .iter()
                        .take(num_rebalance)
                        .map(|&i| (by_common_asc[i].0, by_common_asc[i].1))
                        .collect()
                }
                3 | 4 => {
                    let k = TOP_K.min(by_net_desc.len());
                    let mut indices: Vec<usize> = (0..k).collect();
                    indices.shuffle(&mut rng);
                    indices
                        .iter()
                        .take(num_rebalance)
                        .map(|&i| (by_net_desc[i].0, by_net_desc[i].1))
                        .collect()
                }
                _ => unreachable!(),
            };

            let mut flips = rebalance.clone();

            // Pair-swap phase for strategy 4: add 1 or 2 pair-swaps.
            if strategy == 4 {
                let n_pairs = 1 + rng.random_range(0..2); // 1 or 2
                let mut used: HashSet<(usize, usize)> = flips.iter().copied().collect();
                for _ in 0..n_pairs {
                    // Minority edge from low-participation end (destroys few).
                    let mk = TOP_K.min(minority_by_part_asc.len());
                    let mut mi = rng.random_range(0..mk);
                    let mut attempts2 = 0;
                    while used.contains(&(minority_by_part_asc[mi].0, minority_by_part_asc[mi].1))
                        && attempts2 < 50
                    {
                        mi = rng.random_range(0..mk);
                        attempts2 += 1;
                    }
                    let min_edge = (minority_by_part_asc[mi].0, minority_by_part_asc[mi].1);
                    used.insert(min_edge);
                    flips.push(min_edge);

                    // Another majority edge by net score, not already chosen.
                    let k = TOP_K.min(by_net_desc.len());
                    let mut mj = rng.random_range(0..k);
                    let mut attempts3 = 0;
                    while used.contains(&(by_net_desc[mj].0, by_net_desc[mj].1)) && attempts3 < 50 {
                        mj = rng.random_range(0..k);
                        attempts3 += 1;
                    }
                    let maj_edge = (by_net_desc[mj].0, by_net_desc[mj].1);
                    used.insert(maj_edge);
                    flips.push(maj_edge);
                }
            }

            let mut key: Vec<(usize, usize)> = flips.clone();
            key.sort();
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);

            let wu_edges: Vec<WorkUnitEdge> = flips
                .iter()
                .map(|&(i, j)| WorkUnitEdge {
                    vertex_one: i as u16,
                    vertex_two: j as u16,
                })
                .collect();
            base_graph.flip_edges(&wu_edges);

            let cc = get_cliques_comprehensive(&mut base_graph, CLIQUE_SIZE);
            let new_bitstring = base_graph.to_bitstring();
            let new_red = new_bitstring.chars().filter(|&c| c == '1').count();
            let new_blue = new_bitstring.len() - new_red;

            base_graph.flip_edges(&wu_edges); // restore

            let flip_str = flips
                .iter()
                .map(|(i, j)| format!("({},{})", i, j))
                .collect::<Vec<_>>()
                .join(";");
            let strategy_tag = format!("{}:{}", strategy, strategy_names[strategy]);

            writeln!(out,
                "INSERT INTO `graph_vreduce` (clique_count, subgraph_size, vertex_count, identified_date, edge_data, source_graph_id, removed_vertices, rebalanced, red_edge_count, blue_edge_count, rebalance_flips) \
                 VALUES ({cc}, 8, 282, NOW(6), '{ed}', {src}, '{rv}', 2, {r}, {b}, '{tag}|{f}');",
                cc = cc, ed = new_bitstring, src = lineage_source_id,
                rv = lineage_removed_csv, r = new_red, b = new_blue,
                tag = strategy_tag, f = flip_str
            ).unwrap();

            summary.push((strategy_names[strategy].to_string(), cc, flips.len()));
            generated += 1;

            if generated % 25 == 0 {
                eprintln!(
                    "  [{}] {}/{} generated (attempts={}, latest_cc={})",
                    strategy_names[strategy], generated, per_strategy, attempts, cc
                );
            }
        }

        eprintln!(
            "  Strategy {} done in {:.1}s ({} generated, {} attempts)",
            strategy_names[strategy],
            t_strat.elapsed().as_secs_f64(),
            generated,
            attempts
        );
    }

    // Per-strategy summary.
    eprintln!("\n=== Summary ===");
    eprintln!("Base CC (graph 53): {}", base_cc);
    for name in strategy_names.iter() {
        let vals: Vec<i32> = summary
            .iter()
            .filter(|(n, _, _)| n == name)
            .map(|(_, cc, _)| *cc)
            .collect();
        if vals.is_empty() {
            continue;
        }
        let min = *vals.iter().min().unwrap();
        let max = *vals.iter().max().unwrap();
        let avg = vals.iter().sum::<i32>() as f64 / vals.len() as f64;
        let improved = vals.iter().filter(|&&v| v < base_cc).count();
        eprintln!(
            "  {:>18}: n={} min={} avg={:.0} max={} improved_over_base={}",
            name,
            vals.len(),
            min,
            avg,
            max,
            improved
        );
    }

    let mut all_vals: Vec<(String, i32)> = summary
        .iter()
        .map(|(n, cc, _)| (n.clone(), *cc))
        .collect();
    all_vals.sort_by_key(|&(_, cc)| cc);
    eprintln!("\nTop 10 overall:");
    for (name, cc) in all_vals.iter().take(10) {
        eprintln!("  {:>18} cc={} (vs base {}, Δ={:+})", name, cc, base_cc, cc - base_cc);
    }

    eprintln!("\nSQL written to {}", out_file);
}
