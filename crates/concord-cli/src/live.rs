//! The live coordination daemon.
//!
//! Agents (Claude Code sessions) report every tool call through hooks
//! (`concord hook`), which forward the hook input to this daemon over a local
//! TCP connection. The daemon keeps each agent's symbol footprint up to date,
//! runs the coherence protocol, delivers notices through `additionalContext`,
//! and enforces the finish gate in the `Stop` hook.
//!
//! The finish gate makes notices *verifiable*: before an agent may finish
//! while depending on a contract another agent changed, Concord waits for that
//! producer to finish, merges the producer's branch into the agent's worktree
//! (so the agent can see the new code), and checks every call site of the
//! changed symbol against the new signature.

use crate::{footprint_and_index, index_revision};
use anyhow::{Context, Result};
use concord_core::{AgentId, ChangeKind, Directory, Notice, NoticeKind, SymbolId, SymbolKind, check_call};
use concord_git::Repo;
use concord_lang::{FileIndex, RepoIndex};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// No hooks at all (baseline).
    Off,
    /// Hooks installed; Concord observes and logs but never intervenes.
    Observe,
    /// Full protocol: notices and finish gate.
    Enforce,
}

impl std::str::FromStr for Mode {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s {
            "off" => Ok(Mode::Off),
            "observe" => Ok(Mode::Observe),
            "enforce" => Ok(Mode::Enforce),
            _ => Err(format!("unknown mode {s} (off | observe | enforce)")),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Finished,
}

/// Upper bound on finish-gate blocks per agent (liveness guarantee).
pub const MAX_GATE_BLOCKS: u32 = 6;

#[derive(Clone, Debug, Default, Serialize)]
pub struct AgentStats {
    pub tool_events: u32,
    pub footprint_refreshes: u32,
    pub notices_delivered: u32,
    pub gate_blocks: u32,
    pub merges: Vec<String>,
    pub merge_conflicts: Vec<String>,
    /// Region conflicts this agent was told to yield.
    pub region_conflicts: u32,
}

#[derive(Debug)]
pub struct AgentInfo {
    pub name: String,
    pub worktree: PathBuf,
    pub branch: String,
    /// Commit the agent's own changes are measured against (moves after merges).
    pub base: String,
    pub status: Status,
    pub session_id: Option<String>,
    pub outbox: Vec<Notice>,
    pub merged_producers: BTreeSet<String>,
    pub index: Option<RepoIndex>,
    pub stats: AgentStats,
    /// Files with conflict markers from a peer merge the agent must resolve.
    pub pending_conflicts: Vec<String>,
    /// Peer whose merge is in progress (committed once conflicts are resolved).
    pub pending_peer: Option<(String, String)>,
    /// Position in the task list; the lower rank owns contested regions.
    pub rank: usize,
    /// Latest snapshot commit of the worktree (region coherence).
    pub snapshot: Option<String>,
    /// Region notices waiting to be delivered.
    pub region_notes: Vec<String>,
}

struct Snapshot {
    files: BTreeMap<String, FileIndex>,
    index: RepoIndex,
}

pub struct State {
    pub repo: Repo,
    pub base: String,
    pub mode: Mode,
    pub dir: Directory,
    pub agents: BTreeMap<String, AgentInfo>,
    pub max_wait: Duration,
    /// Before finishing, merge finished peers whose work conflicts textually
    /// with the agent's, so the agent resolves the conflicts itself.
    pub peer_merge: bool,
    /// Contract layer (notices, contract finish gate, resumes). Off = peer merge
    /// only (ablation).
    pub contracts: bool,
    /// Region coherence: textual overlaps are detected after every edit and
    /// resolved by the yielding agent (see `regions`).
    pub regions: bool,
    /// Region notices already sent: (yielder, owner, file).
    pub(crate) region_alerted: BTreeSet<(String, String, String)>,
    /// Pairs (yielder, owner) with a region yielded at some point: the owner's
    /// finished work is handed over to the yielder.
    pub(crate) region_yields: BTreeSet<(String, String)>,
    /// Waits-for graph of the finish gate, across the contract and region
    /// layers: agent -> agents it is currently waiting for. A wait that would
    /// close a cycle is never entered (no cross-layer deadlock).
    pub(crate) waiting: BTreeMap<String, BTreeSet<String>>,
    revisions: HashMap<String, Arc<Snapshot>>,
    log: Option<std::fs::File>,
    started: Instant,
}

pub type Shared = Arc<Mutex<State>>;

impl State {
    pub fn new(repo: Repo, base: String, mode: Mode, log_path: Option<&Path>, max_wait: Duration) -> Result<Self> {
        let log = match log_path {
            Some(p) => Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(p)
                    .with_context(|| format!("opening {}", p.display()))?,
            ),
            None => None,
        };
        Ok(State {
            repo,
            base,
            mode,
            dir: Directory::new(),
            agents: BTreeMap::new(),
            max_wait,
            peer_merge: false,
            contracts: true,
            regions: false,
            region_alerted: BTreeSet::new(),
            region_yields: BTreeSet::new(),
            waiting: BTreeMap::new(),
            revisions: HashMap::new(),
            log,
            started: Instant::now(),
        })
    }

