//! Cross-language HTTP route contract extraction.
//!
//! Links backend HTTP route definitions (Python FastAPI/Flask decorators,
//! TypeScript/JS Express/Hono/Fastify handlers, and Next.js App Router
//! `app/api/.../route.ts` exports) to frontend/client `fetch(...)` and
//! `axios.<method>(...)` calls via canonical `http.<METHOD>.<path>` symbols.
//!
//! Path parameters (`:id`, `{id}`, `[id]`, `${id}`) are normalized to `{}` so
//! parameterized routes match across languages and frameworks.

use crate::python::truncate;
use crate::{FileIndex, RawRef};
use concord_core::{Contract, Digest, Span, Symbol, SymbolId, SymbolKind};

/// Normalize a URL route path into a canonical dotted suffix:
/// `/api/tasks/:id` | `/api/tasks/{id}` | `/api/tasks/[id]` | `/api/tasks/${id}`
/// -> `api.tasks.{}`
pub fn normalize_route_path(raw: &str) -> Option<String> {
    let stripped = raw.split('?').next().unwrap_or(raw).trim();
    if !stripped.starts_with('/') {
        return None;
    }
    let segs: Vec<String> = stripped
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|seg| {
            if seg.starts_with(':')
                || (seg.starts_with('{') && seg.ends_with('}'))
                || (seg.starts_with('[') && seg.ends_with(']'))
                || (seg.starts_with("${") && seg.ends_with('}'))
            {
                "{}".to_owned()
            } else {
                seg.to_owned()
            }
        })
        .collect();
    if segs.is_empty() {
        Some("root".to_owned())
    } else {
        Some(segs.join("."))
    }
}

pub fn route_symbol_id(method: &str, raw_path: &str) -> Option<SymbolId> {
    let norm = normalize_route_path(raw_path)?;
    Some(SymbolId::new(format!("http.{}.{}", method.to_ascii_uppercase(), norm)))
}

fn make_route_symbol(path: &str, line: u32, method: &str, raw_route: &str) -> Option<Symbol> {
    let id = route_symbol_id(method, raw_route)?;
    let sig = format!("{} {}", method.to_ascii_uppercase(), raw_route);
    Some(Symbol {
        id,
        kind: SymbolKind::Function,
        path: path.to_owned(),
        span: Span {
            start_line: line,
            end_line: line,
        },
        contract: Contract {
            display: truncate(&sig, 120),
            digest: Digest::of(&sig),
            signature: None,
        },
        body: Digest::of(&sig),
    })
}

/// Extract the first quoted string (`"..."`, `'...'`, or `` `...` ``) from `s`.
fn first_quoted(s: &str) -> Option<String> {
    for (idx, ch) in s.char_indices() {
        if matches!(ch, '"' | '\'' | '`') {
            let rest = &s[idx + 1..];
            if let Some(end) = rest.find(ch) {
                return Some(rest[..end].to_owned());
            }
        }
    }
    None
}

/// Enrich a parsed `FileIndex` with cross-language HTTP route symbols and
/// client `fetch()`/`axios` references.
pub fn enrich_http_contracts(fi: &mut FileIndex, source: &str) {
    let p = fi.path.as_str();
    if crate::python::is_python_path(p) {
        extract_python_routes(fi, source);
    } else if crate::typescript::is_typescript_path(p) {
        extract_ts_routes_and_fetches(fi, source);
    }
}

fn extract_python_routes(fi: &mut FileIndex, source: &str) {
    let methods = ["get", "post", "put", "patch", "delete", "options", "head"];
    for (idx, raw) in source.lines().enumerate() {
        let line = raw.trim();
        let ln = (idx + 1) as u32;
        if !line.starts_with('@') {
            continue;
        }
        // e.g. `@app.post("/api/invoices")` or `@router.get("/api/tasks/{id}")`
        if let Some(dot) = line.find('.')
            && let Some(paren) = line[dot + 1..].find('(')
        {
            let m = line[dot + 1..dot + 1 + paren].trim().to_ascii_lowercase();
            let args = &line[dot + 1 + paren..];
            if methods.contains(&m.as_str()) {
                if let Some(route) = first_quoted(args)
                    && let Some(sym) = make_route_symbol(&fi.path, ln, &m, &route)
                {
                    fi.symbols.push(sym);
                }
            } else if m == "route"
                && let Some(route) = first_quoted(args)
            {
                // Flask `@app.route("/path", methods=["POST"])`
                let mut found = false;
                for fm in &methods {
                    let upper = fm.to_ascii_uppercase();
                    if args.contains(&format!("\"{upper}\"")) || args.contains(&format!("'{upper}'")) {
                        if let Some(sym) = make_route_symbol(&fi.path, ln, &upper, &route) {
                            fi.symbols.push(sym);
                        }
                        found = true;
                    }
                }
                if !found && let Some(sym) = make_route_symbol(&fi.path, ln, "GET", &route) {
                    fi.symbols.push(sym);
                }
            }
        }
    }
}

fn nextjs_app_route(path: &str) -> Option<String> {
    let clean = path.trim_start_matches("./");
    let file = clean.rsplit('/').next()?;
    if !matches!(file, "route.ts" | "route.tsx" | "route.js" | "route.mjs") {
        return None;
    }
    let dir = clean.rsplit_once('/').map(|(d, _)| d)?;
    let rel = dir.strip_prefix("src/app").or_else(|| dir.strip_prefix("app"))?;
    Some(if rel.is_empty() { "/".into() } else { rel.to_owned() })
}

