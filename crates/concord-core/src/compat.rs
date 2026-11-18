//! Call compatibility of contract changes.
//!
//! A contract change is **breaking** if some call site that was valid against
//! the old contract may be invalid against the new one (e.g. a removed or newly
//! required parameter). It is **compatible** if every old call site stays valid
//! (e.g. a new parameter with a default, a new `**kwargs`). Compatible changes
//! still produce informational notices but never close the finish gate.
//!
//! The rules are deliberately conservative: anything whose effect on call sites
//! cannot be decided syntactically (decorator changes, changed annotations or
//! return types) counts as breaking. The speculative type-check catches what
//! the rules approve wrongly in the other direction.

use crate::symbol::{Param, ParamKind, Signature};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Compat {
    pub breaking: bool,
    /// Human-readable reasons, breaking ones first.
    pub reasons: Vec<String>,
}

fn is_positional(k: ParamKind) -> bool {
    matches!(k, ParamKind::PositionalOnly | ParamKind::PositionalOrKeyword)
}

/// Classify the change from `old` to `new` for existing call sites.
pub fn classify(old: &Signature, new: &Signature) -> Compat {
    let mut breaking = Vec::new();
    let mut compatible = Vec::new();

    if old.is_async != new.is_async {
        breaking.push(format!("became {}", if new.is_async { "async" } else { "sync" }));
    }

    for d in old.decorators.iter().filter(|d| !new.decorators.contains(d)) {
        breaking.push(format!("decorator removed: {d}"));
    }
    for d in new.decorators.iter().filter(|d| !old.decorators.contains(d)) {
        breaking.push(format!("decorator added: {d}"));
    }

    for b in old.bases.iter().filter(|b| !new.bases.contains(b)) {
        breaking.push(format!("base removed: {b}"));
    }
    for b in new.bases.iter().filter(|b| !old.bases.contains(b)) {
        compatible.push(format!("base added: {b}"));
    }

    match (&old.returns, &new.returns) {
        (Some(a), Some(b)) if a != b => breaking.push(format!("return type changed: {a} -> {b}")),
        (Some(a), None) => compatible.push(format!("return annotation removed: {a}")),
        (None, Some(b)) => compatible.push(format!("return annotation added: {b}")),
        _ => {}
    }

    let find = |name: &str| new.params.iter().find(|p| p.name == name);
    let old_pos: Vec<&Param> = old.params.iter().filter(|p| is_positional(p.kind)).collect();
    let new_pos: Vec<&Param> = new.params.iter().filter(|p| is_positional(p.kind)).collect();
    let new_has_var_kw = new.params.iter().any(|p| p.kind == ParamKind::VarKeyword);
    let new_has_var_pos = new.params.iter().any(|p| p.kind == ParamKind::VarPositional);

    for p in &old.params {
        let Some(q) = find(&p.name) else {
            match p.kind {
                ParamKind::VarPositional if new_has_var_pos => compatible.push(format!("*{} renamed", p.name)),
                ParamKind::VarKeyword if new_has_var_kw => compatible.push(format!("**{} renamed", p.name)),
                ParamKind::KeywordOnly if new_has_var_kw => {
                    compatible.push(format!("parameter `{}` absorbed by **kwargs", p.name))
                }
                _ => breaking.push(format!("parameter `{}` removed or renamed", p.name)),
            }
            continue;
        };
        match (p.kind, q.kind) {
            (a, b) if a == b => {}
            (ParamKind::PositionalOrKeyword, ParamKind::KeywordOnly) => {
                breaking.push(format!("parameter `{}` became keyword-only", p.name))
            }
            (ParamKind::PositionalOrKeyword, ParamKind::PositionalOnly) => {
                breaking.push(format!("parameter `{}` became positional-only", p.name))
            }
            (ParamKind::KeywordOnly | ParamKind::PositionalOnly, ParamKind::PositionalOrKeyword) => {
                compatible.push(format!("parameter `{}` accepts more call forms", p.name))
            }
            (a, b) => breaking.push(format!("parameter `{}` kind changed: {a:?} -> {b:?}", p.name)),
        }
        if p.has_default() && !q.has_default() {
            breaking.push(format!("parameter `{}` is now required", p.name));
        } else if !p.has_default() && q.has_default() {
            compatible.push(format!("parameter `{}` is now optional", p.name));
        } else if p.default != q.default {
            compatible.push(format!("default of `{}` changed (behavioural)", p.name));
        }
        match (&p.annotation, &q.annotation) {
            (Some(a), Some(b)) if a != b => breaking.push(format!("type of `{}` changed: {a} -> {b}", p.name)),
            (None, Some(b)) => compatible.push(format!("`{}` annotated as {b}", p.name)),
            _ => {}
        }
    }

    // Positional callers depend on the order of positional parameters.
    for (i, p) in old_pos.iter().enumerate() {
        if let Some(j) = new_pos.iter().position(|q| q.name == p.name)
            && j != i
        {
            breaking.push(format!(
                "positional parameter `{}` moved from #{} to #{}",
                p.name,
                i + 1,
                j + 1
            ));
        }
    }
    for q in new
        .params
        .iter()
        .filter(|q| !old.params.iter().any(|p| p.name == q.name))
    {
        match q.kind {
            ParamKind::VarPositional | ParamKind::VarKeyword => compatible.push(format!(
                "accepts {}{}",
                if q.kind == ParamKind::VarPositional { "*" } else { "**" },
                q.name
            )),
            _ if q.has_default() => compatible.push(format!("new optional parameter `{}`", q.name)),
            _ => breaking.push(format!("new required parameter `{}`", q.name)),
        }
    }

    let is_breaking = !breaking.is_empty();
    breaking.extend(compatible);
    Compat {
        breaking: is_breaking,
        reasons: breaking,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str, kind: ParamKind, default: Option<&str>) -> Param {
        Param {
            name: name.into(),
            kind,
            annotation: None,
            default: default.map(str::to_owned),
        }
    }
    use ParamKind::*;

    fn sig(params: Vec<Param>) -> Signature {
        Signature {
            params,
            ..Default::default()
        }
    }

    #[test]
    fn new_required_keyword_only_is_breaking() {
        // send_email(to, body) -> send_email(to, body, *, user_id)
        let old = sig(vec![
            p("to", PositionalOrKeyword, None),
            p("body", PositionalOrKeyword, None),
        ]);
        let new = sig(vec![
            p("to", PositionalOrKeyword, None),
            p("body", PositionalOrKeyword, None),
            p("user_id", KeywordOnly, None),
        ]);
        let c = classify(&old, &new);
        assert!(c.breaking);
        assert!(c.reasons[0].contains("user_id"));
    }

    #[test]
    fn new_optional_parameter_is_compatible() {
        let old = sig(vec![p("to", PositionalOrKeyword, None)]);
        let new = sig(vec![
            p("to", PositionalOrKeyword, None),
            p("retries", KeywordOnly, Some("3")),
        ]);
        assert!(!classify(&old, &new).breaking);
    }

    #[test]
    fn removed_renamed_reordered_or_required_parameters_are_breaking() {
        let old = sig(vec![
            p("a", PositionalOrKeyword, None),
            p("b", PositionalOrKeyword, Some("1")),
        ]);
        assert!(classify(&old, &sig(vec![p("a", PositionalOrKeyword, None)])).breaking);
        assert!(
            classify(
                &old,
                &sig(vec![
                    p("a", PositionalOrKeyword, None),
                    p("c", PositionalOrKeyword, Some("1"))
                ])
            )
            .breaking
        );
        assert!(
            classify(
                &old,
                &sig(vec![
                    p("b", PositionalOrKeyword, Some("1")),
                    p("a", PositionalOrKeyword, None)
                ])
            )
            .breaking
        );
        assert!(
            classify(
                &old,
                &sig(vec![
                    p("a", PositionalOrKeyword, None),
                    p("b", PositionalOrKeyword, None)
                ])
            )
            .breaking
        );
    }

    #[test]
    fn adding_kwargs_and_relaxing_is_compatible() {
        let old = sig(vec![p("a", KeywordOnly, None)]);
        let new = sig(vec![p("a", PositionalOrKeyword, Some("0")), p("kw", VarKeyword, None)]);
        let c = classify(&old, &new);
        assert!(!c.breaking, "{:?}", c.reasons);
    }

    #[test]
    fn decorator_and_return_type_changes_are_conservatively_breaking() {
        let old = sig(vec![]);
        let mut new = sig(vec![]);
        new.decorators = vec!["@ staticmethod".into()];
        assert!(classify(&old, &new).breaking);
        let mut a = sig(vec![]);
        a.returns = Some("int".into());
        let mut b = sig(vec![]);
        b.returns = Some("str".into());
        assert!(classify(&a, &b).breaking);
    }
}