    pub fn add_agent(&mut self, name: &str, worktree: PathBuf, branch: String, task: &str) {
        let rank = self.agents.len();
        self.dir
            .register_agent(&AgentId::new(name), Some(first_line(task, 120)));
        self.agents.insert(
            name.to_owned(),
            AgentInfo {
                name: name.to_owned(),
                worktree,
                branch,
                base: self.base.clone(),
                status: Status::Running,
                session_id: None,
                outbox: Vec::new(),
                merged_producers: BTreeSet::new(),
                index: None,
                stats: AgentStats::default(),
                pending_conflicts: Vec::new(),
                pending_peer: None,
                rank,
                snapshot: None,
                region_notes: Vec::new(),
            },
        );
    }

    /// Whether `from` is (transitively) waiting for `to` in the finish gate.
    pub(crate) fn waits_for(&self, from: &str, to: &str) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack = vec![from.to_owned()];
        while let Some(a) = stack.pop() {
            if a == to {
                return true;
            }
            if seen.insert(a.clone())
                && let Some(next) = self.waiting.get(&a)
            {
                stack.extend(next.iter().cloned());
            }
        }
        false
    }

    pub fn log(&mut self, event: &str, data: Value) {
        let t = self.started.elapsed().as_secs_f64();
        if let Some(f) = self.log.as_mut() {
            let line = json!({ "t": (t * 1000.0).round() / 1000.0, "event": event, "data": data });
            let _ = writeln!(f, "{line}");
        }
    }

    fn revision(&mut self, rev: &str) -> Result<Arc<Snapshot>> {
        if let Some(s) = self.revisions.get(rev) {
            return Ok(s.clone());
        }
        let mut warnings = Vec::new();
        let files = index_revision(&self.repo, rev, &mut warnings)?;
        let index = RepoIndex::new(files.values().cloned());
        let snap = Arc::new(Snapshot { files, index });
        self.revisions.insert(rev.to_owned(), snap.clone());
        Ok(snap)
    }

    /// Recompute an agent's footprint from its worktree, run the protocol, and
    /// route resulting notices. Contract changes are published immediately.
    pub fn refresh(&mut self, agent: &str) -> Result<()> {
        let (wt, base) = {
            let a = self.agents.get(agent).context("unknown agent")?;
            (a.worktree.clone(), a.base.clone())
        };
        let snap_commit = Repo::snapshot_worktree(&wt)?;
        let base_snap = self.revision(&base)?;
        let mut warnings = Vec::new();
        let (fp, index) = footprint_and_index(
            &self.repo,
            &base,
            &snap_commit,
            &base_snap.files,
            &base_snap.index,
            &mut warnings,
        )?;
        let id = AgentId::new(agent);
        let mut notices = self.dir.observe_footprint(&id, &fp.changes, fp.uses.clone());
        notices.extend(self.dir.publish_all(&id));
        let summary: Vec<Value> = fp
            .changes
            .iter()
            .map(|c| {
                json!({
                    "symbol": c.symbol,
                    "kind": match &c.kind {
                        ChangeKind::ContractChanged => if c.is_breaking() { "contract_breaking" } else { "contract_compatible" },
                        ChangeKind::BodyChanged => "body",
                        ChangeKind::Added => "added",
                        ChangeKind::Removed => "removed",
                        ChangeKind::Renamed { .. } => "renamed",
                    }
                })
            })
            .collect();
        self.log(
            "footprint",
            json!({ "agent": agent, "changes": summary, "uses": fp.uses.len(), "warnings": warnings }),
        );
        if let Some(a) = self.agents.get_mut(agent) {
            a.index = Some(index);
            a.stats.footprint_refreshes += 1;
            a.snapshot = Some(snap_commit);
        }
        self.route(notices);
        Ok(())
    }

    fn route(&mut self, notices: Vec<Notice>) {
        for n in notices {
            self.log(
                "notice",
                json!({ "to": n.to, "symbol": n.symbol, "kind": notice_kind(&n), "sites": n.sites.len() }),
            );
            if let Some(a) = self.agents.get_mut(&n.to.0) {
                a.outbox.push(n);
            }
        }
    }

    fn observe_read(&mut self, agent: &str, input: &Value) {
        let Some(path) = input.pointer("/tool_input/file_path").and_then(Value::as_str) else {
            return;
        };
        let Some(a) = self.agents.get(agent) else { return };
        let Some(rel) = relative(&a.worktree, Path::new(path)) else {
            return;
        };
        let offset = input
            .pointer("/tool_input/offset")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .max(1) as u32;
        let limit = input
            .pointer("/tool_input/limit")
            .and_then(Value::as_u64)
            .unwrap_or(100_000) as u32;
        let symbols = match &a.index {
            Some(ix) => ix.symbols_in_lines(&rel, offset, offset.saturating_add(limit)),
            None => match self.revisions.get(&a.base) {
                Some(s) => s.index.symbols_in_lines(&rel, offset, offset.saturating_add(limit)),
                None => vec![],
            },
        };
        if symbols.is_empty() {
            return;
        }
        let n = symbols.len();
        let notices = self.dir.observe_reads(&AgentId::new(agent), symbols);
        self.log("read", json!({ "agent": agent, "path": rel, "symbols": n }));
        self.route(notices);
    }

    /// Problems of the agent's call sites of `symbol` against its latest
    /// published contract (empty = conforms).
    fn conformance(&self, agent: &str, symbol: &SymbolId) -> Vec<String> {
        let id = AgentId::new(agent);
        let uses = self.dir.uses_of(&id, symbol);
        if uses.is_empty() {
            return vec![];
        }
        let Some(published) = self.dir.published_contract(symbol) else {
            return vec![];
        };
        let Some(contract) = published else {
            return uses
                .iter()
                .map(|u| format!("{}:{}  {}  -> `{}` was removed", u.path, u.line, u.text, symbol))
                .collect();
        };
        let Some(sig) = &contract.signature else {
            return vec![];
        };
        let a = &self.agents[agent];
        let kind = a.index.as_ref().and_then(|ix| ix.symbol(symbol)).map(|s| s.kind);
        let is_static = sig.decorators.iter().any(|d| d.contains("staticmethod"));
        let bound = kind == Some(SymbolKind::Method) && !is_static;
        let mut out = Vec::new();
        for u in &uses {
            if let Some(call) = &u.call {
                for p in check_call(sig, call, bound) {
                    out.push(format!("{}:{}  {}  -> {p}", u.path, u.line, u.text));
                }
            }
        }
        out
    }

    /// Acknowledge every stale symbol whose call sites now conform. Returns the
    /// remaining problems per symbol.
    fn auto_ack(&mut self, agent: &str) -> BTreeMap<SymbolId, Vec<String>> {
        let id = AgentId::new(agent);
        let mut remaining = BTreeMap::new();
        for s in self.dir.stale_symbols(&id) {
            // The producer already merged this agent's work, so its contract is
            // built on ours: adapting is the producer's job, not ours. (Resuming
            // the agent here would make it diverge from what the producer merged.)
            let subsumed = self
                .dir
                .published_by(&s)
                .and_then(|p| self.agents.get(&p.0))
                .is_some_and(|p| p.merged_producers.contains(agent));
            if subsumed {
                self.dir.acknowledge(&id, &s);
                self.log("ack", json!({ "agent": agent, "symbol": s, "subsumed": true }));
                continue;
            }
            let problems = self.conformance(agent, &s);
            let producer_merged = self
                .dir
                .published_by(&s)
                .map(|p| self.agents[agent].merged_producers.contains(&p.0))
                .unwrap_or(true);
            if problems.is_empty() && producer_merged {
                self.dir.acknowledge(&id, &s);
                self.log("ack", json!({ "agent": agent, "symbol": s }));
            } else {
                remaining.insert(s, problems);
            }
        }
        remaining
    }

    fn take_outbox(&mut self, agent: &str) -> Vec<Notice> {
        self.agents
            .get_mut(agent)
            .map(|a| std::mem::take(&mut a.outbox))
            .unwrap_or_default()
    }

    /// Text injected after a tool result.
    fn render_outbox(&mut self, agent: &str) -> Option<String> {
        let notices = self.take_outbox(agent);
        if notices.is_empty() {
            return None;
        }
        if let Some(a) = self.agents.get_mut(agent) {
            a.stats.notices_delivered += notices.len() as u32;
        }
        let mut text = String::new();
        for n in &notices {
            text.push_str(&n.render());
            if matches!(n.kind, NoticeKind::ContractChanged { breaking: true, .. }) {
                text.push_str(
                    "  note  : this change is in the other agent's branch, not yet in your worktree. Concord merges it \
                     into your worktree before you finish, so you can then verify it in the code.\n  \
                     action: update your own call sites to the new contract; do not edit the definition itself \
                     (it belongs to the other agent, and editing it would conflict with their change).\n",
                );
            }
        }
        Some(text)
    }

    pub fn needs_resume(&mut self, agent: &str) -> Option<String> {
        if !self.contracts {
            return None;
        }
        let remaining = self.auto_ack(agent);
        if remaining.is_empty() {
            return None;
        }
        let mut msg = String::from(
            "[concord] After you finished, another agent changed a contract that your code depends on. \
             Concord will merge that change into your worktree when you finish this turn and check your call sites. \
             Review your work against the change below, update your code and tests if needed, run the test suite, then finish.\n",
        );
        if let Some(t) = self.render_outbox(agent) {
            msg.push_str(&t);
        }
        Some(msg)
    }
}

