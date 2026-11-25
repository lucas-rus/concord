//! Rust front-end built on `tree-sitter-rust`.
//!
//! Extracts symbols (`fn`, `struct`, `enum`, `trait`, `impl` methods, `type`,
//! `const`, `static`), `use` bindings (including `crate::`, `super::`, `self::`
//! and nested `{...}` use trees), and resolved call/path references.

use crate::python::{collapse_ws, span_of, tidy_params, truncate};
use crate::{FileIndex, ImportBinding, LangError, RawRef};
use concord_core::{CallArgs, Contract, Digest, Param, ParamKind, Signature, Span, Symbol, SymbolId, SymbolKind};
use std::collections::BTreeMap;
use tree_sitter::{Node, Parser};

pub fn is_rust_path(path: &str) -> bool {
    path.ends_with(".rs") && !path.split('/').any(|s| s == "target")
}

/// Method names too ubiquitous on standard-library types (`Vec`, `Option`,
/// `Result`, `Iterator`, `String`, `HashMap`, `BTreeMap`) to resolve by unique
/// name on an untyped receiver.
pub fn is_builtin_method(name: &str) -> bool {
    matches!(
        name,
        "new"
            | "default"
            | "clone"
            | "len"
            | "is_empty"
            | "iter"
            | "iter_mut"
            | "into_iter"
            | "map"
            | "filter"
            | "filter_map"
            | "flat_map"
            | "find"
            | "any"
            | "all"
            | "fold"
            | "collect"
            | "unwrap"
            | "unwrap_or"
            | "unwrap_or_default"
            | "unwrap_or_else"
            | "expect"
            | "ok"
            | "err"
            | "is_some"
            | "is_none"
            | "is_ok"
            | "is_err"
            | "and_then"
            | "or_else"
            | "as_ref"
            | "as_mut"
            | "as_str"
            | "as_bytes"
            | "as_slice"
            | "to_string"
            | "to_owned"
            | "into"
            | "from"
            | "push"
            | "push_str"
            | "pop"
            | "insert"
            | "remove"
            | "get"
            | "get_mut"
            | "entry"
            | "or_default"
            | "or_insert"
            | "contains"
            | "contains_key"
            | "clear"
            | "extend"
            | "trim"
            | "split"
            | "join"
            | "fmt"
            | "read"
            | "write"
            | "flush"
    )
}

/// Dotted module name for a repository-relative Rust file path:
/// `src/notify/email.rs` -> `notify.email`
/// `crates/foo/src/bar/mod.rs` -> `foo.bar`
/// `src/lib.rs` -> `crate`
pub fn module_name(path: &str) -> (String, String) {
    let p = path.trim_start_matches("./");
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let (crate_prefix, rest) = if parts.len() >= 3 && parts[0] == "crates" && parts[2] == "src" {
        (parts[1].replace('-', "_"), &parts[3..])
    } else if !parts.is_empty() && parts[0] == "src" {
        (String::new(), &parts[1..])
    } else {
        (String::new(), &parts[..])
    };
    let mut mod_parts: Vec<String> = Vec::new();
    if !crate_prefix.is_empty() {
        mod_parts.push(crate_prefix.clone());
    }
    for (i, seg) in rest.iter().enumerate() {
        let stem = seg.strip_suffix(".rs").unwrap_or(seg);
        let is_last = i + 1 == rest.len();
        if is_last && matches!(stem, "lib" | "main" | "mod") {
            continue;
        }
        mod_parts.push(stem.to_owned());
    }
    let module = if mod_parts.is_empty() {
        "crate".to_owned()
    } else {
        mod_parts.join(".")
    };
    let crate_root = if crate_prefix.is_empty() {
        String::new()
    } else {
        crate_prefix
    };
    (module, crate_root)
}

