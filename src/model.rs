use crate::graph::WorkUnitEdge;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkUnit {
    pub id: i32,
    #[serde(rename = "baseGraphId")]
    pub base_graph_id: i32,
    #[serde(rename = "stageId")]
    pub stage_id: i32,
    #[serde(rename = "edgesToFlip")]
    pub edges_to_flip: Vec<WorkUnitEdge>,
    pub status: WorkUnitStatus,
    #[serde(rename = "cliqueCount")]
    pub clique_count: Option<i32>,
    #[serde(rename = "assignedClient")]
    pub assigned_client: Option<String>,
    #[serde(rename = "workUnitAnalysisType")]
    pub analysis_type: WorkUnitAnalysisType,
    pub priority: Option<WorkUnitPriority>,
    #[serde(rename = "createdDate")]
    pub created_date: Option<String>,
    #[serde(rename = "assignedDate")]
    pub assigned_date: Option<String>,
    #[serde(rename = "processingStartedDate")]
    pub processing_started_date: Option<String>,
    #[serde(rename = "completedDate")]
    pub completed_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WorkUnitAnalysisType {
    NAIVE,
    COMPREHENSIVE,
    TARGETED,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WorkUnitStatus {
    NEW,
    ASSIGNED,
    COMPLETE,
    ERROR, // Keeping internal error state if needed, but won't send to server
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WorkUnitPriority {
    LOW,
    MEDIUM,
    HIGH,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphData {
    #[serde(rename = "graphId")]
    pub id: i32,
    #[serde(rename = "vertexCount")]
    pub vertex_count: usize,
    #[serde(rename = "edgeData")]
    pub structure_data: String, // bitstring
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Client {
    #[serde(rename = "clientId")]
    pub client_id: Option<i32>,
    #[serde(rename = "campaignId")]
    pub campaign_id: i32,
    #[serde(rename = "type")]
    pub type_: ClientType,
    pub status: ClientStatus,
    #[serde(rename = "createdDate")]
    pub created_date: Option<String>, // ISO8601 string
    #[serde(rename = "lastPhoneHomeDate")]
    pub last_phone_home_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientType {
    CLIQUECHECKER,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientStatus {
    ACTIVE,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Campaign {
    #[serde(rename = "campaignId")]
    pub campaign_id: i32,
    #[serde(rename = "vertexCount")]
    pub vertex_count: i32,
    #[serde(rename = "subgraphSize")]
    pub subgraph_size: i32,
}
