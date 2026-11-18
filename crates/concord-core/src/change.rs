//! Classifying symbol-level changes between two versions of a program.

use crate::symbol::{Contract, Symbol, SymbolId, SymbolKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChangeKind {
    /// A new symbol. Never breaks existing users (but may collide with a concurrent addition).
    Added,
    /// The symbol disappeared. Breaks every user.
    Removed,
    /// The symbol disappeared and an identical body reappeared under a new name.
    Renamed { to: SymbolId },
    /// The contract (signature, annotations, decorators, bases, ...) changed.
    ContractChanged,
    /// Only the implementation changed; the contract is identical.
    BodyChanged,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolChange {
    pub symbol: SymbolId,
    pub symbol_kind: SymbolKind,
    pub path: String,
    #[serde(flatten)]
    pub kind: ChangeKind,
    pub before: Option<Contract>,
    pub after: Option<Contract>,
}

impl SymbolChange {
    /// True if other code that uses this symbol may stop working.
    pub fn is_contract_change(&self) -> bool {
        matches!(
            self.kind,
            ChangeKind::Removed | ChangeKind::Renamed { .. } | ChangeKind::ContractChanged
        )
    }

    pub fn is_body_change(&self) -> bool {
        matches!(self.kind, ChangeKind::BodyChanged)
    }
}

/// Compare two symbol tables (e.g. the base version and an agent's worktree).
///
/// Output order is deterministic (sorted by symbol id).
pub fn diff_symbols<'a>(
    before: impl IntoIterator<Item = &'a Symbol>,
    after: impl IntoIterator<Item = &'a Symbol>,
) -> Vec<SymbolChange> {
    let before: BTreeMap<&SymbolId, &Symbol> = before.into_iter().map(|s| (&s.id, s)).collect();
    let after: BTreeMap<&SymbolId, &Symbol> = after.into_iter().map(|s| (&s.id, s)).collect();

    let mut changes = Vec::new();
    let mut removed: Vec<&Symbol> = Vec::new();
    let mut added: Vec<&Symbol> = Vec::new();

    for (id, old) in &before {
        match after.get(id) {
            Some(new) => {
                if old.contract.digest != new.contract.digest {
                    changes.push(SymbolChange {
                        symbol: (*id).clone(),
                        symbol_kind: new.kind,
                        path: new.path.clone(),
                        kind: ChangeKind::ContractChanged,
                        before: Some(old.contract.clone()),
                        after: Some(new.contract.clone()),
                    });
                } else if old.body != new.body {
                    changes.push(SymbolChange {
                        symbol: (*id).clone(),
                        symbol_kind: new.kind,
                        path: new.path.clone(),
                        kind: ChangeKind::BodyChanged,
                        before: Some(old.contract.clone()),
                        after: Some(new.contract.clone()),
                    });
                }
            }
            None => removed.push(old),
        }
    }
    for (id, new) in &after {
        if !before.contains_key(id) {
            added.push(new);
        }
    }

    // Rename detection: a removed and an added symbol of the same kind in the same
    // file with an identical, unambiguous body digest.
    let mut added_by_body: HashMap<(&str, SymbolKind, &str), Vec<usize>> = HashMap::new();
    for (i, s) in added.iter().enumerate() {
        added_by_body
            .entry((s.path.as_str(), s.kind, s.body.0.as_str()))
            .or_default()
            .push(i);
    }
    let mut consumed = vec![false; added.len()];
    for old in removed {
        let key = (old.path.as_str(), old.kind, old.body.0.as_str());
        let candidate = added_by_body
            .get(&key)
            .filter(|v| v.len() == 1)
            .map(|v| v[0])
            .filter(|&i| !consumed[i]);
        match candidate {
            Some(i) => {
                consumed[i] = true;
                changes.push(SymbolChange {
                    symbol: old.id.clone(),
                    symbol_kind: old.kind,
                    path: old.path.clone(),
                    kind: ChangeKind::Renamed {
                        to: added[i].id.clone(),
                    },
                    before: Some(old.contract.clone()),
                    after: Some(added[i].contract.clone()),
                });
            }
            None => changes.push(SymbolChange {
                symbol: old.id.clone(),
                symbol_kind: old.kind,
                path: old.path.clone(),
                kind: ChangeKind::Removed,
                before: Some(old.contract.clone()),
                after: None,
            }),
        }
    }
    for (i, new) in added.iter().enumerate() {
        if !consumed[i] {
            changes.push(SymbolChange {
                symbol: new.id.clone(),
                symbol_kind: new.kind,
                path: new.path.clone(),
                kind: ChangeKind::Added,
                before: None,
                after: Some(new.contract.clone()),
            });
        }
    }

    changes.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    changes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::{Digest, Span};

    fn sym(id: &str, contract: &str, body: &str) -> Symbol {
        Symbol {
            id: SymbolId::new(id),
            kind: SymbolKind::Function,
            path: "m.py".into(),
            span: Span {
                start_line: 1,
                end_line: 2,
            },
            contract: Contract::new(contract, contract),
            body: Digest::of(body),
        }
    }

    #[test]
    fn classifies_contract_body_add_remove() {
        let before = [
            sym("m.a", "def a(x)", "1"),
            sym("m.b", "def b()", "2"),
            sym("m.c", "def c()", "3"),
        ];
        let after = [
            sym("m.a", "def a(x, y)", "1"),
            sym("m.b", "def b()", "2'"),
            sym("m.d", "def d()", "4"),
        ];
        let changes = diff_symbols(&before, &after);
        let kinds: Vec<(&str, &ChangeKind)> = changes.iter().map(|c| (c.symbol.as_str(), &c.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                ("m.a", &ChangeKind::ContractChanged),
                ("m.b", &ChangeKind::BodyChanged),
                ("m.c", &ChangeKind::Removed),
                ("m.d", &ChangeKind::Added),
            ]
        );
        assert!(changes[0].is_contract_change());
        assert!(!changes[1].is_contract_change());
    }

    #[test]
    fn detects_rename_by_identical_body() {
        let before = [sym("m.old", "def old(x)", "body")];
        let after = [sym("m.new", "def new(x)", "body")];
        let changes = diff_symbols(&before, &after);
        assert_eq!(changes.len(), 1);
        assert_eq!(
            changes[0].kind,
            ChangeKind::Renamed {
                to: SymbolId::new("m.new")
            }
        );
        assert!(changes[0].is_contract_change());
    }

    #[test]
    fn unchanged_symbols_produce_nothing() {
        let s = [sym("m.a", "def a()", "x")];
        assert!(diff_symbols(&s, &s).is_empty());
    }
}
