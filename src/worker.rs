use crate::algorithm::{get_all_cliques, get_new_cliques_with_limit};
use crate::client::MiddlewareClient;
use crate::clique_collection::CliqueCollection;
use crate::enumeration::{WorkEnumerator, create_enumerator};
use crate::graph::Graph;
use crate::model::{
    Client, ClientStatus, ClientType, StageConfig, WorkResult, WorkUnitAnalysisType,
};
use crate::redis_client::RedisClient;
use chrono::Utc;
use std::collections::HashMap;
use std::error::Error;
use std::time::{Duration, Instant};
use tokio::time::sleep;

pub struct Worker {
    mw_client: MiddlewareClient,
    redis_client: Option<RedisClient>,
    client_id: Option<i32>,
    clique_size: usize,
    vertex_count: usize,
    graph_cache: HashMap<i32, Graph>,
    clique_collection_cache: HashMap<i32, CliqueCollection>,
    poll_interval: Duration,
    heartbeat_interval: Duration,
    fetch_size: i32,
    publish_size: i32,
    campaign_id: i32,
    stage_id: Option<i32>,
    base_graph_clique_count: Option<i32>,
    publish_results: bool,
    // Counter-based mode state
    stage_config: Option<StageConfig>,
    enumerator: Option<Box<dyn WorkEnumerator + Send>>,
}

impl Worker {
    pub fn new(
        base_url: String,
        vertex_count: usize,
        clique_size: usize,
        campaign_id: i32,
        poll_interval_ms: u64,
        heartbeat_interval_ms: u64,
        fetch_size: i32,
        publish_size: i32,
        publish_results: bool,
    ) -> Self {
        Worker {
            mw_client: MiddlewareClient::new(base_url),
            redis_client: None,
            client_id: None,
            vertex_count,
            clique_size,
            graph_cache: HashMap::new(),
            clique_collection_cache: HashMap::new(),
            poll_interval: Duration::from_millis(poll_interval_ms),
            heartbeat_interval: Duration::from_millis(heartbeat_interval_ms),
            fetch_size,
            publish_size,
            campaign_id,
            stage_id: None,
            base_graph_clique_count: None,
            publish_results,
            stage_config: None,
            enumerator: None,
        }
    }

    /// Connect to Redis
    pub async fn connect_redis(&mut self, host: &str, port: u16) -> Result<(), Box<dyn Error>> {
        let redis_client = RedisClient::new(host, port).await?;
        self.redis_client = Some(redis_client);
        Ok(())
    }

    pub async fn register(&mut self) -> Result<(), Box<dyn Error>> {
        println!("Registering worker with campaign ID: {}", self.campaign_id);

        let client_data = Client {
            client_id: None,
            campaign_id: self.campaign_id,
            type_: ClientType::CLIQUECHECKER,
            status: ClientStatus::ACTIVE,
            created_date: Some(
                Utc::now()
                    .naive_utc()
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string(),
            ),
            last_phone_home_date: Some(
                Utc::now()
                    .naive_utc()
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string(),
            ),
        };

        let campaign = self.mw_client.get_campaign(self.campaign_id).await?;
        println!("Campaign Info: {:?}", campaign);
        self.vertex_count = campaign.vertex_count as usize;
        self.clique_size = campaign.subgraph_size as usize;

        let registered_client = self.mw_client.create_client(&client_data).await?;
        if let Some(id) = registered_client.client_id {
            self.client_id = Some(id);
            println!("Registered with Client ID: {}", id);
        }

        Ok(())
    }

