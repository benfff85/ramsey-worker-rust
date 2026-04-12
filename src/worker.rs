use crate::algorithm::{get_all_cliques, get_cliques_comprehensive, get_new_cliques_with_limit};
use crate::client::MiddlewareClient;
use crate::clique_collection::CliqueCollection;
use crate::enumeration::{WorkEnumerator, create_enumerator};
use crate::graph::Graph;
use crate::model::{StageConfig, WorkResult, WorkUnitAnalysisType};
use crate::redis_client::RedisClient;
use crate::sa::{SaConfig, run_sa};
use crate::vds::{VdsConfig, run_vds};
use crate::{log_error, log_info};
use chrono::Utc;
use std::collections::HashMap;
use std::error::Error;
use std::time::Duration;
use tokio::time::sleep;

pub struct Worker {
    mw_client: MiddlewareClient,
    redis_client: Option<RedisClient>,
    clique_size: usize,
    vertex_count: usize,
    graph_cache: HashMap<i32, Graph>,
    clique_collection_cache: HashMap<i32, CliqueCollection>,
    poll_interval: Duration,
    fetch_size: i32,
    publish_size: i32,
    campaign_id: i32,
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
}

impl Worker {
    pub fn new(
        base_url: String,
        vertex_count: usize,
        clique_size: usize,
        campaign_id: i32,
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
    ) -> Self {
        Worker {
            mw_client: MiddlewareClient::new(base_url),
            redis_client: None,
            vertex_count,
            clique_size,
            graph_cache: HashMap::new(),
            clique_collection_cache: HashMap::new(),
            poll_interval: Duration::from_millis(poll_interval_ms),
            fetch_size,
            publish_size,
            campaign_id,
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
        }
    }

    /// Connect to Redis
    pub async fn connect_redis(&mut self, host: &str, port: u16) -> Result<(), Box<dyn Error>> {
        let redis_client = RedisClient::new(host, port).await?;
        self.redis_client = Some(redis_client);
        Ok(())
    }

    pub async fn initialize(&mut self) -> Result<(), Box<dyn Error>> {
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
        // Get stage_id first
        let stage_id = self.get_or_fetch_stage_id().await?;

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
        } else {
            self.cycle_counter_based(stage_id).await
        }
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
                    log_info!(
                        "Building graph from stage_config (first time for graph {})",
                        config.base_graph_id
                    );
                    let graph =
                        Graph::from_bitstring(&config.graph.edge_data, config.graph.vertex_count);
                    self.graph_cache.insert(config.base_graph_id, graph);

                    // Build clique collection for this graph
                    let graph = self.graph_cache.get_mut(&config.base_graph_id).unwrap();
                    let all_cliques = get_all_cliques(graph, self.clique_size);
                    let mut cc = CliqueCollection::new(self.vertex_count);
                    cc.set_cliques(all_cliques, self.vertex_count);
                    self.clique_collection_cache
                        .insert(config.base_graph_id, cc);
                } else {
                    log_info!(
                        "Reusing cached graph {} for new stage {}",
                        config.base_graph_id,
                        stage_id
                    );
                }

                // Create enumerator for this stage (uses cached graph)
                let graph = self.graph_cache.get(&config.base_graph_id).unwrap();
                self.enumerator = Some(create_enumerator(&config.strategy, graph));
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

        // Fetch the current threshold for top-N results (None = accept anything)
        let top_threshold: Option<i32> = {
            let redis = self.redis_client.as_mut().ok_or("Redis not connected")?;
            redis
                .get_top_results_threshold(stage_id, self.top_results_count)
                .await
                .unwrap_or(None)
        };

        let mut processed_results: Vec<WorkResult> = Vec::new();

        // Process each work unit in the range
        for idx in start_index..end_index {
            let (red_edge, blue_edge) = enumerator.index_to_edge_pair(idx);
            let edges_to_flip = vec![red_edge, blue_edge];

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
                        if max_new < 0 { 0 } else { max_new }
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
                if let Some(redis) = self.redis_client.as_mut() {
                    let _ = redis
                        .add_to_top_results(
                            stage_id,
                            base_graph_id,
                            &edges_to_flip,
                            count,
                            self.top_results_count,
                        )
                        .await;
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
        let mut base_graph = Graph::from_bitstring(&config.graph.edge_data, config.graph.vertex_count);
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
        let result = run_sa(&base_graph, self.clique_size, &self.sa_config, &clique_collection, threshold);

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

    async fn cycle_variable_depth_search(&mut self, stage_id: i32) -> Result<usize, Box<dyn Error>> {
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

        // Build graph and CliqueCollection from stage config.
        let mut base_graph = Graph::from_bitstring(&config.graph.edge_data, config.graph.vertex_count);
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
        let should_submit = result.improved && match threshold {
            None => true,
            Some(t) => result.final_clique_count < t,
        };

        if should_submit {
            if let Some(redis) = self.redis_client.as_mut() {
                let _ = redis
                    .add_to_top_results(
                        stage_id,
                        base_graph_id,
                        &result.edges_to_flip,
                        result.final_clique_count,
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

    fn clear_stage_cache(&mut self) {
        self.stage_id = None;
        self.base_graph_clique_count = None;
        self.stage_config = None;
        self.enumerator = None;
        // Clear graph caches to prevent memory leak on stage progression
        self.graph_cache.clear();
        self.clique_collection_cache.clear();
    }

    async fn get_or_fetch_stage_id(&mut self) -> Result<i32, Box<dyn Error>> {
        if let Some(stage_id) = self.stage_id {
            return Ok(stage_id);
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
        Ok(stage.stage_id)
    }
}
