//! Generic graph queries and analytics over HakoDB reference edges.
//!
//! All query parameters are caller supplied (collection, via edge, direction,
//! depth, projection). Responses are normalized: references are resolved to
//! `{collection,id,data}` objects so clients never dereference `__ref__`.

use crate::error::{Error, Result};
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::engine::{Hako, TraverseHit};
use hakodb::index::traverse::{TraverseDir, TraverseSpec};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as Json};

/// Graph node key: (collection, document id).
type GraphNode = (String, String);

fn default_collection() -> String {
    "symbols".into()
}

fn default_via() -> String {
    "edges".into()
}

fn default_depth() -> usize {
    2
}

fn default_limit() -> usize {
    200
}

fn default_damping() -> f64 {
    0.85
}

fn default_top() -> usize {
    20
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Outgoing,
    Incoming,
    #[default]
    Both,
}

impl From<Direction> for TraverseDir {
    fn from(value: Direction) -> Self {
        match value {
            Direction::Outgoing => TraverseDir::Outgoing,
            Direction::Incoming => TraverseDir::Incoming,
            Direction::Both => TraverseDir::Both,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GraphRequest {
    #[serde(default = "default_collection")]
    pub collection: String,
    /// Seed as `collection/id` or just `id` (uses `collection`).
    #[serde(default)]
    pub seed: Option<String>,
    #[serde(default = "default_via")]
    pub via: String,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default = "default_depth")]
    pub max_depth: usize,
    #[serde(default = "default_limit")]
    pub limit: usize,
    #[serde(default)]
    pub edge_type: Option<String>,
    /// Fields to keep for node projection; empty = all except `emb`.
    #[serde(default)]
    pub projection: Vec<String>,
    #[serde(default = "default_damping")]
    pub damping: f64,
    #[serde(default = "default_top")]
    pub top: usize,
}

impl Default for GraphRequest {
    fn default() -> Self {
        Self {
            collection: default_collection(),
            seed: None,
            via: default_via(),
            direction: Direction::Both,
            max_depth: default_depth(),
            limit: default_limit(),
            edge_type: None,
            projection: Vec::new(),
            damping: default_damping(),
            top: default_top(),
        }
    }
}

fn spec(request: &GraphRequest, include_path: bool) -> Result<TraverseSpec> {
    if request.max_depth == 0 || request.max_depth > 256 {
        return Err(Error::InvalidInput("max_depth must be 1..=256".into()));
    }
    if request.limit == 0 || request.limit > 100_000 {
        return Err(Error::InvalidInput("limit must be 1..=100000".into()));
    }
    Ok(TraverseSpec {
        direction: request.direction.into(),
        via_edge: Some(request.via.clone()),
        max_depth: request.max_depth,
        limit: request.limit,
        edge_field: None,
        weight_field: None,
        node_match: None,
        edge_type: request
            .edge_type
            .as_ref()
            .map(|kind| ("type".to_string(), Value::String(kind.clone()))),
        edge_match: None,
        include_path,
        include_via: include_path,
    })
}

fn seed_node(request: &GraphRequest) -> Result<GraphNode> {
    let seed = request.seed.as_ref().ok_or_else(|| {
        Error::InvalidInput("seed is required (collection/id or id)".into())
    })?;
    if let Some((collection, id)) = seed.split_once('/') {
        Ok((collection.to_string(), id.to_string()))
    } else {
        Ok((request.collection.clone(), seed.clone()))
    }
}

fn doc_json(doc: &HakoDoc, projection: &[String]) -> Json {
    let mut map = serde_json::Map::new();
    for (field, value) in doc.fields.iter() {
        if field.as_ref() == "emb" {
            continue;
        }
        if !projection.is_empty()
            && !projection.iter().any(|name| name.as_str() == field.as_ref())
        {
            continue;
        }
        map.insert(field.to_string(), value.to_json());
    }
    Json::Object(map)
}

fn node_view(db: &Hako, node: &GraphNode, projection: &[String], depth: usize) -> Json {
    let data = db
        .get(&node.0, &node.1)
        .ok()
        .flatten()
        .map(|doc| normalize_refs(db, doc_json(&doc, projection), depth + 1))
        .unwrap_or(Json::Null);
    json!({ "collection": node.0, "id": node.1, "data": data })
}

/// Resolve `{"__ref__": "collection/id"}` recursively, including refs nested
/// in arrays/objects. A bounded depth prevents pathological self-reference;
/// at the cap the reference becomes a plain `{collection,id}` stub, never a
/// raw `__ref__` object.
fn normalize_refs(db: &Hako, value: Json, depth: usize) -> Json {
    match value {
        Json::Object(mut map) => {
            if map.len() == 1 {
                if let Some(Json::String(reference)) = map.get("__ref__") {
                    if let Some((collection, id)) = reference.split_once('/') {
                        if depth > 4 {
                            return json!({ "collection": collection, "id": id, "data": null });
                        }
                        return node_view(db, &(collection.to_string(), id.to_string()), &[], depth);
                    }
                }
            }
            if depth <= 4 {
                for entry in map.values_mut() {
                    *entry = normalize_refs(db, entry.clone(), depth + 1);
                }
            }
            Json::Object(map)
        }
        Json::Array(items) if depth <= 4 => Json::Array(
            items
                .into_iter()
                .map(|item| normalize_refs(db, item, depth + 1))
                .collect(),
        ),
        other => other,
    }
}

fn relation_from_via(db: &Hako, via: &str) -> Option<Json> {
    let (collection, id) = via.split_once('/')?;
    let doc = db.get(collection, id).ok().flatten()?;
    let from = match doc.get("_from") {
        Some(Value::Reference { collection, doc_id }) => {
            (collection.clone(), doc_id.clone())
        }
        _ => return None,
    };
    let to = match doc.get("_to") {
        Some(Value::Reference { collection, doc_id }) => {
            (collection.clone(), doc_id.clone())
        }
        _ => return None,
    };
    Some(json!({
        "collection": collection,
        "id": id,
        "from": node_view(db, &from, &[], 0),
        "to": node_view(db, &to, &[], 0),
        "data": normalize_refs(db, doc_json(&doc, &[]), 0),
    }))
}

/// Build `{entities, relations}` from a set of traversal hits.
fn graph_view<'a>(
    db: &Hako,
    hits: impl IntoIterator<Item = &'a TraverseHit>,
    projection: &[String],
) -> Json {
    let mut entities: Vec<Json> = Vec::new();
    let mut relations: Vec<Json> = Vec::new();
    let mut seen_nodes = std::collections::HashSet::new();
    let mut seen_edges = std::collections::HashSet::new();
    for hit in hits {
        for node in &hit.path {
            if seen_nodes.insert(node.clone()) {
                entities.push(node_view(db, node, projection, 0));
            }
        }
        for via in &hit.via {
            let Some(via) = via else { continue };
            if !seen_edges.insert(via.clone()) {
                continue;
            }
            if let Some(relation) = relation_from_via(db, via) {
                relations.push(relation);
            }
        }
        if seen_nodes.insert((hit.collection.clone(), hit.id.clone())) {
            entities.push(node_view(db, &(hit.collection.clone(), hit.id.clone()), projection, 0));
        }
    }
    json!({ "entities": entities, "relations": relations })
}

