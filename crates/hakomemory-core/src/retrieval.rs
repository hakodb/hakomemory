//! Composable retrieval over document chunks with filter pushdown,
//! token budgeting and citations.

use crate::docs::{self, Citation};
use crate::embed::SharedEmbedder;
use anyhow::Result;
use hakodb::document::hako_doc::HakoDoc;
use hakodb::document::value::Value;
use hakodb::engine::Hako;
use hakodb::query::filter::{Filter, Operator};
use hakodb::query::query::Query;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RetrievalMode {
    /// Vector ANN only (`find_near_scored_filtered`).
    Vector,
    /// Full-text only (`Match` filter, engine FTS index).
    Fts,
    /// Reciprocal-rank fusion of vector + FTS (`hybrid_search_filtered`).
    #[default]
    Hybrid,
    /// Hybrid seeds expanded through generic `relations` graph edges.
    HybridGraph,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FilterSpec {
    pub field: String,
    pub op: String,
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetrievalQuery {
    pub query: String,
    #[serde(default)]
    pub mode: RetrievalMode,
    #[serde(default)]
    pub filters: Vec<FilterSpec>,
    #[serde(default = "default_k")]
    pub k: usize,
    #[serde(default = "default_rrf_k")]
    pub rrf_k: usize,
    #[serde(default)]
    pub budget_tokens: Option<usize>,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default = "default_citations")]
    pub citations: bool,
    /// Include the parent section chunk in expanded hits.
    #[serde(default)]
    pub expand_parent: bool,
    /// Edge collection used by `hybridgraph` expansion (default `relations`).
    #[serde(default)]
    pub graph_via: Option<String>,
    #[serde(default = "default_graph_hops")]
    pub graph_hops: usize,
}

fn default_k() -> usize {
    8
}

fn default_rrf_k() -> usize {
    60
}

fn default_citations() -> bool {
    true
}