/// Handle one hook event. Returns (stdout, exit_code) for the hook process.
pub fn handle(shared: &Shared, agent: &str, event: &str, input: &Value) -> (String, i32) {
    match event {
        "PostToolUse" => post_tool_use(shared, agent, input),
        "Stop" => stop(shared, agent, input),
        _ => (String::new(), 0),
    }
}

fn post_tool_use(shared: &Shared, agent: &str, input: &Value) -> (String, i32) {
    let mut st = shared.lock().expect("state");
    if !st.agents.contains_key(agent) {
        return (String::new(), 0);
    }
    if let Some(sid) = input.get("session_id").and_then(Value::as_str)
        && let Some(a) = st.agents.get_mut(agent)
    {
        a.session_id = Some(sid.to_owned());
        a.stats.tool_events += 1;
        a.status = Status::Running;
    }
    let tool = input.get("tool_name").and_then(Value::as_str).unwrap_or("");
    match tool {
        "Read" => st.observe_read(agent, input),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "Bash" => {
            if let Err(e) = st.refresh(agent) {
                st.log("error", json!({ "agent": agent, "error": e.to_string() }));
            } else if st.regions && st.mode == Mode::Enforce {
                crate::regions::after_edit(&mut st, agent);
            }
        }
        _ => {}
    }
    if st.mode != Mode::Enforce {
        return (String::new(), 0);
    }
    let mut parts: Vec<String> = Vec::new();
    if st.contracts
        && let Some(t) = st.render_outbox(agent)
    {
        parts.push(t);
    }
    if st.regions
        && let Some(a) = st.agents.get_mut(agent)
    {
        parts.append(&mut a.region_notes);
    }
    match (!parts.is_empty()).then(|| parts.join("\n")) {
        Some(text) => {
            st.log("deliver", json!({ "agent": agent, "via": "additionalContext" }));
            let out = json!({
                "hookSpecificOutput": { "hookEventName": "PostToolUse", "additionalContext": text }
            });
            (out.to_string(), 0)
        }
        None => (String::new(), 0),
    }
}

