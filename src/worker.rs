use crate::algorithm::{get_all_cliques, get_cliques_comprehensive, get_new_cliques_with_limit};
use crate::client::MiddlewareClient;
use crate::clique_collection::CliqueCollection;
use crate::enumeration::{WorkEnumerator, WorkUnit, create_enumerator};
use crate::graph::Graph;
use crate::model::{StageConfig, WorkResult, WorkUnitAnalysisType};
use crate::redis_client::RedisClient;
use crate::sa::{SaConfig, run_sa};
use crate::tabu::{TabuConfig, run_tabu};
use crate::vds::{VdsConfig, run_vds};
use crate::{log_error, log_info};
use chrono::Utc;
use std::collections::HashMap;
use std::error::Error;
use std::time::Duration;
use tokio::time::sleep;

/// TTL for shared per-edge clique counts. Only needs to outlive the window in which the fleet
/// picks up a given stage (seconds), so a few minutes is generous; the short life keeps Redis
/// bounded — at ~318 KB per 282-vertex graph only the last handful of stages are ever resident.
const SHARED_EDGE_COUNTS_TTL_SECONDS: u64 = 300;
/// Lifetime of the "I am building the counts" election lock. Comfortably longer than a build
/// (~0.6s on a 1.7M-clique graph) so peers wait rather than duplicating, but short enough that
/// a crashed builder doesn't stall the next stage.
const EDGE_COUNTS_BUILD_LOCK_TTL_SECONDS: u64 = 30;
/// How peers wait for the elected builder's result: poll interval and max polls (~3s total).
const EDGE_COUNTS_POLL_INTERVAL_MS: u64 = 25;
const EDGE_COUNTS_WAIT_POLLS: usize = 120;
/// Most flips we will chase incrementally. Stage-to-stage moves are 1 edge (singles) or 2 (pairs);
/// anything larger (a perturbation kick) is cheaper to rebuild than to walk edge by edge.
const MAX_INCREMENTAL_FLIPS: usize = 2;
/// Graphs/collections retained per worker. The incremental path needs only the previous one.
const GRAPH_CACHE_MAX: usize = 3;

pub struct Worker {
    mw_client: MiddlewareClient,
    redis_client: Option<RedisClient>,
    clique_size: usize,
    vertex_count: usize,
    graph_cache: HashMap<i32, Graph>,
    clique_collection_cache: HashMap<i32, CliqueCollection>,
    /// Base graph of the stage we most recently set up, so the next stage (one flip away) can be
    /// derived from it instead of rebuilt.
    last_base_graph_id: Option<i32>,
    poll_interval: Duration,
    fetch_size: i32,
    publish_size: i32,
    campaign_id: i32,
    /// Fleet abstraction: when Some(platform), the worker resolves its stage via
    /// GET /fleets/{platform}/active-stage each cycle (repoint/pause is a DB
    /// update, no redeploy). When None, falls back to the pinned campaign_id.
    fleet: Option<String>,
    stage_id: Option<i32>,
    base_graph_clique_count: Option<i32>,
    publish_results: bool,
    top_results_count: usize,
    // Counter-based mode state
    stage_config: Option<StageConfig>,
    enumerator: Option<Box<dyn WorkEnumerator + Send>>,
    // Simulated annealing mode config
    sa_mode: bool,
    sa_config: SaConfig,
    // Variable-depth search mode config
    vds_mode: bool,
    vds_config: VdsConfig,
    // Tabu search mode config
    tabu_mode: bool,
    tabu_config: TabuConfig,
}

