//! Schema & cross-service API contract front-end.
//!
//! Extracts normalised Concord symbols from:
//! - SQL DDL (`.sql`: `CREATE TABLE`, `CREATE VIEW`, `CREATE TYPE ... AS ENUM`)
//! - Prisma schemas (`.prisma`: `model`, `enum`, `type`)
//! - Protocol Buffers (`.proto`: `message`, `enum`, `service`, `rpc`)
//! - OpenAPI / Swagger specs (`.json`, `.yaml`, `.yml` with `openapi`/`swagger` root key)
//!
//! Keeping schema definitions in the same symbol graph lets backend, database,
//! and frontend agents coordinate API & data-model changes through Concord.

use crate::python::{collapse_ws, truncate};
use crate::{FileIndex, LangError};
use concord_core::{Contract, Digest, Span, Symbol, SymbolId, SymbolKind};
use serde_json::Value;
use std::collections::BTreeSet;

/// True if `path` is a schema / IDL file handled by this module.
pub fn is_schema_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.ends_with(".sql") || lower.ends_with(".prisma") || lower.ends_with(".proto") || is_openapi_candidate(&lower)
}

fn is_openapi_candidate(lower: &str) -> bool {
    let is_doc = lower.ends_with(".json") || lower.ends_with(".yaml") || lower.ends_with(".yml");
    if !is_doc {
        return false;
    }
    let file = lower.rsplit('/').next().unwrap_or(lower);
    file.contains("openapi") || file.contains("swagger")
}

fn make_symbol(
    id: String,
    kind: SymbolKind,
    path: &str,
    start_line: u32,
    end_line: u32,
    contract_text: &str,
    body_text: &str,
) -> Symbol {
    let canon = collapse_ws(contract_text);
    Symbol {
        id: SymbolId::new(id),
        kind,
        path: path.to_owned(),
        span: Span { start_line, end_line },
        contract: Contract {
            display: truncate(&canon, 120),
            digest: Digest::of(&canon),
            signature: None,
        },
        body: Digest::of(&collapse_ws(body_text)),
    }
}

fn strip_ident(raw: &str) -> String {
    raw.trim()
        .trim_matches(|c| matches!(c, '"' | '`' | '[' | ']' | ';'))
        .to_owned()
}

pub fn index_file(path: &str, source: &str) -> Result<FileIndex, LangError> {
    let lower = path.to_ascii_lowercase();
    let module = path
        .trim_start_matches("./")
        .rsplit_once('.')
        .map(|(stem, _)| stem.replace('/', "."))
        .unwrap_or_else(|| path.replace('/', "."));

    let symbols = if lower.ends_with(".sql") {
        extract_sql(path, source)
    } else if lower.ends_with(".prisma") {
        extract_brace_blocks(path, source, "prisma", &["model", "enum", "type"])
    } else if lower.ends_with(".proto") {
        extract_brace_blocks(path, source, "proto", &["message", "enum", "service"])
    } else if lower.ends_with(".json") {
        extract_openapi_json(path, source)
    } else if lower.ends_with(".yaml") || lower.ends_with(".yml") {
        extract_openapi_yaml(path, source)
    } else {
        Vec::new()
    };

    Ok(FileIndex {
        path: path.to_owned(),
        module,
        symbols,
        imports: Vec::new(),
        exports: Vec::new(),
        refs: Vec::new(),
        has_errors: false,
    })
}

