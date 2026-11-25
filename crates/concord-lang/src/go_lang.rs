//! Go front-end built on `tree-sitter-go`.
//!
//! Extracts package-level functions, receiver methods (`func (r *T) M(...)`),
//! structs, interfaces, type aliases, constants/variables, imports, and call /
//! selector references. All `.go` files in the same directory share the same
//! Concord module name, matching Go's package scoping rules.

use crate::python::{collapse_ws, span_of, tidy_params, truncate};
use crate::{FileIndex, ImportBinding, LangError, RawRef};
use concord_core::{CallArgs, Contract, Digest, Param, ParamKind, Signature, Span, Symbol, SymbolId, SymbolKind};
use std::collections::BTreeMap;
use tree_sitter::{Node, Parser};

pub fn is_go_path(path: &str) -> bool {
    path.ends_with(".go") && !path.split('/').any(|s| s == "vendor")
}

/// Standard Go interface/builtin method names that should not match by unique
/// name on an untyped receiver.
pub fn is_builtin_method(name: &str) -> bool {
    matches!(
        name,
        "Error"
            | "String"
            | "GoString"
            | "Read"
            | "Write"
            | "Close"
            | "Flush"
            | "Len"
            | "Less"
            | "Swap"
            | "Lock"
            | "Unlock"
            | "RLock"
            | "RUnlock"
            | "Done"
            | "Err"
            | "Value"
            | "Deadline"
            | "ServeHTTP"
            | "MarshalJSON"
            | "UnmarshalJSON"
    )
}

/// Dotted module name for a Go file's directory (`pkg/notify/email.go` -> `pkg.notify`).
pub fn module_name(path: &str) -> String {
    let p = path.trim_start_matches("./");
    let mut parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    parts.pop(); // file name; Go scopes by directory package
    parts.join(".")
}

pub fn index_file(path: &str, source: &str) -> Result<FileIndex, LangError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .map_err(|e| LangError::Grammar(e.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| LangError::Parse(path.to_owned()))?;
    let mut module = module_name(path);
    let root = tree.root_node();
    if module.is_empty() {
        let mut c = root.walk();
        for ch in root.named_children(&mut c) {
            if ch.kind() == "package_clause" {
                let pkg = ch
                    .utf8_text(source.as_bytes())
                    .unwrap_or("")
                    .trim_start_matches("package")
                    .trim();
                if !pkg.is_empty() {
                    module = pkg.to_owned();
                }
                break;
            }
        }
    }
    let mut x = Extractor {
        src: source.as_bytes(),
        builders: BTreeMap::new(),
        order: Vec::new(),
        imports: Vec::new(),
        refs: Vec::new(),
    };
    let prefix = SymbolId::new(module.clone());
    x.visit_top(root, &prefix);
    let symbols = x
        .order
        .iter()
        .map(|id| x.builders.remove(id).expect("builder").finish(path))
        .collect();
    Ok(FileIndex {
        path: path.to_owned(),
        module,
        symbols,
        imports: x.imports,
        exports: Vec::new(),
        refs: x.refs,
        has_errors: root.has_error(),
    })
}

struct Builder {
    id: SymbolId,
    kind: SymbolKind,
    span: Span,
    display: String,
    contract_canon: String,
    body_canon: String,
    signature: Option<Signature>,
    defs: usize,
}

impl Builder {
    fn finish(self, path: &str) -> Symbol {
        let display = if self.defs > 1 {
            format!("{}  (+{} more definitions)", self.display, self.defs - 1)
        } else {
            self.display
        };
        Symbol {
            id: self.id,
            kind: self.kind,
            path: path.to_owned(),
            span: self.span,
            contract: Contract {
                display,
                digest: Digest::of(&self.contract_canon),
                signature: if self.defs == 1 { self.signature } else { None },
            },
            body: Digest::of(&self.body_canon),
        }
    }
}

