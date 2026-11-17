//! Ekstraksi simbol + call-graph per file via tree-sitter.
//!
//! Wave 1: definisi simbol (fn/struct/trait/enum/method/class/...) dan
//! panggilan (identifier + method/attribute) per bahasa yang didukung.
//! Resolusi callee = name match project-wide (prefer file yang sama) —
//! tanpa resolver tipe penuh; cukup untuk konteks "siapa memanggil apa".
//!
//! Setiap pattern symbol divalidasi sendiri; pattern yang tidak cocok dengan
//! versi grammar di-skip dengan log (bukan mematikan seluruh query).
//!
//! TODO (fase lanjut): pembersihan simbol stale saat re-scan, bahasa lain,
//! resolusi import path.

use streaming_iterator::StreamingIteratorMut;
use tree_sitter::{Language, Parser, Query, QueryCursor};

pub struct SymbolDef {
    pub name: String,
    pub kind: &'static str,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_row: usize,
}

pub struct CallRef {
    pub callee: String,
    pub enclosing: usize, // index ke ParsedFile.symbols
}

pub struct ParsedFile {
    pub symbols: Vec<SymbolDef>,
    pub calls: Vec<CallRef>,
}

pub struct LangDef {
    /// (pattern, kind) — divalidasi per pattern.
    pub symbols: Vec<(&'static str, &'static str)>,
    pub calls: &'static str,
    lang: Language,
}

fn rust_lang() -> LangDef {
    LangDef {
        lang: Language::new(tree_sitter_rust::LANGUAGE),
        symbols: vec![
            ("(function_item name: (identifier) @name)", "fn"),
            ("(struct_item name: (type_identifier) @name)", "struct"),
            ("(enum_item name: (type_identifier) @name)", "enum"),
            ("(trait_item name: (type_identifier) @name)", "trait"),
            ("(type_item name: (type_identifier) @name)", "type"),
        ],
        calls: r#"
(call_expression function: (identifier) @callee)
(call_expression function: (field_expression field: (field_identifier) @callee))
(call_expression function: (scoped_identifier name: (identifier) @callee))
"#,
    }
}

fn ts_lang(tsx: bool) -> LangDef {
    LangDef {
        lang: Language::new(if tsx {
            tree_sitter_typescript::LANGUAGE_TSX
        } else {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT
        }),
        symbols: vec![
            ("(function_declaration name: (identifier) @name)", "fn"),
            ("(method_definition name: (property_identifier) @name)", "method"),
            ("(class_declaration name: (type_identifier) @name)", "class"),
            ("(interface_declaration name: (type_identifier) @name)", "interface"),
        ],
        calls: r#"
(call_expression function: (identifier) @callee)
(call_expression function: (member_expression property: (property_identifier) @callee))
"#,
    }
}

fn js_lang() -> LangDef {
    LangDef {
        lang: Language::new(tree_sitter_javascript::LANGUAGE),
        symbols: vec![
            ("(function_declaration name: (identifier) @name)", "fn"),
            ("(method_definition name: (property_identifier) @name)", "method"),
            ("(class_declaration name: (identifier) @name)", "class"),
        ],
        calls: r#"
(call_expression function: (identifier) @callee)
(call_expression function: (member_expression property: (property_identifier) @callee))
"#,
    }
}

fn py_lang() -> LangDef {
    LangDef {
        lang: Language::new(tree_sitter_python::LANGUAGE),
        symbols: vec![
            ("(function_definition name: (identifier) @name)", "fn"),
            ("(class_definition name: (identifier) @name)", "class"),
        ],
        calls: r#"
(call function: (identifier) @callee)
(call function: (attribute attribute: (identifier) @callee))
"#,
    }
}

fn go_lang() -> LangDef {
    LangDef {
        lang: Language::new(tree_sitter_go::LANGUAGE),
        symbols: vec![
            ("(function_declaration name: (identifier) @name)", "fn"),
            ("(method_declaration name: (field_identifier) @name)", "method"),
        ],
        calls: r#"
(call_expression function: (identifier) @callee)
(call_expression function: (selector_expression field: (field_identifier) @callee))
"#,
    }
}

pub fn for_ext(ext: &str) -> Option<LangDef> {
    Some(match ext.to_ascii_lowercase().as_str() {
        "rs" => rust_lang(),
        "ts" => ts_lang(false),
        "tsx" => ts_lang(true),
        "js" | "jsx" => js_lang(),
        "py" => py_lang(),
        "go" => go_lang(),
        _ => return None,
    })
}

/// Simbol terkecil yang memuat node (simbol diurutkan span kecil → besar).
fn enclosing_symbol(symbols: &[SymbolDef], node: tree_sitter::Node) -> Option<usize> {
    let (s, e) = (node.start_byte(), node.end_byte());
    symbols
        .iter()
        .position(|sym| sym.start_byte <= s && e <= sym.end_byte)
}