fn extract_sql(path: &str, source: &str) -> Vec<Symbol> {
    let lines: Vec<&str> = source.lines().collect();
    let mut symbols = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let clean = trimmed.split("--").next().unwrap_or("").trim();
        let upper = clean.to_ascii_uppercase();
        if upper.starts_with("CREATE TABLE") {
            let after = clean["CREATE TABLE".len()..].trim();
            let after = after
                .strip_prefix("IF NOT EXISTS")
                .or_else(|| after.strip_prefix("if not exists"))
                .unwrap_or(after)
                .trim();
            let raw_name = after
                .split(|c: char| c.is_whitespace() || c == '(')
                .next()
                .unwrap_or("");
            let table_name = strip_ident(raw_name.rsplit('.').next().unwrap_or(raw_name));
            if !table_name.is_empty() {
                let start_line = (i + 1) as u32;
                let mut end = i;
                let mut depth = 0i32;
                let mut opened = false;
                let mut block = String::new();
                while end < lines.len() {
                    let l = lines[end];
                    block.push_str(l);
                    block.push('\n');
                    for ch in l.chars() {
                        if ch == '(' {
                            depth += 1;
                            opened = true;
                        } else if ch == ')' {
                            depth -= 1;
                        }
                    }
                    if opened && depth <= 0 {
                        break;
                    }
                    end += 1;
                }
                let end_line = (end.min(lines.len().saturating_sub(1)) + 1) as u32;
                let table_id = format!("db.{table_name}");
                let mut col_summaries = Vec::new();
                for (offset, line_text) in lines[i..=end.min(lines.len().saturating_sub(1))].iter().enumerate() {
                    if offset == 0 {
                        continue;
                    }
                    let col_raw = line_text.split("--").next().unwrap_or("").trim().trim_end_matches(',');
                    if col_raw.is_empty() || col_raw.starts_with(')') {
                        continue;
                    }
                    let cu = col_raw.to_ascii_uppercase();
                    if cu.starts_with("PRIMARY KEY")
                        || cu.starts_with("FOREIGN KEY")
                        || cu.starts_with("CONSTRAINT")
                        || cu.starts_with("UNIQUE")
                        || cu.starts_with("CHECK")
                        || cu.starts_with("INDEX")
                    {
                        col_summaries.push(collapse_ws(col_raw));
                        continue;
                    }
                    if let Some((col_tok, rest)) = col_raw.split_once(char::is_whitespace) {
                        let col_name = strip_ident(col_tok);
                        if !col_name.is_empty() {
                            let ln = (i + offset + 1) as u32;
                            let col_sig = format!("{col_name} {}", collapse_ws(rest));
                            col_summaries.push(col_sig.clone());
                            symbols.push(make_symbol(
                                format!("{table_id}.{col_name}"),
                                SymbolKind::Variable,
                                path,
                                ln,
                                ln,
                                &col_sig,
                                &col_sig,
                            ));
                        }
                    }
                }
                let table_contract = format!("table {table_name} ({})", col_summaries.join(", "));
                symbols.push(make_symbol(
                    table_id,
                    SymbolKind::Class,
                    path,
                    start_line,
                    end_line,
                    &table_contract,
                    &block,
                ));
                i = end + 1;
                continue;
            }
        }
        i += 1;
    }
    symbols
}

