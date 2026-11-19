//! Source adapters for generic document ingestion.
//!
//! Built-ins need no external parser. PDF support is feature-gated so the
//! default build stays small; Office/other formats can be added by implementing
//! `SourceAdapter` without touching the ingestion pipeline.

use anyhow::{bail, Result};
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Extracted {
    pub text: String,
    pub title: Option<String>,
    pub metadata: serde_json::Value,
}

pub trait SourceAdapter: Send + Sync {
    fn name(&self) -> &'static str;
    fn extract(&self, path: &Path) -> Result<Extracted>;
}

fn read(path: &Path) -> Result<String> {
    if std::fs::metadata(path)?.len() > 4 * 1024 * 1024 {
        bail!("file too large (limit 4 MiB)");
    }
    Ok(std::fs::read_to_string(path)?)
}

struct Markdown;
impl SourceAdapter for Markdown {
    fn name(&self) -> &'static str { "markdown" }
    fn extract(&self, path: &Path) -> Result<Extracted> {
        Ok(Extracted {
            text: read(path)?,
            title: None,
            metadata: serde_json::Value::Object(Default::default()),
        })
    }
}

struct PlainText;
impl SourceAdapter for PlainText {
    fn name(&self) -> &'static str { "text" }
    fn extract(&self, path: &Path) -> Result<Extracted> {
        Ok(Extracted {
            text: read(path)?,
            title: None,
            metadata: serde_json::Value::Object(Default::default()),
        })
    }
}

/// Minimal HTML-to-text: strip script/style, decode common entities, keep
/// block boundaries as newlines. No parser dependency in the default build.
struct Html;
impl SourceAdapter for Html {
    fn name(&self) -> &'static str { "html" }
    fn extract(&self, path: &Path) -> Result<Extracted> {
        let mut raw = read(path)?;
        let title = raw.find("<title").and_then(|start| {
            raw[start..].find('>').map(|offset| {
                let begin = start + offset + 1;
                raw[begin..]
                    .find("</title>")
                    .map(|end| raw[begin..begin + end].trim().to_string())
                    .unwrap_or_default()
            })
        });
        for tag in ["script", "style", "head"] {
            while let Some(start) = raw.find(&format!("<{tag}")) {
                let Some(end) = raw[start..].find(&format!("</{tag}>")) else { break };
                raw.replace_range(start..start + end + tag.len() + 3, "");
            }
        }
        let mut text = String::new();
        let mut inside_tag = false;
        for ch in raw.chars() {
            match ch {
                '<' => inside_tag = true,
                '>' => inside_tag = false,
                _ if !inside_tag => text.push(ch),
                _ => {}
            }
        }
        for (entity, replacement) in [
            ("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"),
            ("&quot;", "\""), ("&#39;", "'"), ("&nbsp;", " "),
        ] {
            text = text.replace(entity, replacement);
        }
        Ok(Extracted {
            text,
            title,
            metadata: serde_json::json!({ "adapter": "html" }),
        })
    }
}

struct Json;
impl SourceAdapter for Json {
    fn name(&self) -> &'static str { "json" }
    fn extract(&self, path: &Path) -> Result<Extracted> {
        let raw = read(path)?;
        let value: serde_json::Value = serde_json::from_str(&raw)?;
        let text = match value {
            serde_json::Value::String(text) => text,
            other => serde_json::to_string_pretty(&other)?,
        };
        Ok(Extracted {
            text,
            title: None,
            metadata: serde_json::json!({ "adapter": "json" }),
        })
    }
}

#[cfg(feature = "source-pdf")]
struct Pdf;
#[cfg(feature = "source-pdf")]
impl SourceAdapter for Pdf {
    fn name(&self) -> &'static str { "pdf" }
    fn extract(&self, path: &Path) -> Result<Extracted> {
        let text = pdf_extract::extract_text(path)?;
        Ok(Extracted {
            text,
            title: path.file_name().map(|n| n.to_string_lossy().to_string()),
            metadata: serde_json::json!({ "adapter": "pdf" }),
        })
    }
}

static MARKDOWN: Markdown = Markdown;
static TEXT: PlainText = PlainText;
static HTML: Html = Html;
static JSON: Json = Json;
#[cfg(feature = "source-pdf")]
static PDF: Pdf = Pdf;

pub fn adapter_for(path: &Path) -> Option<&'static dyn SourceAdapter> {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("md" | "markdown") => Some(&MARKDOWN),
        Some("txt" | "text") => Some(&TEXT),
        Some("html" | "htm") => Some(&HTML),
        Some("json") => Some(&JSON),
        #[cfg(feature = "source-pdf")]
        Some("pdf") => Some(&PDF),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapters_selected_by_extension() {
        assert_eq!(adapter_for(Path::new("a.md")).unwrap().name(), "markdown");
        assert_eq!(adapter_for(Path::new("a.html")).unwrap().name(), "html");
        assert_eq!(adapter_for(Path::new("a.json")).unwrap().name(), "json");
        assert!(adapter_for(Path::new("a.exe")).is_none());
    }
}