impl LangDef {
    /// Parse satu file source → daftar simbol + panggilan (dengan simbol
    /// pembungkus terdalam). Gagal parse bahasa = hasil kosong (fail-soft).
    pub fn parse(&self, source: &str) -> ParsedFile {
        let empty = ParsedFile { symbols: Vec::new(), calls: Vec::new() };
        let mut parser = Parser::new();
        if parser.set_language(&self.lang).is_err() {
            return empty;
        }
        let Some(tree) = parser.parse(source, None) else {
            return empty;
        };
        let root = tree.root_node();
        let bytes = source.as_bytes();

        // ---- simbol (query per pattern) ----
        let mut cursor = QueryCursor::new();
        let mut symbols: Vec<SymbolDef> = Vec::new();
        for (pattern, kind) in &self.symbols {
            let Ok(q) = Query::new(&self.lang, pattern) else {
                eprintln!("[memory-box-indexer] skip pattern invalid: {pattern}");
                continue;
            };
            let mut matches = cursor.matches(&q, root, bytes);
            while let Some(m) = matches.next_mut() {
                for cap in m.captures() {
                    let node = cap.node;
                    let Ok(name) = node.utf8_text(bytes).map(str::to_owned) else { continue };
                    // span = node definisi (parent dari capture @name)
                    let span = node.parent().unwrap_or(node);
                    symbols.push(SymbolDef {
                        name,
                        kind,
                        start_byte: span.start_byte(),
                        end_byte: span.end_byte(),
                        start_row: span.start_position().row + 1, // 1-based
                    });
                }
            }
        }
        // urutkan: start_byte DESC → simbol terdalam (start terbesar yang
        // masih memuat node) ditemukan lebih dulu oleh enclosing_symbol
        symbols.sort_by_key(|s| (std::cmp::Reverse(s.start_byte), s.end_byte));

        // ---- panggilan ----
        let cq = match Query::new(&self.lang, self.calls) {
            Ok(q) => q,
            Err(e) => {
                eprintln!("[memory-box-indexer] call query gagal: {e}");
                return ParsedFile { symbols, calls: Vec::new() };
            }
        };
        let mut calls: Vec<CallRef> = Vec::new();
        let mut matches = cursor.matches(&cq, root, bytes);
        while let Some(m) = matches.next_mut() {
            for cap in m.captures() {
                let node = cap.node;
                let Ok(callee) = node.utf8_text(bytes).map(str::to_owned) else { continue };
                if callee.len() < 3 || callee.len() > 64 {
                    continue;
                }
                if let Some(enclosing) = enclosing_symbol(&symbols, node) {
                    calls.push(CallRef { callee, enclosing });
                }
            }
        }
        ParsedFile { symbols, calls }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_symbols_and_calls() {
        let l = rust_lang();
        let src = r#"
fn helper() -> u32 { 1 }

pub fn scan() { helper(); }
struct Point { x: i32 }
type Alias = Point;
"#;
        let p = l.parse(src);
        assert!(p.symbols.iter().any(|s| s.name == "helper" && s.kind == "fn"));
        assert!(p.symbols.iter().any(|s| s.name == "Point" && s.kind == "struct"));
        assert!(p.symbols.iter().any(|s| s.name == "Alias" && s.kind == "type"));
        assert!(p.calls.iter().any(|c| c.callee == "helper"), "calls: {:?}", p.calls.iter().map(|c| &c.callee).collect::<Vec<_>>());
        // call di dalam scan → enclosing = simbol scan
        let scan_idx = p.symbols.iter().position(|s| s.name == "scan").unwrap();
        assert!(p.calls.iter().any(|c| c.callee == "helper" && c.enclosing == scan_idx));
    }

    #[test]
    fn ts_symbols_and_calls() {
        let l = ts_lang(false);
        let src = r#"
export function helper() { return 1 }
export class Svc { run() { helper(); } }
interface Ctx { a: number }
"#;
        let p = l.parse(src);
        assert!(p.symbols.iter().any(|s| s.name == "helper" && s.kind == "fn"));
        assert!(p.symbols.iter().any(|s| s.name == "Svc" && s.kind == "class"));
        assert!(p.symbols.iter().any(|s| s.name == "Ctx" && s.kind == "interface"));
        let run_idx = p.symbols.iter().position(|s| s.name == "run");
        assert!(run_idx.is_some(), "method run harus ditemukan");
        assert!(p.calls.iter().any(|c| c.callee == "helper" && c.enclosing == run_idx.unwrap()), "call helper dari run");
    }
}



