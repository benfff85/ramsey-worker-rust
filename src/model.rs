use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WorkUnitAnalysisType {
    NAIVE,
    COMPREHENSIVE,
    TARGETED,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphData {
    #[serde(rename = "graphId")]
    pub id: i32,
    #[serde(rename = "vertexCount")]
    pub vertex_count: usize,
    #[serde(rename = "edgeData")]
    pub structure_data: String, // bitstring
    #[serde(rename = "cliqueCount")]
    pub clique_count: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Campaign {
    #[serde(rename = "campaignId")]
    pub campaign_id: i32,
    #[serde(rename = "vertexCount")]
    pub vertex_count: i32,
    #[serde(rename = "subgraphSize")]
    pub subgraph_size: i32,
    #[serde(rename = "totalPairs")]
    pub total_pairs: Option<i64>,
}

/// Work result - simplified structure for submitting processing results
/// Matches the Java WorkResult entity
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    #[serde(rename = "baseGraphId")]
    pub base_graph_id: i32,
    #[serde(rename = "stageId")]
    pub stage_id: i32,
    #[serde(rename = "edgesToFlip")]
    pub edges_to_flip: Vec<crate::graph::WorkUnitEdge>,
    #[serde(rename = "cliqueCount")]
    pub clique_count: i32,
    #[serde(rename = "workUnitAnalysisType")]
    pub work_unit_analysis_type: WorkUnitAnalysisType,
}

/// Stage - represents a stage in a campaign
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stage {
    #[serde(rename = "stageId")]
    pub stage_id: i32,
    pub status: StageStatus,
    #[serde(rename = "baseGraphId")]
    pub base_graph_id: i32,
    #[serde(rename = "campaignId")]
    pub campaign_id: i32,
    #[serde(rename = "latestWorkUnitId")]
    pub latest_work_unit_id: Option<i32>,
    #[serde(rename = "workEnumerationStrategy")]
    pub work_enumeration_strategy: Option<WorkEnumerationStrategy>,
    #[serde(rename = "details")]
    pub details: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum StageStatus {
    ACTIVE,
    INACTIVE,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum WorkEnumerationStrategy {
    BASIC,
    SINGLE_EDGE_CARDINALITY,
    DUAL_EDGE_CARDINALITY,
    DUAL_EDGE_CARDINALITY_WITH_SINGLES,
}

/// Stage configuration stored in Redis for counter-based work
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageConfig {
    #[serde(rename = "stageId")]
    pub stage_id: i32,
    #[serde(rename = "baseGraphId")]
    pub base_graph_id: i32,
    pub strategy: WorkEnumerationStrategy,
    #[serde(rename = "totalPairs")]
    pub total_pairs: i64,
    pub graph: GraphSnapshot,
}

/// Graph snapshot for embedding in StageConfig
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSnapshot {
    #[serde(rename = "vertexCount")]
    pub vertex_count: usize,
    #[serde(rename = "edgeData")]
    pub edge_data: String,
}
