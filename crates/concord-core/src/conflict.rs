//! Offline pairwise analysis of concurrent branches.
//!
//! Given the symbol-level footprint of several branches developed in parallel
//! from the same base, report where they interfere. This is the instrument for
//! RQ1 of the thesis (where do parallel agents break each other?) and the core
//! of the `concord analyze` command.

use crate::change::{ChangeKind, SymbolChange};
use crate::compat::check_call;
use crate::symbol::SymbolKind;
use crate::symbol::{Contract, SymbolId, UseSite};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The symbol-level footprint of one branch relative to the common base.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BranchFootprint {
    pub name: String,
    pub changed_files: BTreeSet<String>,
    pub changes: Vec<SymbolChange>,
    /// Symbols the branch's new or changed code depends on, with use sites.
    pub uses: BTreeMap<SymbolId, Vec<UseSite>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
// A report type, built once per analysis: variant size does not matter.
#[allow(clippy::large_enum_variant)]
pub enum Finding {
    /// Both branches modify the same file. If `shared_symbols` is empty the edits
    /// are disjoint at symbol level, so file-level locking would serialise them
    /// needlessly.
    FileOverlap {
        a: String,
        b: String,
        path: String,
        shared_symbols: Vec<SymbolId>,
    },
    /// Both branches change the same symbol.
    SameSymbol {
        a: String,
        b: String,
        symbol: SymbolId,
        a_change: ChangeKind,
        b_change: ChangeKind,
    },
    /// `producer` changed the contract of a symbol that `consumer`'s new code uses.
    ContractDependency {
        producer: String,
        consumer: String,
        symbol: SymbolId,
        before: Option<Contract>,
        after: Option<Contract>,
        sites: Vec<UseSite>,
        /// Whether call sites written against `before` may break (in general).
        breaking: bool,
        /// Concrete problems of the consumer's own uses against the new contract
        /// (calls checked against the new signature; any use of a removed symbol).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        violations: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        reasons: Vec<String>,
        /// True if the two branches touch no common file, i.e. the conflict is
        /// invisible to file-level coordination and to textual merge.
        file_disjoint: bool,
    },
    /// Both branches add a symbol with the same qualified name.
    DuplicateDefinition { a: String, b: String, symbol: SymbolId },
}

pub fn analyze(branches: &[BranchFootprint]) -> Vec<Finding> {
    let mut out = Vec::new();
    for i in 0..branches.len() {
        for j in (i + 1)..branches.len() {
            let (a, b) = (&branches[i], &branches[j]);
            pair(a, b, &mut out);
        }
    }
    out
}

fn pair(a: &BranchFootprint, b: &BranchFootprint, out: &mut Vec<Finding>) {
    let a_changes: BTreeMap<&SymbolId, &SymbolChange> = a.changes.iter().map(|c| (&c.symbol, c)).collect();
    let b_changes: BTreeMap<&SymbolId, &SymbolChange> = b.changes.iter().map(|c| (&c.symbol, c)).collect();

    for path in a.changed_files.intersection(&b.changed_files) {
        let shared: Vec<SymbolId> = a_changes
            .iter()
            .filter(|(s, c)| c.path == *path && b_changes.get(*s).is_some_and(|bc| bc.path == *path))
            .map(|(s, _)| (*s).clone())
            .collect();
        out.push(Finding::FileOverlap {
            a: a.name.clone(),
            b: b.name.clone(),
            path: path.clone(),
            shared_symbols: shared,
        });
    }

    for (s, ac) in &a_changes {
        if let Some(bc) = b_changes.get(s) {
            if matches!(ac.kind, ChangeKind::Added) && matches!(bc.kind, ChangeKind::Added) {
                out.push(Finding::DuplicateDefinition {
                    a: a.name.clone(),
                    b: b.name.clone(),
                    symbol: (*s).clone(),
                });
            } else {
                out.push(Finding::SameSymbol {
                    a: a.name.clone(),
                    b: b.name.clone(),
                    symbol: (*s).clone(),
                    a_change: ac.kind.clone(),
                    b_change: bc.kind.clone(),
                });
            }
        }
    }

    let file_disjoint = a.changed_files.is_disjoint(&b.changed_files);
    for (producer, consumer) in [(a, b), (b, a)] {
        for c in producer.changes.iter().filter(|c| c.is_contract_change()) {
            if let Some(sites) = consumer.uses.get(&c.symbol) {
                out.push(Finding::ContractDependency {
                    producer: producer.name.clone(),
                    consumer: consumer.name.clone(),
                    symbol: c.symbol.clone(),
                    before: c.before.clone(),
                    after: c.after.clone(),
                    sites: sites.clone(),
                    breaking: c.is_breaking(),
                    violations: violations(c, sites),
                    reasons: c.compat.as_ref().map(|x| x.reasons.clone()).unwrap_or_default(),
                    file_disjoint,
                });
            }
        }
    }
}

/// Problems of `sites` against the contract after change `c`.
pub fn violations(c: &SymbolChange, sites: &[UseSite]) -> Vec<String> {
    if !c.is_breaking() {
        return vec![];
    }
    let Some(after) = &c.after else {
        return sites
            .iter()
            .map(|s| format!("{}:{}  {}  -> `{}` no longer exists", s.path, s.line, s.text, c.symbol))
            .collect();
    };
    if matches!(c.kind, ChangeKind::Renamed { .. }) {
        return sites
            .iter()
            .map(|s| format!("{}:{}  {}  -> renamed to {}", s.path, s.line, s.text, after.display))
            .collect();
    }
    let Some(sig) = &after.signature else {
        // No structured signature: cannot check calls, keep the conservative flag.
        return sites
            .iter()
            .filter(|s| s.call.is_some())
            .map(|s| format!("{}:{}  {}  -> contract changed (unchecked)", s.path, s.line, s.text))
            .collect();
    };
    let bound = c.symbol_kind == SymbolKind::Method && !sig.decorators.iter().any(|d| d.contains("staticmethod"));
    let mut out = Vec::new();
    for s in sites {
        if let Some(call) = &s.call {
            for p in check_call(sig, call, bound) {
                out.push(format!("{}:{}  {}  -> {p}", s.path, s.line, s.text));
            }
        }
    }
    out
}

/// Aggregate counts over findings (one row per analysed task in RQ1).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub file_overlaps: usize,
    /// File overlaps where no symbol is shared: needless serialisation under file locks.
    pub file_overlaps_symbol_disjoint: usize,
    pub same_symbol: usize,
    pub same_symbol_both_body_only: usize,
    pub contract_dependencies: usize,
    /// Contract dependencies between branches that share no file.
    pub contract_dependencies_file_disjoint: usize,
    /// Contract dependencies where the change may break the consumer's call sites.
    pub breaking_contract_dependencies: usize,
    pub breaking_contract_dependencies_file_disjoint: usize,
    /// Contract dependencies where the consumer's own call sites violate the new contract.
    pub violating_contract_dependencies: usize,
    pub duplicate_definitions: usize,
}