pub fn rank(db: &Hako, request: &GraphRequest) -> Result<Json> {
    let spec = spec(request, false)?;
    let seeds = vec![seed_node(request)?];
    let rows = db
        .graph_pagerank(&request.collection, &seeds, &spec, request.damping, request.top)
        .map_err(|error| Error::Storage(error.to_string()))?;
    let ranks: Vec<Json> = rows
        .iter()
        .map(|((collection, id), score)| json!({ "collection": collection, "id": id, "score": score }))
        .collect();
    Ok(json!({ "ranks": ranks, "algorithm": "pagerank" }))
}

pub fn community(db: &Hako, request: &GraphRequest) -> Result<Json> {
    let spec = spec(request, false)?;
    let communities = db
        .graph_communities(&request.collection, &spec)
        .map_err(|error| Error::Storage(error.to_string()))?;
    let groups: Vec<Json> = communities
        .iter()
        .map(|group| {
            let nodes: Vec<Json> = group.iter().map(|node| node_view(db, node, &request.projection, 0)).collect();
            json!({ "size": nodes.len(), "nodes": nodes })
        })
        .collect();
    Ok(json!({ "communities": groups, "algorithm": "label-propagation" }))
}

pub fn components(db: &Hako, request: &GraphRequest) -> Result<Json> {
    let spec = spec(request, false)?;
    let groups = db
        .graph_components(&request.collection, &spec)
        .map_err(|error| Error::Storage(error.to_string()))?;
    let result: Vec<Json> = groups
        .iter()
        .map(|group| {
            let nodes: Vec<Json> = group.iter().map(|node| node_view(db, node, &request.projection, 0)).collect();
            json!({ "size": nodes.len(), "nodes": nodes })
        })
        .collect();
    Ok(json!({ "components": result }))
}