impl Worker {
    pub fn new(
        base_url: String,
        vertex_count: usize,
        clique_size: usize,
        campaign_id: i32,
        fleet: Option<String>,
        poll_interval_ms: u64,
        fetch_size: i32,
        publish_size: i32,
        publish_results: bool,
        top_results_count: usize,
        sa_mode: bool,
        sa_max_iterations: u64,
        sa_initial_temp: f64,
        sa_cooling_rate: f64,
        sa_min_pairs: usize,
        sa_max_pairs: usize,
        vds_mode: bool,
        vds_max_depth: usize,
        vds_top_first_edges: usize,
        vds_branching_factor: usize,
        vds_worsening_tolerance: i32,
        vds_random_seed: Option<u64>,
        vds_start_depth: usize,
        tabu_mode: bool,
        tabu_max_iterations: u64,
        tabu_base_tenure: usize,
        tabu_max_tenure: usize,
        tabu_restart_after: u64,
        tabu_candidate_pool_size: usize,
        tabu_diversification_pairs: usize,
        tabu_random_seed: Option<u64>,
    ) -> Self {
        Worker {
            mw_client: MiddlewareClient::new(base_url),
            redis_client: None,
            vertex_count,
            clique_size,
            graph_cache: HashMap::new(),
            clique_collection_cache: HashMap::new(),
            last_base_graph_id: None,
            poll_interval: Duration::from_millis(poll_interval_ms),
            fetch_size,
            publish_size,
            campaign_id,
            fleet,
            stage_id: None,
            base_graph_clique_count: None,
            publish_results,
            top_results_count,
            stage_config: None,
            enumerator: None,
            sa_mode,
            sa_config: SaConfig {
                max_iterations: sa_max_iterations,
                initial_temp: sa_initial_temp,
                cooling_rate: sa_cooling_rate,
                min_pairs: sa_min_pairs,
                max_pairs: sa_max_pairs,
            },
            vds_mode,
            vds_config: VdsConfig {
                max_depth: vds_max_depth,
                top_first_edges: vds_top_first_edges,
                branching_factor: vds_branching_factor,
                worsening_tolerance: vds_worsening_tolerance,
                random_seed: vds_random_seed,
                start_depth: vds_start_depth,
            },
            tabu_mode,
            tabu_config: TabuConfig {
                max_iterations: tabu_max_iterations,
                base_tabu_tenure: tabu_base_tenure,
                max_tabu_tenure: tabu_max_tenure,
                restart_after: tabu_restart_after,
                candidate_pool_size: tabu_candidate_pool_size,
                diversification_pair_count: tabu_diversification_pairs,
                random_seed: tabu_random_seed,
            },
        }
    }

    /// Connect to Redis
    pub async fn connect_redis(&mut self, host: &str, port: u16) -> Result<(), Box<dyn Error>> {
        let redis_client = RedisClient::new(host, port).await?;
        self.redis_client = Some(redis_client);
        Ok(())
    }

    pub async fn initialize(&mut self) -> Result<(), Box<dyn Error>> {
        // Fleet mode: campaign is resolved dynamically from the fleet mapping, so
        // vertex_count/clique_size are set lazily the first time a stage is seen
        // (see get_or_fetch_stage_id). Nothing to fetch up front.
        if let Some(fleet) = &self.fleet {
            log_info!("Initializing worker for fleet: {}", fleet);
            return Ok(());
        }

        log_info!("Initializing worker for campaign ID: {}", self.campaign_id);

        let campaign = self.mw_client.get_campaign(self.campaign_id).await?;
        log_info!("Campaign Info: {:?}", campaign);
        self.vertex_count = campaign.vertex_count as usize;
        self.clique_size = campaign.subgraph_size as usize;

        Ok(())
    }

    pub async fn run(&mut self) {
        if let Err(e) = self.initialize().await {
            log_error!("Failed to initialize: {}", e);
            return;
        }

        log_info!("Worker started for campaign: {}", self.campaign_id);

        loop {
            let cycle_start = std::time::Instant::now();
            match self.cycle().await {
                Ok(count) => {
                    if count == 0 {
                        sleep(self.poll_interval).await;
                    } else {
                        let elapsed_ms = cycle_start.elapsed().as_millis();
                        log_info!("Processed {} work items in {}ms", count, elapsed_ms);
                    }
                }
                Err(e) => {
                    eprintln!(
                        "[{}] Error in worker cycle: {}",
                        Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"),
                        e
                    );
                    sleep(self.poll_interval).await;
                }
            }
        }
    }

    async fn cycle(&mut self) -> Result<usize, Box<dyn Error>> {
        // Get stage_id first. None => nothing to work (fleet paused / unmapped /
        // no active stage) => idle via the poll interval.
        let stage_id = match self.get_or_fetch_stage_id().await? {
            Some(id) => id,
            None => return Ok(0),
        };

        // Verify counter-based mode is available
        let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
        let has_counter = redis_client.has_stage_config(stage_id).await?;

        if !has_counter {
            // Stage config missing - likely stage progressed externally
            // Clear cache so we re-fetch the active stage on next cycle
            log_info!(
                "Stage config missing for stage {} - stage may have progressed, refreshing...",
                stage_id
            );
            self.clear_stage_cache();
            return Ok(0); // Return 0 to trigger poll interval, then retry with fresh stage
        }

        if self.sa_mode {
            self.cycle_simulated_annealing(stage_id).await
        } else if self.vds_mode {
            self.cycle_variable_depth_search(stage_id).await
        } else if self.tabu_mode {
            self.cycle_tabu_search(stage_id).await
        } else {
            self.cycle_counter_based(stage_id).await
        }
    }