fn qualify(prefix: &SymbolId, name: &str) -> SymbolId {
    if prefix.as_str().is_empty() {
        SymbolId::new(name)
    } else {
        prefix.child(name)
    }
}

struct Extractor<'a> {
    src: &'a [u8],
    builders: BTreeMap<SymbolId, Builder>,
    order: Vec<SymbolId>,
    imports: Vec<ImportBinding>,
    refs: Vec<RawRef>,
}

impl<'a> Extractor<'a> {
    fn text(&self, n: Node) -> &'a str {
        n.utf8_text(self.src).unwrap_or("")
    }

    #[allow(clippy::too_many_arguments)]
    fn add_symbol(
        &mut self,
        id: SymbolId,
        kind: SymbolKind,
        node: Node,
        display: String,
        contract_canon: String,
        body_canon: String,
        signature: Option<Signature>,
    ) {
        let span = span_of(node);
        match self.builders.get_mut(&id) {
            Some(b) => {
                b.defs += 1;
                b.span.start_line = b.span.start_line.min(span.start_line);
                b.span.end_line = b.span.end_line.max(span.end_line);
                b.contract_canon.push_str(" ;; ");
                b.contract_canon.push_str(&contract_canon);
                b.body_canon.push_str(" ;; ");
                b.body_canon.push_str(&body_canon);
            }
            None => {
                self.order.push(id.clone());
                self.builders.insert(
                    id.clone(),
                    Builder {
                        id,
                        kind,
                        span,
                        display,
                        contract_canon,
                        body_canon,
                        signature,
                        defs: 1,
                    },
                );
            }
        }
    }

    fn visit_top(&mut self, root: Node, prefix: &SymbolId) {
        let mut c = root.walk();
        let children: Vec<Node> = root.named_children(&mut c).collect();
        for n in children {
            match n.kind() {
                "comment" | "package_clause" => {}
                "import_declaration" => self.imports(n, prefix),
                "function_declaration" => self.function(n, prefix),
                "method_declaration" => self.method(n, prefix),
                "type_declaration" => self.type_decl(n, prefix),
                "const_declaration" | "var_declaration" => self.var_decl(n, prefix),
                _ => self.collect_refs(n, prefix),
            }
        }
    }

    fn function(&mut self, n: Node, prefix: &SymbolId) {
        let Some(name_node) = n.child_by_field_name("name") else {
            return;
        };
        let id = qualify(prefix, self.text(name_node));
        let body = n.child_by_field_name("body");
        let params_node = n.child_by_field_name("parameters");
        let result_node = n.child_by_field_name("result");

        let end = body.map(|b| b.start_byte()).unwrap_or(n.end_byte());
        let raw = std::str::from_utf8(&self.src[n.start_byte()..end]).unwrap_or("");
        let display = tidy_params(collapse_ws(raw).trim_end_matches('{').trim());
        let canon = self.canon_range(n, body);
        let body_canon = body.map(|b| self.canon(b)).unwrap_or_default();
        let sig = Signature {
            params: params_node.map(|p| self.params_of(p)).unwrap_or_default(),
            returns: result_node.map(|r| self.canon(r)),
            decorators: Vec::new(),
            is_async: false,
            bases: Vec::new(),
        };
        self.add_symbol(
            id.clone(),
            SymbolKind::Function,
            n,
            display,
            canon,
            body_canon,
            Some(sig),
        );
        if let Some(p) = params_node {
            self.collect_refs(p, &id);
        }
        if let Some(r) = result_node {
            self.collect_refs(r, &id);
        }
        if let Some(b) = body {
            self.collect_refs(b, &id);
        }
    }

    fn method(&mut self, n: Node, prefix: &SymbolId) {
        let Some(recv) = n.child_by_field_name("receiver") else {
            return;
        };
        let Some(name_node) = n.child_by_field_name("name") else {
            return;
        };
        let recv_type = self.receiver_type(recv);
        if recv_type.is_empty() {
            return;
        }
        let class_id = qualify(prefix, &recv_type);
        let id = class_id.child(self.text(name_node));
        let body = n.child_by_field_name("body");
        let params_node = n.child_by_field_name("parameters");
        let result_node = n.child_by_field_name("result");

        let end = body.map(|b| b.start_byte()).unwrap_or(n.end_byte());
        let raw = std::str::from_utf8(&self.src[n.start_byte()..end]).unwrap_or("");
        let display = tidy_params(collapse_ws(raw).trim_end_matches('{').trim());
        let canon = self.canon_range(n, body);
        let body_canon = body.map(|b| self.canon(b)).unwrap_or_default();

        let mut params = vec![Param {
            name: "self".into(),
            kind: ParamKind::PositionalOnly,
            annotation: None,
            default: None,
        }];
        if let Some(p) = params_node {
            params.extend(self.params_of(p));
        }
        let sig = Signature {
            params,
            returns: result_node.map(|r| self.canon(r)),
            decorators: Vec::new(),
            is_async: false,
            bases: Vec::new(),
        };
        let recv_var = self.receiver_var(recv);
        self.add_symbol(id.clone(), SymbolKind::Method, n, display, canon, body_canon, Some(sig));
        if let Some(b) = body {
            self.collect_refs_with_recv(b, &id, recv_var.as_deref());
        }
    }

    fn receiver_type(&self, recv: Node) -> String {
        let mut c = recv.walk();
        for ch in recv.named_children(&mut c) {
            if ch.kind() == "parameter_declaration"
                && let Some(ty) = ch.child_by_field_name("type")
            {
                let raw = self.text(ty).trim_start_matches('*').trim();
                let base = raw.split('[').next().unwrap_or(raw).trim();
                return base.to_owned();
            }
        }
        String::new()
    }

    fn receiver_var(&self, recv: Node) -> Option<String> {
        let mut c = recv.walk();
        for ch in recv.named_children(&mut c) {
            if ch.kind() == "parameter_declaration"
                && let Some(nm) = ch.child_by_field_name("name")
            {
                return Some(self.text(nm).to_owned());
            }
        }
        None
    }

    fn params_of(&self, params: Node) -> Vec<Param> {
        let mut out = Vec::new();
        let mut c = params.walk();
        let mut idx = 0usize;
        for ch in params.named_children(&mut c) {
            match ch.kind() {
                "parameter_declaration" => {
                    let ty = ch.child_by_field_name("type").map(|t| self.canon(t));
                    let mut c2 = ch.walk();
                    let names: Vec<String> = ch
                        .children_by_field_name("name", &mut c2)
                        .map(|n| self.text(n).to_owned())
                        .collect();
                    if names.is_empty() {
                        idx += 1;
                        out.push(Param {
                            name: format!("arg{idx}"),
                            kind: ParamKind::PositionalOnly,
                            annotation: ty,
                            default: None,
                        });
                    } else {
                        for nm in names {
                            idx += 1;
                            out.push(Param {
                                name: nm,
                                kind: ParamKind::PositionalOnly,
                                annotation: ty.clone(),
                                default: None,
                            });
                        }
                    }
                }
                "variadic_parameter_declaration" => {
                    let nm = ch
                        .child_by_field_name("name")
                        .map(|n| self.text(n).to_owned())
                        .unwrap_or_else(|| "args".into());
                    let ty = ch.child_by_field_name("type").map(|t| self.canon(t));
                    out.push(Param {
                        name: nm,
                        kind: ParamKind::VarPositional,
                        annotation: ty,
                        default: None,
                    });
                }
                _ => {}
            }
        }
        out
    }

    fn type_decl(&mut self, n: Node, prefix: &SymbolId) {
        let mut c = n.walk();
        let children: Vec<Node> = n.named_children(&mut c).collect();
        for ch in children {
            if matches!(ch.kind(), "type_spec" | "type_alias")
                && let Some(name_node) = ch.child_by_field_name("name")
            {
                let id = qualify(prefix, self.text(name_node));
                let ty = ch.child_by_field_name("type");
                let kind = match ty.map(|t| t.kind()) {
                    Some("struct_type" | "interface_type") => SymbolKind::Class,
                    _ => SymbolKind::Variable,
                };
                let canon = self.canon(ch);
                let display = truncate(&collapse_ws(self.text(ch)), 120);
                self.add_symbol(id.clone(), kind, ch, display, canon, String::new(), None);
                self.collect_refs(ch, &id);
            }
        }
    }

    fn var_decl(&mut self, n: Node, prefix: &SymbolId) {
        let mut c = n.walk();
        let children: Vec<Node> = n.named_children(&mut c).collect();
        for ch in children {
            if matches!(ch.kind(), "const_spec" | "var_spec") {
                let ty = ch.child_by_field_name("type").map(|t| self.canon(t));
                let val = ch
                    .child_by_field_name("value")
                    .map(|v| self.canon(v))
                    .unwrap_or_default();
                let mut c2 = ch.walk();
                let names: Vec<Node> = ch.children_by_field_name("name", &mut c2).collect();
                for nm in names {
                    let name = self.text(nm);
                    let id = qualify(prefix, name);
                    let (canon, display) = match &ty {
                        Some(t) => (format!("{name} {t}"), format!("{name} {t}")),
                        None => (name.to_owned(), name.to_owned()),
                    };
                    self.add_symbol(id.clone(), SymbolKind::Variable, ch, display, canon, val.clone(), None);
                    self.collect_refs(ch, &id);
                }
            }
        }
    }

    fn imports(&mut self, n: Node, _from: &SymbolId) {
        let mut c = n.walk();
        for ch in n.named_children(&mut c) {
            if ch.kind() == "import_spec" {
                self.import_spec(ch);
            } else if ch.kind() == "import_spec_list" {
                let mut c2 = ch.walk();
                for sp in ch.named_children(&mut c2) {
                    if sp.kind() == "import_spec" {
                        self.import_spec(sp);
                    }
                }
            }
        }
    }

    fn import_spec(&mut self, sp: Node) {
        let Some(path_node) = sp.child_by_field_name("path") else {
            return;
        };
        let raw = self.text(path_node).trim_matches(['"', '`']);
        if raw.is_empty() {
            return;
        }
        let line = sp.start_position().row as u32 + 1;
        let alias = sp.child_by_field_name("name").map(|n| self.text(n).to_owned());
        let last = raw.rsplit('/').next().unwrap_or(raw).to_owned();
        let local = match alias.as_deref() {
            Some(".") => "*".to_owned(),
            Some("_") => return,
            Some(a) => a.to_owned(),
            None => last,
        };
        // Store suffix dotted path (e.g. `github.com/org/repo/pkg/notify` -> `goimport:pkg.notify` or dotted)
        let dotted = raw.replace('/', ".");
        self.imports.push(ImportBinding {
            local,
            target: format!("go:{dotted}"),
            line,
        });
    }

    fn canon(&self, n: Node) -> String {
        let mut out = String::new();
        self.canon_into(n, None, &mut out);
        out
    }

    fn canon_range(&self, n: Node, stop_at: Option<Node>) -> String {
        let mut out = String::new();
        self.canon_into(n, stop_at.map(|s| s.id()), &mut out);
        out
    }

    fn canon_into(&self, n: Node, stop_id: Option<usize>, out: &mut String) {
        if stop_id == Some(n.id()) {
            return;
        }
        match n.kind() {
            "comment" => {}
            "interpreted_string_literal" | "raw_string_literal" | "rune_literal" => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(self.text(n));
            }
            _ if n.child_count() == 0 => {
                let tok = self.text(n);
                if !tok.trim().is_empty() && tok != ";" {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(tok);
                }
            }
            _ => {
                let mut c = n.walk();
                for ch in n.children(&mut c) {
                    if stop_id == Some(ch.id()) {
                        break;
                    }
                    self.canon_into(ch, stop_id, out);
                }
            }
        }
    }

    fn collect_refs(&mut self, n: Node, from: &SymbolId) {
        self.collect_refs_with_recv(n, from, None);
    }

    fn collect_refs_with_recv(&mut self, n: Node, from: &SymbolId, recv_var: Option<&str>) {
        match n.kind() {
            "comment" | "interpreted_string_literal" | "raw_string_literal" | "import_declaration" => {}
            "call_expression" => {
                let func = n.child_by_field_name("function");
                if let Some(chain) = func.and_then(|f| self.chain_of(f, recv_var)) {
                    let args_node = n.child_by_field_name("arguments");
                    let mut args = CallArgs::default();
                    if let Some(a) = args_node {
                        let mut c = a.walk();
                        for ch in a.named_children(&mut c) {
                            if ch.kind() == "variadic_argument" {
                                args.star = true;
                            } else if ch.kind() != "comment" {
                                args.positional += 1;
                            }
                        }
                    }
                    self.refs.push(RawRef {
                        from: from.clone(),
                        chain,
                        line: n.start_position().row as u32 + 1,
                        text: truncate(&collapse_ws(self.text(n)), 100),
                        is_call: true,
                        args: Some(args),
                    });
                    if let Some(a) = args_node {
                        self.collect_refs_with_recv(a, from, recv_var);
                    }
                } else {
                    self.recurse(n, from, recv_var);
                }
            }
            "selector_expression" | "type_identifier" | "identifier" => {
                if let Some(chain) = self.chain_of(n, recv_var) {
                    self.refs.push(RawRef {
                        from: from.clone(),
                        chain,
                        line: n.start_position().row as u32 + 1,
                        text: truncate(&collapse_ws(self.text(n)), 100),
                        is_call: false,
                        args: None,
                    });
                } else {
                    self.recurse(n, from, recv_var);
                }
            }
            _ => self.recurse(n, from, recv_var),
        }
    }

    fn recurse(&mut self, n: Node, from: &SymbolId, recv_var: Option<&str>) {
        let mut c = n.walk();
        let children: Vec<Node> = n.named_children(&mut c).collect();
        for ch in children {
            self.collect_refs_with_recv(ch, from, recv_var);
        }
    }

    fn chain_of(&self, n: Node, recv_var: Option<&str>) -> Option<Vec<String>> {
        match n.kind() {
            "identifier" | "type_identifier" | "field_identifier" => {
                let s = self.text(n);
                if recv_var == Some(s) {
                    Some(vec!["self".into()])
                } else {
                    Some(vec![s.to_owned()])
                }
            }
            "selector_expression" => {
                let op = n.child_by_field_name("operand")?;
                let field = n.child_by_field_name("field")?;
                if let Some(mut base) = self.chain_of(op, recv_var) {
                    base.push(self.text(field).to_owned());
                    Some(base)
                } else {
                    Some(vec!["?".into(), self.text(field).to_owned()])
                }
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concord_core::diff_symbols;

    #[test]
    fn indexes_go_functions_methods_and_breaking_changes() {
        let v1 = "package notify\n\nfunc SendEmail(to string, body string) error {\n\treturn nil\n}\n";
        let v2 = "package notify\n\nfunc SendEmail(to string, body string, userID string) error {\n\treturn nil\n}\n";
        let a = index_file("notify/email.go", v1).unwrap();
        let b = index_file("notify/email.go", v2).unwrap();
        let ch = diff_symbols(&a.symbols, &b.symbols);
        assert_eq!(ch.len(), 1);
        assert!(ch[0].is_contract_change() && ch[0].is_breaking());
    }
}
