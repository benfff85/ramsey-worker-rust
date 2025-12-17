use crate::algorithm::get_new_cliques;
use crate::client::MiddlewareClient;
use crate::graph::Graph;
use crate::model::{Client, ClientStatus, ClientType, WorkUnitAnalysisType, WorkUnitStatus};
use chrono::Utc;
use std::collections::HashMap;
use std::error::Error;
use std::time::{Duration, Instant};
use tokio::time::sleep;

pub struct Worker {
    client: MiddlewareClient,
    client_id: Option<String>,
    clique_size: usize,
    vertex_count: usize,
    graph_cache: HashMap<i32, Graph>,
    poll_interval: Duration,
    campaign_id: i32,
}

impl Worker {
    pub fn new(
        base_url: String,
        vertex_count: usize,
        clique_size: usize,
        campaign_id: i32,
    ) -> Self {
        Worker {
            client: MiddlewareClient::new(base_url),
            client_id: None,
            vertex_count,
            clique_size,
            graph_cache: HashMap::new(),
            poll_interval: Duration::from_secs(5),
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
            created_date: Some(Utc::now().to_rfc3339()),
            last_phone_home_date: Some(Utc::now().to_rfc3339()),
        };

        // In Java it gets campaign info first, updates config, then creates client
        // We will simplify and assume config is passed in or we fetch campaign first

        let campaign = self.client.get_campaign(self.campaign_id).await?;
        println!("Campaign Info: {:?}", campaign);
        self.vertex_count = campaign.vertex_count as usize;
        self.clique_size = campaign.subgraph_size as usize;

        let registered_client = self.client.create_client(&client_data).await?;
        if let Some(id) = registered_client.client_id {
            self.client_id = Some(id.clone());
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
        let heartbeat_interval = Duration::from_secs(30);

        loop {
            // Heartbeat check
            if last_heartbeat.elapsed() > heartbeat_interval {
                let hb_client = Client {
                    client_id: self.client_id.clone(),
                    campaign_id: self.campaign_id,
                    type_: ClientType::CLIQUECHECKER,
                    status: ClientStatus::ACTIVE,
                    created_date: None,
                    last_phone_home_date: Some(Utc::now().to_rfc3339()),
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
        let client_id = self.client_id.as_ref().expect("Client ID not set").clone();
        let work_units = self
            .client
            .get_work_units(&client_id, WorkUnitStatus::ASSIGNED, 100)
            .await?;

        if work_units.is_empty() {
            return Ok(0);
        }

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

            let count = match unit.analysis_type {
                WorkUnitAnalysisType::TARGETED => {
                    get_new_cliques(graph, self.clique_size, &unit.edges_to_flip)
                }
                WorkUnitAnalysisType::COMPREHENSIVE | WorkUnitAnalysisType::NAIVE => {
                    // For comprehensive, we might need to apply the flips first?
                    // Java ComprehensiveWorkUnitProcessor does logic: flip -> check -> return count
                    // Since specific edges to flip might be part of the work unit even for comprehensive checks (e.g. verifying a state)
                    graph.flip_edges(&unit.edges_to_flip);
                    let c = crate::algorithm::get_cliques_comprehensive(graph, self.clique_size);
                    graph.flip_edges(&unit.edges_to_flip); // revert back for cache consistency if needed
                    c
                }
            };

            unit.clique_count = Some(count);
            unit.status = WorkUnitStatus::COMPLETED;

            // Logic for completion date etc could be added here or handled by server

            processed_units.push(unit);
        }

        if !processed_units.is_empty() {
            self.client.update_work_units(&processed_units).await?;
        }

        Ok(processed_units.len())
    }
}