    async fn cycle_tabu_search(&mut self, stage_id: i32) -> Result<usize, Box<dyn Error>> {
        if self.stage_config.is_none() || self.stage_config.as_ref().unwrap().stage_id != stage_id {
            let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
            if let Some(config) = redis_client.get_stage_config(stage_id).await? {
                log_info!(
                    "Tabu: Loaded stage config: baseGraphId={}, strategy={:?}",
                    config.base_graph_id,
                    config.strategy
                );
                self.stage_config = Some(config);
            } else {
                return Err(
                    format!("Stage config not found in Redis for stage {}", stage_id).into(),
                );
            }
        }

        let config = self.stage_config.as_ref().unwrap();
        let base_graph_id = config.base_graph_id;

        let mut base_graph =
            Graph::from_bitstring(&config.graph.edge_data, config.graph.vertex_count);
        let all_cliques = get_all_cliques(&mut base_graph, self.clique_size);
        let mut clique_collection = CliqueCollection::new(self.vertex_count);
        clique_collection.set_cliques(all_cliques, self.vertex_count);
        get_cliques_comprehensive(&mut base_graph, self.clique_size);

        let threshold: Option<i32> = {
            let redis = self.redis_client.as_mut().ok_or("Redis not connected")?;
            redis
                .get_top_results_threshold(stage_id, self.top_results_count)
                .await
                .unwrap_or(None)
        };

        let result = run_tabu(
            &base_graph,
            self.clique_size,
            &self.tabu_config,
            &clique_collection,
            threshold,
        );

        // Submit if it beats the threshold (matches SA pattern: bitstring submission).
        let should_submit = match threshold {
            None => true,
            Some(t) => result.best_clique_count < t,
        };

        if should_submit {
            if let Some(redis) = self.redis_client.as_mut() {
                let _ = redis
                    .add_sa_result_to_top_results(
                        stage_id,
                        base_graph_id,
                        &result.best_graph_bitstring,
                        result.best_clique_count,
                        self.top_results_count,
                    )
                    .await;
            }
        }

        if let Some(redis) = self.redis_client.as_mut() {
            let _ = redis.increment_processed_count(stage_id, 1).await;
        }

        Ok(1)
    }