fn finish(st: &mut State, agent: &str) {
    if let Some(a) = st.agents.get_mut(agent) {
        a.status = Status::Finished;
    }
    st.dir.release(&AgentId::new(agent));
    st.log("finished", json!({ "agent": agent }));
}

fn stop(shared: &Shared, agent: &str, input: &Value) -> (String, i32) {
    let deadline = {
        let mut st = shared.lock().expect("state");
        if !st.agents.contains_key(agent) {
            return (String::new(), 0);
        }
        if let Some(sid) = input.get("session_id").and_then(Value::as_str)
            && let Some(a) = st.agents.get_mut(agent)
        {
            a.session_id = Some(sid.to_owned());
        }
        if let Err(e) = st.refresh(agent) {
            st.log("error", json!({ "agent": agent, "error": e.to_string() }));
        }
        if st.mode != Mode::Enforce {
            let remaining = st.auto_ack(agent);
            st.log(
                "stop",
                json!({ "agent": agent, "would_block": !remaining.is_empty(), "stale": remaining.keys().collect::<Vec<_>>() }),
            );
            finish(&mut st, agent);
            return (String::new(), 0);
        }
        Instant::now() + st.max_wait
    };

    loop {
        let mut st = shared.lock().expect("state");
        // Re-established below only if this agent keeps waiting.
        st.waiting.remove(agent);
        // Liveness: Concord never holds an agent forever. After MAX_GATE_BLOCKS
        // blocks the agent may finish; the integration check reports the rest.
        if st
            .agents
            .get(agent)
            .is_some_and(|a| a.stats.gate_blocks >= MAX_GATE_BLOCKS)
        {
            st.log(
                "stop",
                json!({ "agent": agent, "decision": "allow", "reason": "block_limit" }),
            );
            finish(&mut st, agent);
            return (String::new(), 0);
        }
        // A merge with conflict markers is in progress: finish it first.
        if st.peer_merge
            && st.agents.get(agent).is_some_and(|a| a.pending_peer.is_some())
            && let Some(reason) = integrate_peers(&mut st, agent)
        {
            if let Some(a) = st.agents.get_mut(agent) {
                a.stats.gate_blocks += 1;
            }
            return (json!({ "decision": "block", "reason": reason }).to_string(), 0);
        }
        let (remaining, gate_open) = if st.contracts {
            let remaining = st.auto_ack(agent);
            let open = st.dir.finish_gate(&AgentId::new(agent)).is_open();
            (remaining, open)
        } else {
            (BTreeMap::new(), true)
        };
        if remaining.is_empty() && gate_open {
            if st.regions {
                match crate::regions::gate(&mut st, agent) {
                    crate::regions::Gate::Clear => {}
                    crate::regions::Gate::Wait(owners, text) => {
                        // Never wait for an owner that is itself waiting for us.
                        let waitable: BTreeSet<String> =
                            owners.iter().filter(|o| !st.waits_for(o, agent)).cloned().collect();
                        if !waitable.is_empty() && Instant::now() < deadline {
                            st.waiting.insert(agent.to_owned(), waitable);
                            st.log("wait", json!({ "agent": agent, "for": owners, "reason": "region" }));
                            drop(st);
                            std::thread::sleep(Duration::from_secs(2));
                            continue;
                        }
                        if let Some(a) = st.agents.get_mut(agent) {
                            a.stats.gate_blocks += 1;
                        }
                        st.log(
                            "stop",
                            json!({ "agent": agent, "decision": "block", "reason": "region_owner_running", "owners": owners }),
                        );
                        return (json!({ "decision": "block", "reason": text }).to_string(), 0);
                    }
                    crate::regions::Gate::Block(text) => {
                        if let Some(a) = st.agents.get_mut(agent) {
                            a.stats.gate_blocks += 1;
                        }
                        return (json!({ "decision": "block", "reason": text }).to_string(), 0);
                    }
                }
            }
            if st.peer_merge
                && let Some(reason) = integrate_peers(&mut st, agent)
            {
                if let Some(a) = st.agents.get_mut(agent) {
                    a.stats.gate_blocks += 1;
                }
                return (json!({ "decision": "block", "reason": reason }).to_string(), 0);
            }
            st.log("stop", json!({ "agent": agent, "decision": "allow" }));
            finish(&mut st, agent);
            return (String::new(), 0);
        }

        // Producers of the contracts this agent is stale on.
        let producers: BTreeSet<String> = remaining
            .keys()
            .filter_map(|s| st.dir.published_by(s).map(|p| p.0.clone()))
            .filter(|p| p != agent)
            .collect();
        let unmerged: Vec<String> = producers
            .iter()
            .filter(|p| !st.agents[agent].merged_producers.contains(*p))
            .cloned()
            .collect();
        let running: Vec<String> = unmerged
            .iter()
            .filter(|p| st.agents.get(*p).is_some_and(|a| a.status == Status::Running))
            .cloned()
            .collect();

        if !running.is_empty() {
            // Never wait for a producer that is itself waiting for us: adapt to
            // the contract as published instead (below).
            let waitable: BTreeSet<String> = running.iter().filter(|p| !st.waits_for(p, agent)).cloned().collect();
            if !waitable.is_empty() && Instant::now() < deadline {
                st.log("wait", json!({ "agent": agent, "for": running }));
                st.waiting.insert(agent.to_owned(), waitable);
                drop(st);
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            let mut reason = format!(
                "[concord] {} changed contracts your code depends on and is still working, so Concord cannot merge \
                 their code yet. Do not wait, poll or schedule anything for them: adapt your own call sites to the \
                 contracts as published now, run the tests, and finish. Concord merges their code later.\n",
                running.join(", ")
            );
            for (s, problems) in &remaining {
                let now = st
                    .dir
                    .published_contract(s)
                    .map(|c| c.map(|c| c.display.clone()).unwrap_or_else(|| "(removed)".into()))
                    .unwrap_or_default();
                reason.push_str(&format!("  {s} is now: {now}\n"));
                for p in problems {
                    reason.push_str(&format!("    {p}\n"));
                }
            }
            if let Some(a) = st.agents.get_mut(agent) {
                a.stats.gate_blocks += 1;
            }
            st.log(
                "stop",
                json!({ "agent": agent, "decision": "block", "reason": "producer_running" }),
            );
            return (json!({ "decision": "block", "reason": reason }).to_string(), 0);
        }

        // Merge finished producers into the agent's worktree.
        let mut merge_notes = Vec::new();
        for p in &unmerged {
            let (wt, base) = {
                let a = &st.agents[agent];
                (a.worktree.clone(), a.base.clone())
            };
            let Some((p_branch, p_wt)) = st.agents.get(p).map(|x| (x.branch.clone(), x.worktree.clone())) else {
                continue;
            };
            // The producer has finished, so its worktree is quiescent: commit its
            // work so the merge actually carries the change.
            if let Err(e) = Repo::commit_all(&p_wt, &format!("agent {p}: work committed by concord for merging")) {
                st.log("error", json!({ "agent": p, "error": e.to_string() }));
            }
            let p_head = match st.repo.rev_parse(&p_branch) {
                Ok(h) => h,
                Err(e) => {
                    st.log("error", json!({ "agent": agent, "error": e.to_string() }));
                    continue;
                }
            };
            if st.peer_merge {
                // With peer merge on, a conflicting producer merge is handed to the
                // agent with conflict markers instead of being aborted.
                let result = Repo::commit_all(&wt, &format!("concord: checkpoint of {agent} before merging {p}"))
                    .and_then(|_| {
                        Repo::merge_keep_conflicts(&wt, &p_head, &format!("concord: merge {p} into {agent}"))
                    });
                match result {
                    Ok(out) if !out.clean => {
                        if let Some(a) = st.agents.get_mut(agent) {
                            a.merged_producers.insert(p.clone());
                            a.stats.merges.push(p.clone());
                            a.stats.gate_blocks += 1;
                            a.pending_conflicts = out.conflicted_files.clone();
                            a.pending_peer = Some((p.clone(), p_head.clone()));
                        }
                        st.log(
                            "merge",
                            json!({ "agent": agent, "producer": p, "clean": false, "files": out.conflicted_files, "kept_markers": true }),
                        );
                        let reason = format!(
                            "[concord] Agent `{p}` changed a contract your code depends on and has finished. Concord \
                             has merged their work into your worktree; it conflicts with yours in: {}. These files now \
                             contain git conflict markers (<<<<<<<, =======, >>>>>>>). Edit them so that both your \
                             feature and theirs are kept and work together (remove all markers), adapt your code to \
                             their new contracts, run the tests, then finish. Do not discard their changes.",
                            out.conflicted_files.join(", ")
                        );
                        return (json!({ "decision": "block", "reason": reason }).to_string(), 0);
                    }
                    Ok(_) => {
                        let new_base = st
                            .repo
                            .virtual_merge(&base, &p_head)
                            .ok()
                            .flatten()
                            .unwrap_or(p_head.clone());
                        if let Some(a) = st.agents.get_mut(agent) {
                            a.base = new_base;
                            a.merged_producers.insert(p.clone());
                            a.stats.merges.push(p.clone());
                        }
                        merge_notes.push(format!(
                            "Concord merged agent `{p}`'s finished work into your worktree (commit {}).",
                            &p_head[..p_head.len().min(10)]
                        ));
                        st.log("merge", json!({ "agent": agent, "producer": p, "clean": true }));
                    }
                    Err(e) => st.log("error", json!({ "agent": agent, "error": e.to_string() })),
                }
                continue;
            }
            let result = Repo::commit_all(&wt, &format!("concord: checkpoint of {agent} before merging {p}"))
                .and_then(|_| Repo::merge_into(&wt, &p_head, &format!("concord: merge {p} into {agent}")));
            match result {
                Ok(m) if m.clean => {
                    let new_base = st
                        .repo
                        .virtual_merge(&base, &p_head)
                        .ok()
                        .flatten()
                        .unwrap_or(p_head.clone());
                    if let Some(a) = st.agents.get_mut(agent) {
                        a.base = new_base;
                        a.merged_producers.insert(p.clone());
                        a.stats.merges.push(p.clone());
                    }
                    merge_notes.push(format!(
                        "Concord merged agent `{p}`'s finished work into your worktree (commit {}).",
                        &p_head[..p_head.len().min(10)]
                    ));
                    st.log("merge", json!({ "agent": agent, "producer": p, "clean": true }));
                }
                Ok(m) => {
                    if let Some(a) = st.agents.get_mut(agent) {
                        a.merged_producers.insert(p.clone());
                        a.stats.merge_conflicts.push(p.clone());
                    }
                    merge_notes.push(format!(
                        "Concord could not merge agent `{p}`'s work automatically (conflicts in {}). Adapt your code \
                         to the new contract described below by hand.",
                        m.conflicted_files.join(", ")
                    ));
                    st.log(
                        "merge",
                        json!({ "agent": agent, "producer": p, "clean": false, "files": m.conflicted_files }),
                    );
                }
                Err(e) => st.log("error", json!({ "agent": agent, "error": e.to_string() })),
            }
        }
        if !merge_notes.is_empty() {
            if let Err(e) = st.refresh(agent) {
                st.log("error", json!({ "agent": agent, "error": e.to_string() }));
            }
            let remaining = st.auto_ack(agent);
            let gate = st.dir.finish_gate(&AgentId::new(agent));
            if remaining.is_empty() && gate.is_open() {
                // The merge alone resolved it, but the agent has not seen the merged code yet.
                let reason = format!(
                    "[concord] {} Your code already conforms to the changed contracts. Run the test suite once \
                     more on the merged code, then finish.",
                    merge_notes.join(" ")
                );
                if let Some(a) = st.agents.get_mut(agent) {
                    a.stats.gate_blocks += 1;
                }
                st.log(
                    "stop",
                    json!({ "agent": agent, "decision": "block", "reason": "verify_after_merge" }),
                );
                return (json!({ "decision": "block", "reason": reason }).to_string(), 0);
            }
        }

        // Still non-conforming: block with precise, verifiable instructions.
        let remaining = st.auto_ack(agent);
        let mut reason = String::from("[concord] ");
        if !merge_notes.is_empty() {
            reason.push_str(&merge_notes.join(" "));
            reason.push(' ');
        }
        if remaining.is_empty() {
            // Gated only by contention: say so concretely.
            let gate = st.dir.finish_gate(&AgentId::new(agent));
            reason.push_str("You are changing the contract of symbols another agent is also changing:\n");
            for b in &gate.blockers {
                if let concord_core::Blocker::Contended { symbol, owner } = b {
                    let who = owner.as_ref().map(|o| o.0.as_str()).unwrap_or("another agent");
                    reason.push_str(&format!("  {symbol} (lease held by {who})\n"));
                }
            }
            reason.push_str("Keep its existing signature working (add new parameters as optional), then finish again.");
            if let Some(a) = st.agents.get_mut(agent) {
                a.stats.gate_blocks += 1;
            }
            st.log(
                "stop",
                json!({ "agent": agent, "decision": "block", "reason": "contended" }),
            );
            return (json!({ "decision": "block", "reason": reason }).to_string(), 0);
        }
        reason.push_str(
            "Another agent changed a contract your code depends on, and your code does not conform to it yet.\n",
        );
        for (s, problems) in &remaining {
            let now = st
                .dir
                .published_contract(s)
                .map(|c| c.map(|c| c.display.clone()).unwrap_or_else(|| "(removed)".into()))
                .unwrap_or_default();
            reason.push_str(&format!("  {s} is now: {now}\n"));
            for p in problems {
                reason.push_str(&format!("    {p}\n"));
            }
        }
        reason.push_str("Update these call sites (and any tests that use them), run the test suite, then finish.");
        if let Some(a) = st.agents.get_mut(agent) {
            a.stats.gate_blocks += 1;
        }
        let _ = st.take_outbox(agent);
        st.log("stop", json!({ "agent": agent, "decision": "block", "reason": "non_conforming", "symbols": remaining.keys().collect::<Vec<_>>() }));
        return (json!({ "decision": "block", "reason": reason }).to_string(), 0);
    }
}

fn has_markers(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .map(|t| {
            t.lines()
                .any(|l| l.starts_with("<<<<<<< ") || l.starts_with(">>>>>>> ") || l == "=======")
        })
        .unwrap_or(false)
}

/// Peer integration step of the finish gate. Returns a block reason, or None
/// if the agent may finish.
fn integrate_peers(st: &mut State, agent: &str) -> Option<String> {
    let wt = st.agents[agent].worktree.clone();

    // 1. A peer merge is in progress: wait until all markers are gone, then commit it.
    if let Some((peer, head)) = st.agents[agent].pending_peer.clone() {
        let unresolved: Vec<String> = st.agents[agent]
            .pending_conflicts
            .iter()
            .filter(|f| has_markers(&wt.join(f)))
            .cloned()
            .collect();
        if !unresolved.is_empty() {
            st.log(
                "stop",
                json!({ "agent": agent, "decision": "block", "reason": "unresolved_conflicts" }),
            );
            return Some(format!(
                "[concord] The merge of agent `{peer}`'s work still has unresolved conflict markers in: {}. \
                 Resolve them so that both your feature and theirs work, run the tests, then finish.",
                unresolved.join(", ")
            ));
        }
        if let Err(e) = Repo::commit_all(&wt, &format!("concord: merge {peer} into {agent} (conflicts resolved)")) {
            st.log("error", json!({ "agent": agent, "error": e.to_string() }));
        }
        let base = st.agents[agent].base.clone();
        let new_base = st.repo.virtual_merge(&base, &head).ok().flatten().unwrap_or(head);
        if let Some(a) = st.agents.get_mut(agent) {
            a.base = new_base;
            a.pending_conflicts.clear();
            a.pending_peer = None;
        }
        st.log("peer_merge_resolved", json!({ "agent": agent, "peer": peer }));
        let _ = st.refresh(agent);
    }

    // 2. Find a finished peer whose work conflicts textually with ours.
    let me = match Repo::snapshot_worktree(&wt) {
        Ok(c) => c,
        Err(e) => {
            st.log("error", json!({ "agent": agent, "error": e.to_string() }));
            return None;
        }
    };
    let peers: Vec<(String, PathBuf, String)> = st
        .agents
        .values()
        .filter(|p| p.name != agent && p.status == Status::Finished)
        .filter(|p| !st.agents[agent].merged_producers.contains(&p.name))
        .map(|p| (p.name.clone(), p.worktree.clone(), p.branch.clone()))
        .collect();
    for (peer, p_wt, p_branch) in peers {
        let _ = Repo::commit_all(&p_wt, &format!("agent {peer}: work committed by concord for merging"));
        let Ok(head) = st.repo.rev_parse(&p_branch) else {
            continue;
        };
        let Ok(m) = st.repo.merge_tree(&me, &head) else {
            continue;
        };
        if m.clean {
            continue;
        }
        let _ = Repo::commit_all(&wt, &format!("concord: checkpoint of {agent} before merging {peer}"));
        match Repo::merge_keep_conflicts(&wt, &head, &format!("concord: merge {peer} into {agent}")) {
            Ok(out) => {
                if let Some(a) = st.agents.get_mut(agent) {
                    a.merged_producers.insert(peer.clone());
                    a.stats.merges.push(peer.clone());
                }
                st.log(
                    "peer_merge",
                    json!({ "agent": agent, "peer": peer, "clean": out.clean, "files": out.conflicted_files }),
                );
                if out.clean {
                    // merge-tree saw a conflict that git merge resolved (e.g. rename detection).
                    let base = st.agents[agent].base.clone();
                    let new_base = st.repo.virtual_merge(&base, &head).ok().flatten().unwrap_or(head);
                    if let Some(a) = st.agents.get_mut(agent) {
                        a.base = new_base;
                    }
                    let _ = st.refresh(agent);
                    return Some(format!(
                        "[concord] Concord merged agent `{peer}`'s finished work into your worktree. Run the \
                         tests on the merged code and fix anything that broke, then finish."
                    ));
                }
                if let Some(a) = st.agents.get_mut(agent) {
                    a.pending_conflicts = out.conflicted_files.clone();
                    a.pending_peer = Some((peer.clone(), head));
                }
                return Some(format!(
                    "[concord] Agent `{peer}` finished work that conflicts with yours in: {}. Concord has merged \
                     their work into your worktree; the files now contain git conflict markers (<<<<<<<, =======, \
                     >>>>>>>). Edit these files so that both your feature and theirs are kept and work together \
                     (remove all markers), run the tests, then finish. Do not discard their changes.",
                    out.conflicted_files.join(", ")
                ));
            }
            Err(e) => st.log("error", json!({ "agent": agent, "error": e.to_string() })),
        }
    }
    None
}

/// Serve hook requests on `listener` until the process exits.
pub fn serve(listener: TcpListener, shared: Shared) {
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let shared = shared.clone();
        std::thread::spawn(move || {
            let _ = serve_one(stream, &shared);
        });
    }
}

