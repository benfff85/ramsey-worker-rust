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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WorkUnitAnalysisType {
    NAIVE,
    COMPREHENSIVE,
    TARGETED,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WorkUnitStatus {
    CREATED,
    ASSIGNED,
    COMPLETED,
    ERROR,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphData {
    pub id: i32,
    #[serde(rename = "vertexCount")]
    pub vertex_count: usize,
    #[serde(rename = "structureData")]
    pub structure_data: String, // bitstring
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Client {
    #[serde(rename = "clientId")]
    pub client_id: Option<String>,
    #[serde(rename = "campaignId")]
    pub campaign_id: i32,
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