    /// Counter-based work cycle: claim index ranges and enumerate locally
    async fn cycle_counter_based(&mut self, stage_id: i32) -> Result<usize, Box<dyn Error>> {
        // Ensure we have stage config cached
        if self.stage_config.is_none() || self.stage_config.as_ref().unwrap().stage_id != stage_id {
            let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
            if let Some(config) = redis_client.get_stage_config(stage_id).await? {
                log_info!(
                    "Loaded stage config: strategy={:?}, totalPairs={}, baseGraphId={}",
                    config.strategy,
                    config.total_pairs,
                    config.base_graph_id
                );

                // Check if we already have this graph cached (reuse across stages!)
                if !self.graph_cache.contains_key(&config.base_graph_id) {
                    let graph_id = config.base_graph_id;

                    // FAST PATH: consecutive stages differ by a single edge flip, so derive this
                    // stage's graph AND counts from the previous stage's cached ones — a seeded
                    // traversal of one edge's neighbourhood (~0.4ms) instead of a whole-graph pass
                    // (~420ms at 800k cliques; measured 1075x). This matters because the counts are
                    // the serialized head of every stage: until they exist no worker can evaluate
                    // anything, so it set the floor on stage duration for the whole fleet.
                    if self.derive_from_previous_stage(&config) {
                        self.last_base_graph_id = Some(graph_id);
                        self.prune_graph_caches(graph_id);
                    } else {
                    log_info!(
                        "Building graph from stage_config (first time for graph {})",
                        config.base_graph_id
                    );
                    let graph =
                        Graph::from_bitstring(&config.graph.edge_data, config.graph.vertex_count);
                    self.graph_cache.insert(config.base_graph_id, graph);

                    // Per-edge clique cardinalities for this graph. This path reads only the
                    // per-edge counts and the total (see `broken` / `base_total` below), so we
                    // build counts-only — no clique list, no edge->cliques index. Cost scales
                    // with the clique count (~1s at 1.6M cliques), and every worker would
                    // otherwise pay it for the SAME graph on every stage, so the first one to
                    // build it shares it via Redis and the rest skip the traversal entirely.
                    let mut shared = match self.redis_client.as_mut() {
                        Some(redis) => redis.get_shared_edge_counts(graph_id).await.unwrap_or(None),
                        None => None,
                    };
                    // On a miss, elect ONE builder: every worker sees a new stage within
                    // milliseconds, so without this they all miss and all traverse in parallel
                    // (no sharing at all). Losers wait for the winner — they would have been
                    // busy building anyway, and freeing those cores lets the winner finish
                    // sooner. If the winner dies or is slow, the lock TTL expires / the wait
                    // times out and they build locally.
                    if shared.is_none() {
                        let i_build = match self.redis_client.as_mut() {
                            Some(redis) => redis
                                .try_acquire_edge_counts_build_lock(
                                    graph_id,
                                    EDGE_COUNTS_BUILD_LOCK_TTL_SECONDS,
                                )
                                .await
                                .unwrap_or(true),
                            None => true,
                        };
                        if !i_build {
                            for _ in 0..EDGE_COUNTS_WAIT_POLLS {
                                sleep(Duration::from_millis(EDGE_COUNTS_POLL_INTERVAL_MS)).await;
                                if let Some(redis) = self.redis_client.as_mut() {
                                    if let Some(v) =
                                        redis.get_shared_edge_counts(graph_id).await.unwrap_or(None)
                                    {
                                        shared = Some(v);
                                        break;
                                    }
                                }
                            }
                            if shared.is_none() {
                                log_info!(
                                    "Waited for shared edge counts for graph {} without success; building locally",
                                    graph_id
                                );
                            }
                        }
                    }
                    let cc = match shared {
                        Some((counts, total)) => {
                            log_info!(
                                "Reusing shared edge counts for graph {} (total cliques {})",
                                graph_id,
                                total
                            );
                            CliqueCollection::from_shared_counts(self.vertex_count, counts, total)
                        }
                        None => {
                            let graph = self.graph_cache.get_mut(&graph_id).unwrap();
                            let mut cc = CliqueCollection::new(self.vertex_count);
                            cc.build_counts_only(graph, self.clique_size);
                            log_info!(
                                "Built edge counts for graph {} (total cliques {}); sharing",
                                graph_id,
                                cc.total()
                            );
                            if let Some(redis) = self.redis_client.as_mut() {
                                if let Err(e) = redis
                                    .set_shared_edge_counts(
                                        graph_id,
                                        cc.edge_counts(),
                                        cc.total(),
                                        SHARED_EDGE_COUNTS_TTL_SECONDS,
                                    )
                                    .await
                                {
                                    log_error!("Could not share edge counts for graph {graph_id}: {e}");
                                }
                            }
                            cc
                        }
                    };
                    self.clique_collection_cache.insert(graph_id, cc);
                    self.last_base_graph_id = Some(graph_id);
                    self.prune_graph_caches(graph_id);
                    }
                } else {
                    log_info!(
                        "Reusing cached graph {} for new stage {}",
                        config.base_graph_id,
                        stage_id
                    );
                }

                // Create enumerator for this stage (uses cached graph)
                let graph = self.graph_cache.get(&config.base_graph_id).unwrap();
                let enumerator = create_enumerator(&config.strategy, graph);
                if enumerator.total_work_units() != config.total_pairs {
                    return Err(format!(
                        "Enumerator total_work_units {} != stage config totalPairs {} for stage {} (strategy {:?}) — worker and queue manager disagree on the work space; refusing to process",
                        enumerator.total_work_units(),
                        config.total_pairs,
                        stage_id,
                        config.strategy
                    )
                    .into());
                }
                self.enumerator = Some(enumerator);
                self.stage_config = Some(config);
            } else {
                return Err(
                    format!("Stage config not found in Redis for stage {}", stage_id).into(),
                );
            }
        }

        let config = self.stage_config.as_ref().unwrap();
        let batch_size = self.fetch_size as i64;
        let total_pairs = config.total_pairs;
        let base_graph_id = config.base_graph_id;
        // Base graph bitstring + vertex count for derived-graph hashing (the novelty
        // filter). Captured once per batch — constant for the stage's base graph.
        // vertex_count must equal the QM's (config value) so the hashes agree.
        let base_bitstring = config.graph.edge_data.clone();
        let derived_vertex_count = config.graph.vertex_count;

        // Claim work range
        let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
        let range = redis_client
            .claim_work_range(stage_id, batch_size, total_pairs)
            .await?;

        let (start_index, end_index) = match range {
            Some(r) => r,
            None => {
                log_info!("All work claimed for stage {}, clearing cache...", stage_id);
                self.clear_stage_cache();
                return Ok(0);
            }
        };

        let work_count = (end_index - start_index) as usize;
        let enumerator = self.enumerator.as_ref().unwrap();

        let graph = self.graph_cache.get_mut(&base_graph_id).unwrap();
        let clique_collection = self.clique_collection_cache.get(&base_graph_id).unwrap();

        // Fetch the current threshold for top-N results (None = accept anything).
        // Declared mut so it can be tightened in-loop as the sorted set fills up,
        // eliminating the burst of unfiltered submissions when a fresh stage starts.
        let mut top_threshold: Option<i32> = {
            let redis = self.redis_client.as_mut().ok_or("Redis not connected")?;
            redis
                .get_top_results_threshold(stage_id, self.top_results_count)
                .await
                .unwrap_or(None)
        };

        let mut processed_results: Vec<WorkResult> = Vec::new();

        // Process each work unit in the range
        for idx in start_index..end_index {
            let edges_to_flip = match enumerator.index_to_work_unit(idx) {
                WorkUnit::SingleFlip(edge) => vec![edge],
                WorkUnit::PairFlip(red_edge, blue_edge) => vec![red_edge, blue_edge],
            };

            let broken = clique_collection.get_count_of_cliques_containing_edges(&edges_to_flip);
            let base_total = clique_collection.total() as i32;

            // Early termination when not publishing: stop counting if result can't be in top-N
            let count = if !self.publish_results {
                // Calculate the limit for early termination based on threshold
                // If threshold exists: max_new = threshold - (base_total - broken) - 1
                // This is the max new cliques that would still beat the threshold
                let early_limit = match top_threshold {
                    Some(threshold) => {
                        let max_count = threshold - 1; // Must be strictly less
                        let max_new = max_count - base_total + broken;
                        if max_new < 0 {
                            // base_total - broken >= threshold: this flip cannot beat the
                            // threshold even if it creates ZERO new cliques, so the kernel can
                            // only confirm what the per-edge counts already prove. Skip it
                            // outright instead of paying two flip_edges plus a seeded traversal.
                            continue;
                        }
                        max_new
                    }
                    None => i32::MAX, // No threshold, count everything
                };

                graph.flip_edges(&edges_to_flip);
                let (new, exceeded) = get_new_cliques_with_limit(
                    graph,
                    self.clique_size,
                    &edges_to_flip,
                    early_limit,
                );
                graph.flip_edges(&edges_to_flip);

                if exceeded {
                    continue;
                }
                base_total - broken + new
            } else {
                graph.flip_edges(&edges_to_flip);
                let (new, _) =
                    get_new_cliques_with_limit(graph, self.clique_size, &edges_to_flip, i32::MAX);
                graph.flip_edges(&edges_to_flip);
                base_total - broken + new
            };

            // Track as a potential best result (stored in top-N sorted set)
            // Submit if: threshold is None (set not full) OR count < threshold (better than worst)
            let should_submit = match top_threshold {
                None => true, // Set is not full, accept any result
                Some(threshold) => count < threshold,
            };
            if should_submit {
                // Derived-graph hash so the set stays novel-only (slot 0 = best novel).
                // Only computed for record-breakers (count < best novel), so it's rare.
                let hash = crate::hash::derived_graph_hash(
                    &base_bitstring,
                    derived_vertex_count,
                    &edges_to_flip,
                );
                if let Some(redis) = self.redis_client.as_mut() {
                    if let Ok((kept, new_threshold)) = redis
                        .add_to_top_results(
                            stage_id,
                            base_graph_id,
                            &edges_to_flip,
                            count,
                            &hash,
                            self.top_results_count,
                        )
                        .await
                    {
                        // Only a real insert is news; a rejected (already-visited) candidate
                        // changes nothing for the QM. Fire-and-forget — the QM keeps a polling
                        // fallback, so a dropped message costs latency, not correctness.
                        if kept {
                            let _ = redis.publish_best_result(stage_id, count).await;
                        }
                        // Update threshold in-place so early termination tightens
                        // within this batch rather than staying stale for all 250K units.
                        if let Some(t) = new_threshold {
                            top_threshold = Some(match top_threshold {
                                Some(current) => current.min(t),
                                None => t,
                            });
                        }
                    }
                }
            }

            // Collect results for publishing
            if self.publish_results {
                let result = WorkResult {
                    id: None,
                    base_graph_id,
                    stage_id,
                    edges_to_flip: edges_to_flip.clone(),
                    clique_count: count,
                    work_unit_analysis_type: WorkUnitAnalysisType::TARGETED,
                };
                processed_results.push(result);

                if processed_results.len() >= self.publish_size as usize {
                    self.mw_client.submit_results(&processed_results).await?;
                    processed_results.clear();
                }
            }
        }

        // Submit remaining results
        if self.publish_results && !processed_results.is_empty() {
            self.mw_client.submit_results(&processed_results).await?;
        }

        // Update processed count
        if work_count > 0 {
            if let Some(redis) = self.redis_client.as_mut() {
                let _ = redis
                    .increment_processed_count(stage_id, work_count as i64)
                    .await;
            }
        }

        Ok(work_count)
    }

