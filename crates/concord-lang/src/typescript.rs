//! TypeScript / JavaScript front-end built on tree-sitter-typescript.
//!
//! Extracts, per file, the same facts as the Python front-end:
//! - symbols (functions, functions bound to `const`, classes, methods, class
//!   fields, module variables, interfaces, type aliases, enums) with a
//!   normalised *contract* and a *body* digest;
//! - import bindings (`import`, `require`) and re-exports (`export ... from`,
//!   `export { a as b }`, `export default x`);
//! - raw references (calls, `new`, member chains, type names, JSX components)
//!   to be resolved by [`crate::RepoIndex`].
//!
//! Mapping onto the language-independent model:
//! - TypeScript has no keyword arguments, so every named parameter is
//!   [`ParamKind::PositionalOnly`]; `x?: T` and `x = v` have a default; `...rest`
//!   is [`ParamKind::VarPositional`]. Type annotations are part of the contract.
//! - Methods get a synthetic leading `this` parameter (the analogue of Python's
//!   `self`), so the existing "bound method" rule of call checking applies; it
//!   is never shown in displays. Static methods are treated the same way
//!   (called through the class, the receiver is implicit as well).
//! - A class's call contract is its constructor (`new C(...)` is a call of `C`).
//! - Interfaces and enums are recorded as [`SymbolKind::Class`], type aliases as
//!   [`SymbolKind::Variable`]; their whole declaration is the contract.
//! - Export status (`export`, `export default`) is part of the contract text.
//!
//! Module names: the path without extension, `/` replaced by `.`, `index`
//! files named after their directory (`src/ui/index.ts` -> `src.ui`). Relative
//! specifiers are resolved against the importing file; bare specifiers
//! (`react`, `@scope/pkg`, path aliases) are external and never resolve into
//! the repository.
//!
//! Canonicalisation: as for Python, digests are computed over the token
//! stream (comments removed). In addition, semicolons, trailing commas, the
//! quote style of string literals and parentheses around a single arrow
//! parameter are normalised, so the usual formatter options never produce a
//! change.

use crate::python::{collapse_ws, span_of, tidy_params, truncate};
use crate::{FileIndex, ImportBinding, LangError, RawRef};
use concord_core::{CallArgs, Contract, Digest, Param, ParamKind, Signature, Span, Symbol, SymbolId, SymbolKind};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::{Node, Parser};

/// Prefix of import targets that lie outside the repository (packages).
pub const EXTERNAL_PREFIX: &str = "extern:";

const EXTENSIONS: [&str; 8] = [".tsx", ".ts", ".mts", ".cts", ".jsx", ".js", ".mjs", ".cjs"];

/// TypeScript or JavaScript source (declaration files `.d.ts` and minified
/// bundles excluded).
pub fn is_typescript_path(path: &str) -> bool {
    EXTENSIONS.iter().any(|e| path.ends_with(e))
        && !is_declaration_file(path)
        && !path.ends_with(".min.js")
        && !path.split('/').any(|seg| seg == "node_modules")
}

fn is_declaration_file(path: &str) -> bool {
    [".d.ts", ".d.mts", ".d.cts"].iter().any(|e| path.ends_with(e))
}

/// Dotted module name for a repository-relative path (`src/ui/button.tsx` ->
/// `src.ui.button`, `src/ui/index.ts` -> `src.ui`).
pub fn module_name(path: &str) -> String {
    let p = path.trim_start_matches("./");
    let p = strip_extension(p);
    let mut parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if parts.last() == Some(&"index") {
        parts.pop();
    }
    parts.join(".")
}

fn strip_extension(p: &str) -> &str {
    EXTENSIONS.iter().find_map(|e| p.strip_suffix(e)).unwrap_or(p)
}

/// Module targeted by an import specifier written in `importer` (a
/// repository-relative path): relative specifiers are resolved against the
/// importing file, `@/...` and `~/...` tsconfig root aliases resolve into the
/// repository, and package specifiers are external (`extern:react`).
pub fn resolve_specifier(importer: &str, spec: &str) -> String {
    if let Some(rest) = spec.strip_prefix("@/").or_else(|| spec.strip_prefix("~/")) {
        return module_name(rest);
    }
    if !(spec == "." || spec == ".." || spec.starts_with("./") || spec.starts_with("../")) {
        return format!("{EXTERNAL_PREFIX}{spec}");
    }
    let importer = importer.trim_start_matches("./");
    let mut parts: Vec<&str> = importer.split('/').filter(|s| !s.is_empty()).collect();
    parts.pop(); // the importing file itself
    for seg in spec.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    // Escapes the repository.
                    return format!("{EXTERNAL_PREFIX}{spec}");
                }
            }
            s => parts.push(s),
        }
    }
    module_name(&parts.join("/"))
}

/// Globals of the JavaScript runtimes: references to them are never
/// repository symbols (unless the file imports or defines the name).
const GLOBALS: &[&str] = &[
    "console",
    "Math",
    "JSON",
    "Object",
    "Array",
    "Promise",
    "window",
    "document",
    "globalThis",
    "process",
    "require",
    "module",
    "exports",
    "undefined",
    "Number",
    "String",
    "Boolean",
    "Symbol",
    "BigInt",
    "Date",
    "Error",
    "TypeError",
    "RangeError",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "Reflect",
    "Proxy",
    "RegExp",
    "Intl",
    "setTimeout",
    "setInterval",
    "clearTimeout",
    "clearInterval",
    "fetch",
    "URL",
    "Buffer",
    "structuredClone",
    "queueMicrotask",
    "parseInt",
    "parseFloat",
    "isNaN",
    "NaN",
    "Infinity",
    "arguments",
    "__dirname",
    "__filename",
    "super",
];

/// Methods of the built-in types (arrays, strings, maps, promises, ...).
/// A call `x.map(...)` on an unknown receiver is almost always one of these,
/// so [`crate::RepoIndex::resolve`] does not guess a repository method of the
/// same name for it in TypeScript/JavaScript files.
pub fn is_builtin_method(name: &str) -> bool {
    const BUILTIN: &[&str] = &[
        "map",
        "filter",
        "forEach",
        "reduce",
        "reduceRight",
        "find",
        "findIndex",
        "findLast",
        "some",
        "every",
        "includes",
        "indexOf",
        "lastIndexOf",
        "push",
        "pop",
        "shift",
        "unshift",
        "slice",
        "splice",
        "concat",
        "join",
        "split",
        "sort",
        "reverse",
        "flat",
        "flatMap",
        "fill",
        "at",
        "keys",
        "values",
        "entries",
        "get",
        "set",
        "has",
        "delete",
        "add",
        "clear",
        "then",
        "catch",
        "finally",
        "toString",
        "valueOf",
        "toFixed",
        "trim",
        "trimStart",
        "trimEnd",
        "replace",
        "replaceAll",
        "startsWith",
        "endsWith",
        "toLowerCase",
        "toUpperCase",
        "padStart",
        "padEnd",
        "charAt",
        "substring",
        "match",
        "matchAll",
        "test",
        "exec",
        "apply",
        "call",
        "bind",
        "json",
        "text",
        "emit",
        "on",
        "off",
        "once",
    ];
    BUILTIN.contains(&name)
}

