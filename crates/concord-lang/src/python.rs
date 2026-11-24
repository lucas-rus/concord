//! Python front-end built on tree-sitter.
//!
//! Extracts, per file:
//! - symbols (functions, methods, classes, module/class variables) with a
//!   normalised *contract* and a *body* digest;
//! - import bindings;
//! - raw references (calls, attribute chains, names) to be resolved by
//!   [`crate::RepoIndex`].
//!
//! Canonicalisation: digests are computed over the token stream of the syntax
//! tree (leaves joined by a single space, comments removed, string literals kept
//! verbatim), so formatting and comments never produce a change.

use crate::{FileIndex, ImportBinding, LangError, RawRef};
use concord_core::{CallArgs, Contract, Digest, Param, ParamKind, Signature, Span, Symbol, SymbolId, SymbolKind};
use std::collections::BTreeMap;
use tree_sitter::{Node, Parser};

/// Dotted module name for a repository-relative path (`notify/email.py` ->
/// `notify.email`, `pkg/__init__.py` -> `pkg`, `src/pkg/m.py` -> `pkg.m`).
/// Returns the module name and whether the file is a package `__init__`.
pub fn module_name(path: &str) -> (String, bool) {
    let p = path.trim_start_matches("./");
    let p = p.strip_prefix("src/").unwrap_or(p);
    let p = p.strip_suffix(".pyi").or_else(|| p.strip_suffix(".py")).unwrap_or(p);
    let mut parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let is_package = parts.last() == Some(&"__init__");
    if is_package {
        parts.pop();
    }
    (parts.join("."), is_package)
}

pub fn is_python_path(path: &str) -> bool {
    path.ends_with(".py") || path.ends_with(".pyi")
}

/// Parse and index one Python file.
pub fn index_file(path: &str, source: &str) -> Result<FileIndex, LangError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_python::LANGUAGE.into())
        .map_err(|e| LangError::Grammar(e.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| LangError::Parse(path.to_owned()))?;
    let (module, is_package) = module_name(path);
    let mut x = Extractor {
        src: source.as_bytes(),
        module: module.clone(),
        is_package,
        builders: BTreeMap::new(),
        order: Vec::new(),
        imports: Vec::new(),
        refs: Vec::new(),
    };
    let module_id = SymbolId::new(module.clone());
    x.visit_block(
        tree.root_node(),
        &Scope {
            prefix: module_id,
            class: None,
        },
    );

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
        has_errors: tree.root_node().has_error(),
    })
}

struct Scope {
    prefix: SymbolId,
    /// Set when the scope is a class body.
    class: Option<SymbolId>,
}

/// Accumulates one symbol; repeated definitions with the same qualified name
/// (overloads, property setters, conditional definitions) are merged.
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
                // Several definitions (overloads, property setters): no single signature.
                signature: if self.defs == 1 { self.signature } else { None },
            },
            body: Digest::of(&self.body_canon),
        }
    }
}

struct Extractor<'a> {
    src: &'a [u8],
    module: String,
    is_package: bool,
    builders: BTreeMap<SymbolId, Builder>,
    order: Vec<SymbolId>,
    imports: Vec<ImportBinding>,
    refs: Vec<RawRef>,
}