fn extract_ts_routes_and_fetches(fi: &mut FileIndex, source: &str) {
    let lines: Vec<&str> = source.lines().collect();
    let http_methods = ["GET", "POST", "PUT", "PATCH", "DELETE", "OPTIONS", "HEAD"];

    // 1. Next.js App Router `app/api/.../route.ts` exported handlers
    if let Some(next_route) = nextjs_app_route(&fi.path) {
        for (idx, raw) in lines.iter().enumerate() {
            let trimmed = raw.trim();
            let ln = (idx + 1) as u32;
            for m in http_methods {
                let is_handler = trimmed.starts_with(&format!("export async function {m}("))
                    || trimmed.starts_with(&format!("export function {m}("))
                    || trimmed.starts_with(&format!("export const {m} "))
                    || trimmed.starts_with(&format!("export const {m}="));
                if is_handler && let Some(sym) = make_route_symbol(&fi.path, ln, m, &next_route) {
                    fi.symbols.push(sym);
                }
            }
        }
    }

    // 2. Express / Hono / Fastify routes (`app.get("/api/...", ...)` / `router.post(...)`)
    let lower_methods = ["get", "post", "put", "patch", "delete"];
    let module_from = SymbolId::new(&fi.module);
    for (idx, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim();
        let ln = (idx + 1) as u32;

        for m in lower_methods {
            for prefix in ["app.", "router.", "server.", "fastify."] {
                let pat = format!("{prefix}{m}(");
                if let Some(pos) = trimmed.find(&pat)
                    && let Some(route) = first_quoted(&trimmed[pos + pat.len()..])
                    && let Some(sym) = make_route_symbol(&fi.path, ln, m, &route)
                {
                    fi.symbols.push(sym);
                }
            }
            // axios.<method>("/api/...")
            let ax = format!("axios.{m}(");
            if let Some(pos) = trimmed.find(&ax)
                && let Some(route) = first_quoted(&trimmed[pos + ax.len()..])
                && let Some(target) = route_symbol_id(m, &route)
            {
                fi.refs.push(RawRef {
                    from: module_from.clone(),
                    chain: vec![target.0],
                    line: ln,
                    text: truncate(trimmed, 120),
                    is_call: true,
                    args: None,
                });
            }
        }

        // 3. `fetch("/api/...", { method: "POST" })` (checks current + next 3 lines for method)
        if let Some(pos) = trimmed.find("fetch(")
            && let Some(route) = first_quoted(&trimmed[pos + "fetch(".len()..])
            && route.starts_with('/')
        {
            let window = lines[idx..(idx + 4).min(lines.len())].join(" ");
            let mut method = "GET";
            for m in http_methods {
                if window.contains(&format!("method: \"{m}\""))
                    || window.contains(&format!("method: '{m}'"))
                    || window.contains(&format!("method:\"{m}\""))
                    || window.contains(&format!("method:'{m}'"))
                {
                    method = m;
                    break;
                }
            }
            if let Some(target) = route_symbol_id(method, &route) {
                fi.refs.push(RawRef {
                    from: module_from.clone(),
                    chain: vec![target.0],
                    line: ln,
                    text: truncate(trimmed, 120),
                    is_call: true,
                    args: None,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RepoIndex, index_file};

    #[test]
    fn links_python_fastapi_route_to_typescript_fetch_call() {
        let py = r#"
from fastapi import FastAPI
app = FastAPI()

@app.post("/api/invoices/{invoice_id}")
def close_invoice(invoice_id: str):
    return {"ok": True}
"#;
        let tsx = r#"
export async function submitInvoice(id: string) {
  return fetch(`/api/invoices/${id}`, {
    method: "POST",
  });
}
"#;
        let f_py = index_file("backend/api.py", py).unwrap().unwrap();
        let f_ts = index_file("frontend/invoice.tsx", tsx).unwrap().unwrap();
        let idx = RepoIndex::new([f_py.clone(), f_ts.clone()]);

        let target_id = SymbolId::new("http.POST.api.invoices.{}");
        assert!(idx.symbol(&target_id).is_some(), "FastAPI route symbol missing");

        let resolved: Vec<SymbolId> = f_ts.refs.iter().filter_map(|r| idx.resolve(&f_ts, r)).collect();
        assert!(
            resolved.contains(&target_id),
            "TSX fetch() should resolve to Python FastAPI route symbol; got {resolved:?}"
        );
    }

    #[test]
    fn links_nextjs_app_router_handler_to_frontend_fetch() {
        let route = r#"
export async function GET() {
  return Response.json({ ok: true });
}
"#;
        let ui = r#"
export async function loadTasks() {
  return fetch("/api/tasks");
}
"#;
        let f_route = index_file("app/api/tasks/route.ts", route).unwrap().unwrap();
        let f_ui = index_file("components/board.tsx", ui).unwrap().unwrap();
        let idx = RepoIndex::new([f_route, f_ui.clone()]);
        let target_id = SymbolId::new("http.GET.api.tasks");
        assert!(idx.symbol(&target_id).is_some());
        assert!(
            f_ui.refs
                .iter()
                .any(|r| idx.resolve(&f_ui, r) == Some(target_id.clone()))
        );
    }
}