fn default_graph_hops() -> usize {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetrievalHit {
    pub chunk_id: String,
    pub document_id: String,
    pub score: f64,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub citation: Option<Citation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RetrievalResponse {
    pub hits: Vec<RetrievalHit>,
    pub used_tokens: usize,
    pub budget_tokens: Option<usize>,
    pub mode: RetrievalMode,
}

fn engine_value(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::String(text) => Value::String(text.clone()),
        serde_json::Value::Number(number) if number.is_i64() => {
            Value::Int(number.as_i64().unwrap_or_default())
        }
        serde_json::Value::Number(number) => Value::Float(number.as_f64().unwrap_or_default()),
        serde_json::Value::Bool(flag) => Value::Bool(*flag),
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Array(items) => {
            Value::Array(items.iter().map(engine_value).collect())
        }
        serde_json::Value::Object(_) => Value::String(value.to_string()),
    }
}

fn engine_filters(specs: &[FilterSpec], collection: Option<&str>) -> Vec<Filter> {
    let mut filters: Vec<Filter> = specs
        .iter()
        .filter_map(|spec| {
            let op = match spec.op.as_str() {
                "eq" => Operator::Eq,
                "ne" => Operator::Ne,
                "gt" => Operator::Gt,
                "gte" => Operator::Gte,
                "lt" => Operator::Lt,
                "lte" => Operator::Lte,
                "match" => Operator::Match,
                "matchPrefix" => Operator::MatchPrefix,
                "contains" => Operator::Contains,
                "startsWith" => Operator::StartsWith,
                "in" => Operator::In,
                _ => return None,
            };
            Some(Filter {
                field: spec.field.clone(),
                op,
                value: engine_value(&spec.value),
            })
        })
        .collect();
    if let Some(collection) = collection {
        filters.push(Filter {
            field: "collection".into(),
            op: Operator::Eq,
            value: Value::String(collection.to_string()),
        });
    }
    filters
}

fn str_field(doc: &HakoDoc, field: &str) -> String {
    match doc.get(field) {
        Some(Value::String(value)) => value.clone(),
        Some(value) => format!("{value:?}"),
        None => String::new(),
    }
}

fn int_field(doc: &HakoDoc, field: &str) -> usize {
    match doc.get(field) {
        Some(Value::Int(value)) => (*value).max(0) as usize,
        _ => 0,
    }
}

fn citation(doc: &HakoDoc) -> Citation {
    let document_id = str_field(doc, "doc_id");
    Citation {
        source_uri: {
            let uri = str_field(doc, "source_uri");
            if uri.is_empty() { str_field(doc, "path") } else { uri }
        },
        document_id,
        heading: str_field(doc, "heading_path"),
        start_line: int_field(doc, "start_line"),
        end_line: int_field(doc, "end_line"),
    }
}

/// Collect document ids connected to a seed through generic edge documents
/// (`_from`/`_to` references). Bounded BFS; embedded scale only.
fn related_doc_ids(
    db: &Hako,
    seed: &str,
    via: &str,
    hops: usize,
    limit: usize,
) -> Vec<String> {
    let mut frontier = std::collections::HashSet::from([seed.to_string()]);
    let mut visited = frontier.clone();
    let mut output = Vec::new();
    for _ in 0..hops.clamp(1, 4) {
        if output.len() >= limit {
            break;
        }
        let rows = db
            .query(Query::new(via).limit(limit.max(100)))
            .unwrap_or_default();
        let mut next = std::collections::HashSet::new();
        for (_, doc) in rows {
            let (from, to) = match (doc.get("_from"), doc.get("_to")) {
                (Some(Value::Reference { collection: fc, doc_id: fi }),
                 Some(Value::Reference { collection: tc, doc_id: ti })) => {
                    ((fc, fi), (tc, ti))
                }
                _ => continue,
            };
            if from.0.as_str() != docs::DOCUMENTS && to.0.as_str() != docs::DOCUMENTS {
                continue;
            }
            if from.0.as_str() == docs::DOCUMENTS && frontier.contains(from.1.as_str())
                && visited.insert(to.1.clone())
            {
                output.push(to.1.clone());
                next.insert(to.1.clone());
            }
            if to.0.as_str() == docs::DOCUMENTS && frontier.contains(to.1.as_str())
                && visited.insert(from.1.clone())
            {
                output.push(from.1.clone());
                next.insert(from.1.clone());
            }
        }
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }
    output
}

/// Retrieve chunks. Filters go into the engine admission path before any
/// side-candidate truncation, so `k` and the FTS 4096 cap do not lose matches.
pub fn search(
    db: &Hako,
    embedder: &SharedEmbedder,
    request: &RetrievalQuery,
) -> Result<RetrievalResponse> {
    let k = request.k.clamp(1, 1000);
    let pool = k.saturating_mul(4).min(4000);
    let filters = engine_filters(&request.filters, request.collection.as_deref());
    let vector = embedder.embed_one(&request.query)?;

    let mut candidates: Vec<(String, f64, HakoDoc)> = match request.mode {
        RetrievalMode::Hybrid | RetrievalMode::HybridGraph => db.hybrid_search_filtered(
            docs::CHUNKS,
            "content",
            &request.query,
            "emb",
            &vector,
            pool,
            request.rrf_k,
            &filters,
        )?,
        RetrievalMode::Vector => db.find_near_scored_filtered(
            docs::CHUNKS,
            "emb",
            &vector,
            pool,
            None,
            &filters,
        )?,
        RetrievalMode::Fts => {
            let mut query = Query::new(docs::CHUNKS)
                .where_filter("content", Operator::Match, Value::String(request.query.clone()))
                .limit(pool);
            for filter in &filters {
                query = query.where_filter(&filter.field, filter.op, filter.value.clone());
            }
            db.query(query)?
                .into_iter()
                .map(|(id, doc)| (id, 0.0, doc))
                .collect()
        }
    };

    if request.mode == RetrievalMode::HybridGraph {
        let via = request
            .graph_via
            .clone()
            .unwrap_or_else(|| "relations".to_string());
        let seeds: Vec<String> = candidates
            .iter()
            .take(5)
            .map(|(_, _, doc)| str_field(doc, "doc_id"))
            .filter(|id| !id.is_empty())
            .collect();
        let mut related = Vec::new();
        for seed in seeds {
            related.extend(related_doc_ids(db, &seed, &via, request.graph_hops, pool));
        }
        let mut seen: std::collections::HashSet<String> =
            candidates.iter().map(|(id, _, _)| id.clone()).collect();
        for document_id in related {
            let mut query = Query::new(docs::CHUNKS)
                .where_filter("doc_id", Operator::Eq, Value::String(document_id))
                .limit(pool);
            for filter in &filters {
                query = query.where_filter(&filter.field, filter.op, filter.value.clone());
            }
            for (id, doc) in db.query(query).unwrap_or_default() {
                if seen.insert(id.clone()) {
                    candidates.push((id, 0.0, doc));
                }
            }
        }
    }

    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let budget = request.budget_tokens;
    let mut used_tokens = 0usize;
    let mut hits = Vec::new();
    for (chunk_id, score, doc) in candidates {
        if hits.len() >= k {
            break;
        }
        let mut text = str_field(&doc, "content");
        if request.expand_parent {
            if let Some(Value::String(parent_id)) = doc.get("parent_id") {
                if let Ok(Some(parent)) = db.get(docs::CHUNKS, parent_id) {
                    let parent_text = str_field(&parent, "content");
                    if !parent_text.is_empty() && parent_text != text {
                        text = format!("{parent_text}\n{text}");
                    }
                }
            }
        }
        let tokens = text.chars().count().div_ceil(4);
        if let Some(budget) = budget {
            if used_tokens + tokens > budget {
                continue;
            }
            used_tokens += tokens;
        }
        hits.push(RetrievalHit {
            chunk_id,
            document_id: str_field(&doc, "doc_id"),
            score,
            text,
            citation: request.citations.then(|| citation(&doc)),
        });
    }

    Ok(RetrievalResponse {
        hits,
        used_tokens,
        budget_tokens: budget,
        mode: request.mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_conversion_covers_common_ops() {
        let specs = vec![
            FilterSpec { field: "collection".into(), op: "eq".into(), value: json_str("wiki") },
            FilterSpec { field: "price".into(), op: "gte".into(), value: serde_json::json!(10) },
            FilterSpec { field: "tags".into(), op: "in".into(), value: serde_json::json!(["ai"]) },
            FilterSpec { field: "x".into(), op: "bogus".into(), value: serde_json::json!(1) },
        ];
        let filters = engine_filters(&specs, Some("notes"));
        assert_eq!(filters.len(), 4); // 3 valid + collection pushdown; bogus dropped
        assert_eq!(filters[0].op, Operator::Eq);
        assert_eq!(filters[1].op, Operator::Gte);
        assert_eq!(filters[2].op, Operator::In);
        assert_eq!(filters[3].field, "collection");
    }

    fn json_str(value: &str) -> serde_json::Value {
        serde_json::Value::String(value.to_string())
    }
}