    async fn cycle_simulated_annealing(&mut self, stage_id: i32) -> Result<usize, Box<dyn Error>> {
        // Ensure we have stage config cached
        if self.stage_config.is_none() || self.stage_config.as_ref().unwrap().stage_id != stage_id {
            let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
            if let Some(config) = redis_client.get_stage_config(stage_id).await? {
                log_info!(
                    "SA: Loaded stage config: baseGraphId={}, strategy={:?}",
                    config.base_graph_id,
                    config.strategy
                );
                self.stage_config = Some(config);
            } else {
                return Err(
                    format!("Stage config not found in Redis for stage {}", stage_id).into(),
                );
            }
        }

        let config = self.stage_config.as_ref().unwrap();
        let base_graph_id = config.base_graph_id;

        // Build graph and CliqueCollection from stage config.
        // The clique collection provides per-edge participation scores for guided edge selection.
        let mut base_graph =
            Graph::from_bitstring(&config.graph.edge_data, config.graph.vertex_count);
        let all_cliques = get_all_cliques(&mut base_graph, self.clique_size);
        let mut clique_collection = CliqueCollection::new(self.vertex_count);
        clique_collection.set_cliques(all_cliques, self.vertex_count);

        // Recount cliques after get_all_cliques (which may mutate graph state)
        get_cliques_comprehensive(&mut base_graph, self.clique_size);

        // Get current threshold for top-N filtering
        let threshold: Option<i32> = {
            let redis = self.redis_client.as_mut().ok_or("Redis not connected")?;
            redis
                .get_top_results_threshold(stage_id, self.top_results_count)
                .await
                .unwrap_or(None)
        };

        // Run one complete SA schedule
        let result = run_sa(
            &base_graph,
            self.clique_size,
            &self.sa_config,
            &clique_collection,
            threshold,
        );

        // Submit best result to Redis if it's worth tracking
        let should_submit = match threshold {
            None => true,
            Some(t) => result.best_clique_count < t,
        };

        if should_submit {
            if let Some(redis) = self.redis_client.as_mut() {
                let _ = redis
                    .add_sa_result_to_top_results(
                        stage_id,
                        base_graph_id,
                        &result.best_graph_bitstring,
                        result.best_clique_count,
                        self.top_results_count,
                    )
                    .await;
            }
        }

        // Update processed count (1 per SA run)
        if let Some(redis) = self.redis_client.as_mut() {
            let _ = redis.increment_processed_count(stage_id, 1).await;
        }

        // Return 1 to indicate work was done (avoids poll sleep)
        Ok(1)
    }

