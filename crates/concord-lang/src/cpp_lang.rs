//! C / C++ front-end built on `tree-sitter-cpp`.
//!
//! Extracts:
//! - functions & header prototypes (`.h`, `.hpp`, `.hh`, `.hxx`, `.c`, `.cc`, `.cpp`, `.cxx`)
//! - structs, classes, enums, and namespaces
//! - inline and out-of-line (`Class::method`) methods with structured `Signature`
//! - local `#include "..."` bindings
//! - call expressions (`fn(...)`, `ns::fn(...)`, `obj.m(...)`, `ptr->m(...)`)

use crate::python::{collapse_ws, span_of, tidy_params, truncate};
use crate::{FileIndex, ImportBinding, LangError, RawRef};
use concord_core::{CallArgs, Contract, Digest, Param, ParamKind, Signature, Span, Symbol, SymbolId, SymbolKind};
use std::collections::BTreeMap;
use tree_sitter::{Node, Parser};

const EXTENSIONS: [&str; 8] = [".cpp", ".cc", ".cxx", ".hpp", ".hh", ".hxx", ".h", ".c"];

pub fn is_cpp_path(path: &str) -> bool {
    EXTENSIONS.iter().any(|e| path.ends_with(e))
        && !path.split('/').any(|seg| {
            matches!(
                seg,
                "build" | "cmake-build-debug" | "cmake-build-release" | "third_party"
            )
        })
}

pub fn module_name(path: &str) -> String {
    let p = path.trim_start_matches("./");
    let stripped = EXTENSIONS.iter().find_map(|e| p.strip_suffix(e)).unwrap_or(p);
    stripped
        .split('/')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(".")
}

fn resolve_include(importer: &str, inc: &str) -> String {
    let inc = inc.trim_matches(|c| c == '"' || c == '<' || c == '>');
    let stripped = EXTENSIONS.iter().find_map(|e| inc.strip_suffix(e)).unwrap_or(inc);
    let mut parts: Vec<&str> = importer
        .trim_start_matches("./")
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    parts.pop();
    for seg in stripped.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        stripped.replace('/', ".")
    } else {
        parts.join(".")
    }
}