pub fn index_file(path: &str, source: &str) -> Result<FileIndex, LangError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|e| LangError::Grammar(e.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| LangError::Parse(path.to_owned()))?;
    let (module, crate_root) = module_name(path);
    let mut x = Extractor {
        src: source.as_bytes(),
        module: module.clone(),
        crate_root,
        builders: BTreeMap::new(),
        order: Vec::new(),
        imports: Vec::new(),
        exports: Vec::new(),
        refs: Vec::new(),
    };
    let prefix = if module == "crate" {
        SymbolId::new("")
    } else {
        SymbolId::new(module.clone())
    };
    x.visit_items(tree.root_node(), &prefix, None);
    let symbols = x
        .order
        .iter()
        .map(|id| x.builders.remove(id).expect("builder").finish(path))
        .collect();
    let final_module = if module == "crate" { String::new() } else { module };
    Ok(FileIndex {
        path: path.to_owned(),
        module: final_module,
        symbols,
        imports: x.imports,
        exports: x.exports,
        refs: x.refs,
        has_errors: tree.root_node().has_error(),
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
    module: String,
    crate_root: String,
    builders: BTreeMap<SymbolId, Builder>,
    order: Vec<SymbolId>,
    imports: Vec<ImportBinding>,
    exports: Vec<ImportBinding>,
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

    fn visit_items(&mut self, block: Node, prefix: &SymbolId, impl_type: Option<&SymbolId>) {
        let mut c = block.walk();
        let children: Vec<Node> = block.named_children(&mut c).collect();
        let mut attrs: Vec<Node> = Vec::new();
        for n in children {
            if n.kind() == "attribute_item" {
                attrs.push(n);
                continue;
            }
            let item_attrs = std::mem::take(&mut attrs);
            match n.kind() {
                "line_comment" | "block_comment" | "inner_attribute_item" => {}
                "function_item" | "function_signature_item" => {
                    self.function(n, &item_attrs, prefix, impl_type.is_some());
                }
                "struct_item" => self.struct_item(n, &item_attrs, prefix),
                "enum_item" | "trait_item" => self.type_container(n, &item_attrs, prefix),
                "type_item" | "const_item" | "static_item" => self.value_item(n, &item_attrs, prefix),
                "impl_item" => self.impl_block(n, prefix),
                "mod_item" => {
                    if let (Some(name), Some(body)) = (n.child_by_field_name("name"), n.child_by_field_name("body")) {
                        let sub = qualify(prefix, self.text(name));
                        self.visit_items(body, &sub, None);
                    }
                }
                "use_declaration" => self.use_decl(n, prefix),
                _ => self.collect_refs(n, prefix),
            }
        }
    }

    fn function(&mut self, n: Node, attrs: &[Node], prefix: &SymbolId, is_method: bool) {
        let Some(name_node) = n.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node);
        let id = qualify(prefix, name);
        let kind = if is_method {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };
        let body = n.child_by_field_name("body");
        let params_node = n.child_by_field_name("parameters");
        let ret_node = n.child_by_field_name("return_type");

        let is_async = self.header_has(n, body, "async");
        let header_end = body.map(|b| b.start_byte()).unwrap_or(n.end_byte());
        let header_raw = std::str::from_utf8(&self.src[n.start_byte()..header_end]).unwrap_or("");
        let display = tidy_params(collapse_ws(header_raw).trim_end_matches(['{', ';']).trim());
        let mut canon = String::new();
        for a in attrs {
            canon.push_str(&self.canon(*a));
            canon.push(' ');
        }
        canon.push_str(&self.canon_range(n, body));
        let body_canon = body.map(|b| self.canon(b)).unwrap_or_default();

        let mut params = params_node.map(|p| self.params_of(p)).unwrap_or_default();
        if is_method && !params.first().is_some_and(|p| p.name == "self") {
            // Associated function called as `Type::func(...)` or `self.method(...)`:
            // keep `self` synthetic only when `self_parameter` was present.
        } else if is_method && params.first().is_some_and(|p| p.name == "self") {
            // `self` is already first parameter, matching bound-method rules.
        } else if is_method {
            // Associated function without `self` (like `@staticmethod` in Python).
        }
        let mut decorators: Vec<String> = attrs.iter().map(|a| self.canon(*a)).collect();
        if is_method && !params.first().is_some_and(|p| p.name == "self") {
            decorators.push("staticmethod".into());
        }
        let returns = ret_node.map(|r| self.canon(r));
        let sig = Signature {
            params: std::mem::take(&mut params),
            returns,
            decorators,
            is_async,
            bases: Vec::new(),
        };
        self.add_symbol(id.clone(), kind, n, display, canon, body_canon, Some(sig));
        if let Some(p) = params_node {
            self.collect_refs(p, &id);
        }
        if let Some(r) = ret_node {
            self.collect_refs(r, &id);
        }
        if let Some(b) = body {
            self.collect_refs(b, &id);
        }
    }

    fn params_of(&self, params: Node) -> Vec<Param> {
        let mut out = Vec::new();
        let mut c = params.walk();
        for ch in params.named_children(&mut c) {
            match ch.kind() {
                "self_parameter" => out.push(Param {
                    name: "self".into(),
                    kind: ParamKind::PositionalOnly,
                    annotation: None,
                    default: None,
                }),
                "parameter" => {
                    let pat = ch
                        .child_by_field_name("pattern")
                        .map(|p| self.text(p).trim_start_matches("mut ").trim().to_owned())
                        .unwrap_or_else(|| "_".into());
                    let ty = ch.child_by_field_name("type").map(|t| self.canon(t));
                    out.push(Param {
                        name: pat,
                        kind: ParamKind::PositionalOnly,
                        annotation: ty,
                        default: None,
                    });
                }
                "variadic_parameter" => out.push(Param {
                    name: "va_args".into(),
                    kind: ParamKind::VarPositional,
                    annotation: None,
                    default: None,
                }),
                _ => {}
            }
        }
        out
    }

    fn struct_item(&mut self, n: Node, attrs: &[Node], prefix: &SymbolId) {
        let Some(name_node) = n.child_by_field_name("name") else {
            return;
        };
        let id = qualify(prefix, self.text(name_node));
        let mut params = Vec::new();
        let mut has_named_fields = false;
        if let Some(body) = n.child_by_field_name("body") {
            has_named_fields = body.kind() == "field_declaration_list";
            let mut c = body.walk();
            for f in body.named_children(&mut c) {
                if f.kind() == "field_declaration" {
                    let fname = f
                        .child_by_field_name("name")
                        .map(|x| self.text(x).to_owned())
                        .unwrap_or_else(|| "_".into());
                    let fty = f.child_by_field_name("type").map(|t| self.canon(t));
                    params.push(Param {
                        name: fname,
                        kind: ParamKind::PositionalOrKeyword,
                        annotation: fty,
                        default: None,
                    });
                }
                self.collect_refs(f, &id);
            }
        }
        let mut canon = String::new();
        for a in attrs {
            canon.push_str(&self.canon(*a));
            canon.push(' ');
        }
        canon.push_str(&self.canon(n));
        let display = truncate(&collapse_ws(self.text(n)), 120);
        let sig = has_named_fields.then(|| Signature {
            params,
            returns: None,
            decorators: attrs.iter().map(|a| self.canon(*a)).collect(),
            is_async: false,
            bases: Vec::new(),
        });
        self.add_symbol(id, SymbolKind::Class, n, display, canon, String::new(), sig);
    }

    fn type_container(&mut self, n: Node, attrs: &[Node], prefix: &SymbolId) {
        let Some(name_node) = n.child_by_field_name("name") else {
            return;
        };
        let id = qualify(prefix, self.text(name_node));
        let mut canon = String::new();
        for a in attrs {
            canon.push_str(&self.canon(*a));
            canon.push(' ');
        }
        canon.push_str(&self.canon(n));
        let display = truncate(&collapse_ws(self.text(n)), 120);
        if n.kind() == "trait_item"
            && let Some(body) = n.child_by_field_name("body")
        {
            self.visit_items(body, &id, Some(&id));
        }
        self.add_symbol(id.clone(), SymbolKind::Class, n, display, canon, String::new(), None);
        self.collect_refs(n, &id);
    }

    fn value_item(&mut self, n: Node, attrs: &[Node], prefix: &SymbolId) {
        let Some(name_node) = n.child_by_field_name("name") else {
            return;
        };
        let id = qualify(prefix, self.text(name_node));
        let val = n.child_by_field_name("value");
        let mut canon = String::new();
        for a in attrs {
            canon.push_str(&self.canon(*a));
            canon.push(' ');
        }
        canon.push_str(&self.canon_range(n, val));
        let body_canon = val.map(|v| self.canon(v)).unwrap_or_default();
        let end = val.map(|v| v.start_byte()).unwrap_or(n.end_byte());
        let raw = std::str::from_utf8(&self.src[n.start_byte()..end]).unwrap_or("");
        let display = collapse_ws(raw).trim_end_matches(['=', ';']).trim().to_owned();
        self.add_symbol(id.clone(), SymbolKind::Variable, n, display, canon, body_canon, None);
        self.collect_refs(n, &id);
    }

    fn impl_block(&mut self, n: Node, prefix: &SymbolId) {
        let Some(ty) = n.child_by_field_name("type") else {
            return;
        };
        let Some(body) = n.child_by_field_name("body") else {
            return;
        };
        let ty_name = self.type_head_name(ty);
        if ty_name.is_empty() {
            return;
        }
        let target = qualify(prefix, &ty_name);
        self.visit_items(body, &target, Some(&target));
    }

    fn type_head_name(&self, ty: Node) -> String {
        match ty.kind() {
            "type_identifier" | "identifier" => self.text(ty).to_owned(),
            "generic_type" => ty
                .child_by_field_name("type")
                .map(|t| self.type_head_name(t))
                .unwrap_or_default(),
            "scoped_type_identifier" => ty
                .child_by_field_name("name")
                .map(|t| self.text(t).to_owned())
                .unwrap_or_default(),
            _ => String::new(),
        }
    }

    fn use_decl(&mut self, n: Node, from: &SymbolId) {
        let line = n.start_position().row as u32 + 1;
        let text = truncate(&collapse_ws(self.text(n)), 100);
        let is_pub = self.text(n).trim_start().starts_with("pub ");
        if let Some(arg) = n.child_by_field_name("argument") {
            let mut bindings = Vec::new();
            self.collect_use_tree(arg, "", &mut bindings);
            for (local, target) in bindings {
                let resolved = self.resolve_rust_path(&target);
                if is_pub {
                    self.exports.push(ImportBinding {
                        local: local.clone(),
                        target: resolved.clone(),
                        line,
                    });
                }
                self.imports.push(ImportBinding {
                    local: local.clone(),
                    target: resolved,
                    line,
                });
                if local != "*" {
                    self.refs.push(RawRef {
                        from: from.clone(),
                        chain: vec![local],
                        line,
                        text: text.clone(),
                        is_call: false,
                        args: None,
                    });
                }
            }
        }
    }

    fn collect_use_tree(&self, n: Node, prefix: &str, out: &mut Vec<(String, String)>) {
        let join_p = |p: &str, s: &str| {
            if p.is_empty() {
                s.to_owned()
            } else {
                format!("{p}::{s}")
            }
        };
        match n.kind() {
            "identifier" | "crate" | "self" | "super" => {
                let s = self.text(n);
                out.push((s.to_owned(), join_p(prefix, s)));
            }
            "scoped_identifier" => {
                let full = self.text(n).replace(' ', "");
                let local = full.rsplit("::").next().unwrap_or(&full).to_owned();
                out.push((local, join_p(prefix, &full)));
            }
            "use_as_clause" => {
                let path = n
                    .child_by_field_name("path")
                    .map(|p| self.text(p).replace(' ', ""))
                    .unwrap_or_default();
                let alias = n
                    .child_by_field_name("alias")
                    .map(|a| self.text(a).to_owned())
                    .unwrap_or_default();
                if !path.is_empty() && !alias.is_empty() {
                    out.push((alias, join_p(prefix, &path)));
                }
            }
            "use_wildcard" => {
                let raw = self.text(n).replace(' ', "");
                let p = raw.strip_suffix("::*").unwrap_or("");
                out.push(("*".into(), join_p(prefix, p)));
            }
            "scoped_use_list" => {
                let path = n
                    .child_by_field_name("path")
                    .map(|p| self.text(p).replace(' ', ""))
                    .unwrap_or_default();
                let next_prefix = if path.is_empty() {
                    prefix.to_owned()
                } else {
                    join_p(prefix, &path)
                };
                if let Some(list) = n.child_by_field_name("list") {
                    let mut c = list.walk();
                    for ch in list.named_children(&mut c) {
                        self.collect_use_tree(ch, &next_prefix, out);
                    }
                }
            }
            "use_list" => {
                let mut c = n.walk();
                for ch in n.named_children(&mut c) {
                    self.collect_use_tree(ch, prefix, out);
                }
            }
            _ => {}
        }
    }

    fn resolve_rust_path(&self, raw: &str) -> String {
        let segs: Vec<&str> = raw.split("::").filter(|s| !s.is_empty()).collect();
        if segs.is_empty() {
            return String::new();
        }
        match segs[0] {
            "crate" => {
                let mut out = Vec::new();
                if !self.crate_root.is_empty() {
                    out.push(self.crate_root.as_str());
                }
                out.extend_from_slice(&segs[1..]);
                out.join(".")
            }
            "self" => {
                let mut out: Vec<&str> = if self.module == "crate" {
                    Vec::new()
                } else {
                    self.module.split('.').collect()
                };
                out.extend_from_slice(&segs[1..]);
                out.join(".")
            }
            "super" => {
                let mut out: Vec<&str> = if self.module == "crate" {
                    Vec::new()
                } else {
                    self.module.split('.').collect()
                };
                let mut idx = 0;
                while idx < segs.len() && segs[idx] == "super" {
                    out.pop();
                    idx += 1;
                }
                out.extend_from_slice(&segs[idx..]);
                out.join(".")
            }
            _ => segs.join("."),
        }
    }

    fn header_has(&self, n: Node, until: Option<Node>, kw: &str) -> bool {
        let end = until.map(|u| u.start_byte()).unwrap_or(n.end_byte());
        let mut c = n.walk();
        n.children(&mut c)
            .take_while(|ch| ch.end_byte() <= end)
            .any(|ch| ch.kind() == kw || self.text(ch) == kw)
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
            "line_comment" | "block_comment" => {}
            "string_literal" | "raw_string_literal" | "char_literal" => {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(self.text(n));
            }
            _ if n.child_count() == 0 => {
                let tok = self.text(n);
                if !tok.trim().is_empty() {
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

    fn call_args(&self, args: Node) -> CallArgs {
        let mut out = CallArgs::default();
        let mut c = args.walk();
        for ch in args.named_children(&mut c) {
            if !matches!(ch.kind(), "line_comment" | "block_comment") {
                out.positional += 1;
            }
        }
        out
    }

    fn collect_refs(&mut self, n: Node, from: &SymbolId) {
        match n.kind() {
            "line_comment" | "block_comment" | "string_literal" | "raw_string_literal" | "use_declaration" => {}
            "call_expression" => {
                let func = n.child_by_field_name("function");
                if let Some(chain) = func.and_then(|f| self.chain_of(f)) {
                    let args_node = n.child_by_field_name("arguments");
                    self.refs.push(RawRef {
                        from: from.clone(),
                        chain,
                        line: n.start_position().row as u32 + 1,
                        text: truncate(&collapse_ws(self.text(n)), 100),
                        is_call: true,
                        args: args_node.map(|a| self.call_args(a)),
                    });
                    if let Some(a) = args_node {
                        self.collect_refs(a, from);
                    }
                    if let Some(f) = func
                        && f.kind() == "field_expression"
                        && let Some(val) = f.child_by_field_name("value")
                    {
                        self.collect_refs(val, from);
                    }
                } else {
                    self.recurse(n, from);
                }
            }
            "struct_expression" => {
                if let Some(name) = n.child_by_field_name("name")
                    && let Some(chain) = self.chain_of(name)
                {
                    let mut keywords = Vec::new();
                    if let Some(body) = n.child_by_field_name("body") {
                        let mut c = body.walk();
                        for f in body.named_children(&mut c) {
                            if f.kind() == "field_initializer"
                                && let Some(fnm) = f.child_by_field_name("field")
                            {
                                keywords.push(self.text(fnm).to_owned());
                            } else if f.kind() == "shorthand_field_initializer" {
                                keywords.push(self.text(f).trim().to_owned());
                            }
                        }
                    }
                    self.refs.push(RawRef {
                        from: from.clone(),
                        chain,
                        line: n.start_position().row as u32 + 1,
                        text: truncate(&collapse_ws(self.text(n)), 100),
                        is_call: true,
                        args: Some(CallArgs {
                            positional: 0,
                            keywords,
                            star: false,
                            double_star: false,
                        }),
                    });
                }
                self.recurse(n, from);
            }
            "scoped_identifier" | "field_expression" | "type_identifier" | "identifier" => {
                if let Some(chain) = self.chain_of(n) {
                    self.refs.push(RawRef {
                        from: from.clone(),
                        chain,
                        line: n.start_position().row as u32 + 1,
                        text: truncate(&collapse_ws(self.text(n)), 100),
                        is_call: false,
                        args: None,
                    });
                } else {
                    self.recurse(n, from);
                }
            }
            _ => self.recurse(n, from),
        }
    }

    fn recurse(&mut self, n: Node, from: &SymbolId) {
        let mut c = n.walk();
        let children: Vec<Node> = n.named_children(&mut c).collect();
        for ch in children {
            self.collect_refs(ch, from);
        }
    }

    fn chain_of(&self, n: Node) -> Option<Vec<String>> {
        match n.kind() {
            "identifier" | "type_identifier" | "self" => Some(vec![self.text(n).to_owned()]),
            "scoped_identifier" | "scoped_type_identifier" => {
                let raw = self.text(n).replace(' ', "");
                let resolved = self.resolve_rust_path(&raw);
                let parts: Vec<String> = resolved
                    .split('.')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                (!parts.is_empty()).then_some(parts)
            }
            "field_expression" => {
                let val = n.child_by_field_name("value")?;
                let field = n.child_by_field_name("field")?;
                if let Some(mut base) = self.chain_of(val) {
                    base.push(self.text(field).to_owned());
                    Some(base)
                } else {
                    Some(vec!["?".into(), self.text(field).to_owned()])
                }
            }
            "generic_function" => n.child_by_field_name("function").and_then(|f| self.chain_of(f)),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concord_core::diff_symbols;

    #[test]
    fn indexes_rust_functions_structs_methods_and_calls() {
        let email = r#"
pub fn send_email(to: &str, body: &str) -> bool {
    !to.is_empty() && !body.is_empty()
}
"#;
        let email2 = r#"
pub fn send_email(to: &str, body: &str, user_id: u64) -> bool {
    user_id > 0 && !to.is_empty() && !body.is_empty()
}
"#;
        let a = index_file("src/notify/email.rs", email).unwrap();
        let b = index_file("src/notify/email.rs", email2).unwrap();
        let changes = diff_symbols(&a.symbols, &b.symbols);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].is_contract_change() && changes[0].is_breaking());

        let reset = r#"
use crate::notify::email::send_email;

pub fn request_reset(addr: &str) -> bool {
    send_email(addr, "token")
}
"#;
        let r = index_file("src/auth/reset.rs", reset).unwrap();
        assert!(
            r.imports
                .iter()
                .any(|i| i.local == "send_email" && i.target == "notify.email.send_email")
        );
        assert!(r.refs.iter().any(|rf| rf.is_call && rf.chain == ["send_email"]));
    }
}
