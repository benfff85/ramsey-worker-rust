use crate::algorithm::{get_all_cliques, get_new_cliques};
use crate::client::MiddlewareClient;
use crate::clique_collection::CliqueCollection;
use crate::graph::Graph;
use crate::model::{Client, ClientStatus, ClientType, WorkResult, WorkUnitAnalysisType};
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
        // Get stage_id first (before borrowing redis_client)
        let stage_id = self.get_or_fetch_stage_id().await?;
        let fetch_size = self.fetch_size as usize;

        let redis_client = self.redis_client.as_mut().ok_or("Redis not connected")?;

        // Pop work items from Redis
        let work_items = redis_client.pop_work_items(stage_id, fetch_size).await?;

        if work_items.is_empty() {
            println!(
                "[{}] No work items available in Redis queue for stage {}, clearing cache to check for new stage...",
                Utc::now().format("%Y-%m-%dT%H:%M:%S"),
                stage_id
            );
            // Clear cached stage to force re-fetch on next cycle
            // This allows detecting stage progression
            self.stage_id = None;
            self.base_graph_clique_count = None;
            return Ok(0);
        }

        let total_work = work_items.len();
        let work_stage_id = stage_id; // stage_id comes from MW API, not work item
        let mut processed_results: Vec<WorkResult> = Vec::new();

        for item in work_items {
            // Ensure we have the base graph
            if !self.graph_cache.contains_key(&item.base_graph_id) {
                let graph_data = self.mw_client.get_graph(item.base_graph_id).await?;
                let graph =
                    Graph::from_bitstring(&graph_data.structure_data, graph_data.vertex_count);
                self.graph_cache.insert(item.base_graph_id, graph);
            }

            let graph = self.graph_cache.get_mut(&item.base_graph_id).unwrap();

            // Ensure we have the clique collection for this graph
            if !self
                .clique_collection_cache
                .contains_key(&item.base_graph_id)
            {
                let all_cliques = get_all_cliques(graph, self.clique_size);
                let mut cc = CliqueCollection::new(self.vertex_count);
                cc.set_cliques(all_cliques, self.vertex_count);
                self.clique_collection_cache.insert(item.base_graph_id, cc);
            }

            let clique_collection = self
                .clique_collection_cache
                .get(&item.base_graph_id)
                .unwrap();

            let count = match item.analysis_type {
                WorkUnitAnalysisType::TARGETED => {
                    let broken = clique_collection
                        .get_count_of_cliques_containing_edges(&item.edges_to_flip);

                    graph.flip_edges(&item.edges_to_flip);
                    let new = get_new_cliques(graph, self.clique_size, &item.edges_to_flip);
                    graph.flip_edges(&item.edges_to_flip); // revert

                    let total = (clique_collection.total() as i32) - broken + new;
                    total
                }
                WorkUnitAnalysisType::COMPREHENSIVE | WorkUnitAnalysisType::NAIVE => {
                    graph.flip_edges(&item.edges_to_flip);
                    let c = crate::algorithm::get_cliques_comprehensive(graph, self.clique_size);
                    graph.flip_edges(&item.edges_to_flip); // revert
                    c
                }
            };

            // Create WorkResult for submission
            let result = WorkResult {
                id: None,
                base_graph_id: item.base_graph_id,
                stage_id: work_stage_id, // Use stage_id from MW API
                edges_to_flip: item.edges_to_flip.clone(),
                clique_count: count,
                work_unit_analysis_type: item.analysis_type,
            };

            // Check if this result is better than the base graph
            if let Some(base_count) = self.base_graph_clique_count {
                if count < base_count {
                    // Update best result in Redis
                    if let Some(redis) = self.redis_client.as_mut() {
                        let _ = redis
                            .update_best_if_better(
                                work_stage_id, // Use stage_id from MW API
                                item.base_graph_id,
                                &item.edges_to_flip,
                                count,
                            )
                            .await;
                    }
                }
            }

            // Only collect results if publishing is enabled
            if self.publish_results {
                processed_results.push(result);

                // Submit batch if we reached publish size
                if processed_results.len() >= self.publish_size as usize {
                    self.mw_client.submit_results(&processed_results).await?;
                    processed_results.clear();
                }
            }
        }

        // Submit remaining results (only if publishing enabled)
        if self.publish_results && !processed_results.is_empty() {
            self.mw_client.submit_results(&processed_results).await?;
        }

        // Always update processed count in Redis
        if total_work > 0 {
            if let Some(redis) = self.redis_client.as_mut() {
                let _ = redis
                    .increment_processed_count(work_stage_id, total_work as i64)
                    .await;
            }
        }

        Ok(total_work)
    }

    /// Get or fetch the stage ID for the current campaign
    async fn get_or_fetch_stage_id(&mut self) -> Result<i32, Box<dyn Error>> {
        if let Some(stage_id) = self.stage_id {
            return Ok(stage_id);
        }

        // Fetch the active stage for this campaign
        let stages = self
            .mw_client
            .get_stages_by_campaign(self.campaign_id, "ACTIVE")
            .await?;

        if stages.is_empty() {
            return Err("No active stage found for campaign".into());
        }

        if stages.len() > 1 {
            eprintln!(
                "Warning: Multiple active stages found for campaign {}, using first one",
                self.campaign_id
            );
        }

        let stage = &stages[0];
        println!(
            "Using stage {} for campaign {} (base_graph_id: {})",
            stage.stage_id, self.campaign_id, stage.base_graph_id
        );

        // Fetch the base graph to get its clique count
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