fn extract_brace_blocks(path: &str, source: &str, prefix: &str, keywords: &[&str]) -> Vec<Symbol> {
    let lines: Vec<&str> = source.lines().collect();
    let mut symbols = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].split("//").next().unwrap_or("").trim();
        let mut matched: Option<(&str, &str)> = None;
        for &kw in keywords {
            if let Some(rest) = line.strip_prefix(kw)
                && rest.starts_with(char::is_whitespace)
            {
                let name = rest
                    .trim()
                    .split(|c: char| c.is_whitespace() || c == '{')
                    .next()
                    .unwrap_or("");
                if !name.is_empty() {
                    matched = Some((kw, name));
                    break;
                }
            }
        }
        if let Some((kw, name)) = matched {
            let start_line = (i + 1) as u32;
            let mut end = i;
            let mut depth = 0i32;
            let mut opened = false;
            let mut block = String::new();
            while end < lines.len() {
                let l = lines[end];
                block.push_str(l);
                block.push('\n');
                for ch in l.chars() {
                    if ch == '{' {
                        depth += 1;
                        opened = true;
                    } else if ch == '}' {
                        depth -= 1;
                    }
                }
                if opened && depth <= 0 {
                    break;
                }
                end += 1;
            }
            let end_line = (end.min(lines.len().saturating_sub(1)) + 1) as u32;
            let parent_id = format!("{prefix}.{name}");
            let mut member_contracts = Vec::new();
            for (offset, raw_line) in lines[i..=end.min(lines.len().saturating_sub(1))].iter().enumerate() {
                if offset == 0 {
                    continue;
                }
                let member = raw_line.split("//").next().unwrap_or("").trim();
                if member.is_empty() || member.starts_with('}') || member.starts_with("@@") {
                    continue;
                }
                let ln = (i + offset + 1) as u32;
                let (member_name, kind, sig) = if kw == "service" && member.starts_with("rpc ") {
                    let after = member["rpc ".len()..].trim();
                    let rpc_name = after
                        .split(|c: char| c.is_whitespace() || c == '(')
                        .next()
                        .unwrap_or("");
                    (rpc_name.to_owned(), SymbolKind::Method, collapse_ws(member))
                } else if kw == "enum" {
                    let val = member
                        .split(|c: char| c.is_whitespace() || c == '=' || c == ';')
                        .next()
                        .unwrap_or("");
                    (val.to_owned(), SymbolKind::Variable, collapse_ws(member))
                } else if prefix == "proto" {
                    // e.g. `repeated string tags = 3;` or `string id = 1;`
                    let lhs = member.split('=').next().unwrap_or(member).trim();
                    let fname = lhs.split_whitespace().last().unwrap_or("");
                    (fname.to_owned(), SymbolKind::Variable, collapse_ws(member))
                } else {
                    // Prisma field: `email String @unique`
                    let fname = member.split_whitespace().next().unwrap_or("");
                    (fname.to_owned(), SymbolKind::Variable, collapse_ws(member))
                };
                if !member_name.is_empty() {
                    member_contracts.push(sig.clone());
                    symbols.push(make_symbol(
                        format!("{parent_id}.{member_name}"),
                        kind,
                        path,
                        ln,
                        ln,
                        &sig,
                        &sig,
                    ));
                }
            }
            let contract = format!("{kw} {name} {{ {} }}", member_contracts.join("; "));
            symbols.push(make_symbol(
                parent_id,
                SymbolKind::Class,
                path,
                start_line,
                end_line,
                &contract,
                &block,
            ));
            i = end + 1;
            continue;
        }
        i += 1;
    }
    symbols
}

fn extract_openapi_json(path: &str, source: &str) -> Vec<Symbol> {
    let Ok(val) = serde_json::from_str::<Value>(source) else {
        return Vec::new();
    };
    if val.get("openapi").is_none() && val.get("swagger").is_none() {
        return Vec::new();
    }
    let mut symbols = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    let find_line = |needle: &str| -> u32 {
        lines
            .iter()
            .position(|l| l.contains(needle))
            .map(|idx| (idx + 1) as u32)
            .unwrap_or(1)
    };
    if let Some(paths) = val.get("paths").and_then(Value::as_object) {
        for (route, methods) in paths {
            let Some(mobj) = methods.as_object() else { continue };
            for (method, spec) in mobj {
                let upper = method.to_ascii_uppercase();
                if !matches!(
                    upper.as_str(),
                    "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS" | "HEAD"
                ) {
                    continue;
                }
                let norm_route = route.trim_matches('/').replace('/', ".");
                let id = if norm_route.is_empty() {
                    format!("api.{upper}.root")
                } else {
                    format!("api.{upper}.{norm_route}")
                };
                let ln = find_line(&format!("\"{route}\""));
                let body = serde_json::to_string(spec).unwrap_or_default();
                let params = spec
                    .get("parameters")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|p| p.get("name").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let sig = format!("{upper} {route} ({params})");
                symbols.push(make_symbol(id, SymbolKind::Function, path, ln, ln, &sig, &body));
            }
        }
    }
    if let Some(schemas) = val
        .get("components")
        .and_then(|c| c.get("schemas"))
        .and_then(Value::as_object)
    {
        for (name, schema) in schemas {
            let ln = find_line(&format!("\"{name}\""));
            let props = schema
                .get("properties")
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect::<Vec<_>>().join(", "))
                .unwrap_or_default();
            let required: BTreeSet<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let sig = format!("schema {name} ({props}; req: {:?})", required);
            let body = serde_json::to_string(schema).unwrap_or_default();
            symbols.push(make_symbol(
                format!("api.schema.{name}"),
                SymbolKind::Class,
                path,
                ln,
                ln,
                &sig,
                &body,
            ));
        }
    }
    symbols
}