pub fn cycle(db: &Hako, request: &GraphRequest) -> Result<Json> {
    let spec = spec(request, false)?;
    let cycle = db
        .graph_find_cycle(&request.collection, &spec)
        .map_err(|error| Error::Storage(error.to_string()))?;
    match cycle {
        Some(path) => {
            let nodes: Vec<Json> = path.iter().map(|node| node_view(db, node, &request.projection, 0)).collect();
            Ok(json!({ "cycle": nodes }))
        }
        None => Ok(json!({ "cycle": [] })),
    }
}

pub fn topo(db: &Hako, request: &GraphRequest) -> Result<Json> {
    let spec = spec(request, false)?;
    let order = db
        .graph_topo_sort(&request.collection, &spec)
        .map_err(|error| Error::Storage(error.to_string()))?;
    match order {
        Some(path) => {
            let nodes: Vec<Json> = path.iter().map(|node| node_view(db, node, &request.projection, 0)).collect();
            Ok(json!({ "order": nodes, "acyclic": true }))
        }
        None => Ok(json!({ "order": [], "acyclic": false })),
    }
}

pub fn degrees(db: &Hako, request: &GraphRequest) -> Result<Json> {
    let spec = spec(request, false)?;
    let rows = db
        .graph_degree_centrality(&request.collection, &spec)
        .map_err(|error| Error::Storage(error.to_string()))?;
    let result: Vec<Json> = rows
        .iter()
        .map(|((collection, id), incoming, outgoing)| {
            json!({
                "collection": collection, "id": id,
                "incoming": incoming, "outgoing": outgoing,
                "data": node_view(db, &(collection.clone(), id.clone()), &request.projection, 0)["data"],
            })
        })
        .collect();
    Ok(json!({ "degrees": result }))
}

pub fn path(db: &Hako, request: &GraphRequest, target: &str) -> Result<Json> {
    let spec = spec(request, true)?;
    let seed = seed_node(request)?;
    let target_node = if let Some((collection, id)) = target.split_once('/') {
        (collection.to_string(), id.to_string())
    } else {
        (request.collection.clone(), target.to_string())
    };
    let hits = db
        .find_simple_paths(&request.collection, &seed.1, &spec)
        .map_err(|error| Error::Storage(error.to_string()))?;
    let matching: Vec<_> = hits
        .iter()
        .filter(|hit| (hit.collection.clone(), hit.id.clone()) == target_node)
        .collect();
    Ok(graph_view(db, matching, &request.projection))
}

pub fn subgraph(db: &Hako, request: &GraphRequest) -> Result<Json> {
    let spec = spec(request, true)?;
    let seed = seed_node(request)?;
    let hits = db
        .find_simple_paths(&seed.0, &seed.1, &spec)
        .map_err(|error| Error::Storage(error.to_string()))?;
    Ok(graph_view(db, hits.iter(), &request.projection))
}

pub fn export(db: &Hako, request: &GraphRequest) -> Result<Json> {
    let spec = spec(request, true)?;
    let raw = db
        .export_graph_json(&request.collection, &spec)
        .map_err(|error| Error::Storage(error.to_string()))?;
    let value: Json = serde_json::from_str(&raw).map_err(|error| Error::Storage(error.to_string()))?;
    let normalized = normalize_refs(db, value, 0);
    // Engine shape is `{nodes, edges}`; expose the same normalized contract
    // as subgraph so clients do not need two parsers.
    let stub = |reference: &str| -> Json {
        match reference.split_once('/') {
            Some((collection, id)) => json!({ "collection": collection, "id": id }),
            None => Json::Null,
        }
    };
    let nodes = normalized
        .get("nodes")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let edges = normalized
        .get("edges")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default();
    let entities: Vec<Json> = nodes
        .iter()
        .map(|node| {
            json!({
                "collection": node.get("collection").cloned().unwrap_or(Json::Null),
                "id": node.get("id").cloned().unwrap_or(Json::Null),
                "data": node.get("doc").cloned().unwrap_or(Json::Null),
            })
        })
        .collect();
    let relations: Vec<Json> = edges
        .iter()
        .map(|edge| {
            json!({
                "from": edge.get("from").and_then(Json::as_str).map(stub).unwrap_or(Json::Null),
                "to": edge.get("to").and_then(Json::as_str).map(stub).unwrap_or(Json::Null),
            })
        })
        .collect();
    Ok(json!({ "entities": entities, "relations": relations }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_accepts_bare_id_and_qualified_id() {
        let request = GraphRequest {
            seed: Some("node-1".into()),
            ..Default::default()
        };
        assert_eq!(seed_node(&request).unwrap(), ("symbols".into(), "node-1".into()));
        let request = GraphRequest {
            seed: Some("entities/node-2".into()),
            ..Default::default()
        };
        assert_eq!(seed_node(&request).unwrap(), ("entities".into(), "node-2".into()));
    }
}