/// Parse and index one TypeScript or JavaScript file. `.tsx`, `.jsx` and the
/// JavaScript extensions use the TSX grammar (JavaScript may contain JSX and
/// never contains `<T>x` type assertions); `.ts`, `.mts`, `.cts` use the
/// TypeScript grammar.
pub fn index_file(path: &str, source: &str) -> Result<FileIndex, LangError> {
    let tsx = !(path.ends_with(".ts") || path.ends_with(".mts") || path.ends_with(".cts"));
    let language = if tsx {
        tree_sitter_typescript::LANGUAGE_TSX
    } else {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT
    };
    let mut parser = Parser::new();
    parser
        .set_language(&language.into())
        .map_err(|e| LangError::Grammar(e.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| LangError::Parse(path.to_owned()))?;
    let module = module_name(path);
    let mut x = Extractor {
        src: source.as_bytes(),
        path,
        builders: BTreeMap::new(),
        order: Vec::new(),
        imports: Vec::new(),
        exports: Vec::new(),
        local_exports: Vec::new(),
        refs: Vec::new(),
    };
    let module_id = SymbolId::new(module.clone());
    x.visit_block(
        tree.root_node(),
        &Scope {
            prefix: module_id.clone(),
        },
    );

    // `export { a as b }` / `export default a`: `a` is an import binding or
    // a symbol of this module.
    for (exported, local) in std::mem::take(&mut x.local_exports) {
        let target = match x.imports.iter().rev().find(|b| b.local == local) {
            Some(b) => b.target.clone(),
            None => module_id.child(&local).0,
        };
        if target != module_id.child(&exported).0 {
            x.exports.push(ImportBinding {
                local: exported,
                target,
                line: 0,
            });
        }
    }

    let symbols: Vec<Symbol> = x
        .order
        .iter()
        .map(|id| x.builders.remove(id).expect("builder").finish(path))
        .collect();

    // Drop references to runtime globals, unless the name is imported or
    // defined at module level here.
    let bound: BTreeSet<&str> = x
        .imports
        .iter()
        .map(|b| b.local.as_str())
        .chain(
            symbols
                .iter()
                .filter(|s| s.id.parent() == Some(module.as_str()) || (module.is_empty() && !s.id.0.contains('.')))
                .map(|s| s.id.name()),
        )
        .collect();
    let refs = x
        .refs
        .into_iter()
        .filter(|r| {
            let head = r.chain[0].as_str();
            !GLOBALS.contains(&head) || bound.contains(head)
        })
        .collect();

    Ok(FileIndex {
        path: path.to_owned(),
        module,
        symbols,
        imports: x.imports,
        exports: x.exports,
        refs,
        has_errors: tree.root_node().has_error(),
    })
}

struct Scope {
    prefix: SymbolId,
}

/// Accumulates one symbol; repeated definitions with the same qualified name
/// (overload signatures, get/set accessor pairs, declaration merging) are merged.
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

/// How a declaration is exported (part of its contract).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Export {
    No,
    Named,
    Default,
}

impl Export {
    fn canon(self) -> &'static str {
        match self {
            Export::No => "",
            Export::Named => "export ",
            Export::Default => "export default ",
        }
    }
}

struct Extractor<'a> {
    src: &'a [u8],
    path: &'a str,
    builders: BTreeMap<SymbolId, Builder>,
    order: Vec<SymbolId>,
    imports: Vec<ImportBinding>,
    exports: Vec<ImportBinding>,
    /// (exported name, local name) pairs resolved after the traversal.
    local_exports: Vec<(String, String)>,
    refs: Vec<RawRef>,
}

fn is_function_value(n: Node) -> bool {
    matches!(
        n.kind(),
        "arrow_function" | "function_expression" | "function" | "generator_function"
    )
}

