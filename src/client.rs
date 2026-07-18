use crate::log_error;
use crate::model::{Campaign, GraphData, WorkResult};
use reqwest::Client as HttpClient;
use std::error::Error;
use std::time::Duration;
use tokio::time::sleep;

const MAX_RETRIES: u32 = 3;
const INITIAL_RETRY_DELAY_MS: u64 = 1000;
const REQUEST_TIMEOUT_SECS: u64 = 30;
const CONNECT_TIMEOUT_SECS: u64 = 15;

pub struct MiddlewareClient {
    client: HttpClient,
    base_url: String,
}

impl MiddlewareClient {
    pub fn new(base_url: String) -> Self {
        let client = HttpClient::builder()
            .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(2)
            .tcp_keepalive(Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| HttpClient::new());

        MiddlewareClient { client, base_url }
    }

    /// Retry helper with exponential backoff for transient failures
    async fn retry_async<T, F, Fut>(
        &self,
        operation_name: &str,
        mut operation: F,
    ) -> Result<T, Box<dyn Error>>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, Box<dyn Error>>>,
    {
        let mut last_error: Option<Box<dyn Error>> = None;

        for attempt in 0..MAX_RETRIES {
            match operation().await {
                Ok(result) => return Ok(result),
                Err(e) => {
                    let delay = INITIAL_RETRY_DELAY_MS * 2u64.pow(attempt);
                    log_error!(
                        "{} failed (attempt {}/{}): {}. Retrying in {}ms...",
                        operation_name,
                        attempt + 1,
                        MAX_RETRIES,
                        e,
                        delay
                    );
                    last_error = Some(e);
                    sleep(Duration::from_millis(delay)).await;
                }
            }
        }

        Err(last_error.unwrap_or_else(|| "Unknown error after retries".into()))
    }

    /// Submit work results to the new /api/ramsey/results endpoint
    pub async fn submit_results(&self, results: &[WorkResult]) -> Result<(), Box<dyn Error>> {
        let url = format!("{}/results", self.base_url);

        let response = self.client.post(&url).json(results).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            log_error!("DEBUG: Failed to submit results: {} - {}", status, text);
            return Err(format!("Failed to submit results: {} - {}", status, text).into());
        }
        Ok(())
    }

    pub async fn get_graph(&self, graph_id: i32) -> Result<GraphData, Box<dyn Error>> {
        let url = format!("{}/graphs/{}", self.base_url, graph_id);
        let client = self.client.clone();

        self.retry_async("get_graph", || {
            let url = url.clone();
            let client = client.clone();
            async move {
                let graph_data = client.get(&url).send().await?.json::<GraphData>().await?;
                Ok(graph_data)
            }
        })
        .await
    }

    pub async fn get_campaign(&self, campaign_id: i32) -> Result<Campaign, Box<dyn Error>> {
        let url = format!("{}/campaigns/{}", self.base_url, campaign_id);
        let client = self.client.clone();

        self.retry_async("get_campaign", || {
            let url = url.clone();
            let client = client.clone();
            async move {
                let campaign = client.get(&url).send().await?.json::<Campaign>().await?;
                Ok(campaign)
            }
        })
        .await
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
        let client = self.client.clone();

        self.retry_async("get_stages_by_campaign", || {
            let url = url.clone();
            let client = client.clone();
            async move {
                let response = client.get(&url).send().await?;

                if !response.status().is_success() {
                    let status_code = response.status();
                    let text = response.text().await.unwrap_or_default();
                    return Err(format!("Failed to get stages: {} - {}", status_code, text).into());
                }

                let stages = response.json::<Vec<crate::model::Stage>>().await?;
                Ok(stages)
            }
        })
        .await
    }

    /// Resolve a fleet's active stage (fleet abstraction — see
    /// docs/investigations/fleet-abstraction-plan.md in ramsey-mw).
    /// 200 -> Some(stage); 204 -> None (paused / unmapped / no active stage -> idle);
    /// other -> error.
    pub async fn get_fleet_active_stage(
        &self,
        platform: &str,
    ) -> Result<Option<crate::model::Stage>, Box<dyn Error>> {
        let url = format!("{}/fleets/{}/active-stage", self.base_url, platform);
        let client = self.client.clone();

        self.retry_async("get_fleet_active_stage", || {
            let url = url.clone();
            let client = client.clone();
            async move {
                let response = client.get(&url).send().await?;
                let status = response.status();
                if status.as_u16() == 204 {
                    return Ok(None);
                }
                if !status.is_success() {
                    let text = response.text().await.unwrap_or_default();
                    return Err(
                        format!("Failed to resolve fleet stage: {} - {}", status, text).into(),
                    );
                }
                let stage = response.json::<crate::model::Stage>().await?;
                Ok(Some(stage))
            }
        })
        .await
    }

}