impl<'a> Extractor<'a> {
    fn text(&self, n: Node) -> &'a str {
        n.utf8_text(self.src).unwrap_or("")
    }

    #[allow(clippy::too_many_arguments)] // private helper; a parameter struct would add noise
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

    fn visit_block(&mut self, block: Node, scope: &Scope) {
        let mut cursor = block.walk();
        let children: Vec<Node> = block.named_children(&mut cursor).collect();
        for stmt in children {
            self.visit_statement(stmt, scope);
        }
    }

    fn visit_statement(&mut self, stmt: Node, scope: &Scope) {
        match stmt.kind() {
            "function_definition" => self.function(stmt, stmt, &[], scope),
            "class_definition" => self.class(stmt, stmt, &[], scope),
            "decorated_definition" => {
                let mut c = stmt.walk();
                let decorators: Vec<Node> = stmt
                    .named_children(&mut c)
                    .filter(|n| n.kind() == "decorator")
                    .collect();
                if let Some(def) = stmt.child_by_field_name("definition") {
                    match def.kind() {
                        "function_definition" => self.function(def, stmt, &decorators, scope),
                        "class_definition" => self.class(def, stmt, &decorators, scope),
                        _ => {}
                    }
                }
            }
            "import_statement" | "import_from_statement" | "future_import_statement" => {
                self.import(stmt, &scope.prefix)
            }
            "expression_statement" => {
                let mut c = stmt.walk();
                let first = stmt.named_children(&mut c).next();
                match first {
                    Some(a) if a.kind() == "assignment" => self.assignment(a, stmt, scope),
                    _ => self.collect_refs(stmt, &scope.prefix),
                }
            }
            // Compound statements at module/class level may contain definitions
            // and imports (e.g. `if TYPE_CHECKING:`, `try: import x`).
            "if_statement" | "try_statement" | "with_statement" | "else_clause" | "elif_clause" | "except_clause"
            | "finally_clause" | "block" => {
                let mut c = stmt.walk();
                let children: Vec<Node> = stmt.named_children(&mut c).collect();
                for ch in children {
                    match ch.kind() {
                        "block" | "else_clause" | "elif_clause" | "except_clause" | "finally_clause" => {
                            self.visit_statement(ch, scope)
                        }
                        _ => self.collect_refs(ch, &scope.prefix),
                    }
                }
            }
            "comment" => {}
            _ => self.collect_refs(stmt, &scope.prefix),
        }
    }

    /// `outer` is the decorated_definition (if any) so the span includes decorators.
    fn function(&mut self, def: Node, outer: Node, decorators: &[Node], scope: &Scope) {
        let Some(name_node) = def.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node).to_owned();
        let id = scope.prefix.child(&name);
        let kind = if scope.class.is_some() {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };
        let is_async = {
            let mut c = def.walk();
            def.children(&mut c).any(|n| n.kind() == "async")
        };
        let params = def.child_by_field_name("parameters");
        let ret = def.child_by_field_name("return_type");
        let type_params = def.child_by_field_name("type_parameters");
        let body = def.child_by_field_name("body");

        let mut canon = String::new();
        let mut display = String::new();
        for d in decorators {
            canon.push_str(&self.canon(*d));
            canon.push(' ');
            display.push_str(&collapse_ws(self.text(*d)));
            display.push(' ');
        }
        if is_async {
            canon.push_str("async ");
            display.push_str("async ");
        }
        canon.push_str("def ");
        canon.push_str(&name);
        display.push_str("def ");
        display.push_str(&name);
        if let Some(tp) = type_params {
            canon.push_str(&self.canon(tp));
            display.push_str(&collapse_ws(self.text(tp)));
        }
        if let Some(p) = params {
            canon.push(' ');
            canon.push_str(&self.canon(p));
            display.push_str(&tidy_params(&collapse_ws(self.text(p))));
        }
        if let Some(r) = ret {
            canon.push_str(" -> ");
            canon.push_str(&self.canon(r));
            display.push_str(" -> ");
            display.push_str(&collapse_ws(self.text(r)));
        }
        let body_canon = body.map(|b| self.canon(b)).unwrap_or_default();
        let signature = Signature {
            params: params.map(|p| self.params_of(p)).unwrap_or_default(),
            returns: ret.map(|r| self.canon(r)),
            decorators: decorators.iter().map(|d| self.canon(*d)).collect(),
            is_async,
            bases: Vec::new(),
        };
        self.add_symbol(id.clone(), kind, outer, display, canon, body_canon, Some(signature));

        // References: decorators, parameter annotations/defaults, return type, body.
        for d in decorators {
            self.collect_refs(*d, &id);
        }
        if let Some(p) = params {
            self.collect_refs(p, &id);
        }
        if let Some(r) = ret {
            self.collect_refs(r, &id);
        }
        if let Some(b) = body {
            self.collect_refs(b, &id);
        }
    }

    fn class(&mut self, def: Node, outer: Node, decorators: &[Node], scope: &Scope) {
        let Some(name_node) = def.child_by_field_name("name") else {
            return;
        };
        let name = self.text(name_node).to_owned();
        let id = scope.prefix.child(&name);
        let bases = def.child_by_field_name("superclasses");
        let body = def.child_by_field_name("body");

        // Members first, so the class contract can include __init__ and fields.
        if let Some(b) = body {
            self.visit_block(
                b,
                &Scope {
                    prefix: id.clone(),
                    class: Some(id.clone()),
                },
            );
        }

        let mut canon = String::new();
        let mut display = String::new();
        for d in decorators {
            canon.push_str(&self.canon(*d));
            canon.push(' ');
            display.push_str(&collapse_ws(self.text(*d)));
            display.push(' ');
        }
        canon.push_str("class ");
        canon.push_str(&name);
        display.push_str("class ");
        display.push_str(&name);
        if let Some(bs) = bases {
            canon.push(' ');
            canon.push_str(&self.canon(bs));
            display.push_str(&tidy_params(&collapse_ws(self.text(bs))));
        }

        // Constructor-relevant parts of the contract: annotated fields
        // (dataclass/attrs/pydantic style) and the __init__ signature.
        let mut fields_display = Vec::new();
        let mut field_params: Vec<Param> = Vec::new();
        let mut body_canon = String::new();
        if let Some(b) = body {
            let mut c = b.walk();
            for stmt in b.named_children(&mut c) {
                match stmt.kind() {
                    "function_definition" | "decorated_definition" | "class_definition" | "comment" => {}
                    "expression_statement" => {
                        let mut c2 = stmt.walk();
                        if let Some(a) = stmt.named_children(&mut c2).next()
                            && a.kind() == "assignment"
                            && a.child_by_field_name("type").is_some()
                        {
                            let left = a.child_by_field_name("left").map(|n| self.canon(n)).unwrap_or_default();
                            let ty = a.child_by_field_name("type").map(|n| self.canon(n)).unwrap_or_default();
                            let has_default = a.child_by_field_name("right").is_some();
                            field_params.push(Param {
                                name: a
                                    .child_by_field_name("left")
                                    .map(|n| self.text(n))
                                    .unwrap_or("")
                                    .to_owned(),
                                kind: ParamKind::PositionalOrKeyword,
                                annotation: Some(ty.clone()),
                                default: a.child_by_field_name("right").map(|r| self.canon(r)),
                            });
                            canon.push_str(&format!(" | {left} : {ty}{}", if has_default { " =" } else { "" }));
                            fields_display.push(format!(
                                "{}: {}{}",
                                a.child_by_field_name("left").map(|n| self.text(n)).unwrap_or(""),
                                a.child_by_field_name("type")
                                    .map(|n| collapse_ws(self.text(n)))
                                    .unwrap_or_default(),
                                if has_default { " = ..." } else { "" }
                            ));
                        }
                        body_canon.push_str(&self.canon(stmt));
                        body_canon.push(' ');
                    }
                    _ => {
                        body_canon.push_str(&self.canon(stmt));
                        body_canon.push(' ');
                    }
                }
            }
        }
        let init_id = id.child("__init__");
        let init = self
            .builders
            .get(&init_id)
            .map(|b| (b.contract_canon.clone(), b.display.clone()));
        if let Some((init_canon, _)) = &init {
            canon.push_str(" | init ");
            canon.push_str(init_canon);
        }
        let mut extra = Vec::new();
        if !fields_display.is_empty() {
            extra.push(fields_display.join(", "));
        }
        if let Some((_, init_display)) = &init {
            extra.push(init_display.trim_start_matches("def ").to_owned());
        }
        if !extra.is_empty() {
            display.push_str(": ");
            display.push_str(&extra.join("; "));
        }

        // Constructor signature: __init__ without `self`, else synthesised from
        // annotated fields, else empty if there is no base class to inherit from.
        let init_sig = self
            .builders
            .get(&init_id)
            .and_then(|b| if b.defs == 1 { b.signature.clone() } else { None });
        let bases_canon: Vec<String> = bases
            .map(|bs| {
                let mut c = bs.walk();
                bs.named_children(&mut c).map(|n| self.canon(n)).collect()
            })
            .unwrap_or_default();
        let ctor_params: Option<Vec<Param>> = match init_sig {
            Some(sig) => Some(sig.params.into_iter().skip(1).collect()),
            None if !field_params.is_empty() => Some(field_params),
            None if bases_canon.is_empty() => Some(Vec::new()),
            None => None,
        };
        let signature = ctor_params.map(|params| Signature {
            params,
            returns: None,
            decorators: decorators.iter().map(|d| self.canon(*d)).collect(),
            is_async: false,
            bases: bases_canon,
        });
        self.add_symbol(
            id.clone(),
            SymbolKind::Class,
            outer,
            display,
            canon,
            body_canon,
            signature,
        );
        for d in decorators {
            self.collect_refs(*d, &id);
        }
        if let Some(bs) = bases {
            self.collect_refs(bs, &id);
        }
    }

    fn assignment(&mut self, a: Node, stmt: Node, scope: &Scope) {
        let left = a.child_by_field_name("left");
        let ty = a.child_by_field_name("type");
        let right = a.child_by_field_name("right");
        match left {
            Some(l) if l.kind() == "identifier" => {
                let name = self.text(l).to_owned();
                let id = scope.prefix.child(&name);
                let mut canon = name.clone();
                let mut display = name.clone();
                if let Some(t) = ty {
                    canon.push_str(" : ");
                    canon.push_str(&self.canon(t));
                    display.push_str(": ");
                    display.push_str(&collapse_ws(self.text(t)));
                }
                let body = right.map(|r| self.canon(r)).unwrap_or_default();
                self.add_symbol(id, SymbolKind::Variable, stmt, display, canon, body, None);
                if let Some(t) = ty {
                    self.collect_refs(t, &scope.prefix.child(&name));
                }
                if let Some(r) = right {
                    self.collect_refs(r, &scope.prefix.child(&name));
                }
            }
            _ => self.collect_refs(stmt, &scope.prefix),
        }
    }

    fn import(&mut self, stmt: Node, from: &SymbolId) {
        let line = stmt.start_position().row as u32 + 1;
        let text = truncate(&collapse_ws(self.text(stmt)), 100);
        match stmt.kind() {
            "import_statement" => {
                let mut c = stmt.walk();
                for n in stmt.children_by_field_name("name", &mut c) {
                    match n.kind() {
                        "dotted_name" => {
                            let full = self.text(n).to_owned();
                            let top = full.split('.').next().unwrap_or(&full).to_owned();
                            self.imports.push(ImportBinding {
                                local: top.clone(),
                                target: top,
                                line,
                            });
                        }
                        "aliased_import" => {
                            let target = n.child_by_field_name("name").map(|x| self.text(x).to_owned());
                            let alias = n.child_by_field_name("alias").map(|x| self.text(x).to_owned());
                            if let (Some(t), Some(a)) = (target, alias) {
                                self.imports.push(ImportBinding {
                                    local: a,
                                    target: t,
                                    line,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
            "import_from_statement" => {
                let Some(m) = stmt.child_by_field_name("module_name") else {
                    return;
                };
                let module = match m.kind() {
                    "relative_import" => self.resolve_relative(m),
                    _ => self.text(m).to_owned(),
                };
                let mut c = stmt.walk();
                let names: Vec<Node> = stmt.children_by_field_name("name", &mut c).collect();
                if names.is_empty() {
                    let mut c2 = stmt.walk();
                    if stmt.named_children(&mut c2).any(|n| n.kind() == "wildcard_import") {
                        self.imports.push(ImportBinding {
                            local: "*".into(),
                            target: module,
                            line,
                        });
                    }
                    return;
                }
                for n in names {
                    let (name, local) = match n.kind() {
                        "aliased_import" => (
                            n.child_by_field_name("name").map(|x| self.text(x).to_owned()),
                            n.child_by_field_name("alias").map(|x| self.text(x).to_owned()),
                        ),
                        _ => (Some(self.text(n).to_owned()), None),
                    };
                    let Some(name) = name else { continue };
                    let local = local.unwrap_or_else(|| name.rsplit('.').next().unwrap_or(&name).to_owned());
                    let target = if module.is_empty() {
                        name.clone()
                    } else {
                        format!("{module}.{name}")
                    };
                    self.imports.push(ImportBinding {
                        local: local.clone(),
                        target,
                        line,
                    });
                    // The import itself depends on the imported symbol.
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
            _ => {}
        }
    }

    /// Argument shape of a call's `arguments` node.
    fn call_args(&self, args: Node) -> CallArgs {
        let mut out = CallArgs::default();
        if args.kind() != "argument_list" {
            out.positional = 1; // e.g. a generator expression argument
            return out;
        }
        let mut c = args.walk();
        for a in args.named_children(&mut c) {
            match a.kind() {
                "keyword_argument" => {
                    if let Some(n) = a.child_by_field_name("name") {
                        out.keywords.push(self.text(n).to_owned());
                    }
                }
                "list_splat" => out.star = true,
                "dictionary_splat" => out.double_star = true,
                "comment" => {}
                _ => out.positional += 1,
            }
        }
        out
    }

    /// Structured parameters of a `parameters` node.
    fn params_of(&self, params: Node) -> Vec<Param> {
        let mut out: Vec<Param> = Vec::new();
        let mut kw_only = false;
        let named_kind = |kw_only: bool| {
            if kw_only {
                ParamKind::KeywordOnly
            } else {
                ParamKind::PositionalOrKeyword
            }
        };
        let mut c = params.walk();
        for ch in params.named_children(&mut c) {
            match ch.kind() {
                "identifier" => out.push(Param {
                    name: self.text(ch).to_owned(),
                    kind: named_kind(kw_only),
                    annotation: None,
                    default: None,
                }),
                "default_parameter" | "typed_default_parameter" => {
                    let Some(name) = ch.child_by_field_name("name") else {
                        continue;
                    };
                    out.push(Param {
                        name: self.text(name).to_owned(),
                        kind: named_kind(kw_only),
                        annotation: ch.child_by_field_name("type").map(|t| self.canon(t)),
                        default: ch.child_by_field_name("value").map(|v| self.canon(v)),
                    });
                }
                "typed_parameter" => {
                    let annotation = ch.child_by_field_name("type").map(|t| self.canon(t));
                    let Some(inner) = ch.named_child(0) else { continue };
                    let (name, kind) = match inner.kind() {
                        "list_splat_pattern" => {
                            kw_only = true;
                            (
                                inner.named_child(0).map(|n| self.text(n)).unwrap_or("args"),
                                ParamKind::VarPositional,
                            )
                        }
                        "dictionary_splat_pattern" => (
                            inner.named_child(0).map(|n| self.text(n)).unwrap_or("kwargs"),
                            ParamKind::VarKeyword,
                        ),
                        _ => (self.text(inner), named_kind(kw_only)),
                    };
                    out.push(Param {
                        name: name.to_owned(),
                        kind,
                        annotation,
                        default: None,
                    });
                }
                "list_splat_pattern" => {
                    kw_only = true;
                    out.push(Param {
                        name: ch.named_child(0).map(|n| self.text(n)).unwrap_or("args").to_owned(),
                        kind: ParamKind::VarPositional,
                        annotation: None,
                        default: None,
                    });
                }
                "dictionary_splat_pattern" => out.push(Param {
                    name: ch.named_child(0).map(|n| self.text(n)).unwrap_or("kwargs").to_owned(),
                    kind: ParamKind::VarKeyword,
                    annotation: None,
                    default: None,
                }),
                "keyword_separator" => kw_only = true,
                "positional_separator" => {
                    for p in out.iter_mut().filter(|p| p.kind == ParamKind::PositionalOrKeyword) {
                        p.kind = ParamKind::PositionalOnly;
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn resolve_relative(&self, rel: Node) -> String {
        let mut dots = 0usize;
        let mut tail = None;
        let mut c = rel.walk();
        for n in rel.children(&mut c) {
            match n.kind() {
                "import_prefix" => dots = self.text(n).chars().filter(|&ch| ch == '.').count(),
                "dotted_name" => tail = Some(self.text(n).to_owned()),
                _ => {}
            }
        }
        let mut parts: Vec<&str> = self.module.split('.').filter(|s| !s.is_empty()).collect();
        if !self.is_package {
            parts.pop();
        }
        for _ in 1..dots {
            parts.pop();
        }
        let mut base = parts.join(".");
        if let Some(t) = tail {
            if !base.is_empty() {
                base.push('.');
            }
            base.push_str(&t);
        }
        base
    }

    /// Canonical token stream of a subtree.
    fn canon(&self, n: Node) -> String {
        let mut out = String::new();
        self.canon_into(n, &mut out);
        out
    }

    fn canon_into(&self, n: Node, out: &mut String) {
        match n.kind() {
            "comment" => {}
            "string" | "concatenated_string" => push_token(out, self.text(n)),
            _ if n.child_count() == 0 => push_token(out, self.text(n)),
            _ => {
                let mut c = n.walk();
                for ch in n.children(&mut c) {
                    self.canon_into(ch, out);
                }
            }
        }
    }

    /// Collect raw references (calls, names, attribute chains) inside `n`.
    fn collect_refs(&mut self, n: Node, from: &SymbolId) {
        match n.kind() {
            "comment" | "import_statement" | "import_from_statement" => {}
            // Only the `{...}` interpolations of (f-)strings contain code.
            "string" | "concatenated_string" => {
                let mut c = n.walk();
                let children: Vec<Node> = n.named_children(&mut c).collect();
                for ch in children {
                    match ch.kind() {
                        "interpolation" | "string" => self.collect_refs(ch, from),
                        _ => {}
                    }
                }
            }
            "call" => {
                let func = n.child_by_field_name("function");
                let chain = func.and_then(|f| self.chain_of(f));
                match chain {
                    Some(chain) => {
                        self.refs.push(RawRef {
                            from: from.clone(),
                            chain,
                            line: n.start_position().row as u32 + 1,
                            text: truncate(&collapse_ws(self.text(n)), 100),
                            is_call: true,
                            args: n.child_by_field_name("arguments").map(|a| self.call_args(a)),
                        });
                        if let Some(args) = n.child_by_field_name("arguments") {
                            self.collect_refs(args, from);
                        }
                    }
                    None => self.recurse(n, from),
                }
            }
            "attribute" | "identifier" => match self.chain_of(n) {
                Some(chain) => self.refs.push(RawRef {
                    from: from.clone(),
                    chain,
                    line: n.start_position().row as u32 + 1,
                    text: truncate(&collapse_ws(self.text(n)), 100),
                    is_call: false,
                    args: None,
                }),
                None => self.recurse(n, from),
            },
            "keyword_argument" => {
                if let Some(v) = n.child_by_field_name("value") {
                    self.collect_refs(v, from);
                }
            }
            "default_parameter" | "typed_default_parameter" => {
                for f in ["type", "value"] {
                    if let Some(v) = n.child_by_field_name(f) {
                        self.collect_refs(v, from);
                    }
                }
            }
            "typed_parameter" => {
                if let Some(t) = n.child_by_field_name("type") {
                    self.collect_refs(t, from);
                }
            }
            "parameters" | "lambda_parameters" => {
                let mut c = n.walk();
                let children: Vec<Node> = n.named_children(&mut c).collect();
                for ch in children {
                    if ch.kind() != "identifier" {
                        self.collect_refs(ch, from);
                    }
                }
            }
            "function_definition" | "class_definition" => {
                let mut c = n.walk();
                let children: Vec<Node> = n.named_children(&mut c).collect();
                let name = n.child_by_field_name("name");
                for ch in children {
                    if Some(ch) != name {
                        self.collect_refs(ch, from);
                    }
                }
            }
            "assignment" | "augmented_assignment" => {
                if let Some(l) = n.child_by_field_name("left")
                    && l.kind() != "identifier"
                    && l.kind() != "pattern_list"
                    && l.kind() != "tuple_pattern"
                {
                    self.collect_refs(l, from);
                }
                for f in ["type", "right"] {
                    if let Some(v) = n.child_by_field_name(f) {
                        self.collect_refs(v, from);
                    }
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

    /// `a.b.c` -> ["a","b","c"]; anything that is not a plain name/attribute chain -> None.
    fn chain_of(&self, n: Node) -> Option<Vec<String>> {
        match n.kind() {
            "identifier" => Some(vec![self.text(n).to_owned()]),
            "attribute" => {
                let obj = n.child_by_field_name("object")?;
                let attr = n.child_by_field_name("attribute")?;
                let mut chain = self.chain_of(obj)?;
                chain.push(self.text(attr).to_owned());
                Some(chain)
            }
            _ => None,
        }
    }
}

pub(crate) fn span_of(n: Node) -> Span {
    Span {
        start_line: n.start_position().row as u32 + 1,
        end_line: n.end_position().row as u32 + 1,
    }
}

fn push_token(out: &mut String, tok: &str) {
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(tok);
}

pub(crate) fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `( a, b, )` -> `(a, b)` for display purposes.
pub(crate) fn tidy_params(s: &str) -> String {
    s.replace("( ", "(")
        .replace(" )", ")")
        .replace(", )", ")")
        .replace(",)", ")")
}

pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let mut t: String = s.chars().take(max.saturating_sub(3)).collect();
        t.push_str("...");
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym<'a>(f: &'a FileIndex, id: &str) -> &'a Symbol {
        f.symbols.iter().find(|s| s.id.as_str() == id).unwrap_or_else(|| {
            panic!(
                "symbol {id} not found in {:?}",
                f.symbols.iter().map(|s| &s.id).collect::<Vec<_>>()
            )
        })
    }

    #[test]
    fn module_names() {
        assert_eq!(module_name("notify/email.py"), ("notify.email".into(), false));
        assert_eq!(module_name("pkg/__init__.py"), ("pkg".into(), true));
        assert_eq!(module_name("src/pkg/m.py"), ("pkg.m".into(), false));
    }

    #[test]
    fn extracts_functions_classes_methods_variables() {
        let src = r#"
import os
from typing import Optional

MAX_RETRIES: int = 3

@cache
async def fetch(url: str, *, timeout: float = 1.0) -> bytes:
    """Fetch."""
    return await get(url)

class Cart(Base):
    owner: str
    items: list = []

    def __init__(self, owner):
        self.owner = owner

    def add(self, item, qty=1):
        self.items.append(item)
"#;
        let f = index_file("shop/cart.py", src).unwrap();
        assert_eq!(f.module, "shop.cart");
        let fetch = sym(&f, "shop.cart.fetch");
        assert_eq!(fetch.kind, SymbolKind::Function);
        assert_eq!(
            fetch.contract.display,
            "@cache async def fetch(url: str, *, timeout: float = 1.0) -> bytes"
        );
        assert_eq!(sym(&f, "shop.cart.Cart").kind, SymbolKind::Class);
        assert_eq!(sym(&f, "shop.cart.Cart.add").kind, SymbolKind::Method);
        assert_eq!(sym(&f, "shop.cart.MAX_RETRIES").contract.display, "MAX_RETRIES: int");
        assert!(sym(&f, "shop.cart.Cart").contract.display.contains("owner: str"));
        assert!(
            sym(&f, "shop.cart.Cart")
                .contract
                .display
                .contains("__init__(self, owner)")
        );
        assert!(!f.has_errors);
    }

    #[test]
    fn formatting_and_comments_do_not_change_digests() {
        let a = index_file("m.py", "def f(a, b):\n    return a + b\n").unwrap();
        let b = index_file(
            "m.py",
            "def f(a,\n      b):  # comment\n    # explain\n    return a+b\n",
        )
        .unwrap();
        assert_eq!(a.symbols[0].contract.digest, b.symbols[0].contract.digest);
        assert_eq!(a.symbols[0].body, b.symbols[0].body);
    }

    #[test]
    fn signature_vs_body_changes() {
        let base = index_file("m.py", "def f(a):\n    return a\n").unwrap();
        let body = index_file("m.py", "def f(a):\n    return a * 2\n").unwrap();
        let sig = index_file("m.py", "def f(a, *, b):\n    return a\n").unwrap();
        assert_eq!(base.symbols[0].contract.digest, body.symbols[0].contract.digest);
        assert_ne!(base.symbols[0].body, body.symbols[0].body);
        assert_ne!(base.symbols[0].contract.digest, sig.symbols[0].contract.digest);
    }

    #[test]
    fn method_body_change_does_not_change_class_digests() {
        let a = index_file("m.py", "class C:\n    def m(self):\n        return 1\n").unwrap();
        let b = index_file("m.py", "class C:\n    def m(self):\n        return 2\n").unwrap();
        assert_eq!(sym(&a, "m.C").contract.digest, sym(&b, "m.C").contract.digest);
        assert_eq!(sym(&a, "m.C").body, sym(&b, "m.C").body);
        assert_ne!(sym(&a, "m.C.m").body, sym(&b, "m.C.m").body);
    }

    #[test]
    fn init_signature_is_part_of_class_contract() {
        let a = index_file("m.py", "class C:\n    def __init__(self, x):\n        pass\n").unwrap();
        let b = index_file("m.py", "class C:\n    def __init__(self, x, y):\n        pass\n").unwrap();
        assert_ne!(sym(&a, "m.C").contract.digest, sym(&b, "m.C").contract.digest);
    }

    #[test]
    fn extracts_structured_signatures() {
        let f = index_file(
            "m.py",
            "def f(a, /, b: int, c=1, *args, d, e: str = 'x', **kw) -> bool:\n    pass\n",
        )
        .unwrap();
        let sig = f.symbols[0].contract.signature.as_ref().unwrap();
        let kinds: Vec<(&str, ParamKind, bool)> = sig
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.kind, p.has_default()))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("a", ParamKind::PositionalOnly, false),
                ("b", ParamKind::PositionalOrKeyword, false),
                ("c", ParamKind::PositionalOrKeyword, true),
                ("args", ParamKind::VarPositional, false),
                ("d", ParamKind::KeywordOnly, false),
                ("e", ParamKind::KeywordOnly, true),
                ("kw", ParamKind::VarKeyword, false),
            ]
        );
        assert_eq!(sig.returns.as_deref(), Some("bool"));
    }

    #[test]
    fn breaking_vs_compatible_changes_end_to_end() {
        use concord_core::diff_symbols;
        let base = index_file(
            "m.py",
            "def send(to, body):\n    pass\n\nclass E:\n    def __init__(self, a):\n        pass\n",
        )
        .unwrap();
        let compatible = index_file(
            "m.py",
            "def send(to, body, *, retries=3):\n    pass\n\nclass E:\n    def __init__(self, a, b=None):\n        pass\n",
        )
        .unwrap();
        let breaking = index_file(
            "m.py",
            "def send(to, body, *, user_id):\n    pass\n\nclass E:\n    def __init__(self, b):\n        pass\n",
        )
        .unwrap();
        let c = diff_symbols(&base.symbols, &compatible.symbols);
        assert!(
            c.iter().filter(|c| c.is_contract_change()).all(|c| !c.is_breaking()),
            "{c:#?}"
        );
        let b = diff_symbols(&base.symbols, &breaking.symbols);
        assert!(b.iter().any(|c| c.symbol.as_str() == "m.send" && c.is_breaking()));
        assert!(b.iter().any(|c| c.symbol.as_str() == "m.E" && c.is_breaking()));
    }

    #[test]
    fn finds_calls_inside_f_strings() {
        let f = index_file("m.py", "def r(o):\n    return f\"Total: {price_order(o)} EUR\"\n").unwrap();
        let call = f
            .refs
            .iter()
            .find(|r| r.chain == ["price_order"])
            .expect("call inside f-string");
        assert!(call.is_call);
        assert_eq!(call.args.as_ref().unwrap().positional, 1);
    }

    #[test]
    fn collects_imports_and_references() {
        let src = "from notify.email import send_email as se\nimport auth.tokens\nfrom . import util\n\ndef go(u):\n    se(u.email, msg(1))\n    auth.tokens.issue(u)\n    return util.x\n";
        let f = index_file("auth/reset.py", src).unwrap();
        let bind = |l: &str| f.imports.iter().find(|b| b.local == l).map(|b| b.target.clone());
        assert_eq!(bind("se").as_deref(), Some("notify.email.send_email"));
        assert_eq!(bind("auth").as_deref(), Some("auth"));
        assert_eq!(bind("util").as_deref(), Some("auth.util"));
        let chains: Vec<String> = f
            .refs
            .iter()
            .filter(|r| r.from.as_str() == "auth.reset.go")
            .map(|r| r.chain.join("."))
            .collect();
        assert!(chains.contains(&"se".to_string()));
        assert!(chains.contains(&"auth.tokens.issue".to_string()));
        assert!(chains.contains(&"msg".to_string()));
        assert!(chains.contains(&"util.x".to_string()));
    }
}
