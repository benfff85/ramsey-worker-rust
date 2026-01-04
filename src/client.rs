use crate::model::{Campaign, Client, GraphData, WorkResult, WorkUnit, WorkUnitStatus};
use reqwest::{Client as HttpClient, StatusCode};
use std::error::Error;
use std::time::Duration;

pub struct MiddlewareClient {
    client: HttpClient,
    base_url: String, // e.g. http://localhost:8080 or from config
                      // Cache map for graphs if needed? Or just fetch on demand
}

impl MiddlewareClient {
    pub fn new(base_url: String) -> Self {
        let client = HttpClient::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| HttpClient::new());

        MiddlewareClient { client, base_url }
    }

    pub async fn get_work_units(
        &self,
        client_id: i32,
        status: WorkUnitStatus,
        fetch_size: i32,
    ) -> Result<Vec<WorkUnit>, Box<dyn Error>> {
        let url = format!(
            "{}/work-units?assignedClientId={}&status={:?}&pageSize={}",
            self.base_url, client_id, status, fetch_size
        );

        let response: reqwest::Response = self.client.get(&url).send().await?;

        if response.status() == StatusCode::NO_CONTENT {
            return Ok(vec![]);
        }

        let work_units = response.json::<Vec<WorkUnit>>().await?;
        Ok(work_units)
    }

    pub async fn update_work_units(&self, work_units: &[WorkUnit]) -> Result<(), Box<dyn Error>> {
        let url = format!("{}/work-units", self.base_url); // PUT endpoint

        let json_payload = serde_json::to_string(work_units)?;

        let response = self
            .client
            .put(&url)
            .header("Content-Type", "application/json")
            .body(json_payload)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            eprintln!("DEBUG: Failed to update work units: {} - {}", status, text);
            return Err(format!("Failed to update work units: {} - {}", status, text).into());
        }
        Ok(())
    }

    /// Submit work results to the new /api/ramsey/results endpoint
    pub async fn submit_results(&self, results: &[WorkResult]) -> Result<(), Box<dyn Error>> {
        let url = format!("{}/results", self.base_url);

        let response = self.client.post(&url).json(results).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            eprintln!("DEBUG: Failed to submit results: {} - {}", status, text);
            return Err(format!("Failed to submit results: {} - {}", status, text).into());
        }
        Ok(())
    }

    pub async fn get_graph(&self, graph_id: i32) -> Result<GraphData, Box<dyn Error>> {
        let url = format!("{}/graphs/{}", self.base_url, graph_id);
        let graph_data = self
            .client
            .get(&url)
            .send()
            .await?
            .json::<GraphData>()
            .await?;
        Ok(graph_data)
    }

    pub async fn get_campaign(&self, campaign_id: i32) -> Result<Campaign, Box<dyn Error>> {
        let url = format!("{}/campaigns/{}", self.base_url, campaign_id);
        let campaign = self
            .client
            .get(&url)
            .send()
            .await?
            .json::<Campaign>()
            .await?;
        Ok(campaign)
    }

    /// Get stages by campaign ID and status
    pub async fn get_stages_by_campaign(
        &self,
        campaign_id: i32,
        status: &str,
    ) -> Result<Vec<crate::model::Stage>, Box<dyn Error>> {
        let url = format!(
            "{}/stages?campaignId={}&status={}",
            self.base_url, campaign_id, status
        );

        let response: reqwest::Response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            let status_code = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("Failed to get stages: {} - {}", status_code, text).into());
        }

        let stages = response.json::<Vec<crate::model::Stage>>().await?;
        Ok(stages)
    }

    pub async fn create_client(&self, client: &Client) -> Result<Client, Box<dyn Error>> {
        let url = format!("{}/clients", self.base_url);
        let response = self.client.post(&url).json(client).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            eprintln!("DEBUG: Server returned error: {} - {}", status, text);
            return Err(format!("Server error: {} - {}", status, text).into());
        }

        let created_client = response.json::<Client>().await?;
        Ok(created_client)
    }

    pub async fn update_client(&self, client: &Client) -> Result<(), Box<dyn Error>> {
        if let Some(id) = &client.client_id {
            let url = format!("{}/clients/{}", self.base_url, id);
            let response = self.client.put(&url).json(client).send().await?;
            if !response.status().is_success() {
                return Err(format!("Failed to update client: {}", response.status()).into());
            }
        }
        Ok(())
    }
}