pub fn index_file(path: &str, source: &str) -> Result<FileIndex, LangError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_cpp::LANGUAGE.into())
        .map_err(|e| LangError::Grammar(e.to_string()))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| LangError::Parse(path.to_owned()))?;
    let root = tree.root_node();
    let module = module_name(path);

    let mut ex = Extractor {
        path,
        src: source,
        symbols: BTreeMap::new(),
        imports: Vec::new(),
        refs: Vec::new(),
    };
    ex.walk(root, &SymbolId::new(&module), false);

    let symbols: Vec<Symbol> = ex.symbols.into_values().map(|b| b.finish(path)).collect();

    Ok(FileIndex {
        path: path.to_owned(),
        module,
        symbols,
        imports: ex.imports,
        exports: Vec::new(),
        refs: ex.refs,
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

struct Extractor<'a> {
    path: &'a str,
    src: &'a str,
    symbols: BTreeMap<SymbolId, Builder>,
    imports: Vec<ImportBinding>,
    refs: Vec<RawRef>,
}

impl<'a> Extractor<'a> {
    fn text(&self, node: Node<'_>) -> &'a str {
        node.utf8_text(self.src.as_bytes()).unwrap_or("")
    }

    fn record(
        &mut self,
        id: SymbolId,
        kind: SymbolKind,
        span: Span,
        contract_text: &str,
        body_text: &str,
        sig: Option<Signature>,
    ) {
        let canon = collapse_ws(contract_text);
        let body = collapse_ws(body_text);
        match self.symbols.get_mut(&id) {
            Some(b) => {
                b.defs += 1;
                b.contract_canon.push('\n');
                b.contract_canon.push_str(&canon);
                b.body_canon.push('\n');
                b.body_canon.push_str(&body);
                if b.signature.is_none() {
                    b.signature = sig;
                }
            }
            None => {
                self.symbols.insert(
                    id.clone(),
                    Builder {
                        id,
                        kind,
                        span,
                        display: truncate(&canon, 120),
                        contract_canon: canon,
                        body_canon: body,
                        signature: sig,
                        defs: 1,
                    },
                );
            }
        }
    }

    fn walk(&mut self, node: Node<'_>, scope: &SymbolId, in_class: bool) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "preproc_include" => {
                    if let Some(p) = child.child_by_field_name("path") {
                        let raw = self.text(p);
                        if raw.starts_with('"') {
                            let target = resolve_include(self.path, raw);
                            let local = target.rsplit('.').next().unwrap_or(&target).to_owned();
                            let line = (child.start_position().row + 1) as u32;
                            self.imports.push(ImportBinding {
                                local,
                                target: target.clone(),
                                line,
                            });
                            self.imports.push(ImportBinding {
                                local: "*".into(),
                                target,
                                line,
                            });
                        }
                    }
                }
                "namespace_definition" => {
                    let ns_scope = child
                        .child_by_field_name("name")
                        .map(|n| scope.child(self.text(n).trim()))
                        .unwrap_or_else(|| scope.clone());
                    if let Some(body) = child.child_by_field_name("body") {
                        self.walk(body, &ns_scope, false);
                    }
                }
                "class_specifier" | "struct_specifier" => {
                    let kw = if child.kind() == "class_specifier" {
                        "class"
                    } else {
                        "struct"
                    };
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = self.text(name_node).trim();
                        if !name.is_empty() {
                            let id = scope.child(name);
                            let body_node = child.child_by_field_name("body");
                            let header = format!("{kw} {name}");
                            let body_text = body_node.map(|b| self.text(b)).unwrap_or("");
                            self.record(id.clone(), SymbolKind::Class, span_of(child), &header, body_text, None);
                            if let Some(b) = body_node {
                                self.walk(b, &id, true);
                            }
                        }
                    }
                }
                "enum_specifier" => {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        let name = self.text(name_node).trim();
                        if !name.is_empty() {
                            let id = scope.child(name);
                            let txt = self.text(child);
                            self.record(id, SymbolKind::Class, span_of(child), txt, txt, None);
                        }
                    }
                }
                "function_definition" => {
                    self.extract_function(child, scope, in_class, true);
                }
                "declaration" | "field_declaration" => {
                    // Header prototype or class method declaration: `int foo(int a, int b);`
                    if self.find_func_declarator(child).is_some() {
                        self.extract_function(child, scope, in_class, false);
                    }
                }
                "template_declaration" | "linkage_specification" | "field_declaration_list" | "declaration_list" => {
                    self.walk(child, scope, in_class);
                }
                _ => {}
            }
        }
    }

    fn find_func_declarator<'n>(&self, node: Node<'n>) -> Option<Node<'n>> {
        if node.kind() == "function_declarator" {
            return Some(node);
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            if matches!(
                ch.kind(),
                "function_declarator" | "pointer_declarator" | "reference_declarator" | "attributed_declarator"
            ) && let Some(found) = self.find_func_declarator(ch)
            {
                return Some(found);
            }
        }
        None
    }

    fn extract_function(&mut self, node: Node<'_>, scope: &SymbolId, in_class: bool, has_body: bool) {
        let Some(fdecl) = self.find_func_declarator(node) else {
            return;
        };
        let Some(name_node) = fdecl.child_by_field_name("declarator") else {
            return;
        };
        let raw_name = collapse_ws(self.text(name_node));
        if raw_name.is_empty() {
            return;
        }
        let is_method = in_class || raw_name.contains("::");
        let clean_name = raw_name.replace("::", ".");
        let id = scope.child(&clean_name);
        let kind = if is_method {
            SymbolKind::Method
        } else {
            SymbolKind::Function
        };

        let ret_type = node
            .child_by_field_name("type")
            .map(|t| collapse_ws(self.text(t)))
            .unwrap_or_default();
        let params_node = fdecl.child_by_field_name("parameters");
        let params_text = params_node
            .map(|p| tidy_params(self.text(p)))
            .unwrap_or_else(|| "()".into());
        let sig = params_node.map(|p| self.extract_signature(p, is_method, &ret_type));

        let contract = if ret_type.is_empty() {
            format!("{raw_name}{params_text}")
        } else {
            format!("{ret_type} {raw_name}{params_text}")
        };
        let body_node = has_body.then(|| node.child_by_field_name("body")).flatten();
        let body_text = body_node.map(|b| self.text(b)).unwrap_or("");
        self.record(id.clone(), kind, span_of(node), &contract, body_text, sig);

        if let Some(body) = body_node {
            self.collect_calls(body, &id);
        }
    }

    fn extract_signature(&self, params_node: Node<'_>, is_method: bool, ret_type: &str) -> Signature {
        let mut params = Vec::new();
        if is_method {
            params.push(Param {
                name: "this".into(),
                kind: ParamKind::PositionalOnly,
                annotation: None,
                default: None,
            });
        }
        let mut c = params_node.walk();
        for ch in params_node.children(&mut c) {
            match ch.kind() {
                "parameter_declaration" | "optional_parameter_declaration" => {
                    let txt = collapse_ws(self.text(ch));
                    if txt == "void" {
                        continue;
                    }
                    let default = ch
                        .child_by_field_name("default_value")
                        .map(|d| collapse_ws(self.text(d)))
                        .or_else(|| txt.split_once('=').map(|(_, rhs)| rhs.trim().to_owned()));
                    let ty = ch.child_by_field_name("type").map(|t| collapse_ws(self.text(t)));
                    let name = ch
                        .child_by_field_name("declarator")
                        .map(|d| collapse_ws(self.text(d)).trim_start_matches(['*', '&', ' ']).to_owned())
                        .unwrap_or_else(|| format!("arg{}", params.len()));
                    params.push(Param {
                        name,
                        kind: ParamKind::PositionalOnly,
                        annotation: ty,
                        default,
                    });
                }
                "variadic_parameter_declaration" | "variadic_parameter" => {
                    params.push(Param {
                        name: "...".into(),
                        kind: ParamKind::VarPositional,
                        annotation: None,
                        default: None,
                    });
                }
                _ => {}
            }
        }
        Signature {
            params,
            returns: (!ret_type.is_empty()).then(|| ret_type.to_owned()),
            is_async: false,
            decorators: Vec::new(),
            bases: Vec::new(),
        }
    }

    fn collect_calls(&mut self, node: Node<'_>, from: &SymbolId) {
        if node.kind() == "call_expression"
            && let Some(func) = node.child_by_field_name("function")
        {
            let chain = self.extract_chain(func);
            if !chain.is_empty() {
                let args = node.child_by_field_name("arguments").map(|a| self.extract_call_args(a));
                self.refs.push(RawRef {
                    from: from.clone(),
                    chain,
                    line: (node.start_position().row + 1) as u32,
                    text: truncate(&collapse_ws(self.text(node)), 120),
                    is_call: true,
                    args,
                });
            }
        }
        let mut c = node.walk();
        for ch in node.children(&mut c) {
            self.collect_calls(ch, from);
        }
    }

    fn extract_chain(&self, node: Node<'_>) -> Vec<String> {
        match node.kind() {
            "identifier" | "field_identifier" | "type_identifier" | "namespace_identifier" => {
                vec![self.text(node).trim().to_owned()]
            }
            "qualified_identifier" => {
                let mut parts = Vec::new();
                if let Some(s) = node.child_by_field_name("scope") {
                    parts.extend(self.extract_chain(s));
                }
                if let Some(n) = node.child_by_field_name("name") {
                    parts.extend(self.extract_chain(n));
                }
                parts
            }
            "field_expression" => {
                let mut parts = Vec::new();
                if let Some(a) = node.child_by_field_name("argument") {
                    parts.extend(self.extract_chain(a));
                }
                if let Some(f) = node.child_by_field_name("field") {
                    parts.extend(self.extract_chain(f));
                }
                parts
            }
            _ => Vec::new(),
        }
    }

    fn extract_call_args(&self, args_node: Node<'_>) -> CallArgs {
        let mut positional = 0u32;
        let mut c = args_node.walk();
        for ch in args_node.children(&mut c) {
            if ch.is_named() && ch.kind() != "comment" {
                positional += 1;
            }
        }
        CallArgs {
            positional,
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concord_core::{check_call, classify};

    #[test]
    fn indexes_cpp_headers_classes_methods_and_breaking_calls() {
        let hpp_v1 = r#"
#pragma once
namespace billing {
class Invoice {
public:
    double compute_total(double subtotal, double tax_rate = 0.19);
};
int send_email(const char* to, const char* body);
}
"#;
        let hpp_v2 = r#"
#pragma once
namespace billing {
class Invoice {
public:
    double compute_total(double subtotal, double tax_rate = 0.19);
};
int send_email(const char* to, const char* body, int user_id);
}
"#;
        let cpp = r#"
#include "billing.hpp"
void reset_pw() {
    billing::send_email("a@b.ro", "hi");
}
"#;
        let f1 = index_file("src/billing.hpp", hpp_v1).unwrap();
        let f2 = index_file("src/billing.hpp", hpp_v2).unwrap();
        let fc = index_file("src/auth.cpp", cpp).unwrap();

        let s1 = f1
            .symbols
            .iter()
            .find(|s| s.id.0.ends_with("billing.send_email"))
            .unwrap();
        let s2 = f2
            .symbols
            .iter()
            .find(|s| s.id.0.ends_with("billing.send_email"))
            .unwrap();
        let compat = classify(
            s1.contract.signature.as_ref().unwrap(),
            s2.contract.signature.as_ref().unwrap(),
        );
        assert!(compat.breaking, "adding required int user_id must be breaking");

        let call = fc
            .refs
            .iter()
            .find(|r| r.is_call && r.chain.last().map(String::as_str) == Some("send_email"))
            .unwrap();
        let issues = check_call(
            s2.contract.signature.as_ref().unwrap(),
            call.args.as_ref().unwrap(),
            false,
        );
        assert!(!issues.is_empty(), "2-arg call must violate 3-arg C++ signature");
    }
}