impl<'a> Extractor<'a> {
    fn text(&self, n: Node) -> &'a str {
        n.utf8_text(self.src).unwrap_or("")
    }

    /// Collapsed source text from the start of `from` up to the start of
    /// `until` (or the end of `from`), without a trailing `=>`, `=`, `{` or `;`.
    fn header_display(&self, from: Node, until: Option<Node>) -> String {
        let end = until.map(|u| u.start_byte()).unwrap_or(from.end_byte());
        let raw = std::str::from_utf8(&self.src[from.start_byte()..end.max(from.start_byte())]).unwrap_or("");
        let mut s = collapse_ws(raw);
        loop {
            let t = s
                .trim_end()
                .trim_end_matches("=>")
                .trim_end_matches(['=', '{', ';'])
                .trim_end()
                .to_owned();
            if t == s {
                break;
            }
            s = t;
        }
        tidy_params(&s)
    }

    #[allow(clippy::too_many_arguments)] // private helper, mirrors the Python front-end
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
            self.visit_statement(stmt, stmt, Export::No, scope);
        }
    }

    /// `outer` is the enclosing `export` statement, if any (for spans/displays).
    fn visit_statement(&mut self, stmt: Node, outer: Node, export: Export, scope: &Scope) {
        match stmt.kind() {
            "comment" | "empty_statement" => {}
            "import_statement" => self.import(stmt, &scope.prefix),
            "export_statement" => self.export(stmt, scope),
            "function_declaration" | "generator_function_declaration" | "function_signature" => {
                let Some(name) = stmt.child_by_field_name("name") else {
                    return;
                };
                let id = scope.prefix.child(self.text(name));
                self.function(id, SymbolKind::Function, stmt, outer, export, None);
            }
            "class_declaration" | "abstract_class_declaration" => {
                let Some(name) = stmt.child_by_field_name("name") else {
                    return;
                };
                let id = scope.prefix.child(self.text(name));
                self.class(id, stmt, outer, export, None);
            }
            "lexical_declaration" | "variable_declaration" => self.variables(stmt, outer, export, scope),
            "interface_declaration" | "type_alias_declaration" | "enum_declaration" => {
                self.type_declaration(stmt, outer, export, scope)
            }
            "ambient_declaration" => {
                let mut c = stmt.walk();
                let children: Vec<Node> = stmt.named_children(&mut c).collect();
                for ch in children {
                    self.visit_statement(ch, outer, export, scope);
                }
            }
            "internal_module" => self.namespace(stmt, scope),
            // `declare module "x" { ... }`, `declare global { ... }`: ambient, not code.
            "module" | "global" => {}
            "expression_statement" => {
                let mut c = stmt.walk();
                let first = stmt.named_children(&mut c).next();
                match first {
                    Some(m) if m.kind() == "internal_module" => self.namespace(m, scope),
                    _ => self.collect_refs(stmt, &scope.prefix),
                }
            }
            _ => self.collect_refs(stmt, &scope.prefix),
        }
    }

    fn namespace(&mut self, ns: Node, scope: &Scope) {
        let (Some(name), Some(body)) = (ns.child_by_field_name("name"), ns.child_by_field_name("body")) else {
            return;
        };
        let mut prefix = scope.prefix.clone();
        for part in self.text(name).split('.') {
            prefix = prefix.child(part.trim());
        }
        self.visit_block(body, &Scope { prefix });
    }

    fn export(&mut self, stmt: Node, scope: &Scope) {
        let line = stmt.start_position().row as u32 + 1;
        let is_default = {
            let mut c = stmt.walk();
            stmt.children(&mut c).any(|n| n.kind() == "default")
        };
        let export = if is_default { Export::Default } else { Export::Named };
        if let Some(decl) = stmt.child_by_field_name("declaration") {
            self.visit_statement(decl, stmt, export, scope);
            if is_default && let Some(name) = decl.child_by_field_name("name") {
                self.local_exports.push(("default".into(), self.text(name).to_owned()));
            }
            return;
        }
        let source = stmt
            .child_by_field_name("source")
            .map(|s| resolve_specifier(self.path, &self.string_value(s)));
        if let Some(value) = stmt.child_by_field_name("value") {
            self.default_value(value, stmt, scope);
            return;
        }
        let mut c = stmt.walk();
        let children: Vec<Node> = stmt.named_children(&mut c).collect();
        for &ch in &children {
            match ch.kind() {
                "export_clause" => {
                    let mut c2 = ch.walk();
                    let specs: Vec<Node> = ch
                        .named_children(&mut c2)
                        .filter(|n| n.kind() == "export_specifier")
                        .collect();
                    for s in specs {
                        let Some(name) = s.child_by_field_name("name") else {
                            continue;
                        };
                        let name = self.string_value(name);
                        let exported = s
                            .child_by_field_name("alias")
                            .map(|a| self.string_value(a))
                            .unwrap_or_else(|| name.clone());
                        match &source {
                            Some(m) => self.exports.push(ImportBinding {
                                local: exported,
                                target: join(m, &name),
                                line,
                            }),
                            None => self.local_exports.push((exported, name)),
                        }
                    }
                }
                "namespace_export" => {
                    let alias = ch.named_child(0).map(|a| self.string_value(a));
                    if let (Some(a), Some(m)) = (alias, &source) {
                        self.exports.push(ImportBinding {
                            local: a,
                            target: m.clone(),
                            line,
                        });
                    }
                }
                // `export = x` (TypeScript's CommonJS export).
                "identifier" if source.is_none() => {
                    self.local_exports.push(("default".into(), self.text(ch).to_owned()))
                }
                _ => {}
            }
        }
        if source.is_some()
            && !children
                .iter()
                .any(|n| matches!(n.kind(), "export_clause" | "namespace_export"))
        {
            // `export * from './m'`
            self.exports.push(ImportBinding {
                local: "*".into(),
                target: source.unwrap_or_default(),
                line,
            });
        }
    }

    /// `export default <expression>`.
    fn default_value(&mut self, value: Node, stmt: Node, scope: &Scope) {
        let id = scope.prefix.child("default");
        match value.kind() {
            "identifier" => self.local_exports.push(("default".into(), self.text(value).to_owned())),
            _ if is_function_value(value) => {
                self.function(id, SymbolKind::Function, value, stmt, Export::Default, None)
            }
            "class" => self.class(id, value, stmt, Export::Default, None),
            _ => {
                let canon = "export default".to_owned();
                let body = self.canon(value);
                self.add_symbol(
                    id.clone(),
                    SymbolKind::Variable,
                    stmt,
                    "export default".into(),
                    canon,
                    body,
                    None,
                );
                self.collect_refs(value, &id);
            }
        }
    }

    /// A function-like symbol. `func` is a function declaration/signature,
    /// function expression, arrow function or method; `outer` spans the whole
    /// definition (for the display and span); `binding` is the canonical text
    /// of a `const name: T =` binding the function is assigned to.
    fn function(
        &mut self,
        id: SymbolId,
        kind: SymbolKind,
        func: Node,
        outer: Node,
        export: Export,
        binding: Option<String>,
    ) {
        let body = func.child_by_field_name("body");
        let params = func.child_by_field_name("parameters");
        let single_param = func.child_by_field_name("parameter");
        let ret = func.child_by_field_name("return_type");

        let mut canon = export.canon().to_owned();
        if let Some(b) = &binding {
            canon.push_str(b);
            canon.push(' ');
        }
        canon.push_str(&self.canon_without(func, body));
        let display = self.header_display(outer, body);

        let is_async = {
            let mut c = func.walk();
            func.children(&mut c).any(|n| n.kind() == "async")
        };
        let mut sig_params = Vec::new();
        if kind == SymbolKind::Method {
            sig_params.push(Param {
                name: "this".into(),
                kind: ParamKind::PositionalOnly,
                annotation: None,
                default: None,
            });
        }
        if let Some(p) = params {
            sig_params.extend(self.params_of(p));
        } else if let Some(p) = single_param {
            sig_params.push(Param {
                name: self.text(p).to_owned(),
                kind: ParamKind::PositionalOnly,
                annotation: None,
                default: None,
            });
        }
        let decorators: Vec<String> = {
            let mut c = func.walk();
            func.children(&mut c)
                .filter(|n| n.kind() == "decorator")
                .map(|d| self.canon(d))
                .collect()
        };
        let signature = Signature {
            params: sig_params,
            returns: ret.map(|r| self.type_canon(r)),
            decorators,
            is_async,
            bases: Vec::new(),
        };
        let body_canon = body.map(|b| self.canon(b)).unwrap_or_default();
        self.add_symbol(id.clone(), kind, outer, display, canon, body_canon, Some(signature));

        // References: everything but the name (decorators, parameter types and
        // defaults, return type, body).
        let name = func.child_by_field_name("name").or(single_param);
        self.collect_children_except(func, name, &id);
    }

    fn class(&mut self, id: SymbolId, def: Node, outer: Node, export: Export, binding: Option<String>) {
        let body = def.child_by_field_name("body");

        // Members first, so the class contract can include the constructor.
        let mut body_canon = String::new();
        let class_scope = Scope { prefix: id.clone() };
        if let Some(b) = body {
            let mut c = b.walk();
            let members: Vec<Node> = b.named_children(&mut c).collect();
            for m in members {
                self.member(m, &class_scope, &mut body_canon);
            }
        }

        let mut canon = export.canon().to_owned();
        if let Some(b) = &binding {
            canon.push_str(b);
            canon.push(' ');
        }
        canon.push_str(&self.canon_without(def, body));
        let mut display = self.header_display(outer, body);

        let ctor_id = id.child("constructor");
        let ctor = self.builders.get(&ctor_id).map(|b| {
            (
                b.contract_canon.clone(),
                b.display.clone(),
                if b.defs == 1 { b.signature.clone() } else { None },
                b.defs,
            )
        });
        if let Some((ctor_canon, ctor_display, _, _)) = &ctor {
            canon.push_str(" | init ");
            canon.push_str(ctor_canon);
            display.push_str(": ");
            display.push_str(ctor_display);
        }

        let mut bases = Vec::new();
        let mut extends = false;
        let mut c = def.walk();
        for ch in def.named_children(&mut c) {
            if ch.kind() != "class_heritage" {
                continue;
            }
            let mut c2 = ch.walk();
            for clause in ch.named_children(&mut c2) {
                extends |= clause.kind() == "extends_clause";
                let mut c3 = clause.walk();
                for t in clause.named_children(&mut c3) {
                    if t.kind() != "comment" {
                        bases.push(self.canon(t));
                    }
                }
            }
        }
        let decorators: Vec<String> = {
            let mut c = def.walk();
            def.children(&mut c)
                .filter(|n| n.kind() == "decorator")
                .map(|d| self.canon(d))
                .collect()
        };
        // Constructor signature without the synthetic `this`; with no
        // constructor, empty unless one may be inherited.
        let ctor_params = match ctor {
            Some((_, _, Some(sig), _)) => Some(sig.params.into_iter().skip(1).collect()),
            Some(_) => None,
            None if !extends => Some(Vec::new()),
            None => None,
        };
        let signature = ctor_params.map(|params| Signature {
            params,
            returns: None,
            decorators,
            is_async: false,
            bases,
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

        let name = def.child_by_field_name("name");
        let mut c = def.walk();
        let children: Vec<Node> = def.named_children(&mut c).collect();
        for ch in children {
            if Some(ch) != name && Some(ch) != body {
                self.collect_refs(ch, &id);
            }
        }
    }

    /// One member of a class body.
    fn member(&mut self, m: Node, scope: &Scope, class_body: &mut String) {
        let name = m.child_by_field_name("name").map(|n| self.text(n).to_owned());
        match (m.kind(), name) {
            ("method_definition" | "method_signature" | "abstract_method_signature", Some(name)) => {
                self.function(scope.prefix.child(&name), SymbolKind::Method, m, m, Export::No, None);
            }
            ("public_field_definition" | "property_signature", Some(name)) => {
                let id = scope.prefix.child(&name);
                let value = m.child_by_field_name("value");
                match value {
                    Some(v) if is_function_value(v) => {
                        let binding = self.canon_without(m, Some(v));
                        self.function(id, SymbolKind::Method, v, m, Export::No, Some(binding));
                    }
                    _ => {
                        let canon = self.canon_without(m, value);
                        let display = self.header_display(m, value);
                        let body = value.map(|v| self.canon(v)).unwrap_or_default();
                        self.add_symbol(id.clone(), SymbolKind::Variable, m, display, canon, body, None);
                        let name_node = m.child_by_field_name("name");
                        self.collect_children_except(m, name_node, &id);
                    }
                }
            }
            ("comment", _) => {}
            _ => {
                class_body.push_str(&self.canon(m));
                class_body.push(' ');
                self.collect_refs(m, &scope.prefix);
            }
        }
    }

    fn variables(&mut self, decl: Node, outer: Node, export: Export, scope: &Scope) {
        let kind_kw = decl
            .child_by_field_name("kind")
            .map(|k| self.text(k).to_owned())
            .unwrap_or_else(|| "var".into());
        let mut c = decl.walk();
        let declarators: Vec<Node> = decl
            .named_children(&mut c)
            .filter(|n| n.kind() == "variable_declarator")
            .collect();
        let single = declarators.len() == 1;
        for d in declarators {
            let Some(name) = d.child_by_field_name("name") else {
                continue;
            };
            let value = d.child_by_field_name("value");
            if let Some(v) = value
                && let Some(spec) = self.require_specifier(v)
            {
                self.require_binding(name, &spec, d, &scope.prefix);
                continue;
            }
            if name.kind() != "identifier" {
                self.collect_refs(d, &scope.prefix);
                continue;
            }
            let id = scope.prefix.child(self.text(name));
            let ty = d.child_by_field_name("type");
            let mut binding = format!("{kind_kw} {}", self.text(name));
            if let Some(t) = ty {
                binding.push_str(" : ");
                binding.push_str(&self.type_canon(t));
            }
            let span_node = if single { outer } else { d };
            match value {
                Some(v) if is_function_value(v) => {
                    binding.push_str(" =");
                    self.function(id, SymbolKind::Function, v, span_node, export, Some(binding));
                    if let Some(t) = ty {
                        self.collect_refs(t, &scope.prefix.child(self.text(name)));
                    }
                }
                Some(v) if v.kind() == "class" => {
                    binding.push_str(" =");
                    self.class(id, v, span_node, export, Some(binding));
                }
                _ => {
                    let canon = format!("{}{binding}", export.canon());
                    let display = if single {
                        self.header_display(outer, value)
                    } else {
                        format!("{kind_kw} {}", self.header_display(d, value))
                    };
                    let body = value.map(|v| self.canon(v)).unwrap_or_default();
                    self.add_symbol(id.clone(), SymbolKind::Variable, span_node, display, canon, body, None);
                    self.collect_children_except(d, Some(name), &id);
                }
            }
        }
    }

    /// Interfaces, type aliases and enums: the whole declaration is the contract.
    fn type_declaration(&mut self, decl: Node, outer: Node, export: Export, scope: &Scope) {
        let Some(name) = decl.child_by_field_name("name") else {
            return;
        };
        let id = scope.prefix.child(self.text(name));
        let kind = if decl.kind() == "type_alias_declaration" {
            SymbolKind::Variable
        } else {
            SymbolKind::Class
        };
        let canon = format!("{}{}", export.canon(), self.canon(decl));
        let display = truncate(&tidy_params(&collapse_ws(self.text(outer))), 200);
        self.add_symbol(id.clone(), kind, outer, display, canon, String::new(), None);
        self.collect_children_except(decl, Some(name), &id);
    }

    fn import(&mut self, stmt: Node, from: &SymbolId) {
        let line = stmt.start_position().row as u32 + 1;
        let text = truncate(&collapse_ws(self.text(stmt)), 100);
        let Some(source) = stmt.child_by_field_name("source") else {
            // `import x = require('./m')`
            let mut c = stmt.walk();
            let req = stmt
                .named_children(&mut c)
                .find(|n| n.kind() == "import_require_clause");
            if let Some(r) = req {
                let local = r.named_child(0).map(|n| self.text(n).to_owned());
                let src = r.child_by_field_name("source").map(|s| self.string_value(s));
                if let (Some(local), Some(src)) = (local, src) {
                    let target = resolve_specifier(self.path, &src);
                    self.imports.push(ImportBinding { local, target, line });
                }
            }
            return;
        };
        let module = resolve_specifier(self.path, &self.string_value(source));
        let mut c = stmt.walk();
        let clause = stmt.named_children(&mut c).find(|n| n.kind() == "import_clause");
        let Some(clause) = clause else { return };
        let mut c = clause.walk();
        let mut named: Vec<(String, String)> = Vec::new();
        for ch in clause.named_children(&mut c) {
            match ch.kind() {
                "identifier" => named.push((self.text(ch).to_owned(), "default".into())),
                "namespace_import" => {
                    if let Some(local) = ch.named_child(0) {
                        self.imports.push(ImportBinding {
                            local: self.text(local).to_owned(),
                            target: module.clone(),
                            line,
                        });
                    }
                }
                "named_imports" => {
                    let mut c2 = ch.walk();
                    for s in ch.named_children(&mut c2).filter(|n| n.kind() == "import_specifier") {
                        let Some(name) = s.child_by_field_name("name") else {
                            continue;
                        };
                        let name = self.string_value(name);
                        let local = s
                            .child_by_field_name("alias")
                            .map(|a| self.text(a).to_owned())
                            .unwrap_or_else(|| name.clone());
                        named.push((local, name));
                    }
                }
                _ => {}
            }
        }
        for (local, name) in named {
            self.imports.push(ImportBinding {
                local: local.clone(),
                target: join(&module, &name),
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

    /// `require('./m')` (a plain call with one string argument) -> the specifier.
    fn require_specifier(&self, v: Node) -> Option<String> {
        if v.kind() != "call_expression" {
            return None;
        }
        let f = v.child_by_field_name("function")?;
        if f.kind() != "identifier" || self.text(f) != "require" {
            return None;
        }
        let args = v.child_by_field_name("arguments")?;
        if args.named_child_count() != 1 {
            return None;
        }
        let s = args.named_child(0)?;
        (s.kind() == "string").then(|| self.string_value(s))
    }

    /// `const m = require(..)` / `const { a, b: c } = require(..)`.
    fn require_binding(&mut self, pattern: Node, spec: &str, decl: Node, from: &SymbolId) {
        let line = decl.start_position().row as u32 + 1;
        let module = resolve_specifier(self.path, spec);
        let text = truncate(&collapse_ws(self.text(decl)), 100);
        match pattern.kind() {
            "identifier" => self.imports.push(ImportBinding {
                local: self.text(pattern).to_owned(),
                target: module,
                line,
            }),
            "object_pattern" => {
                let mut c = pattern.walk();
                let props: Vec<Node> = pattern.named_children(&mut c).collect();
                for p in props {
                    let (name, local) = match p.kind() {
                        "shorthand_property_identifier_pattern" => (self.text(p).to_owned(), self.text(p).to_owned()),
                        "pair_pattern" => {
                            let k = p.child_by_field_name("key").map(|k| self.text(k).to_owned());
                            let v = p.child_by_field_name("value");
                            match (k, v) {
                                (Some(k), Some(v)) if v.kind() == "identifier" => (k, self.text(v).to_owned()),
                                _ => continue,
                            }
                        }
                        _ => continue,
                    };
                    self.imports.push(ImportBinding {
                        local: local.clone(),
                        target: join(&module, &name),
                        line,
                    });
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

    /// Contents of a string literal (or the text of an identifier).
    fn string_value(&self, n: Node) -> String {
        if !(n.kind() == "string" && n.is_named()) {
            return self.text(n).to_owned();
        }
        let t = self.text(n);
        t.get(1..t.len().saturating_sub(1)).unwrap_or("").to_owned()
    }

    /// Argument shape of a call's `arguments` node.
    fn call_args(&self, args: Node) -> CallArgs {
        let mut out = CallArgs::default();
        if args.kind() != "arguments" {
            // Tagged template: one argument.
            out.positional = 1;
            return out;
        }
        let mut c = args.walk();
        for a in args.named_children(&mut c) {
            match a.kind() {
                "spread_element" => out.star = true,
                "comment" => {}
                _ => out.positional += 1,
            }
        }
        out
    }

    /// Structured parameters of a `formal_parameters` node.
    fn params_of(&self, params: Node) -> Vec<Param> {
        let mut out = Vec::new();
        let mut c = params.walk();
        for ch in params.named_children(&mut c) {
            let optional = match ch.kind() {
                "required_parameter" => false,
                "optional_parameter" => true,
                _ => continue,
            };
            let Some(pattern) = ch.child_by_field_name("pattern") else {
                continue;
            };
            // `this: T` only types the receiver; it is not an argument.
            if pattern.kind() == "this" {
                continue;
            }
            let annotation = ch.child_by_field_name("type").map(|t| self.type_canon(t));
            let value = ch.child_by_field_name("value").map(|v| self.canon(v));
            let (name, kind) = if pattern.kind() == "rest_pattern" {
                let inner = pattern.named_child(0).map(|n| self.text(n)).unwrap_or("rest");
                (collapse_ws(inner), ParamKind::VarPositional)
            } else {
                (collapse_ws(self.text(pattern)), ParamKind::PositionalOnly)
            };
            let default = match (value, optional) {
                (Some(v), _) => Some(v),
                (None, true) => Some("undefined".to_owned()),
                (None, false) => None,
            };
            out.push(Param {
                name,
                kind,
                annotation,
                default,
            });
        }
        out
    }

    /// Canonical text of a type annotation without its leading `:`.
    fn type_canon(&self, t: Node) -> String {
        let c = self.canon(t);
        c.strip_prefix(": ")
            .or_else(|| c.strip_prefix(':'))
            .unwrap_or(&c)
            .to_owned()
    }

    /// Canonical token stream of a subtree.
    fn canon(&self, n: Node) -> String {
        let mut out = String::new();
        self.canon_into(n, &mut out);
        out
    }

    /// Canonical token stream of `n` without the child `skip` (its body).
    fn canon_without(&self, n: Node, skip: Option<Node>) -> String {
        let mut out = String::new();
        self.canon_children(n, skip, &mut out);
        out
    }

    fn canon_children(&self, n: Node, skip: Option<Node>, out: &mut String) {
        // `x => ...` and `(x) => ...` are the same.
        let arrow_param = if n.kind() == "arrow_function" {
            n.child_by_field_name("parameter")
        } else {
            None
        };
        let mut c = n.walk();
        let children: Vec<Node> = n.children(&mut c).collect();
        for (i, ch) in children.iter().enumerate() {
            if Some(*ch) == skip || self.skip_token(&children, i) {
                continue;
            }
            if Some(*ch) == arrow_param {
                push_token(out, "(");
                self.canon_into(*ch, out);
                push_token(out, ")");
            } else {
                self.canon_into(*ch, out);
            }
        }
    }

    /// Formatting-only tokens: semicolons, trailing commas.
    fn skip_token(&self, siblings: &[Node], i: usize) -> bool {
        let n = siblings[i];
        match n.kind() {
            ";" => true,
            "," => siblings[i + 1..]
                .iter()
                .find(|s| s.kind() != "comment")
                .is_none_or(|s| matches!(s.kind(), ")" | "]" | "}" | ">")),
            _ => n.child_count() == 0 && n.start_byte() == n.end_byte(),
        }
    }

    fn canon_into(&self, n: Node, out: &mut String) {
        match n.kind() {
            "comment" => {}
            // Quote style is formatting.
            "string" if n.is_named() => {
                let t = self.text(n);
                push_token(
                    out,
                    &format!("\"{}\"", t.get(1..t.len().saturating_sub(1)).unwrap_or("")),
                );
            }
            "template_string" | "regex" => push_token(out, self.text(n)),
            "jsx_text" => {
                let t = collapse_ws(self.text(n));
                if !t.is_empty() {
                    push_token(out, &t);
                }
            }
            _ if n.child_count() == 0 => {
                let t = self.text(n);
                if !t.is_empty() {
                    push_token(out, t);
                }
            }
            _ => self.canon_children(n, None, out),
        }
    }

    fn collect_children_except(&mut self, n: Node, except: Option<Node>, from: &SymbolId) {
        let mut c = n.walk();
        let children: Vec<Node> = n.named_children(&mut c).collect();
        for ch in children {
            if Some(ch) != except {
                self.collect_refs(ch, from);
            }
        }
    }

    fn push_ref(&mut self, n: Node, chain: Vec<String>, from: &SymbolId, args: Option<CallArgs>) {
        self.refs.push(RawRef {
            from: from.clone(),
            chain,
            line: n.start_position().row as u32 + 1,
            text: truncate(&collapse_ws(self.text(n)), 100),
            is_call: args.is_some(),
            args,
        });
    }

    /// Collect raw references (calls, `new`, names, member chains, type names,
    /// JSX components) inside `n`.
    fn collect_refs(&mut self, n: Node, from: &SymbolId) {
        match n.kind() {
            "comment"
            | "import_statement"
            | "string"
            | "regex"
            | "jsx_text"
            | "property_identifier"
            | "private_property_identifier"
            | "predefined_type"
            | "jsx_closing_element" => {}
            "template_string" => {
                let mut c = n.walk();
                let subs: Vec<Node> = n
                    .named_children(&mut c)
                    .filter(|ch| ch.kind() == "template_substitution")
                    .collect();
                for s in subs {
                    self.recurse(s, from);
                }
            }
            "call_expression" => {
                let func = n.child_by_field_name("function");
                let args = n.child_by_field_name("arguments");
                match func.and_then(|f| self.chain_of(f)) {
                    Some(chain) => {
                        let shape = args.map(|a| self.call_args(a)).unwrap_or_default();
                        self.push_ref(n, chain, from, Some(shape));
                    }
                    None => {
                        if let Some(f) = func {
                            self.collect_refs(f, from);
                        }
                    }
                }
                for field in ["type_arguments", "arguments"] {
                    if let Some(a) = n.child_by_field_name(field) {
                        self.collect_refs(a, from);
                    }
                }
            }
            "new_expression" => {
                let ctor = n.child_by_field_name("constructor");
                let args = n.child_by_field_name("arguments");
                match ctor.and_then(|f| self.chain_of(f)) {
                    Some(chain) => {
                        let shape = args.map(|a| self.call_args(a)).unwrap_or_default();
                        self.push_ref(n, chain, from, Some(shape));
                    }
                    None => {
                        if let Some(f) = ctor {
                            self.collect_refs(f, from);
                        }
                    }
                }
                for field in ["type_arguments", "arguments"] {
                    if let Some(a) = n.child_by_field_name(field) {
                        self.collect_refs(a, from);
                    }
                }
            }
            "identifier"
            | "member_expression"
            | "this"
            | "type_identifier"
            | "nested_type_identifier"
            | "nested_identifier"
            | "shorthand_property_identifier"
            | "non_null_expression" => match self.chain_of(n) {
                Some(chain) if !(chain.len() == 1 && chain[0] == "this") => self.push_ref(n, chain, from, None),
                Some(_) => {}
                None => self.recurse(n, from),
            },
            "jsx_opening_element" | "jsx_self_closing_element" => {
                if let Some(name) = n.child_by_field_name("name")
                    && let Some(chain) = self.chain_of(name)
                    && chain[0].starts_with(|c: char| c.is_ascii_uppercase())
                {
                    self.push_ref(name, chain, from, None);
                }
                let name = n.child_by_field_name("name");
                self.collect_children_except(n, name, from);
            }
            // Object keys are not references; computed keys and values are.
            "pair" => {
                if let Some(k) = n.child_by_field_name("key")
                    && k.kind() == "computed_property_name"
                {
                    self.collect_refs(k, from);
                }
                if let Some(v) = n.child_by_field_name("value") {
                    self.collect_refs(v, from);
                }
            }
            "required_parameter" | "optional_parameter" => {
                for f in ["type", "value"] {
                    if let Some(v) = n.child_by_field_name(f) {
                        self.collect_refs(v, from);
                    }
                }
                let mut c = n.walk();
                let decorators: Vec<Node> = n.named_children(&mut c).filter(|d| d.kind() == "decorator").collect();
                for d in decorators {
                    self.collect_refs(d, from);
                }
            }
            "variable_declarator" => {
                for f in ["type", "value"] {
                    if let Some(v) = n.child_by_field_name(f) {
                        self.collect_refs(v, from);
                    }
                }
            }
            "assignment_expression" | "augmented_assignment_expression" => {
                if let Some(l) = n.child_by_field_name("left")
                    && l.kind() != "identifier"
                {
                    self.collect_refs(l, from);
                }
                if let Some(r) = n.child_by_field_name("right") {
                    self.collect_refs(r, from);
                }
            }
            "for_in_statement" => {
                for f in ["right", "body"] {
                    if let Some(v) = n.child_by_field_name(f) {
                        self.collect_refs(v, from);
                    }
                }
            }
            "catch_clause" => {
                if let Some(b) = n.child_by_field_name("body") {
                    self.collect_refs(b, from);
                }
            }
            "arrow_function" => {
                let p = n.child_by_field_name("parameter");
                self.collect_children_except(n, p, from);
            }
            "function_declaration"
            | "generator_function_declaration"
            | "function_expression"
            | "function"
            | "generator_function"
            | "class_declaration"
            | "abstract_class_declaration"
            | "class"
            | "method_definition"
            | "public_field_definition"
            | "type_alias_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "labeled_statement"
            | "type_parameter" => {
                let name = n.child_by_field_name("name").or_else(|| n.child_by_field_name("label"));
                self.collect_children_except(n, name, from);
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

    /// `a.b.c` / `this.x` / `ns.T` -> ["a","b","c"]; anything that is not a
    /// plain name/member chain -> None.
    fn chain_of(&self, n: Node) -> Option<Vec<String>> {
        match n.kind() {
            "identifier" | "type_identifier" | "shorthand_property_identifier" | "property_identifier" | "this" => {
                Some(vec![self.text(n).to_owned()])
            }
            "member_expression" => {
                let obj = n.child_by_field_name("object")?;
                let prop = n.child_by_field_name("property")?;
                if !matches!(prop.kind(), "property_identifier" | "private_property_identifier") {
                    return None;
                }
                let mut chain = self.chain_of(obj)?;
                chain.push(self.text(prop).to_owned());
                Some(chain)
            }
            "non_null_expression" => self.chain_of(n.named_child(0)?),
            "nested_identifier" | "nested_type_identifier" => {
                let t = self.text(n);
                Some(t.split('.').map(|s| s.trim().to_owned()).collect())
            }
            _ => None,
        }
    }
}

fn join(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{module}.{name}")
    }
}

fn push_token(out: &mut String, tok: &str) {
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(tok);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RepoIndex;
    use concord_core::{check_call, diff_symbols};

    fn sym<'a>(f: &'a FileIndex, id: &str) -> &'a Symbol {
        f.symbols.iter().find(|s| s.id.as_str() == id).unwrap_or_else(|| {
            panic!(
                "symbol {id} not found in {:?}",
                f.symbols.iter().map(|s| &s.id).collect::<Vec<_>>()
            )
        })
    }

    fn ix(path: &str, src: &str) -> FileIndex {
        let f = index_file(path, src).unwrap();
        assert!(!f.has_errors, "{path}: syntax errors");
        f
    }

    #[test]
    fn module_names_and_specifiers() {
        assert_eq!(module_name("src/notify/email.ts"), "src.notify.email");
        assert_eq!(module_name("src/ui/index.tsx"), "src.ui");
        assert_eq!(module_name("index.js"), "");
        assert_eq!(
            resolve_specifier("src/auth/reset.ts", "../notify/email"),
            "src.notify.email"
        );
        assert_eq!(resolve_specifier("src/auth/reset.ts", "./tokens.js"), "src.auth.tokens");
        assert_eq!(resolve_specifier("src/auth/reset.ts", "."), "src.auth");
        assert_eq!(resolve_specifier("src/auth/reset.ts", "react"), "extern:react");
        assert_eq!(
            resolve_specifier("src/auth/reset.ts", "@scope/pkg"),
            "extern:@scope/pkg"
        );
        assert_eq!(resolve_specifier("app/api/route.ts", "@/lib/matching"), "lib.matching");
        assert_eq!(resolve_specifier("app/api/route.ts", "~/lib/types"), "lib.types");
        assert_eq!(resolve_specifier("a.ts", "../../x"), "extern:../../x");
        assert!(is_typescript_path("a/b.tsx"));
        assert!(is_typescript_path("a/b.cjs"));
        assert!(!is_typescript_path("a/b.d.ts"));
        assert!(!is_typescript_path("a/b.py"));
    }

    #[test]
    fn extracts_symbols() {
        let src = r#"
import { z } from './z';

export const MAX_RETRIES: number = 3;

/** Fetch. */
export async function fetchUrl(url: string, timeout?: number, ...rest: string[]): Promise<Buffer> {
    return get(url);
}

export const double = (x: number): number => x * 2;
const triple = function (x) { return x * 3; };

export default class Cart extends Base implements Sized {
    owner: string;
    private items: Item[] = [];
    static create(): Cart { return new Cart('a'); }
    constructor(owner: string, private readonly limit = 10) { super(); this.owner = owner; }
    add(item: Item, qty = 1): void { this.items.push(item); }
    onClick = (e: Event) => { this.add(e.item); };
}

export interface Sized { size(): number; label?: string }
export type Id = string | number;
enum Color { Red, Green }
"#;
        let f = ix("shop/cart.ts", src);
        assert_eq!(f.module, "shop.cart");
        let fetch = sym(&f, "shop.cart.fetchUrl");
        assert_eq!(fetch.kind, SymbolKind::Function);
        assert_eq!(
            fetch.contract.display,
            "export async function fetchUrl(url: string, timeout?: number, ...rest: string[]): Promise<Buffer>"
        );
        let sig = fetch.contract.signature.as_ref().unwrap();
        let shape: Vec<(&str, ParamKind, bool)> = sig
            .params
            .iter()
            .map(|p| (p.name.as_str(), p.kind, p.has_default()))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("url", ParamKind::PositionalOnly, false),
                ("timeout", ParamKind::PositionalOnly, true),
                ("rest", ParamKind::VarPositional, false),
            ]
        );
        assert_eq!(sig.params[0].annotation.as_deref(), Some("string"));
        assert_eq!(sig.returns.as_deref(), Some("Promise < Buffer >"));
        assert!(sig.is_async);

        let double = sym(&f, "shop.cart.double");
        assert_eq!(double.kind, SymbolKind::Function);
        assert_eq!(double.contract.display, "export const double = (x: number): number");
        assert_eq!(sym(&f, "shop.cart.triple").kind, SymbolKind::Function);
        assert_eq!(sym(&f, "shop.cart.MAX_RETRIES").kind, SymbolKind::Variable);
        assert_eq!(
            sym(&f, "shop.cart.MAX_RETRIES").contract.display,
            "export const MAX_RETRIES: number"
        );

        let cart = sym(&f, "shop.cart.Cart");
        assert_eq!(cart.kind, SymbolKind::Class);
        assert_eq!(
            cart.contract.display,
            "export default class Cart extends Base implements Sized: constructor(owner: string, private readonly limit = 10)"
        );
        let csig = cart.contract.signature.as_ref().unwrap();
        assert_eq!(csig.params.len(), 2);
        assert!(csig.params[1].has_default());
        assert_eq!(csig.bases, vec!["Base".to_string(), "Sized".to_string()]);
        assert_eq!(sym(&f, "shop.cart.Cart.add").kind, SymbolKind::Method);
        assert_eq!(sym(&f, "shop.cart.Cart.create").kind, SymbolKind::Method);
        assert_eq!(sym(&f, "shop.cart.Cart.onClick").kind, SymbolKind::Method);
        assert_eq!(sym(&f, "shop.cart.Cart.owner").kind, SymbolKind::Variable);
        assert_eq!(sym(&f, "shop.cart.Cart.items").kind, SymbolKind::Variable);
        // Methods carry a synthetic receiver, like Python's `self`.
        let add = sym(&f, "shop.cart.Cart.add").contract.signature.as_ref().unwrap();
        assert_eq!(add.params[0].name, "this");
        assert_eq!(add.params.len(), 3);
        assert_eq!(
            sym(&f, "shop.cart.Cart.add").contract.display,
            "add(item: Item, qty = 1): void"
        );

        assert_eq!(sym(&f, "shop.cart.Sized").kind, SymbolKind::Class);
        assert!(
            sym(&f, "shop.cart.Sized")
                .contract
                .display
                .starts_with("export interface Sized")
        );
        assert_eq!(sym(&f, "shop.cart.Id").kind, SymbolKind::Variable);
        assert_eq!(sym(&f, "shop.cart.Color").kind, SymbolKind::Class);
        // `export default class Cart` is reachable as the module's default.
        assert!(
            f.exports
                .iter()
                .any(|e| e.local == "default" && e.target == "shop.cart.Cart")
        );
    }

    #[test]
    fn formatting_and_comments_do_not_change_digests() {
        let a = ix(
            "m.ts",
            "export function f(a: number, b: string) {\n  return g(a, 'x');\n}\n",
        );
        let b = ix(
            "m.ts",
            "export function f(\n  a: number, // first\n  b: string,\n) {\n  // explain\n  return g(a, \"x\")\n}\n",
        );
        assert_eq!(a.symbols[0].contract.digest, b.symbols[0].contract.digest);
        assert_eq!(a.symbols[0].body, b.symbols[0].body);
        let c = ix("m.js", "export const h = x => x + 1;\n");
        let d = ix("m.js", "export const h = (x) => x + 1\n");
        assert_eq!(c.symbols[0].contract.digest, d.symbols[0].contract.digest);
        assert_eq!(c.symbols[0].body, d.symbols[0].body);
    }

    #[test]
    fn body_vs_contract_changes() {
        let base = ix("m.ts", "export function f(a: number): number {\n  return a;\n}\n");
        let body = ix("m.ts", "export function f(a: number): number {\n  return a * 2;\n}\n");
        let ty = ix(
            "m.ts",
            "export function f(a: string): number {\n  return a.length;\n}\n",
        );
        let unexported = ix("m.ts", "function f(a: number): number {\n  return a;\n}\n");
        let c = diff_symbols(&base.symbols, &body.symbols);
        assert_eq!(c.len(), 1);
        assert!(c[0].is_body_change(), "{c:#?}");
        let c = diff_symbols(&base.symbols, &ty.symbols);
        assert!(c[0].is_contract_change() && c[0].is_breaking(), "{c:#?}");
        let c = diff_symbols(&base.symbols, &unexported.symbols);
        assert!(c[0].is_contract_change(), "{c:#?}");

        // Method bodies do not touch the class; constructor parameters do.
        let a = ix(
            "m.ts",
            "class C {\n  constructor(x: number) {}\n  m() { return 1; }\n}\n",
        );
        let b = ix(
            "m.ts",
            "class C {\n  constructor(x: number) {}\n  m() { return 2; }\n}\n",
        );
        let d = ix(
            "m.ts",
            "class C {\n  constructor(x: number, y: number) {}\n  m() { return 1; }\n}\n",
        );
        assert_eq!(sym(&a, "m.C").contract.digest, sym(&b, "m.C").contract.digest);
        assert_eq!(sym(&a, "m.C").body, sym(&b, "m.C").body);
        assert_ne!(sym(&a, "m.C.m").body, sym(&b, "m.C.m").body);
        assert_ne!(sym(&a, "m.C").contract.digest, sym(&d, "m.C").contract.digest);
    }

    #[test]
    fn breaking_vs_compatible_changes() {
        let base = ix(
            "m.ts",
            "export function send(to: string, body: string) {}\nexport class E { constructor(a: number) {} }\nexport const k = (x: number) => x;\n",
        );
        let compatible = ix(
            "m.ts",
            "export function send(to: string, body: string, retries = 3) {}\nexport class E { constructor(a: number, b?: string) {} }\nexport const k = (x: number, ...more: number[]) => x;\n",
        );
        let breaking = ix(
            "m.ts",
            "export function send(to: string, body: string, userId: number) {}\nexport class E { constructor(b: number) {} }\nexport const k = (x: string) => x;\n",
        );
        let c = diff_symbols(&base.symbols, &compatible.symbols);
        // send, E, E.constructor, k
        assert_eq!(c.iter().filter(|c| c.is_contract_change()).count(), 4, "{c:#?}");
        assert!(
            c.iter().filter(|c| c.is_contract_change()).all(|c| !c.is_breaking()),
            "{c:#?}"
        );
        let b = diff_symbols(&base.symbols, &breaking.symbols);
        for s in ["m.send", "m.E", "m.k"] {
            assert!(
                b.iter().any(|c| c.symbol.as_str() == s && c.is_breaking()),
                "{s}: {b:#?}"
            );
        }
    }

    fn repo(files: &[(&str, &str)]) -> RepoIndex {
        RepoIndex::new(files.iter().map(|(p, s)| ix(p, s)))
    }

    #[test]
    fn resolves_imports_and_references() {
        let r = repo(&[
            (
                "src/notify/email.ts",
                "export function sendEmail(to: string, body: string) {}\nexport default function fmt(s: string) { return s; }\n",
            ),
            (
                "src/notify/index.ts",
                "export { sendEmail as send } from './email';\nexport * from './email';\n",
            ),
            (
                "src/shop/cart.ts",
                "export class Cart {\n  addItem(x: number) { this.check(x); }\n  check(x: number) {}\n}\n",
            ),
            (
                "src/auth/reset.tsx",
                r#"import fmt, { sendEmail as se } from '../notify/email';
import * as notify from '../notify';
import { send } from '../notify';
import { Cart } from '../shop/cart';
import React from 'react';
const legacy = require('../notify/email');

export function requestReset(user: User) {
  se(user.email, fmt(`x ${user.name}`));
  notify.sendEmail(user.email, 'b');
  send(user.email, 'c');
  legacy.sendEmail(user.email);
  const c = new Cart();
  c.addItem(1);
  React.createElement('div');
  console.log(user);
  return <Cart />;
}
"#,
            ),
        ]);
        let from: BTreeSet<SymbolId> = [
            "src.auth.reset.requestReset".into(),
            "src.shop.cart.Cart.addItem".into(),
        ]
        .into();
        let uses = r.uses_from(&from, &BTreeSet::new());
        let send = &uses[&SymbolId::new("src.notify.email.sendEmail")];
        let texts: Vec<&str> = send.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(send.len(), 4, "{texts:?}");
        assert!(texts.iter().any(|t| t.starts_with("se(")));
        assert!(texts.iter().any(|t| t.starts_with("notify.sendEmail(")));
        assert!(texts.iter().any(|t| t.starts_with("send(")));
        assert!(texts.iter().any(|t| t.starts_with("legacy.sendEmail(")));
        assert_eq!(
            send.iter()
                .find(|s| s.text.starts_with("se("))
                .unwrap()
                .call
                .as_ref()
                .unwrap()
                .positional,
            2
        );
        // Default import through `export default function fmt`.
        assert!(uses.contains_key(&SymbolId::new("src.notify.email.fmt")), "{uses:#?}");
        // `new Cart()` is a call of the class; JSX `<Cart />` a use.
        let cart = &uses[&SymbolId::new("src.shop.cart.Cart")];
        assert!(cart.iter().any(|s| s.text == "new Cart()" && s.call.is_some()));
        assert!(cart.iter().any(|s| s.text == "Cart" && s.call.is_none()));
        // `this.check(x)` resolves to the enclosing class's method;
        // `c.addItem(1)` through the unique method name.
        assert!(uses.contains_key(&SymbolId::new("src.shop.cart.Cart.check")));
        assert!(uses.contains_key(&SymbolId::new("src.shop.cart.Cart.addItem")));
        // External packages and globals never resolve into the repository.
        let f = r.file("src/auth/reset.tsx").unwrap();
        assert!(
            f.imports
                .iter()
                .any(|b| b.local == "React" && b.target == "extern:react.default")
        );
        assert!(!f.refs.iter().any(|r| r.chain[0] == "console"));
    }

    #[test]
    fn external_calls_and_builtin_methods_are_not_unique_name_matches() {
        let r = repo(&[
            (
                "p/store.ts",
                "export class Store {\n  get(k: string) { return k; }\n  render(x: number) {}\n}\n",
            ),
            (
                "p/use.ts",
                "import * as React from 'react';\nexport function f(m: Map<string, number>, s: any) {\n  m.get('a');\n  React.render(1);\n  s.render(2);\n}\n",
            ),
        ]);
        let uses = r.uses_from(&[SymbolId::new("p.use.f")].into(), &BTreeSet::new());
        assert!(!uses.contains_key(&SymbolId::new("p.store.Store.get")), "{uses:#?}");
        let render = &uses[&SymbolId::new("p.store.Store.render")];
        assert_eq!(render.len(), 1, "{render:?}");
        assert!(render[0].text.starts_with("s.render"));
    }

    #[test]
    fn call_checking_catches_a_missing_new_required_parameter() {
        let old = ix("m.ts", "export function send(to: string, body: string) {}\n");
        let new = ix(
            "m.ts",
            "export function send(to: string, body: string, userId: number) {}\n",
        );
        let user = ix(
            "u.ts",
            "import { send } from './m';\nexport function go() { send('a', 'b'); }\n",
        );
        let r = RepoIndex::new([old.clone(), user.clone()]);
        let uses = r.uses_from(&[SymbolId::new("u.go")].into(), &BTreeSet::new());
        let call = uses[&SymbolId::new("m.send")]
            .iter()
            .find_map(|s| s.call.clone())
            .unwrap();
        let old_sig = old.symbols[0].contract.signature.as_ref().unwrap();
        let new_sig = new.symbols[0].contract.signature.as_ref().unwrap();
        assert!(check_call(old_sig, &call, false).is_empty());
        assert_eq!(
            check_call(new_sig, &call, false),
            vec!["missing required argument `userId`".to_string()]
        );

        // Methods: the synthetic receiver is skipped like `self`.
        let m = ix(
            "c.ts",
            "export class C {\n  m(a: number, b: number) {}\n  static s(a: number) {}\n}\n",
        );
        let msig = sym(&m, "c.C.m").contract.signature.as_ref().unwrap();
        let ssig = sym(&m, "c.C.s").contract.signature.as_ref().unwrap();
        let two = CallArgs {
            positional: 2,
            ..Default::default()
        };
        let one = CallArgs {
            positional: 1,
            ..Default::default()
        };
        assert!(check_call(msig, &two, true).is_empty());
        assert!(!check_call(msig, &one, true).is_empty());
        assert!(check_call(ssig, &one, true).is_empty());
        // Spread arguments make missing arguments unknowable.
        let spread = CallArgs {
            positional: 0,
            star: true,
            ..Default::default()
        };
        assert!(check_call(new_sig, &spread, false).is_empty());
    }

    #[test]
    fn overloads_merge_into_one_symbol() {
        let f = ix(
            "m.ts",
            "export function f(a: string): void;\nexport function f(a: number): void;\nexport function f(a: any) {}\n",
        );
        assert_eq!(f.symbols.len(), 1);
        assert!(f.symbols[0].contract.signature.is_none());
        assert!(f.symbols[0].contract.display.contains("+2 more definitions"));
    }

    #[test]
    fn namespaces_and_reexports() {
        let f = ix(
            "lib/index.ts",
            "export * from './a';\nexport * as b from './b';\nexport { x as y } from '../c';\nimport { q } from './q';\nexport { q as default };\nnamespace N { export function nf() {} }\n",
        );
        assert_eq!(f.module, "lib");
        let ex: Vec<(&str, &str)> = f
            .exports
            .iter()
            .map(|e| (e.local.as_str(), e.target.as_str()))
            .collect();
        assert!(ex.contains(&("*", "lib.a")), "{ex:?}");
        assert!(ex.contains(&("b", "lib.b")), "{ex:?}");
        assert!(ex.contains(&("y", "c.x")), "{ex:?}");
        assert!(ex.contains(&("default", "lib.q.q")), "{ex:?}");
        sym(&f, "lib.N.nf");
    }
}