    async fn cycle_variable_depth_search(
        &mut self,
        stage_id: i32,
    ) -> Result<usize, Box<dyn Error>> {
        // Ensure we have stage config cached
        if self.stage_config.is_none() || self.stage_config.as_ref().unwrap().stage_id != stage_id {
            let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
            if let Some(config) = redis_client.get_stage_config(stage_id).await? {
                log_info!(
                    "VDS: Loaded stage config: baseGraphId={}, strategy={:?}",
                    config.base_graph_id,
                    config.strategy
                );
                self.stage_config = Some(config);
            } else {
                return Err(
                    format!("Stage config not found in Redis for stage {}", stage_id).into(),
                );
            }
        }

        let config = self.stage_config.as_ref().unwrap();
        let base_graph_id = config.base_graph_id;
        let base_bitstring = config.graph.edge_data.clone();
        let derived_vertex_count = config.graph.vertex_count;

        // Build graph and CliqueCollection from stage config.
        let mut base_graph =
            Graph::from_bitstring(&config.graph.edge_data, config.graph.vertex_count);
        let all_cliques = get_all_cliques(&mut base_graph, self.clique_size);
        let base_clique_count = all_cliques.len() as i32;
        let mut clique_collection = CliqueCollection::new(self.vertex_count);
        clique_collection.set_cliques(all_cliques, self.vertex_count);

        // Recount cliques after get_all_cliques (which may mutate graph state)
        get_cliques_comprehensive(&mut base_graph, self.clique_size);

        // Get current threshold for top-N filtering
        let threshold: Option<i32> = {
            let redis = self.redis_client.as_mut().ok_or("Redis not connected")?;
            redis
                .get_top_results_threshold(stage_id, self.top_results_count)
                .await
                .unwrap_or(None)
        };

        // Run one complete VDS
        let result = run_vds(
            &base_graph,
            self.clique_size,
            &self.vds_config,
            &clique_collection,
            base_clique_count,
        );

        // Submit best result to Redis if it improved and beats the threshold
        let should_submit = result.improved
            && match threshold {
                None => true,
                Some(t) => result.final_clique_count < t,
            };

        if should_submit {
            let hash = crate::hash::derived_graph_hash(
                &base_bitstring,
                derived_vertex_count,
                &result.edges_to_flip,
            );
            if let Some(redis) = self.redis_client.as_mut() {
                let _ = redis
                    .add_to_top_results(
                        stage_id,
                        base_graph_id,
                        &result.edges_to_flip,
                        result.final_clique_count,
                        &hash,
                        self.top_results_count,
                    )
                    .await;
            }
        }

        // Update processed count (1 per VDS run)
        if let Some(redis) = self.redis_client.as_mut() {
            let _ = redis.increment_processed_count(stage_id, 1).await;
        }

        Ok(1)
    }

