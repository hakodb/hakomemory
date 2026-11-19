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
        RetrievalMode::Hybrid => db.hybrid_search_filtered(
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

    candidates.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let budget = request.budget_tokens;
    let mut used_tokens = 0usize;
    let mut hits = Vec::new();
    for (chunk_id, score, doc) in candidates {
        if hits.len() >= k {
            break;
        }
        let text = str_field(&doc, "content");
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