fn extract_openapi_yaml(path: &str, source: &str) -> Vec<Symbol> {
    if !source.contains("openapi:") && !source.contains("swagger:") {
        return Vec::new();
    }
    let mut symbols = Vec::new();
    let mut in_paths = false;
    let mut cur_route: Option<String> = None;
    for (idx, raw) in source.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("");
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        if indent == 0 {
            in_paths = trimmed == "paths:";
            cur_route = None;
            continue;
        }
        if in_paths && indent == 2 && trimmed.starts_with('/') && trimmed.ends_with(':') {
            cur_route = Some(trimmed.trim_end_matches(':').to_owned());
        } else if in_paths
            && indent == 4
            && let Some(route) = &cur_route
            && let Some(m) = trimmed.strip_suffix(':')
        {
            let upper = m.to_ascii_uppercase();
            if matches!(
                upper.as_str(),
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS" | "HEAD"
            ) {
                let norm = route.trim_matches('/').replace('/', ".");
                let id = format!("api.{upper}.{}", if norm.is_empty() { "root" } else { &norm });
                let ln = (idx + 1) as u32;
                let sig = format!("{upper} {route}");
                symbols.push(make_symbol(id, SymbolKind::Function, path, ln, ln, &sig, &sig));
            }
        }
    }
    symbols
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_sql_prisma_proto_and_openapi_contracts() {
        let sql = r#"
CREATE TABLE users (
  id UUID PRIMARY KEY,
  email TEXT NOT NULL UNIQUE,
  role VARCHAR(32) NOT NULL DEFAULT 'member'
);
"#;
        let idx_sql = index_file("migrations/001_init.sql", sql).unwrap();
        assert!(idx_sql.symbols.iter().any(|s| s.id.0 == "db.users"));
        assert!(idx_sql.symbols.iter().any(|s| s.id.0 == "db.users.email"));

        let prisma = r#"
model Task {
  id        String @id
  clientId  String?
  priority  String
}
"#;
        let idx_prisma = index_file("prisma/schema.prisma", prisma).unwrap();
        assert!(idx_prisma.symbols.iter().any(|s| s.id.0 == "prisma.Task"));
        assert!(idx_prisma.symbols.iter().any(|s| s.id.0 == "prisma.Task.clientId"));

        let proto = r#"
message DispatchRequest {
  string task_id = 1;
  int32 eta_minutes = 2;
}
service Dispatcher {
  rpc Assign (DispatchRequest) returns (DispatchReply);
}
"#;
        let idx_proto = index_file("proto/dispatch.proto", proto).unwrap();
        assert!(
            idx_proto
                .symbols
                .iter()
                .any(|s| s.id.0 == "proto.DispatchRequest.eta_minutes")
        );
        assert!(idx_proto.symbols.iter().any(|s| s.id.0 == "proto.Dispatcher.Assign"));

        let openapi = r#"{
  "openapi": "3.1.0",
  "paths": {
    "/tasks/{id}": {
      "post": { "parameters": [{ "name": "id" }] }
    }
  },
  "components": {
    "schemas": {
      "Task": { "properties": { "id": {}, "title": {} }, "required": ["id"] }
    }
  }
}"#;
        let idx_api = index_file("api/openapi.json", openapi).unwrap();
        assert!(idx_api.symbols.iter().any(|s| s.id.0 == "api.POST.tasks.{id}"));
        assert!(idx_api.symbols.iter().any(|s| s.id.0 == "api.schema.Task"));
    }
}