fn serve_one(stream: TcpStream, shared: &Shared) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let req: Value = serde_json::from_str(&line)?;
    let agent = req.get("agent").and_then(Value::as_str).unwrap_or("");
    let event = req.get("event").and_then(Value::as_str).unwrap_or("");
    let input = req.get("input").cloned().unwrap_or(Value::Null);
    let (stdout, code) = handle(shared, agent, event, &input);
    let mut w = stream;
    writeln!(w, "{}", json!({ "stdout": stdout, "exit_code": code }))?;
    Ok(())
}

/// The `concord hook` client: forward stdin to the daemon and print its answer.
/// Fails open: if the daemon is unreachable the agent is never disturbed.
pub fn hook_client(addr: &str, agent: &str, event: &str) -> i32 {
    let mut input = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut input);
    let input: Value = serde_json::from_str(&input).unwrap_or(Value::Null);
    let Ok(mut stream) = TcpStream::connect(addr) else {
        return 0;
    };
    let req = json!({ "agent": agent, "event": event, "input": input });
    if writeln!(stream, "{req}").is_err() {
        return 0;
    }
    let mut line = String::new();
    if BufReader::new(stream).read_line(&mut line).is_err() {
        return 0;
    }
    let resp: Value = serde_json::from_str(&line).unwrap_or(Value::Null);
    if let Some(out) = resp.get("stdout").and_then(Value::as_str)
        && !out.is_empty()
    {
        println!("{out}");
    }
    resp.get("exit_code").and_then(Value::as_i64).unwrap_or(0) as i32
}

fn relative(root: &Path, path: &Path) -> Option<String> {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let (root, path) = (canon(root), canon(path));
    path.strip_prefix(&root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"))
}

fn notice_kind(n: &Notice) -> &'static str {
    match &n.kind {
        NoticeKind::HeadsUp { .. } => "heads_up",
        NoticeKind::ContractChanged { breaking: true, .. } => "contract_breaking",
        NoticeKind::ContractChanged { .. } => "contract_compatible",
        NoticeKind::LeaseHeld { .. } => "lease_held",
    }
}

fn first_line(s: &str, max: usize) -> String {
    let l = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    l.chars().take(max).collect()
}

pub fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