    pub async fn run(&mut self) {
        if let Err(e) = self.register().await {
            eprintln!("Failed to register: {}", e);
            return;
        }

        println!(
            "Worker started for client: {}",
            self.client_id.as_ref().unwrap()
        );

        let mut last_heartbeat = Instant::now();

        loop {
            // Heartbeat check
            if last_heartbeat.elapsed() > self.heartbeat_interval {
                let hb_client = Client {
                    client_id: self.client_id,
                    campaign_id: self.campaign_id,
                    type_: ClientType::CLIQUECHECKER,
                    status: ClientStatus::ACTIVE,
                    created_date: None,
                    last_phone_home_date: Some(
                        Utc::now()
                            .naive_utc()
                            .format("%Y-%m-%dT%H:%M:%S")
                            .to_string(),
                    ),
                };
                if let Err(e) = self.mw_client.update_client(&hb_client).await {
                    eprintln!("Heartbeat failed: {}", e);
                } else {
                    last_heartbeat = Instant::now();
                }
            }

            let cycle_start = Instant::now();
            match self.cycle().await {
                Ok(count) => {
                    if count == 0 {
                        sleep(self.poll_interval).await;
                    } else {
                        let elapsed_ms = cycle_start.elapsed().as_millis();
                        println!(
                            "[{}] Processed {} work items in {}ms",
                            Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ"),
                            count,
                            elapsed_ms
                        );
                    }
                }
                Err(e) => {
                    eprintln!("Error in worker cycle: {}", e);
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
            println!(
                "[{}] Stage config missing for stage {} - stage may have progressed, refreshing...",
                Utc::now().format("%Y-%m-%dT%H:%M:%S"),
                stage_id
            );
            self.clear_stage_cache();
            return Ok(0); // Return 0 to trigger poll interval, then retry with fresh stage
        }

        self.cycle_counter_based(stage_id).await
    }

    /// Counter-based work cycle: claim index ranges and enumerate locally
    async fn cycle_counter_based(&mut self, stage_id: i32) -> Result<usize, Box<dyn Error>> {
        // Ensure we have stage config cached
        if self.stage_config.is_none() || self.stage_config.as_ref().unwrap().stage_id != stage_id {
            let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;
            if let Some(config) = redis_client.get_stage_config(stage_id).await? {
                println!(
                    "[{}] Loaded stage config: strategy={:?}, totalPairs={}, baseGraphId={}",
                    Utc::now().format("%Y-%m-%dT%H:%M:%S"),
                    config.strategy,
                    config.total_pairs,
                    config.base_graph_id
                );

                // Check if we already have this graph cached (reuse across stages!)
                if !self.graph_cache.contains_key(&config.base_graph_id) {
                    println!(
                        "[{}] Building graph from stage_config (first time for graph {})",
                        Utc::now().format("%Y-%m-%dT%H:%M:%S"),
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
                    println!(
                        "[{}] Reusing cached graph {} for new stage {}",
                        Utc::now().format("%Y-%m-%dT%H:%M:%S"),
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
                println!(
                    "[{}] All work claimed for stage {}, clearing cache...",
                    Utc::now().format("%Y-%m-%dT%H:%M:%S"),
                    stage_id
                );
                self.clear_stage_cache();
                return Ok(0);
            }
        };

        let work_count = (end_index - start_index) as usize;
        let enumerator = self.enumerator.as_ref().unwrap();

        let graph = self.graph_cache.get_mut(&base_graph_id).unwrap();
        let clique_collection = self.clique_collection_cache.get(&base_graph_id).unwrap();

        let mut processed_results: Vec<WorkResult> = Vec::new();

        // Process each work unit in the range
        for idx in start_index..end_index {
            let (red_edge, blue_edge) = enumerator.index_to_edge_pair(idx);
            let edges_to_flip = vec![red_edge, blue_edge];

            let broken = clique_collection.get_count_of_cliques_containing_edges(&edges_to_flip);

            // Early termination when not publishing
            let count = if !self.publish_results {
                graph.flip_edges(&edges_to_flip);
                let (new, exceeded) =
                    get_new_cliques_with_limit(graph, self.clique_size, &edges_to_flip, broken);
                graph.flip_edges(&edges_to_flip);

                if exceeded {
                    continue;
                }
                (clique_collection.total() as i32) - broken + new
            } else {
                graph.flip_edges(&edges_to_flip);
                let (new, _) =
                    get_new_cliques_with_limit(graph, self.clique_size, &edges_to_flip, i32::MAX);
                graph.flip_edges(&edges_to_flip);
                (clique_collection.total() as i32) - broken + new
            };

            // Check if better than base
            if let Some(base_count) = self.base_graph_clique_count {
                if count < base_count {
                    if let Some(redis) = self.redis_client.as_mut() {
                        let _ = redis
                            .update_best_if_better(stage_id, base_graph_id, &edges_to_flip, count)
                            .await;
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

    fn clear_stage_cache(&mut self) {
        self.stage_id = None;
        self.base_graph_clique_count = None;
        self.stage_config = None;
        self.enumerator = None;
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
            eprintln!(
                "Warning: Multiple active stages for campaign {}, using first",
                self.campaign_id
            );
        }

        let stage = &stages[0];
        println!(
            "Using stage {} for campaign {} (base_graph_id: {})",
            stage.stage_id, self.campaign_id, stage.base_graph_id
        );

        let graph_data = self.mw_client.get_graph(stage.base_graph_id).await?;
        self.base_graph_clique_count = graph_data.clique_count;
        println!(
            "Base graph clique count: {:?}",
            self.base_graph_clique_count
        );

        self.stage_id = Some(stage.stage_id);
        Ok(stage.stage_id)
    }
}
