//! Regression suite: JSON wire contracts with the middleware and the Redis
//! stage config. The Java side renames everything to camelCase; these tests
//! pin the exact field names so a serde annotation regression cannot silently
//! break worker <-> middleware communication.

use ramsey_worker_rust::graph::WorkUnitEdge;
use ramsey_worker_rust::model::{
    GraphData, StageConfig, WorkEnumerationStrategy, WorkResult, WorkUnitAnalysisType,
};
use serde_json::{json, Value};

#[test]
fn work_unit_edge_uses_camel_case_field_names() {
    let edge = WorkUnitEdge {
        vertex_one: 53,
        vertex_two: 167,
    };
    let v: Value = serde_json::to_value(&edge).unwrap();
    assert_eq!(v, json!({"vertexOne": 53, "vertexTwo": 167}));
}

#[test]
fn work_result_matches_java_entity_contract() {
    let result = WorkResult {
        id: None,
        base_graph_id: 8319,
        stage_id: 8319,
        edges_to_flip: vec![
            WorkUnitEdge {
                vertex_one: 17,
                vertex_two: 170,
            },
            WorkUnitEdge {
                vertex_one: 53,
                vertex_two: 167,
            },
        ],
        clique_count: 775_842,
        work_unit_analysis_type: WorkUnitAnalysisType::TARGETED,
    };
    let v: Value = serde_json::to_value(&result).unwrap();

    // id is skipped when None (the middleware assigns it).
    assert!(v.get("id").is_none());
    assert_eq!(v["baseGraphId"], 8319);
    assert_eq!(v["stageId"], 8319);
    assert_eq!(v["cliqueCount"], 775_842);
    assert_eq!(v["workUnitAnalysisType"], "TARGETED");
    assert_eq!(v["edgesToFlip"][0]["vertexOne"], 17);
    assert_eq!(v["edgesToFlip"][1]["vertexTwo"], 167);
}

#[test]
fn stage_config_parses_the_redis_payload_shape() {
    // Mirrors the JSON the queue manager writes to stage_config:{stageId}.
    let payload = r#"{
        "stageId": 8319,
        "baseGraphId": 8319,
        "strategy": "DUAL_EDGE_CARDINALITY",
        "totalPairs": 392455910,
        "graph": {"vertexCount": 6, "edgeData": "110101101010101"}
    }"#;
    let config: StageConfig = serde_json::from_str(payload).unwrap();
    assert_eq!(config.stage_id, 8319);
    assert_eq!(config.base_graph_id, 8319);
    assert_eq!(
        config.strategy,
        WorkEnumerationStrategy::DUAL_EDGE_CARDINALITY
    );
    assert_eq!(config.total_pairs, 392_455_910);
    assert_eq!(config.graph.vertex_count, 6);
    assert_eq!(config.graph.edge_data.len(), 15);

    // Round-trip preserves the camelCase keys.
    let v: Value = serde_json::to_value(&config).unwrap();
    assert!(v.get("stageId").is_some());
    assert!(v.get("baseGraphId").is_some());
    assert!(v.get("totalPairs").is_some());
    assert!(v["graph"].get("vertexCount").is_some());
    assert!(v["graph"].get("edgeData").is_some());
}

#[test]
fn graph_data_parses_the_middleware_response_shape() {
    let payload = r#"{
        "graphId": 8319,
        "vertexCount": 282,
        "edgeData": "0101",
        "cliqueCount": 775842
    }"#;
    let graph: GraphData = serde_json::from_str(payload).unwrap();
    assert_eq!(graph.id, 8319);
    assert_eq!(graph.vertex_count, 282);
    assert_eq!(graph.clique_count, Some(775_842));

    // cliqueCount is optional in the contract.
    let no_count =
        r#"{"graphId": 1, "vertexCount": 5, "edgeData": "1111111111", "cliqueCount": null}"#;
    let graph: GraphData = serde_json::from_str(no_count).unwrap();
    assert_eq!(graph.clique_count, None);
}
