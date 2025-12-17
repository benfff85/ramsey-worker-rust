use crate::algorithm::{get_all_cliques, get_new_cliques};
use crate::client::MiddlewareClient;
use crate::clique_collection::CliqueCollection;
use crate::graph::Graph;
use crate::model::{Client, ClientStatus, ClientType, WorkUnitAnalysisType, WorkUnitStatus};
use chrono::Utc;
use std::collections::HashMap;
use std::error::Error;
use std::time::{Duration, Instant};
use tokio::time::sleep;

pub struct Worker {
    client: MiddlewareClient,
    client_id: Option<i32>,
    clique_size: usize,
    vertex_count: usize,
    graph_cache: HashMap<i32, Graph>,
    clique_collection_cache: HashMap<i32, CliqueCollection>,
    poll_interval: Duration,
    heartbeat_interval: Duration,
    fetch_size: i32,
    publish_size: i32, // Note: batch logic logic in cycle needs update to use this? Currently cycle does fetch-process-publish all in one go for fetched amount
    campaign_id: i32,
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
    ) -> Self {
        Worker {
            client: MiddlewareClient::new(base_url),
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
        }
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

        // In Java it gets campaign info first, updates config, then creates client
        // We will simplify and assume config is passed in or we fetch campaign first

        let campaign = self.client.get_campaign(self.campaign_id).await?;
        println!("Campaign Info: {:?}", campaign);
        self.vertex_count = campaign.vertex_count as usize;
        self.clique_size = campaign.subgraph_size as usize;

        let registered_client = self.client.create_client(&client_data).await?;
        if let Some(id) = registered_client.client_id {
            self.client_id = Some(id);
            println!("Registered with Client ID: {}", id);
        }

        Ok(())
    }

    pub async fn run(&mut self) {
        if let Err(e) = self.register().await {
            eprintln!("Failed to register: {}", e);
            return; // Initial registration failure is fatal
        }

        println!(
            "Worker started for client: {}",
            self.client_id.as_ref().unwrap()
        );

        let mut last_heartbeat = Instant::now();
        // heartbeat_interval is now in self

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
                if let Err(e) = self.client.update_client(&hb_client).await {
                    eprintln!("Heartbeat failed: {}", e);
                } else {
                    last_heartbeat = Instant::now();
                }
            }

            match self.cycle().await {
                Ok(count) => {
                    if count == 0 {
                        // user feedback: sleep briefly
                        sleep(self.poll_interval).await;
                    } else {
                        println!("Processed {} work units", count);
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
        let client_id = self.client_id.expect("Client ID not set");
        let work_units = self
            .client
            .get_work_units(client_id, WorkUnitStatus::ASSIGNED, self.fetch_size)
            .await?;

        if work_units.is_empty() {
            return Ok(0);
        }

        let total_work = work_units.len();
        let mut processed_units = Vec::new();

        for mut unit in work_units {
            // Ensure we have the base graph
            if !self.graph_cache.contains_key(&unit.base_graph_id) {
                let graph_data = self.client.get_graph(unit.base_graph_id).await?;
                let graph =
                    Graph::from_bitstring(&graph_data.structure_data, graph_data.vertex_count);
                self.graph_cache.insert(unit.base_graph_id, graph);
            }

            let graph = self.graph_cache.get_mut(&unit.base_graph_id).unwrap();

            // Replicate logic based on analysis type
            // TARGETED -> get_new_cliques
            // COMPREHENSIVE/NAIVE -> get_cliques_comprehensive

            // Ensure we have the clique collection for this graph
            if !self
                .clique_collection_cache
                .contains_key(&unit.base_graph_id)
            {
                let all_cliques = get_all_cliques(graph, self.clique_size);
                let mut cc = CliqueCollection::new(self.vertex_count);
                cc.set_cliques(all_cliques, self.vertex_count);
                self.clique_collection_cache.insert(unit.base_graph_id, cc);
            }

            let clique_collection = self
                .clique_collection_cache
                .get(&unit.base_graph_id)
                .unwrap();

            let count = match unit.analysis_type {
                WorkUnitAnalysisType::TARGETED => {
                    let broken = clique_collection
                        .get_count_of_cliques_containing_edges(&unit.edges_to_flip);

                    graph.flip_edges(&unit.edges_to_flip);
                    let new = get_new_cliques(graph, self.clique_size, &unit.edges_to_flip);
                    graph.flip_edges(&unit.edges_to_flip); // revert

                    let total = (clique_collection.total() as i32) - broken + new;
                    println!(
                        "DEBUG: Unit {} -> Base: {}, Broken: {}, New: {}, Total: {}",
                        unit.id,
                        clique_collection.total(),
                        broken,
                        new,
                        total
                    );
                    total
                }
                WorkUnitAnalysisType::COMPREHENSIVE | WorkUnitAnalysisType::NAIVE => {
                    graph.flip_edges(&unit.edges_to_flip);
                    let c = crate::algorithm::get_cliques_comprehensive(graph, self.clique_size);
                    graph.flip_edges(&unit.edges_to_flip); // revert
                    c
                }
            };

            unit.clique_count = Some(count);
            unit.status = WorkUnitStatus::COMPLETE;
            unit.completed_date = Some(
                Utc::now()
                    .naive_utc()
                    .format("%Y-%m-%dT%H:%M:%S")
                    .to_string(),
            );
            // Also set processing_started_date if not present, though ideally it should be set when picked up
            if unit.processing_started_date.is_none() {
                unit.processing_started_date = Some(
                    Utc::now()
                        .naive_utc()
                        .format("%Y-%m-%dT%H:%M:%S")
                        .to_string(),
                );
            }

            processed_units.push(unit);

            // Check if we reached publish batch size
            if processed_units.len() >= self.publish_size as usize {
                self.client.update_work_units(&processed_units).await?;
                processed_units.clear();
            }
        }

        if !processed_units.is_empty() {
            self.client.update_work_units(&processed_units).await?;
        }

        Ok(total_work)
    }
}