pub fn summarize(findings: &[Finding]) -> Summary {
    let mut s = Summary::default();
    for f in findings {
        match f {
            Finding::FileOverlap { shared_symbols, .. } => {
                s.file_overlaps += 1;
                if shared_symbols.is_empty() {
                    s.file_overlaps_symbol_disjoint += 1;
                }
            }
            Finding::SameSymbol { a_change, b_change, .. } => {
                s.same_symbol += 1;
                if matches!(a_change, ChangeKind::BodyChanged) && matches!(b_change, ChangeKind::BodyChanged) {
                    s.same_symbol_both_body_only += 1;
                }
            }
            Finding::ContractDependency {
                file_disjoint,
                breaking,
                violations,
                ..
            } => {
                if !violations.is_empty() {
                    s.violating_contract_dependencies += 1;
                }
                s.contract_dependencies += 1;
                if *file_disjoint {
                    s.contract_dependencies_file_disjoint += 1;
                }
                if *breaking {
                    s.breaking_contract_dependencies += 1;
                    if *file_disjoint {
                        s.breaking_contract_dependencies_file_disjoint += 1;
                    }
                }
            }
            Finding::DuplicateDefinition { .. } => s.duplicate_definitions += 1,
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::SymbolKind;

    fn ch(sym: &str, path: &str, kind: ChangeKind) -> SymbolChange {
        SymbolChange {
            symbol: SymbolId::new(sym),
            symbol_kind: SymbolKind::Function,
            path: path.into(),
            kind,
            before: None,
            after: None,
            compat: None,
        }
    }

    #[test]
    fn running_example_findings() {
        let a = BranchFootprint {
            name: "A".into(),
            changed_files: ["notify/email.py".to_string()].into(),
            changes: vec![ch(
                "notify.email.send_email",
                "notify/email.py",
                ChangeKind::ContractChanged,
            )],
            uses: BTreeMap::new(),
        };
        let b = BranchFootprint {
            name: "B".into(),
            changed_files: ["auth/reset.py".to_string()].into(),
            changes: vec![ch("auth.reset.request_reset", "auth/reset.py", ChangeKind::Added)],
            uses: BTreeMap::from([(
                SymbolId::new("notify.email.send_email"),
                vec![UseSite {
                    path: "auth/reset.py".into(),
                    line: 4,
                    from: SymbolId::new("auth.reset.request_reset"),
                    text: "send_email(user.email, reset_msg(token))".into(),
                    call: None,
                }],
            )]),
        };
        let c = BranchFootprint {
            name: "C".into(),
            changed_files: ["notify/email.py".to_string()].into(),
            changes: vec![ch(
                "notify.email.format_subject",
                "notify/email.py",
                ChangeKind::BodyChanged,
            )],
            uses: BTreeMap::new(),
        };
        let findings = analyze(&[a, b, c]);
        let s = summarize(&findings);
        assert_eq!(s.contract_dependencies, 1);
        assert_eq!(
            s.contract_dependencies_file_disjoint, 1,
            "A->B is invisible to file locks"
        );
        assert_eq!(s.file_overlaps, 1, "A and C share notify/email.py");
        assert_eq!(s.file_overlaps_symbol_disjoint, 1, "...but touch different symbols");
        assert_eq!(s.same_symbol, 0);
    }
}