    /// Try to derive this stage's graph and per-edge clique counts from the previous stage's,
    /// which differ by only the edge(s) the search just flipped. Returns false when there is
    /// nothing to derive from (cold start) or the graphs are too far apart, leaving the caller to
    /// do a full build.
    fn derive_from_previous_stage(&mut self, config: &StageConfig) -> bool {
        let graph_id = config.base_graph_id;
        let Some(prev_id) = self.last_base_graph_id else {
            return false;
        };
        if prev_id == graph_id
            || !self.graph_cache.contains_key(&prev_id)
            || !self.clique_collection_cache.contains_key(&prev_id)
        {
            return false;
        }

        let new_bits = &config.graph.edge_data;
        let prev_bits = self.graph_cache[&prev_id].to_bitstring();
        if prev_bits.len() != new_bits.len() {
            return false;
        }
        let flipped: Vec<usize> = prev_bits
            .chars()
            .zip(new_bits.chars())
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, _)| i)
            .collect();
        if flipped.is_empty() || flipped.len() > MAX_INCREMENTAL_FLIPS {
            return false;
        }

        let mut graph = self.graph_cache[&prev_id].clone();
        let mut cc = self.clique_collection_cache[&prev_id].clone();
        for &bit in &flipped {
            match Graph::edge_for_bit_index(bit, config.graph.vertex_count) {
                Some((u, v)) => cc.apply_edge_flip(&mut graph, self.clique_size, u, v),
                None => return false,
            }
        }
        // Cheap belt-and-braces: the derived graph must be exactly the stage's graph. If this ever
        // fails we fall back to a full build rather than search a wrong graph.
        if graph.to_bitstring() != *new_bits {
            log_error!(
                "Derived graph {} does not match stage config; falling back to full build",
                graph_id
            );
            return false;
        }

        log_info!(
            "Derived edge counts for graph {} from {} via {} flip(s) (total cliques {})",
            graph_id,
            prev_id,
            flipped.len(),
            cc.total()
        );
        self.graph_cache.insert(graph_id, graph);
        self.clique_collection_cache.insert(graph_id, cc);
        true
    }

    /// Keep only the newest few graphs/collections. Without this the caches grow by ~360 KB per
    /// stage forever (thousands of stages per descent), and the incremental path only ever needs
    /// the immediately preceding one.
    fn prune_graph_caches(&mut self, keep: i32) {
        if self.graph_cache.len() <= GRAPH_CACHE_MAX {
            return;
        }
        let mut ids: Vec<i32> = self.graph_cache.keys().copied().collect();
        ids.sort_unstable();
        let drop_count = ids.len().saturating_sub(GRAPH_CACHE_MAX);
        for id in ids.into_iter().take(drop_count) {
            if id != keep {
                self.graph_cache.remove(&id);
                self.clique_collection_cache.remove(&id);
            }
        }
    }

    fn clear_stage_cache(&mut self) {
        self.stage_id = None;
        self.base_graph_clique_count = None;
        self.stage_config = None;
        self.enumerator = None;
        // Clear graph caches to prevent memory leak on stage progression
    }

    /// Drop the cross-stage graph/collection caches. These are keyed by GRAPH id and are exactly
    /// what lets the next stage be derived from the previous one, so they must survive an ordinary
    /// stage advance — only a campaign change (different vertex_count/clique_size, which would
    /// make cached collections the wrong shape) or going idle should clear them.
    fn clear_graph_caches(&mut self) {
        self.graph_cache.clear();
        self.clique_collection_cache.clear();
        self.last_base_graph_id = None;
    }

    /// Resolve the stage to work. Ok(None) means "nothing to do right now"
    /// (fleet paused / unmapped / no active stage) so the caller should idle.
    async fn get_or_fetch_stage_id(&mut self) -> Result<Option<i32>, Box<dyn Error>> {
        // ---- Fleet mode: re-resolve each cycle so repoints/pauses take effect
        // within one poll, with no redeploy. ----
        if let Some(fleet) = self.fleet.clone() {
            let stage = match self.mw_client.get_fleet_active_stage(&fleet).await? {
                None => {
                    // Paused / unmapped / no active stage. Drop any cached stage
                    // so we re-init cleanly when work reappears, then idle.
                    if self.stage_id.is_some() {
                        log_info!("Fleet {} has no active stage — idling", fleet);
                        self.clear_stage_cache();
                        self.clear_graph_caches();
                    }
                    return Ok(None);
                }
                Some(s) => s,
            };

            if self.stage_id != Some(stage.stage_id) {
                // Fleet repointed or the stage progressed → reset per-stage state. NOTE: the
                // graph/collection caches deliberately survive, so the new stage (one flip away)
                // can be derived from the previous one instead of rebuilt from scratch.
                self.clear_stage_cache();
                // Different campaign → refresh vertex/clique params (as initialize
                // does in campaign mode), so CliqueCollection sizing stays correct.
                if self.campaign_id != stage.campaign_id {
                    self.clear_graph_caches(); // cached collections are sized for the old campaign
                    self.campaign_id = stage.campaign_id;
                    if let Ok(campaign) = self.mw_client.get_campaign(stage.campaign_id).await {
                        self.vertex_count = campaign.vertex_count as usize;
                        self.clique_size = campaign.subgraph_size as usize;
                    }
                }
                self.stage_id = Some(stage.stage_id);
                log_info!(
                    "Fleet {} → stage {} (campaign {}, base_graph_id {})",
                    fleet,
                    stage.stage_id,
                    stage.campaign_id,
                    stage.base_graph_id
                );
            }
            return Ok(Some(stage.stage_id));
        }

        // ---- Campaign mode (legacy fallback, RAMSEY_CAMPAIGN_ID) ----
        if let Some(stage_id) = self.stage_id {
            return Ok(Some(stage_id));
        }

        let stages = self
            .mw_client
            .get_stages_by_campaign(self.campaign_id, "ACTIVE")
            .await?;

        if stages.is_empty() {
            return Err("No active stage found for campaign".into());
        }

        if stages.len() > 1 {
            log_error!(
                "Warning: Multiple active stages for campaign {}, using first",
                self.campaign_id
            );
        }

        let stage = &stages[0];
        log_info!(
            "Using stage {} for campaign {} (base_graph_id: {})",
            stage.stage_id,
            self.campaign_id,
            stage.base_graph_id
        );

        let graph_data = self.mw_client.get_graph(stage.base_graph_id).await?;
        self.base_graph_clique_count = graph_data.clique_count;
        log_info!(
            "Base graph clique count: {:?}",
            self.base_graph_clique_count
        );

        self.stage_id = Some(stage.stage_id);
        Ok(Some(stage.stage_id))
    }
}
