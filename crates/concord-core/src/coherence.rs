//! The contract-coherence protocol.
//!
//! Each agent's worktree is treated like a private cache and each program symbol
//! like a cache line. Only *contract* writes invalidate other agents, and only
//! the agents whose footprint (reads, uses, writes) includes the symbol.
//!
//! The [`Directory`] is a pure, deterministic state machine: it never performs
//! I/O. Callers (the daemon, tests, simulations) feed it observations and deliver
//! the returned [`Notice`]s to agents.

use crate::change::{ChangeKind, SymbolChange};
use crate::symbol::{Contract, SymbolId, UseSite};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(pub String);

impl AgentId {
    pub fn new(s: impl Into<String>) -> Self {
        AgentId(s.into())
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for AgentId {
    fn from(s: &str) -> Self {
        AgentId(s.to_owned())
    }
}

/// Per-(agent, symbol) coherence state. An absent entry means "no interest" (I).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoherenceState {
    /// The agent reads or uses the symbol and has seen its latest contract.
    Shared,
    /// Another agent published a newer contract the agent has not acknowledged.
    Stale,
    /// The agent changed only the body of the symbol. Never blocks anyone.
    Body,
    /// The agent holds the contract lease of the symbol.
    Owner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WriteKind {
    Contract,
    Body,
    Added,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NoticeKind {
    /// Another agent started changing the contract of a symbol you depend on.
    HeadsUp {
        by: AgentId,
        before: Option<Contract>,
        pending: Option<Contract>,
    },
    /// Another agent published a new contract (`after == None` means removed).
    ContractChanged {
        by: AgentId,
        version: u64,
        before: Option<Contract>,
        after: Option<Contract>,
        rationale: Option<String>,
    },
    /// You changed a contract whose lease is held by another agent.
    LeaseHeld { owner: AgentId },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub to: AgentId,
    pub symbol: SymbolId,
    #[serde(flatten)]
    pub kind: NoticeKind,
    /// The recipient's own code that depends on the symbol.
    pub sites: Vec<UseSite>,
}

impl Notice {
    /// Render the notice as short, factual text suitable for injection into an
    /// agent's context (e.g. Claude Code hook `additionalContext`).
    pub fn render(&self) -> String {
        let contract = |c: &Option<Contract>| {
            c.as_ref()
                .map(|c| c.display.clone())
                .unwrap_or_else(|| "(symbol removed)".to_owned())
        };
        let mut out = String::new();
        match &self.kind {
            NoticeKind::ContractChanged {
                by,
                version,
                before,
                after,
                rationale,
            } => {
                out.push_str(&format!(
                    "[concord] contract change: {}   v{} -> v{}   (by {})\n",
                    self.symbol,
                    version - 1,
                    version,
                    by
                ));
                out.push_str(&format!("  before: {}\n", contract(before)));
                out.push_str(&format!("  after : {}\n", contract(after)));
                if let Some(r) = rationale {
                    out.push_str(&format!("  reason: {by}'s task is \"{r}\"\n"));
                }
                self.push_sites(&mut out);
                out.push_str(&format!(
                    "  status: completion of your task is gated until your uses conform to v{version} \
                     or the change is negotiated with {by}.\n"
                ));
            }
            NoticeKind::HeadsUp { by, before, pending } => {
                out.push_str(&format!(
                    "[concord] heads-up: {} is changing the contract of {}\n",
                    by, self.symbol
                ));
                out.push_str(&format!("  current: {}\n", contract(before)));
                out.push_str(&format!("  pending: {}\n", contract(pending)));
                self.push_sites(&mut out);
                out.push_str("  status: not yet published; a final notice follows when it is.\n");
            }
            NoticeKind::LeaseHeld { owner } => {
                out.push_str(&format!(
                    "[concord] lease conflict: the contract of {} is currently being changed by {}.\n",
                    self.symbol, owner
                ));
                out.push_str(&format!(
                    "  status: two agents are changing the same contract; completion is gated until \
                     this is reconciled with {owner}.\n"
                ));
            }
        }
        out
    }

    fn push_sites(&self, out: &mut String) {
        if self.sites.is_empty() {
            return;
        }
        out.push_str("  your code that depends on it:\n");
        for s in &self.sites {
            out.push_str(&format!("    {}:{}  {}: {}\n", s.path, s.line, s.from.name(), s.text));
        }
    }
}

/// Why an agent may not complete yet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Blocker {
    Stale { symbol: SymbolId, version: u64 },
    Contended { symbol: SymbolId, owner: Option<AgentId> },
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gate {
    pub blockers: Vec<Blocker>,
}

impl Gate {
    pub fn is_open(&self) -> bool {
        self.blockers.is_empty()
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Published {
    version: u64,
    contract: Option<Contract>,
    by: Option<AgentId>,
    rationale: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Pending {
    before: Option<Contract>,
    after: Option<Contract>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct SymbolEntry {
    /// Contract at the common base revision (None if the symbol did not exist).
    base: Option<Contract>,
    base_known: bool,
    published: Option<Published>,
    owner: Option<AgentId>,
    pending: Option<Pending>,
}

impl SymbolEntry {
    fn version(&self) -> u64 {
        self.published.as_ref().map_or(0, |p| p.version)
    }
    fn current_contract(&self) -> Option<Contract> {
        match &self.published {
            Some(p) => p.contract.clone(),
            None => self.base.clone(),
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AgentEntry {
    task: Option<String>,
    reads: BTreeSet<SymbolId>,
    uses: BTreeMap<SymbolId, Vec<UseSite>>,
    writes: BTreeMap<SymbolId, WriteKind>,
    seen: BTreeMap<SymbolId, u64>,
    stale: BTreeSet<SymbolId>,
    contended: BTreeSet<SymbolId>,
    warned: BTreeSet<SymbolId>,
}

impl AgentEntry {
    fn interested_in(&self, s: &SymbolId) -> bool {
        self.reads.contains(s) || self.uses.contains_key(s) || self.writes.contains_key(s)
    }
    fn depends_on(&self, s: &SymbolId) -> bool {
        self.uses.contains_key(s) || self.writes.contains_key(s)
    }
}

/// The coherence directory: protocol state for all agents and symbols.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Directory {
    symbols: BTreeMap<SymbolId, SymbolEntry>,
    agents: BTreeMap<AgentId, AgentEntry>,
}

impl Directory {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_agent(&mut self, agent: &AgentId, task: Option<String>) {
        let e = self.agents.entry(agent.clone()).or_default();
        if task.is_some() {
            e.task = task;
        }
    }

    pub fn agents(&self) -> impl Iterator<Item = &AgentId> {
        self.agents.keys()
    }

    /// Current published version of a symbol's contract (0 = base revision).
    pub fn version(&self, symbol: &SymbolId) -> u64 {
        self.symbols.get(symbol).map_or(0, |e| e.version())
    }

    pub fn owner(&self, symbol: &SymbolId) -> Option<&AgentId> {
        self.symbols.get(symbol).and_then(|e| e.owner.as_ref())
    }

    pub fn state(&self, agent: &AgentId, symbol: &SymbolId) -> Option<CoherenceState> {
        let a = self.agents.get(agent)?;
        if self.owner(symbol) == Some(agent) {
            Some(CoherenceState::Owner)
        } else if a.stale.contains(symbol) {
            Some(CoherenceState::Stale)
        } else if matches!(a.writes.get(symbol), Some(WriteKind::Body | WriteKind::Added)) {
            Some(CoherenceState::Body)
        } else if a.interested_in(symbol) {
            Some(CoherenceState::Shared)
        } else {
            None
        }
    }

    /// The agent observed the definitions of these symbols (read, search, navigation).
    pub fn observe_reads(&mut self, agent: &AgentId, symbols: impl IntoIterator<Item = SymbolId>) -> Vec<Notice> {
        self.agents.entry(agent.clone()).or_default();
        let mut notices = Vec::new();
        for s in symbols {
            let a = self.agents.get_mut(agent).expect("registered");
            let newly = !a.interested_in(&s);
            a.reads.insert(s.clone());
            if newly {
                notices.extend(self.miss_check(agent, &s));
            }
        }
        notices
    }

    /// Replace the agent's write/use footprint with its current state, computed
    /// from its worktree relative to the common base (Algorithm 1 in the thesis).
    pub fn observe_footprint(
        &mut self,
        agent: &AgentId,
        changes: &[SymbolChange],
        uses: BTreeMap<SymbolId, Vec<UseSite>>,
    ) -> Vec<Notice> {
        self.agents.entry(agent.clone()).or_default();
        let mut notices = Vec::new();

        let new_writes: BTreeMap<SymbolId, WriteKind> = changes
            .iter()
            .map(|c| {
                let k = if c.is_contract_change() {
                    WriteKind::Contract
                } else if matches!(c.kind, ChangeKind::Added) {
                    WriteKind::Added
                } else {
                    WriteKind::Body
                };
                (c.symbol.clone(), k)
            })
            .collect();

        // Release leases on contracts the agent no longer changes (e.g. reverted edits).
        let old_writes = self.agents[agent].writes.clone();
        for (s, k) in &old_writes {
            if *k == WriteKind::Contract && new_writes.get(s) != Some(&WriteKind::Contract) {
                if let Some(e) = self.symbols.get_mut(s)
                    && e.owner.as_ref() == Some(agent)
                {
                    e.owner = None;
                    e.pending = None;
                }
                self.agents.get_mut(agent).unwrap().contended.remove(s);
            }
        }

        // Contract changes: take the lease or report contention.
        for c in changes.iter().filter(|c| c.is_contract_change()) {
            let entry = self.symbols.entry(c.symbol.clone()).or_default();
            if !entry.base_known {
                entry.base = c.before.clone();
                entry.base_known = true;
            }
            match entry.owner.clone() {
                Some(owner) if &owner != agent => {
                    let a = self.agents.get_mut(agent).unwrap();
                    if a.contended.insert(c.symbol.clone()) {
                        notices.push(Notice {
                            to: agent.clone(),
                            symbol: c.symbol.clone(),
                            kind: NoticeKind::LeaseHeld { owner },
                            sites: vec![],
                        });
                    }
                }
                _ => {
                    let first = entry.owner.is_none();
                    entry.owner = Some(agent.clone());
                    entry.pending = Some(Pending {
                        before: entry.current_contract(),
                        after: c.after.clone(),
                    });
                    if first {
                        let before = entry.current_contract();
                        let pending = c.after.clone();
                        for (b, be) in self.agents.iter_mut().filter(|(b, _)| *b != agent) {
                            if be.depends_on(&c.symbol) && be.warned.insert(c.symbol.clone()) {
                                notices.push(Notice {
                                    to: b.clone(),
                                    symbol: c.symbol.clone(),
                                    kind: NoticeKind::HeadsUp {
                                        by: agent.clone(),
                                        before: before.clone(),
                                        pending: pending.clone(),
                                    },
                                    sites: be.uses.get(&c.symbol).cloned().unwrap_or_default(),
                                });
                            }
                        }
                    }
                }
            }
        }

        // Update the footprint and handle "read misses" for newly depended-on symbols.
        let newly: Vec<SymbolId> = {
            let a = &self.agents[agent];
            uses.keys()
                .chain(new_writes.keys())
                .filter(|s| !a.interested_in(s))
                .cloned()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        {
            let a = self.agents.get_mut(agent).unwrap();
            a.uses = uses;
            a.writes = new_writes;
        }
        for s in newly {
            notices.extend(self.miss_check(agent, &s));
        }
        notices
    }

    /// Publish the agent's pending contract change of `symbol` to all interested agents.
    pub fn publish(&mut self, agent: &AgentId, symbol: &SymbolId) -> Vec<Notice> {
        let task = self.agents.get(agent).and_then(|a| a.task.clone());
        let Some(entry) = self.symbols.get_mut(symbol) else {
            return vec![];
        };
        if entry.owner.as_ref() != Some(agent) {
            return vec![];
        }
        let Some(pending) = entry.pending.take() else {
            return vec![];
        };
        let version = entry.version() + 1;
        entry.published = Some(Published {
            version,
            contract: pending.after.clone(),
            by: Some(agent.clone()),
            rationale: task.clone(),
        });

        if let Some(a) = self.agents.get_mut(agent) {
            a.seen.insert(symbol.clone(), version);
        }
        let mut notices = Vec::new();
        for (b, be) in self.agents.iter_mut().filter(|(b, _)| *b != agent) {
            if be.interested_in(symbol) {
                be.stale.insert(symbol.clone());
                notices.push(Notice {
                    to: b.clone(),
                    symbol: symbol.clone(),
                    kind: NoticeKind::ContractChanged {
                        by: agent.clone(),
                        version,
                        before: pending.before.clone(),
                        after: pending.after.clone(),
                        rationale: task.clone(),
                    },
                    sites: be.uses.get(symbol).cloned().unwrap_or_default(),
                });
            }
        }
        notices
    }

    /// Publish every pending contract change the agent owns.
    pub fn publish_all(&mut self, agent: &AgentId) -> Vec<Notice> {
        let owned: Vec<SymbolId> = self
            .symbols
            .iter()
            .filter(|(_, e)| e.owner.as_ref() == Some(agent) && e.pending.is_some())
            .map(|(s, _)| s.clone())
            .collect();
        owned.iter().flat_map(|s| self.publish(agent, s)).collect()
    }

    /// The agent has incorporated the latest contract of `symbol`.
    pub fn acknowledge(&mut self, agent: &AgentId, symbol: &SymbolId) {
        let v = self.version(symbol);
        if let Some(a) = self.agents.get_mut(agent) {
            a.seen.insert(symbol.clone(), v);
            a.stale.remove(symbol);
            a.contended.remove(symbol);
        }
    }

    /// May the agent complete its task? (The "finish gate".)
    pub fn finish_gate(&self, agent: &AgentId) -> Gate {
        let Some(a) = self.agents.get(agent) else {
            return Gate::default();
        };
        let mut blockers = Vec::new();
        for s in &a.stale {
            if a.depends_on(s) {
                blockers.push(Blocker::Stale {
                    symbol: s.clone(),
                    version: self.version(s),
                });
            }
        }
        for s in &a.contended {
            blockers.push(Blocker::Contended {
                symbol: s.clone(),
                owner: self.owner(s).cloned(),
            });
        }
        Gate { blockers }
    }

    /// Release all leases held by the agent (after it finished or was stopped).
    pub fn release(&mut self, agent: &AgentId) {
        for e in self.symbols.values_mut() {
            if e.owner.as_ref() == Some(agent) {
                e.owner = None;
                e.pending = None;
            }
        }
    }

    fn miss_check(&mut self, agent: &AgentId, s: &SymbolId) -> Vec<Notice> {
        let Some(entry) = self.symbols.get(s) else {
            return vec![];
        };
        let a = self.agents.get_mut(agent).expect("registered");
        let seen = a.seen.get(s).copied().unwrap_or(0);
        let mut out = Vec::new();
        if let Some(p) = &entry.published
            && p.by.as_ref() != Some(agent)
            && p.version > seen
        {
            a.stale.insert(s.clone());
            out.push(Notice {
                to: agent.clone(),
                symbol: s.clone(),
                kind: NoticeKind::ContractChanged {
                    by: p.by.clone().unwrap_or_else(|| AgentId::new("?")),
                    version: p.version,
                    before: entry.base.clone(),
                    after: p.contract.clone(),
                    rationale: p.rationale.clone(),
                },
                sites: a.uses.get(s).cloned().unwrap_or_default(),
            });
            return out;
        }
        if let (Some(owner), Some(pending)) = (&entry.owner, &entry.pending) {
            // A contending agent already received a LeaseHeld notice.
            if owner != agent && !a.contended.contains(s) && a.warned.insert(s.clone()) {
                out.push(Notice {
                    to: agent.clone(),
                    symbol: s.clone(),
                    kind: NoticeKind::HeadsUp {
                        by: owner.clone(),
                        before: pending.before.clone(),
                        pending: pending.after.clone(),
                    },
                    sites: a.uses.get(s).cloned().unwrap_or_default(),
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbol::SymbolKind;

    fn contract(s: &str) -> Contract {
        Contract::new(s, s)
    }

    fn change(sym: &str, kind: ChangeKind, before: Option<&str>, after: Option<&str>) -> SymbolChange {
        SymbolChange {
            symbol: SymbolId::new(sym),
            symbol_kind: SymbolKind::Function,
            path: "x.py".into(),
            kind,
            before: before.map(contract),
            after: after.map(contract),
        }
    }

    fn uses(sym: &str, path: &str, line: u32, from: &str, text: &str) -> BTreeMap<SymbolId, Vec<UseSite>> {
        BTreeMap::from([(
            SymbolId::new(sym),
            vec![UseSite {
                path: path.into(),
                line,
                from: SymbolId::new(from),
                text: text.into(),
            }],
        )])
    }

    const SEND: &str = "notify.email.send_email";

    /// The running example of the thesis: A changes the contract of `send_email`,
    /// B (in another file) calls it, C only edits a body in A's file.
    #[test]
    fn running_example() {
        let (a, b, c) = (AgentId::new("A"), AgentId::new("B"), AgentId::new("C"));
        let mut d = Directory::new();
        d.register_agent(&a, Some("rate-limit outgoing e-mails per user".into()));
        d.register_agent(&b, Some("add password-reset flow".into()));
        d.register_agent(&c, Some("improve e-mail subjects".into()));

        // B writes new code that calls send_email.
        let n = d.observe_footprint(
            &b,
            &[change(
                "auth.reset.request_reset",
                ChangeKind::Added,
                None,
                Some("def request_reset(user)"),
            )],
            uses(
                SEND,
                "auth/reset.py",
                4,
                "auth.reset.request_reset",
                "send_email(user.email, reset_msg(token))",
            ),
        );
        assert!(n.is_empty());

        // A changes the contract of send_email: lease + heads-up to B only.
        let n = d.observe_footprint(
            &a,
            &[change(
                SEND,
                ChangeKind::ContractChanged,
                Some("def send_email(to, body)"),
                Some("def send_email(to, body, *, user_id)"),
            )],
            BTreeMap::new(),
        );
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].to, b);
        assert!(matches!(n[0].kind, NoticeKind::HeadsUp { .. }));
        assert_eq!(d.state(&a, &SymbolId::new(SEND)), Some(CoherenceState::Owner));

        // C changes only a body in the same file: nobody is notified or blocked.
        let n = d.observe_footprint(
            &c,
            &[change(
                "notify.email.format_subject",
                ChangeKind::BodyChanged,
                Some("def format_subject(s)"),
                Some("def format_subject(s)"),
            )],
            BTreeMap::new(),
        );
        assert!(n.is_empty());
        assert_eq!(
            d.state(&c, &SymbolId::new("notify.email.format_subject")),
            Some(CoherenceState::Body)
        );

        // A publishes: B gets a precise notice with its own call site and becomes stale.
        let n = d.publish_all(&a);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].to, b);
        assert_eq!(n[0].sites.len(), 1);
        let text = n[0].render();
        assert!(text.contains("def send_email(to, body, *, user_id)"));
        assert!(text.contains("auth/reset.py:4"));
        assert_eq!(d.state(&b, &SymbolId::new(SEND)), Some(CoherenceState::Stale));

        // Finish gate: B blocked, A and C free.
        assert!(!d.finish_gate(&b).is_open());
        assert!(d.finish_gate(&a).is_open());
        assert!(d.finish_gate(&c).is_open());

        // B adapts its call site and acknowledges.
        d.acknowledge(&b, &SymbolId::new(SEND));
        assert!(d.finish_gate(&b).is_open());
        assert_eq!(d.state(&b, &SymbolId::new(SEND)), Some(CoherenceState::Shared));
    }

    #[test]
    fn read_miss_delivers_latest_contract() {
        let (a, d_agent) = (AgentId::new("A"), AgentId::new("D"));
        let mut d = Directory::new();
        d.observe_footprint(
            &a,
            &[change(
                SEND,
                ChangeKind::ContractChanged,
                Some("def send_email(to, body)"),
                Some("def send_email(to, body, *, user_id)"),
            )],
            BTreeMap::new(),
        );
        assert!(d.publish_all(&a).is_empty()); // nobody interested yet
        assert_eq!(d.version(&SymbolId::new(SEND)), 1);

        // D starts depending on send_email later: it is told immediately.
        let n = d.observe_footprint(
            &d_agent,
            &[change(
                "billing.invoice",
                ChangeKind::Added,
                None,
                Some("def invoice()"),
            )],
            uses(SEND, "billing.py", 9, "billing.invoice", "send_email(addr, pdf)"),
        );
        assert_eq!(n.len(), 1);
        match &n[0].kind {
            NoticeKind::ContractChanged {
                version, before, after, ..
            } => {
                assert_eq!(*version, 1);
                assert_eq!(before.as_ref().unwrap().display, "def send_email(to, body)");
                assert_eq!(after.as_ref().unwrap().display, "def send_email(to, body, *, user_id)");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(!d.finish_gate(&d_agent).is_open());
    }

    #[test]
    fn concurrent_contract_edits_are_serialised() {
        let (a, e) = (AgentId::new("A"), AgentId::new("E"));
        let mut d = Directory::new();
        let ch = change(
            SEND,
            ChangeKind::ContractChanged,
            Some("def send_email(to, body)"),
            Some("def send_email(to, body, *, user_id)"),
        );
        d.observe_footprint(&a, std::slice::from_ref(&ch), BTreeMap::new());
        let n = d.observe_footprint(&e, &[ch], BTreeMap::new());
        assert_eq!(n.len(), 1);
        assert!(matches!(n[0].kind, NoticeKind::LeaseHeld { .. }));
        assert!(!d.finish_gate(&e).is_open());
        assert!(d.finish_gate(&a).is_open());
    }

    #[test]
    fn reverting_a_contract_change_releases_the_lease() {
        let a = AgentId::new("A");
        let mut d = Directory::new();
        let ch = change(
            SEND,
            ChangeKind::ContractChanged,
            Some("def send_email(to, body)"),
            Some("def send_email(to)"),
        );
        d.observe_footprint(&a, &[ch], BTreeMap::new());
        assert_eq!(d.owner(&SymbolId::new(SEND)), Some(&a));
        d.observe_footprint(&a, &[], BTreeMap::new());
        assert_eq!(d.owner(&SymbolId::new(SEND)), None);
    }

    #[test]
    fn read_only_interest_is_notified_but_not_gated() {
        let (a, r) = (AgentId::new("A"), AgentId::new("R"));
        let mut d = Directory::new();
        d.observe_reads(&r, [SymbolId::new(SEND)]);
        d.observe_footprint(
            &a,
            &[change(
                SEND,
                ChangeKind::ContractChanged,
                Some("def send_email(to, body)"),
                Some("def send_email(to)"),
            )],
            BTreeMap::new(),
        );
        let n = d.publish_all(&a);
        assert_eq!(n.len(), 1);
        assert_eq!(n[0].to, r);
        assert!(d.finish_gate(&r).is_open());
    }
}
