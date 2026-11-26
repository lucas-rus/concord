//! The live coordination daemon.
//!
//! Agents (Claude Code or OpenCode sessions) report every tool call through
//! hooks (`concord hook`), which forward the hook input to this daemon over a
//! local TCP connection. The daemon keeps each agent's symbol footprint up to
//! date, runs the coherence protocol, delivers notices through
//! `additionalContext`, and enforces the finish gate in the `Stop` hook.
//!
//! The finish gate makes notices *verifiable*: before an agent may finish
//! while depending on a contract another agent changed, Concord waits for that
//! producer to finish, merges the producer's branch into the agent's worktree
//! (so the agent can see the new code), and checks every call site of the
//! changed symbol against the new signature.

use crate::{footprint_and_index, index_revision};
use anyhow::{Context, Result};
use concord_core::{AgentId, ChangeKind, Digest, Directory, Notice, NoticeKind, SymbolId, SymbolKind, check_call};
use concord_git::{FileStatus, Repo};
use concord_lang::{FileIndex, RepoIndex};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The agent's process ended without passing the finish gate (out of
    /// turns, crashed). Nobody waits for it; its work is handed over like a
    /// finished agent's.
    Exited,
}

impl Status {
    /// Finished, or ended without finishing: its work will not change.
    pub fn done(self) -> bool {
        matches!(self, Status::Finished | Status::Exited)
    }
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
    /// Times the ticket's acceptance check sent the agent back.
    pub check_blocks: u32,
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
    /// Exact peer commits merged into this agent's worktree, used to filter out
    /// peer-authored changes even when multi-peer virtual merges had conflicts.
    pub merged_revs: BTreeMap<String, String>,
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
    /// File-level or structural notices waiting to be delivered.
    pub file_notes: Vec<String>,
    /// Files currently modified or read by this agent.
    pub touched_files: BTreeSet<String>,
    /// Timestamp of earliest unpublished contract edit (for publish debounce).
    pub unpublished_since: Option<Instant>,
    /// The ticket's acceptance check (shell command), if any.
    pub check: Option<String>,
    /// Failures of the last acceptance check that sent the agent back.
    pub check_failures: Option<usize>,
    /// Baseline outcome `(passed_alone, failing_ids)` of this agent's `check`
    /// recorded when it finishes, used to protect finished peers from regressions.
    pub check_baseline: Option<(bool, BTreeSet<String>)>,
    /// Times `peer_check_gate` sent this agent back for regressing a finished peer.
    pub peer_check_blocks: u32,
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
    /// Baseline B3: exclusive whole-file leases (first writer locks the file).
    pub file_leases: bool,
    /// Baseline B4: Clash/Grove passive textual overlap warnings (no ownership/gate).
    pub overlap_warnings: bool,
    /// Ablation A1: treat every body modification as a contract change.
    pub all_contracts: bool,
    /// Ablation A2 (`false` = `--no-notices`): suppress live PostToolUse notices.
    pub deliver_notices: bool,
    /// Ablation A4 (`false` = `--no-gate`): skip finish-gate blocking on Stop.
    pub enforce_gate: bool,
    /// Debounce window before publishing contract changes (default 0 = immediate).
    pub publish_debounce: Duration,
    /// Region notices already sent: (yielder, owner, file).
    pub(crate) region_alerted: BTreeSet<(String, String, String)>,
    /// Pairs (yielder, owner) with a region yielded at some point: the owner's
    /// finished work is handed over to the yielder.
    pub(crate) region_yields: BTreeSet<(String, String)>,
    /// Waits-for graph of the finish gate, across the contract and region
    /// layers: agent -> agents it is currently waiting for. A wait that would
    /// close a cycle is never entered (no cross-layer deadlock).
    pub waiting: BTreeMap<String, BTreeSet<String>>,
    /// Active whole-file exclusive leases (`file -> owner`) for Baseline B3.
    pub(crate) file_owners: BTreeMap<String, String>,
    /// Passive overlap alerts already sent (`(agent, peer, file)`) for Baseline B4.
    pub(crate) overlap_alerted: BTreeSet<(String, String, String)>,
    /// Structural file alerts already sent (`(agent, peer, file)`).
    pub(crate) structural_alerted: BTreeSet<(String, String, String)>,
    /// Fails-together check at the finish gate: the test command, if enabled.
    pub together: Option<String>,
    /// Lean gate (enforce mode): silent hand-offs when the merge breaks none
    /// of the agent's acceptance-check tests, and at most one check block.
    pub lean: bool,
    /// Last fails-together verdict per agent, keyed by (snapshot, peers).
    together_cache: HashMap<String, (String, crate::together::Verdict)>,
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
            file_leases: false,
            overlap_warnings: false,
            all_contracts: false,
            deliver_notices: true,
            enforce_gate: true,
            publish_debounce: Duration::ZERO,
            region_alerted: BTreeSet::new(),
            region_yields: BTreeSet::new(),
            waiting: BTreeMap::new(),
            file_owners: BTreeMap::new(),
            overlap_alerted: BTreeSet::new(),
            structural_alerted: BTreeSet::new(),
            together: None,
            lean: false,
            together_cache: HashMap::new(),
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
                merged_revs: BTreeMap::new(),
                index: None,
                stats: AgentStats::default(),
                pending_conflicts: Vec::new(),
                pending_peer: None,
                rank,
                snapshot: None,
                region_notes: Vec::new(),
                file_notes: Vec::new(),
                touched_files: BTreeSet::new(),
                unpublished_since: None,
                check: None,
                check_failures: None,
                check_baseline: None,
                peer_check_blocks: 0,
            },
        );
    }

    /// Advance an agent's diff baseline after merging `peer_head` into its
    /// worktree. Uses Concord's verified insertion-union virtual merge so
    /// multi-peer bases compose cleanly, and never overwrites a multi-peer
    /// `current_base` with `peer_head` alone (which previously forgot earlier
    /// merged peers and triggered phantom contract notices).
    pub fn advance_base(&self, current_base: &str, peer_head: &str) -> String {
        if self.repo.is_ancestor(peer_head, current_base) {
            return current_base.to_owned();
        }
        if self.repo.is_ancestor(current_base, peer_head) {
            return peer_head.to_owned();
        }
        if let Ok(Some(m)) = self
            .repo
            .virtual_merge_union(current_base, peer_head, &crate::regions::verify_union)
        {
            return m;
        }
        if current_base == self.base {
            peer_head.to_owned()
        } else {
            current_base.to_owned()
        }
    }

    /// Whether `agent` may wait for `target` in the finish gate. Waits that
    /// would close a cycle are resolved by rank: the wait toward the lower rank
    /// (CoAgent's order, which is also the region owner) is kept, and the
    /// lower-ranked agent stops waiting for a higher-ranked one that waits for
    /// it. Every cycle has an edge toward a higher rank, so none survives.
    pub(crate) fn may_wait(&self, agent: &str, target: &str) -> bool {
        let rank = |a: &str| self.agents.get(a).map(|x| x.rank).unwrap_or(usize::MAX);
        !(rank(target) > rank(agent) && self.waits_for(target, agent))
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

    /// Flush any debounced contract publications for `agent`.
    pub fn flush_publish(&mut self, agent: &str) {
        let id = AgentId::new(agent);
        let notices = self.dir.publish_all(&id);
        if let Some(a) = self.agents.get_mut(agent) {
            a.unpublished_since = None;
        }
        self.route(notices);
    }

    /// Recompute an agent's footprint from its worktree, filter out changes
    /// inherited verbatim from merged peers, run the protocol, and route
    /// resulting notices.
    pub fn refresh(&mut self, agent: &str) -> Result<()> {
        self.refresh_inner(agent, false)
    }

    fn refresh_inner(&mut self, agent: &str, force_publish: bool) -> Result<()> {
        let (wt, base, merged_revs) = {
            let a = self.agents.get(agent).context("unknown agent")?;
            (a.worktree.clone(), a.base.clone(), a.merged_revs.clone())
        };
        let snap_commit = Repo::snapshot_worktree(&wt)?;
        let base_snap = self.revision(&base)?;
        let orig_snap = self.revision(&self.base.clone())?;
        let mut peer_snaps: Vec<Arc<Snapshot>> = Vec::new();
        for rev in merged_revs.values() {
            if let Ok(ps) = self.revision(rev) {
                peer_snaps.push(ps);
            }
        }
        let mut warnings = Vec::new();
        let (mut fp, index) = footprint_and_index(
            &self.repo,
            &base,
            &snap_commit,
            &base_snap.files,
            &base_snap.index,
            &mut warnings,
        )?;

        // Filter out symbol changes and use sites that came verbatim from a
        // merged peer (prevents phantom contract notices when multi-peer bases
        // could not be virtual-merged cleanly).
        if !peer_snaps.is_empty() {
            let mut filtered = Vec::with_capacity(fp.changes.len());
            for mut c in fp.changes {
                let cur = index.symbol(&c.symbol);
                let mut drop_change = false;
                for ps in &peer_snaps {
                    match (cur, ps.index.symbol(&c.symbol)) {
                        (Some(as_), Some(ps_)) => {
                            if as_.contract.digest == ps_.contract.digest && as_.body == ps_.body {
                                drop_change = true;
                                break;
                            }
                            if as_.contract.digest == ps_.contract.digest
                                && matches!(c.kind, ChangeKind::Added | ChangeKind::ContractChanged)
                            {
                                c.kind = ChangeKind::BodyChanged;
                                c.before = Some(ps_.contract.clone());
                            }
                        }
                        (None, None)
                            if c.kind == ChangeKind::Removed && orig_snap.index.symbol(&c.symbol).is_some() =>
                        {
                            drop_change = true;
                            break;
                        }
                        _ => {}
                    }
                }
                if !drop_change {
                    filtered.push(c);
                }
            }
            fp.changes = filtered;

            for sites in fp.uses.values_mut() {
                sites.retain(|u| {
                    let t = u.text.trim();
                    t.is_empty()
                        || !peer_snaps.iter().any(|ps| {
                            ps.files
                                .get(&u.path)
                                .is_some_and(|f| f.refs.iter().any(|r| r.text.trim() == t))
                        })
                });
            }
            fp.uses.retain(|_, v| !v.is_empty());
        }

        // Ablation A1 (`--all-contracts`): treat every body change as a
        // breaking contract modification (no contract vs. body separation).
        if self.all_contracts {
            for c in &mut fp.changes {
                if c.kind == ChangeKind::BodyChanged {
                    c.kind = ChangeKind::ContractChanged;
                    let body_hex = index.symbol(&c.symbol).map(|s| s.body.0.as_str()).unwrap_or("body");
                    if let Some(after) = &mut c.after {
                        after.digest = Digest::of(&format!("{}:{body_hex}", after.digest.0));
                        after.signature = None;
                    }
                }
            }
        }

        let id = AgentId::new(agent);
        let mut notices = Vec::new();
        if self.contracts {
            notices.extend(self.dir.observe_footprint(&id, &fp.changes, fp.uses.clone()));
            let has_contract_edit = fp.changes.iter().any(|c| c.is_contract_change());
            if self.publish_debounce.is_zero() || force_publish {
                notices.extend(self.dir.publish_all(&id));
                if let Some(a) = self.agents.get_mut(agent) {
                    a.unpublished_since = None;
                }
            } else if has_contract_edit {
                let should_flush = self
                    .agents
                    .get(agent)
                    .and_then(|a| a.unpublished_since)
                    .is_some_and(|t| t.elapsed() >= self.publish_debounce);
                if should_flush {
                    notices.extend(self.dir.publish_all(&id));
                    if let Some(a) = self.agents.get_mut(agent) {
                        a.unpublished_since = None;
                    }
                } else if let Some(a) = self.agents.get_mut(agent)
                    && a.unpublished_since.is_none()
                {
                    a.unpublished_since = Some(Instant::now());
                }
            }
        }

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
            a.snapshot = Some(snap_commit.clone());
            a.touched_files.extend(fp.changed_files.iter().cloned());
        }
        self.check_file_layers(agent, &base, &snap_commit, &fp.changed_files);
        self.route(notices);
        Ok(())
    }

    /// Structural file-existence checks, Baseline B3 (`--file-leases`), and
    /// Baseline B4 (`--overlap-warnings`).
    fn check_file_layers(&mut self, agent: &str, base: &str, snap_commit: &str, changed_files: &BTreeSet<String>) {
        // 1. Baseline B3: Exclusive whole-file leases.
        if self.file_leases {
            for f in changed_files {
                match self.file_owners.get(f).cloned() {
                    None => {
                        self.file_owners.insert(f.clone(), agent.to_owned());
                    }
                    Some(owner) if owner == agent => {}
                    Some(owner) => {
                        if self
                            .structural_alerted
                            .insert((agent.to_owned(), owner.clone(), f.clone()))
                            && let Some(a) = self.agents.get_mut(agent)
                        {
                            a.file_notes.push(format!(
                                "[concord] file lease held: `{f}` is currently locked by agent `{owner}`. \
                                 Leave `{f}` untouched until `{owner}` finishes, or Concord will hold you at the finish gate."
                            ));
                        }
                    }
                }
            }
        }

        // 2. Baseline B4: Clash/Grove passive textual overlap warnings.
        if self.overlap_warnings {
            let peers: Vec<(String, String)> = self
                .agents
                .values()
                .filter(|p| p.name != agent)
                .filter_map(|p| p.snapshot.clone().map(|s| (p.name.clone(), s)))
                .collect();
            for (peer, psnap) in peers {
                for fc in crate::regions::conflicts(&self.repo, snap_commit, &psnap) {
                    if self
                        .overlap_alerted
                        .insert((agent.to_owned(), peer.clone(), fc.file.clone()))
                        && let Some(a) = self.agents.get_mut(agent)
                    {
                        a.file_notes.push(format!(
                            "[concord] overlap warning: your changes in `{}` overlap textually with agent `{peer}`.",
                            fc.file
                        ));
                    }
                }
            }
        }

        // 3. Structural file-existence conflicts (deletes vs. reads/edits, duplicate non-code creations).
        if self.contracts
            && let Ok(statuses) = self.repo.changed_files(base, snap_commit)
        {
            for cf in statuses {
                if cf.status == FileStatus::Deleted {
                    let readers: Vec<String> = self
                        .agents
                        .values()
                        .filter(|p| p.name != agent && p.touched_files.contains(&cf.path))
                        .map(|p| p.name.clone())
                        .collect();
                    for peer in readers {
                        if self
                            .structural_alerted
                            .insert((peer.clone(), agent.to_owned(), cf.path.clone()))
                            && let Some(p) = self.agents.get_mut(&peer)
                        {
                            p.file_notes.push(format!(
                                "[concord] structural file conflict: agent `{agent}` deleted `{}` while you read or modified it.",
                                cf.path
                            ));
                        }
                    }
                } else if cf.status == FileStatus::Added && !concord_lang::is_supported_path(&cf.path) {
                    let creators: Vec<String> = self
                        .agents
                        .values()
                        .filter(|p| p.name != agent && p.touched_files.contains(&cf.path))
                        .map(|p| p.name.clone())
                        .collect();
                    for peer in creators {
                        if self
                            .structural_alerted
                            .insert((agent.to_owned(), peer.clone(), cf.path.clone()))
                            && let Some(a) = self.agents.get_mut(agent)
                        {
                            a.file_notes.push(format!(
                                "[concord] structural file conflict: both you and agent `{peer}` created `{}`.",
                                cf.path
                            ));
                        }
                    }
                }
            }
        }
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
        if let Some(a) = self.agents.get_mut(agent) {
            a.touched_files.insert(rel.clone());
        }
        if symbols.is_empty() || !self.contracts {
            return;
        }
        let n = symbols.len();
        let notices = self.dir.observe_reads(&AgentId::new(agent), symbols);
        self.log("read", json!({ "agent": agent, "path": rel, "symbols": n }));
        self.route(notices);
    }

    fn observe_grep(&mut self, agent: &str, input: &Value) {
        if !self.contracts {
            return;
        }
        let pattern = input
            .pointer("/tool_input/pattern")
            .and_then(Value::as_str)
            .unwrap_or("");
        let path_opt = input
            .pointer("/tool_input/file_path")
            .or_else(|| input.pointer("/tool_input/path"))
            .and_then(Value::as_str);
        let Some(a) = self.agents.get(agent) else { return };
        let rel_filter = path_opt.and_then(|p| relative(&a.worktree, Path::new(p)));

        // Extract a meaningful identifier token from the grep pattern (>= 3 chars).
        let ident: Option<String> = pattern
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .filter(|tok| tok.len() >= 3 && tok.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_'))
            .max_by_key(|tok| tok.len())
            .map(str::to_owned);

        let ix_opt = a
            .index
            .as_ref()
            .or_else(|| self.revisions.get(&a.base).map(|s| &s.index));
        let Some(ix) = ix_opt else { return };

        let mut matched: Vec<SymbolId> = Vec::new();
        for s in ix.symbols() {
            if let Some(ref rel) = rel_filter
                && !rel.is_empty()
                && s.path != *rel
                && !s.path.starts_with(&format!("{rel}/"))
            {
                continue;
            }
            if let Some(ref tok) = ident {
                if s.id.name().contains(tok.as_str()) {
                    matched.push(s.id.clone());
                }
            } else if rel_filter.as_deref() == Some(s.path.as_str()) {
                matched.push(s.id.clone());
            }
        }
        if matched.is_empty() || matched.len() > 32 {
            return;
        }
        let n = matched.len();
        let notices = self.dir.observe_reads(&AgentId::new(agent), matched);
        self.log("grep", json!({ "agent": agent, "pattern": pattern, "symbols": n }));
        self.route(notices);
    }

    /// Problems of the agent's call sites of `symbol` against its latest
    /// published contract (empty = conforms).
    fn conformance(&self, agent: &str, symbol: &SymbolId) -> Vec<String> {
        let id = AgentId::new(agent);
        let mut uses = self.dir.uses_of(&id, symbol);
        if uses.is_empty() {
            return vec![];
        }
        // Only call sites the agent wrote are its obligation. A use whose text
        // is already in the agent's base is pre-existing code: adapting it is
        // the job of the agent that changed the definition (as in any
        // refactoring), and blocking a bystander on it cannot make progress.
        let base = self.agents[agent].base.clone();
        let paths: Vec<String> = uses
            .iter()
            .map(|u| u.path.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if let Ok(files) = self.repo.read_files(&base, &paths) {
            uses.retain(|u| {
                let t = u.text.trim();
                t.is_empty() || !files.get(&u.path).is_some_and(|f| f.contains(t))
            });
        }
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
        if !self.contracts || !self.enforce_gate {
            return None;
        }
        let mut remaining = self.auto_ack(agent);
        // A finished agent whose own call sites already satisfy the published
        // contract has nothing to do: integration merges both sides.
        let id = AgentId::new(agent);
        let satisfied: Vec<SymbolId> = remaining
            .iter()
            .filter(|(_, problems)| problems.is_empty())
            .map(|(s, _)| s.clone())
            .collect();
        for s in satisfied {
            self.dir.acknowledge(&id, &s);
            self.log("ack", json!({ "agent": agent, "symbol": s, "finished": true }));
            remaining.remove(&s);
        }
        if remaining.is_empty() {
            let _ = self.take_outbox(agent);
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

/// Handle one hook or IPC event. Returns (stdout, exit_code) for the caller.
pub fn handle(shared: &Shared, agent: &str, event: &str, input: &Value) -> (String, i32) {
    match event {
        "PostToolUse" => post_tool_use(shared, agent, input),
        "Stop" => stop(shared, agent, input),
        "Negotiate" => negotiate(shared, agent, input),
        "Ack" => explicit_ack(shared, agent, input),
        "Status" => live_status(shared),
        _ => (String::new(), 0),
    }
}

fn negotiate(shared: &Shared, agent: &str, input: &Value) -> (String, i32) {
    let mut st = shared.lock().expect("state");
    let symbol = input.get("symbol").and_then(Value::as_str).unwrap_or("");
    let proposal = input.get("proposal").and_then(Value::as_str).unwrap_or("");
    if symbol.is_empty() || proposal.is_empty() {
        return (
            json!({ "ok": false, "error": "symbol and proposal are required" }).to_string(),
            1,
        );
    }
    let sym = SymbolId::new(symbol);
    match st.dir.negotiate(&AgentId::new(agent), &sym, proposal.to_owned()) {
        Some(notice) => {
            let owner = notice.to.0.clone();
            st.log(
                "negotiate",
                json!({ "from": agent, "to": owner, "symbol": sym, "proposal": proposal }),
            );
            st.route(vec![notice]);
            (json!({ "ok": true, "routed_to": owner, "symbol": sym }).to_string(), 0)
        }
        None => (
            json!({ "ok": false, "error": format!("no active peer lease or definition found for {symbol}") })
                .to_string(),
            0,
        ),
    }
}

fn explicit_ack(shared: &Shared, agent: &str, input: &Value) -> (String, i32) {
    let mut st = shared.lock().expect("state");
    let symbol = input.get("symbol").and_then(Value::as_str).unwrap_or("");
    if symbol.is_empty() {
        return (json!({ "ok": false, "error": "symbol is required" }).to_string(), 1);
    }
    let sym = SymbolId::new(symbol);
    st.dir.acknowledge(&AgentId::new(agent), &sym);
    st.log("ack", json!({ "agent": agent, "symbol": sym, "explicit": true }));
    (json!({ "ok": true, "acknowledged": sym }).to_string(), 0)
}

fn live_status(shared: &Shared) -> (String, i32) {
    let st = shared.lock().expect("state");
    let agents: Vec<Value> = st
        .agents
        .values()
        .map(|a| {
            let id = AgentId::new(&a.name);
            json!({
                "name": a.name,
                "status": a.status,
                "branch": a.branch,
                "stale_symbols": st.dir.stale_symbols(&id),
                "notices_delivered": a.stats.notices_delivered,
                "gate_blocks": a.stats.gate_blocks,
                "merges": a.stats.merges,
            })
        })
        .collect();
    (
        json!({
            "mode": st.mode,
            "agents": agents,
            "waiting": st.waiting,
            "file_owners": st.file_owners,
        })
        .to_string(),
        0,
    )
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
        "Grep" => st.observe_grep(agent, input),
        "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "Bash" => {
            // Flush debounced contract publishes immediately on Bash (e.g. test runs).
            let force_publish = tool == "Bash";
            if let Err(e) = st.refresh_inner(agent, force_publish) {
                st.log("error", json!({ "agent": agent, "error": e.to_string() }));
            } else if st.regions && st.mode == Mode::Enforce {
                crate::regions::after_edit(&mut st, agent);
            }
        }
        _ => {}
    }
    if st.mode != Mode::Enforce || !st.deliver_notices {
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
    if let Some(a) = st.agents.get_mut(agent)
        && !a.file_notes.is_empty()
    {
        let notes = std::mem::take(&mut a.file_notes);
        a.stats.notices_delivered += notes.len() as u32;
        parts.extend(notes);
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

/// Most times the acceptance check sends an agent back.
pub const MAX_CHECK_BLOCKS: u32 = 3;
/// Most times `peer_check_gate` sends an agent back for breaking a finished peer's check.
pub const MAX_PEER_CHECK_BLOCKS: u32 = 2;

/// Extract concise assertion failure diffs (`___`, `E `, `--- FAIL`, `AssertionError`,
/// `panicked at`) plus trailing summary lines so `pytest -q` summary tails never
/// scroll off the actual expected-vs-actual assertion diffs.
pub fn assertion_excerpt(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= max_lines {
        return lines.join("\n");
    }
    let mut picked: Vec<&str> = Vec::new();
    let detail_budget = max_lines.saturating_sub(10);
    for &l in &lines {
        if picked.len() >= detail_budget {
            break;
        }
        let t = l.trim_start();
        if (t.starts_with("___") && t.ends_with("___"))
            || t.starts_with("E   ")
            || t.starts_with("E       ")
            || t.starts_with("--- FAIL:")
            || t.starts_with("✖ ")
            || t.starts_with("× ")
            || t.starts_with("not ok ")
            || t.contains("error TS")
            || t.contains("AssertionError")
            || t.contains("panicked at")
        {
            picked.push(l);
        }
    }
    let tail_start = lines.len().saturating_sub(10);
    for &l in &lines[tail_start..] {
        if picked.last() != Some(&l) {
            picked.push(l);
        }
    }
    picked.into_iter().take(max_lines).collect::<Vec<_>>().join("\n")
}

/// What a lean hand-off needs from the agent.
enum LeanHandoff {
    /// The merge broke none of the agent's checked tests: no turn.
    Silent,
    /// The agent must look: the message to append to the hand-off text.
    Turn(String),
}

/// Lean hand-off: run the agent's acceptance check before and after the
/// merge. Silent when it passes, or when the merge broke nothing that passed
/// before and the agent has no yielded lines to rewrite.
fn lean_handoff<'a>(
    shared: &'a Shared,
    st: std::sync::MutexGuard<'a, State>,
    agent: &str,
    h: &crate::regions::Handoff,
) -> (std::sync::MutexGuard<'a, State>, LeanHandoff) {
    let Some(a) = st.agents.get(agent) else {
        return (st, LeanHandoff::Turn(String::new()));
    };
    let (Some(cmd), Some(before)) = (a.check.clone(), h.before.clone()) else {
        return (st, LeanHandoff::Turn(String::new()));
    };
    let (wt, repo) = (a.worktree.clone(), st.repo.clone());
    drop(st);
    let run = |dir: &Path| {
        std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(dir)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .map(|o| {
                let mut t = String::from_utf8_lossy(&o.stdout).into_owned();
                t.push_str(&String::from_utf8_lossy(&o.stderr));
                (o.status.success(), t)
            })
    };
    let after = run(&wt);
    let prior = crate::together::test_at(&repo, &before, &cmd);
    let mut st = shared.lock().expect("state");
    let (Ok((after_ok, after_text)), Ok((before_ok, before_text))) = (after, prior) else {
        return (st, LeanHandoff::Turn(String::new()));
    };
    let after_fail = crate::together::failing_tests(&after_text);
    let before_fail = crate::together::failing_tests(&before_text);
    // Tests that passed before the merge and fail after it.
    let broken: Vec<String> = if after_ok {
        Vec::new()
    } else if after_fail.is_empty() || (!before_ok && before_fail.is_empty()) {
        // Unparseable output: compare pass/fail of the whole check.
        if before_ok {
            vec![format!("`{cmd}`")]
        } else {
            Vec::new()
        }
    } else {
        after_fail.difference(&before_fail).cloned().collect()
    };
    let silent = after_ok || (broken.is_empty() && !h.yielded);
    if let Some(a) = st.agents.get_mut(agent) {
        a.check_baseline = Some((after_ok, after_fail.clone()));
        if silent {
            // The check result after the hand-off is the new reference; the
            // agent is not sent back for failures the merge did not cause.
            a.check_failures = Some(after_fail.len().max(usize::from(!after_ok)));
        } else {
            a.check_failures = None;
        }
    }
    st.log(
        "region_handoff_verified",
        json!({ "agent": agent, "silent": silent, "check_passed": after_ok, "broken": broken.len(), "yielded": h.yielded }),
    );
    if silent {
        return (st, LeanHandoff::Silent);
    }
    let mut extra = String::new();
    if !broken.is_empty() {
        extra.push_str("\nConcord ran your acceptance check: these tests passed before the merge and fail now:\n");
        for t in broken.iter().take(20) {
            extra.push_str(&format!("  {t}\n"));
        }
        extra.push_str(&format!(
            "Assertion details:\n{}\nFix only these; the merge is otherwise verified.",
            assertion_excerpt(&after_text, 30)
        ));
    }
    (st, LeanHandoff::Turn(extra))
}

/// The ticket's acceptance check at the finish gate.
fn check_gate<'a>(
    shared: &'a Shared,
    st: std::sync::MutexGuard<'a, State>,
    agent: &str,
) -> (std::sync::MutexGuard<'a, State>, Option<String>) {
    let Some(a) = st.agents.get(agent) else {
        return (st, None);
    };
    let Some(cmd) = a.check.clone() else { return (st, None) };
    let wt = a.worktree.clone();
    drop(st);
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .current_dir(&wt)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output();
    let mut st = shared.lock().expect("state");
    let (ok, text) = match out {
        Ok(o) => {
            let mut t = String::from_utf8_lossy(&o.stdout).into_owned();
            t.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), t)
        }
        Err(e) => {
            st.log(
                "error",
                json!({ "agent": agent, "error": format!("acceptance check: {e}") }),
            );
            return (st, None);
        }
    };
    if ok {
        if let Some(a) = st.agents.get_mut(agent) {
            a.check_baseline = Some((true, BTreeSet::new()));
        }
        st.log("check", json!({ "agent": agent, "passed": true }));
        return (st, None);
    }
    let failing = crate::together::failing_tests(&text);
    let failures = failing.len().max(1);
    let (blocks, prev) = st
        .agents
        .get(agent)
        .map(|a| (a.stats.check_blocks, a.check_failures))
        .unwrap_or((0, None));
    let progressing = prev.is_none_or(|p| failures < p);
    let max_blocks = if st.lean && st.mode == Mode::Enforce {
        1
    } else {
        MAX_CHECK_BLOCKS
    };
    if blocks >= max_blocks || !progressing {
        if let Some(a) = st.agents.get_mut(agent) {
            a.check_baseline = Some((false, failing));
        }
        st.log(
            "check",
            json!({ "agent": agent, "passed": false, "failures": failures, "decision": "allow", "reason": if progressing { "check_limit" } else { "no_progress" } }),
        );
        return (st, None);
    }
    if let Some(a) = st.agents.get_mut(agent) {
        a.stats.check_blocks += 1;
        a.check_failures = Some(failures);
    }
    st.log(
        "check",
        json!({ "agent": agent, "passed": false, "failures": failures, "decision": "block" }),
    );
    let mut reason = format!("[concord] Your ticket's acceptance check (`{cmd}`) still fails ({failures} failing):\n");
    for t in failing.iter().take(25) {
        reason.push_str(&format!("  {t}\n"));
    }
    if failing.len() > 25 {
        reason.push_str(&format!("  ... and {} more\n", failing.len() - 25));
    }
    reason.push_str(&format!(
        "Assertion details:\n{}\nFix your implementation so that these pass, run the check again, then finish. \
         If a failure depends on a feature that belongs to another ticket, leave it and finish.",
        assertion_excerpt(&text, 35)
    ));
    (st, Some(reason))
}

struct PeerSpec {
    name: String,
    head: String,
    cmd: String,
    passed_alone: bool,
    baseline_fails: BTreeSet<String>,
}

/// Cross-agent acceptance regression gate (enforce mode): ensures that `agent`
/// does not break tests that already passed in a finished peer's `check`. When
/// a regression is detected, Concord merges that finished peer into `agent`'s
/// worktree so `agent` can see both features side-by-side and resolve the
/// semantic interference immediately.
fn peer_check_gate<'a>(
    shared: &'a Shared,
    mut st: std::sync::MutexGuard<'a, State>,
    agent: &str,
) -> (std::sync::MutexGuard<'a, State>, Option<String>) {
    if st.mode != Mode::Enforce || !st.contracts {
        return (st, None);
    }
    let Some(a) = st.agents.get(agent) else {
        return (st, None);
    };
    if a.peer_check_blocks >= MAX_PEER_CHECK_BLOCKS {
        return (st, None);
    }
    let wt = a.worktree.clone();
    let Ok(me) = Repo::snapshot_worktree(&wt) else {
        return (st, None);
    };
    let order = crate::run::merge_order(&st);
    let candidates: Vec<(String, PathBuf, String, String, bool, BTreeSet<String>)> = order
        .into_iter()
        .filter_map(|n| st.agents.get(&n))
        .filter(|p| p.name != agent && p.status == Status::Finished)
        .filter_map(|p| {
            let cmd = p.check.clone()?;
            let (passed_alone, baseline) = p.check_baseline.clone()?;
            Some((
                p.name.clone(),
                p.worktree.clone(),
                p.branch.clone(),
                cmd,
                passed_alone,
                baseline,
            ))
        })
        .collect();
    if candidates.is_empty() {
        return (st, None);
    }
    let mut specs = Vec::new();
    for (name, p_wt, branch, cmd, passed_alone, baseline_fails) in candidates {
        let _ = Repo::commit_if_dirty(&p_wt, &format!("agent {name}: work committed by concord"));
        if let Ok(head) = st.repo.rev_parse(&branch) {
            specs.push(PeerSpec {
                name,
                head,
                cmd,
                passed_alone,
                baseline_fails,
            });
        }
    }
    if specs.is_empty() {
        return (st, None);
    }
    let repo = st.repo.clone();
    let base_commit = st.base.clone();
    drop(st);

    // Build virtual union of finished peers (`peers_only`) and `peers_only + me` (`with_me`).
    let mut peers_only = base_commit;
    for sp in &specs {
        if let Ok(Some(m)) = repo.virtual_merge_union(&peers_only, &sp.head, &crate::regions::verify_union) {
            peers_only = m;
        }
    }
    let Some(with_me) = repo
        .virtual_merge_union(&peers_only, &me, &crate::regions::verify_union)
        .ok()
        .flatten()
    else {
        return (shared.lock().expect("state"), None);
    };

    let tmp_dir = std::env::temp_dir().join(format!("concord-peercheck-{}-{}", std::process::id(), now_millis()));
    let tmp_s = tmp_dir.to_string_lossy().into_owned();
    if repo
        .git(&["worktree", "add", "--detach", "--quiet", &tmp_s, &with_me])
        .is_err()
    {
        return (shared.lock().expect("state"), None);
    }

    let mut culprit: Option<(String, String, String, Vec<String>, String)> = None;
    for sp in &specs {
        let Ok(out) = std::process::Command::new("sh")
            .arg("-c")
            .arg(&sp.cmd)
            .current_dir(&tmp_dir)
            .output()
        else {
            continue;
        };
        if out.status.success() {
            continue;
        }
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        let fails_merged = crate::together::failing_tests(&text);
        let mut regressed: BTreeSet<String> = if !fails_merged.is_empty() {
            fails_merged.difference(&sp.baseline_fails).cloned().collect()
        } else if sp.passed_alone {
            ["(acceptance check failed when merged with your changes)".to_owned()].into()
        } else {
            BTreeSet::new()
        };
        if regressed.is_empty() {
            continue;
        }

        // Confirm `peers_only` (without `me`) did not already fail these tests.
        let p_tmp = std::env::temp_dir().join(format!("concord-peerbase-{}-{}", std::process::id(), now_millis()));
        let p_tmp_s = p_tmp.to_string_lossy().into_owned();
        if repo
            .git(&["worktree", "add", "--detach", "--quiet", &p_tmp_s, &peers_only])
            .is_ok()
        {
            if let Ok(p_out) = std::process::Command::new("sh")
                .arg("-c")
                .arg(&sp.cmd)
                .current_dir(&p_tmp)
                .output()
                && !p_out.status.success()
            {
                let mut pt = String::from_utf8_lossy(&p_out.stdout).into_owned();
                pt.push_str(&String::from_utf8_lossy(&p_out.stderr));
                for pre in crate::together::failing_tests(&pt) {
                    regressed.remove(&pre);
                }
            }
            let _ = repo.git(&["worktree", "remove", "--force", &p_tmp_s]);
            let _ = std::fs::remove_dir_all(&p_tmp);
        }
        if !regressed.is_empty() {
            culprit = Some((
                sp.name.clone(),
                sp.head.clone(),
                sp.cmd.clone(),
                regressed.into_iter().collect(),
                text,
            ));
            break;
        }
    }
    let _ = repo.git(&["worktree", "remove", "--force", &tmp_s]);
    let _ = std::fs::remove_dir_all(&tmp_dir);

    st = shared.lock().expect("state");
    let Some((peer_name, peer_head, peer_cmd, regressed, out_text)) = culprit else {
        return (st, None);
    };

    // Merge the regressed peer's finished branch cleanly into `agent`'s worktree
    // so `agent` can inspect their implementation and run both checks directly.
    let mut merged_into_wt = st.repo.is_ancestor(&peer_head, &me);
    if !merged_into_wt {
        let _ = Repo::commit_if_dirty(
            &wt,
            &format!("concord: checkpoint of {agent} before peer-regression merge"),
        );
        let msg = format!("concord: merge {peer_name} into {agent} to resolve peer regression");
        let res = match Repo::merge_into(&wt, &peer_head, &msg) {
            Ok(m) if m.clean => Ok(m),
            Ok(m) => Repo::merge_into_union(
                &wt,
                &peer_head,
                &msg,
                &m.conflicted_files,
                true,
                &crate::regions::verify_union,
            ),
            Err(e) => Err(e),
        };
        if let Ok(m) = res
            && m.clean
        {
            merged_into_wt = true;
            let new_base = st.advance_base(&st.agents[agent].base.clone(), &peer_head);
            if let Some(a) = st.agents.get_mut(agent) {
                a.base = new_base;
                a.merged_producers.insert(peer_name.clone());
                a.merged_revs.insert(peer_name.clone(), peer_head.clone());
                a.stats.merges.push(peer_name.clone());
            }
            let _ = st.refresh_inner(agent, true);
        }
    }
    if let Some(a) = st.agents.get_mut(agent) {
        a.peer_check_blocks += 1;
        a.stats.gate_blocks += 1;
        a.check_failures = None;
    }
    st.log(
        "stop",
        json!({
            "agent": agent,
            "decision": "block",
            "reason": "peer_check_regression",
            "peer": peer_name,
            "regressed": regressed.len(),
            "merged_peer": merged_into_wt,
        }),
    );
    let mut reason = format!(
        "[concord] peer regression: finished agent `{peer_name}` passed these tests on its own branch, \
         but your changes break them when integrated ({} regressed):\n",
        regressed.len()
    );
    for t in regressed.iter().take(20) {
        reason.push_str(&format!("  {t}\n"));
    }
    if regressed.len() > 20 {
        reason.push_str(&format!("  ... and {} more\n", regressed.len() - 20));
    }
    reason.push_str(&format!("Assertion details:\n{}\n", assertion_excerpt(&out_text, 30)));
    if merged_into_wt {
        reason.push_str(&format!(
            "Concord has merged `{peer_name}`'s finished code into your worktree so you can see how their feature \
             works alongside yours. Adapt your code so you do not overwrite or break `{peer_name}`'s handlers/state, \
             run `{peer_cmd}` AND your own check to verify both pass, then finish."
        ));
    } else {
        reason.push_str(&format!(
            "Adapt your code so `{peer_name}`'s check (`{peer_cmd}`) and your own check both pass together, then finish."
        ));
    }
    (st, Some(reason))
}

fn finish(st: &mut State, agent: &str) {
    if let Some(a) = st.agents.get_mut(agent) {
        let _ = Repo::commit_if_dirty(&a.worktree, &format!("agent {agent}: work committed by concord"));
        if let Ok(h) = st.repo.rev_parse(&a.branch) {
            a.snapshot = Some(h);
        }
        a.status = Status::Finished;
    }
    st.file_owners.retain(|_, owner| owner != agent);
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
        if let Err(e) = st.refresh_inner(agent, true) {
            st.log("error", json!({ "agent": agent, "error": e.to_string() }));
        }
        if st.mode != Mode::Enforce || !st.enforce_gate {
            let remaining = st.auto_ack(agent);
            st.log(
                "stop",
                json!({ "agent": agent, "would_block": !remaining.is_empty(), "stale": remaining.keys().collect::<Vec<_>>() }),
            );
            let (mut st, block) = check_gate(shared, st, agent);
            if let Some(reason) = block {
                return (json!({ "decision": "block", "reason": reason }).to_string(), 0);
            }
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

        // Baseline B3 (`--file-leases`): wait for or block on active file locks.
        if st.file_leases
            && let Some(wt_snap) = st.agents.get(agent).and_then(|a| a.snapshot.clone())
        {
            let base = st.agents[agent].base.clone();
            let modified: Vec<String> = st
                .repo
                .changed_files(&base, &wt_snap)
                .unwrap_or_default()
                .into_iter()
                .map(|c| c.path)
                .collect();
            let locked: Vec<(String, String)> = modified
                .into_iter()
                .filter_map(|f| {
                    let owner = st.file_owners.get(&f)?;
                    (owner != agent).then(|| (f, owner.clone()))
                })
                .collect();
            if !locked.is_empty() {
                let owners: BTreeSet<String> = locked
                    .iter()
                    .map(|(_, o)| o.clone())
                    .filter(|o| st.may_wait(agent, o))
                    .collect();
                if !owners.is_empty() && Instant::now() < deadline {
                    st.waiting.insert(agent.to_owned(), owners.clone());
                    st.log("wait", json!({ "agent": agent, "for": owners, "reason": "file_lease" }));
                    drop(st);
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
                if let Some(a) = st.agents.get_mut(agent) {
                    a.stats.gate_blocks += 1;
                }
                let list: Vec<String> = locked.iter().map(|(f, o)| format!("`{f}` (locked by `{o}`)")).collect();
                let reason = format!(
                    "[concord] Exclusive file leases are held on: {}. Restore those files or wait until their owner finishes.",
                    list.join(", ")
                );
                return (json!({ "decision": "block", "reason": reason }).to_string(), 0);
            }
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
                match crate::regions::gate(&mut st, agent, Instant::now() < deadline) {
                    crate::regions::Gate::Clear => {}
                    crate::regions::Gate::Wait(owners, text) => {
                        // Never wait for an owner that is itself waiting for us.
                        let waitable: BTreeSet<String> =
                            owners.iter().filter(|o| st.may_wait(agent, o)).cloned().collect();
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
                    // Each finished owner hands over at most once: bounded
                    // progress, so it does not count toward MAX_GATE_BLOCKS.
                    // Reset `check_failures` because the worktree just received
                    // a peer's code, so the next check evaluates a new codebase.
                    crate::regions::Gate::Handoff(h) => {
                        let text = h.text.clone();
                        // Lean: the merge is verified by the agent's own
                        // acceptance check instead of an agent turn.
                        if st.lean {
                            let (guard, verdict) = lean_handoff(shared, st, agent, &h);
                            st = guard;
                            match verdict {
                                LeanHandoff::Silent => {
                                    drop(st);
                                    continue;
                                }
                                LeanHandoff::Turn(extra) => {
                                    st.log(
                                        "stop",
                                        json!({ "agent": agent, "decision": "block", "reason": "region_handoff" }),
                                    );
                                    return (
                                        json!({ "decision": "block", "reason": format!("{text}{extra}") }).to_string(),
                                        0,
                                    );
                                }
                            }
                        }
                        if let Some(a) = st.agents.get_mut(agent) {
                            a.check_failures = None;
                        }
                        st.log(
                            "stop",
                            json!({ "agent": agent, "decision": "block", "reason": "region_handoff" }),
                        );
                        return (json!({ "decision": "block", "reason": text }).to_string(), 0);
                    }
                }
            }
            if st.peer_merge
                && let Some(reason) = integrate_peers(&mut st, agent)
            {
                if let Some(a) = st.agents.get_mut(agent) {
                    a.stats.gate_blocks += 1;
                    a.check_failures = None;
                }
                return (json!({ "decision": "block", "reason": reason }).to_string(), 0);
            }
            let (guard, block) = check_gate(shared, st, agent);
            st = guard;
            if let Some(reason) = block {
                return (json!({ "decision": "block", "reason": reason }).to_string(), 0);
            }
            let (guard, block) = peer_check_gate(shared, st, agent);
            st = guard;
            if let Some(reason) = block {
                return (json!({ "decision": "block", "reason": reason }).to_string(), 0);
            }
            if let Some(cmd) = st.together.clone() {
                // Fails together: test the merge with finished peers (outside
                // the state lock, so other agents' hooks are not delayed).
                let wt = st.agents[agent].worktree.clone();
                let me = Repo::snapshot_worktree(&wt).unwrap_or_default();
                let affected = st.agents.get(agent).and_then(|a| {
                    a.index
                        .as_ref()
                        .map(|ix| ix.affected_tests(&BTreeSet::new(), &a.touched_files))
                });
                let order = crate::run::merge_order(&st);
                let finished: Vec<(String, PathBuf, String)> = order
                    .into_iter()
                    .filter_map(|name| st.agents.get(&name))
                    .filter(|p| p.name != agent && p.status == Status::Finished)
                    .map(|p| (p.name.clone(), p.worktree.clone(), p.branch.clone()))
                    .collect();
                let mut peers = Vec::new();
                for (name, p_wt, branch) in finished {
                    let _ = Repo::commit_if_dirty(&p_wt, &format!("agent {name}: work committed by concord"));
                    if let Ok(h) = st.repo.rev_parse(&branch) {
                        peers.push((name, h));
                    }
                }
                let key = format!("{me}:{peers:?}");
                let cached = st
                    .together_cache
                    .get(agent)
                    .filter(|(k, _)| *k == key)
                    .map(|(_, v)| v.clone());
                let verdict = match cached {
                    Some(v) => v,
                    None if me.is_empty() => crate::together::Verdict::Skipped("no snapshot".into()),
                    None => {
                        let repo = st.repo.clone();
                        drop(st);
                        let v = crate::together::check_with_affected(&repo, &cmd, &me, &peers, affected.as_ref());
                        st = shared.lock().expect("state");
                        st.together_cache.insert(agent.to_owned(), (key, v.clone()));
                        v
                    }
                };
                match &verdict {
                    crate::together::Verdict::FailsTogether { tests, culprits, .. } => {
                        let text = crate::together::render(&st.repo, &st.base, &peers, &verdict);
                        if let Some(a) = st.agents.get_mut(agent) {
                            a.stats.gate_blocks += 1;
                        }
                        st.log(
                            "stop",
                            json!({ "agent": agent, "decision": "block", "reason": "fails_together", "tests": tests, "culprits": culprits }),
                        );
                        return (json!({ "decision": "block", "reason": text }).to_string(), 0);
                    }
                    crate::together::Verdict::Skipped(why) => {
                        st.log("together_skipped", json!({ "agent": agent, "why": why }));
                    }
                    crate::together::Verdict::Clean => {
                        st.log("together_clean", json!({ "agent": agent, "peers": peers.len() }));
                    }
                }
            }
            st.log("stop", json!({ "agent": agent, "decision": "allow" }));
            finish(&mut st, agent);
            return (String::new(), 0);
        }

        // Producers of the contracts this agent is stale on, plus active
        // owners of contended/duplicate symbols.
        let gate_state = st.dir.finish_gate(&AgentId::new(agent));
        let mut producers: BTreeSet<String> = remaining
            .keys()
            .filter_map(|s| st.dir.published_by(s).map(|p| p.0.clone()))
            .filter(|p| p != agent)
            .collect();
        for b in &gate_state.blockers {
            if let concord_core::Blocker::Contended { owner: Some(o), .. } = b
                && o.0 != agent
            {
                producers.insert(o.0.clone());
            }
        }
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
            let waitable: BTreeSet<String> = running.iter().filter(|p| st.may_wait(agent, p)).cloned().collect();
            if !waitable.is_empty()
                && Instant::now() < deadline
                && gate_state
                    .blockers
                    .iter()
                    .all(|b| matches!(b, concord_core::Blocker::Stale { .. }))
            {
                st.log("wait", json!({ "agent": agent, "for": running }));
                st.waiting.insert(agent.to_owned(), waitable);
                drop(st);
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            let conforming: Vec<SymbolId> = remaining
                .iter()
                .filter(|(s, problems)| {
                    problems.is_empty() && st.dir.published_by(s).is_some_and(|p| running.contains(&p.0))
                })
                .map(|(s, _)| s.clone())
                .collect();
            if !conforming.is_empty() {
                for s in &conforming {
                    st.dir.acknowledge(&AgentId::new(agent), s);
                    st.log("ack", json!({ "agent": agent, "symbol": s, "producer_running": true }));
                }
                continue;
            }
            if !remaining.is_empty() {
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
        }

        // Merge finished stale-contract producers (not duplicate-symbol owners
        // while we still define a duplicate copy!) into the agent's worktree.
        let stale_producers: Vec<String> = remaining
            .keys()
            .filter_map(|s| st.dir.published_by(s).map(|p| p.0.clone()))
            .filter(|p| p != agent && !st.agents[agent].merged_producers.contains(p))
            .filter(|p| st.agents.get(p).is_some_and(|a| a.status == Status::Finished))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        let mut merge_notes = Vec::new();
        for p in &stale_producers {
            let (wt, base) = {
                let a = &st.agents[agent];
                (a.worktree.clone(), a.base.clone())
            };
            let Some((p_branch, p_wt)) = st.agents.get(p).map(|x| (x.branch.clone(), x.worktree.clone())) else {
                continue;
            };
            if let Err(e) = Repo::commit_if_dirty(&p_wt, &format!("agent {p}: work committed by concord for merging")) {
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
                let result = Repo::commit_if_dirty(&wt, &format!("concord: checkpoint of {agent} before merging {p}"))
                    .and_then(|_| {
                        Repo::merge_keep_conflicts(&wt, &p_head, &format!("concord: merge {p} into {agent}"))
                    });
                match result {
                    Ok(out) if !out.clean => {
                        if let Some(a) = st.agents.get_mut(agent) {
                            a.merged_producers.insert(p.clone());
                            a.merged_revs.insert(p.clone(), p_head.clone());
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
                        let new_base = st.advance_base(&base, &p_head);
                        if let Some(a) = st.agents.get_mut(agent) {
                            a.base = new_base;
                            a.merged_producers.insert(p.clone());
                            a.merged_revs.insert(p.clone(), p_head.clone());
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
            let result = Repo::commit_if_dirty(&wt, &format!("concord: checkpoint of {agent} before merging {p}"))
                .and_then(
                    |_| match Repo::merge_into(&wt, &p_head, &format!("concord: merge {p} into {agent}")) {
                        Ok(m) if m.clean => Ok(m),
                        Ok(m) => Repo::merge_into_union(
                            &wt,
                            &p_head,
                            &format!("concord: merge {p} into {agent}"),
                            &m.conflicted_files,
                            true,
                            &crate::regions::verify_union,
                        ),
                        Err(e) => Err(e),
                    },
                );
            match result {
                Ok(m) if m.clean => {
                    let new_base = st.advance_base(&base, &p_head);
                    if let Some(a) = st.agents.get_mut(agent) {
                        a.base = new_base;
                        a.merged_producers.insert(p.clone());
                        a.merged_revs.insert(p.clone(), p_head.clone());
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
            if let Err(e) = st.refresh_inner(agent, true) {
                st.log("error", json!({ "agent": agent, "error": e.to_string() }));
            }
            let remaining = st.auto_ack(agent);
            let gate = st.dir.finish_gate(&AgentId::new(agent));
            if remaining.is_empty() && gate.is_open() {
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
            // Gated by contract lease contention or duplicate symbol definition.
            let gate = st.dir.finish_gate(&AgentId::new(agent));
            let mut dups = Vec::new();
            let mut leases = Vec::new();
            for b in &gate.blockers {
                if let concord_core::Blocker::Contended { symbol, owner } = b {
                    let who = owner.as_ref().map(|o| o.0.as_str()).unwrap_or("another agent");
                    if st.dir.defined_by(symbol).is_some_and(|d| d.0 == who && d.0 != agent) {
                        dups.push((symbol.clone(), who.to_owned()));
                    } else {
                        leases.push((symbol.clone(), who.to_owned()));
                    }
                }
            }
            if !dups.is_empty() {
                reason
                    .push_str("You defined symbols that another agent already defined with the same qualified name:\n");
                for (sym, who) in &dups {
                    reason.push_str(&format!("  {sym} (already defined by `{who}`)\n"));
                }
                reason.push_str(
                    "Rename your helper to a distinct name if it is internal to your feature, or remove your duplicate \
                     definition so Concord can bring in their finished implementation without duplicate definitions.",
                );
            } else {
                reason.push_str("You are changing the contract of symbols another agent is also changing:\n");
                for (sym, who) in &leases {
                    reason.push_str(&format!("  {sym} (lease held by {who})\n"));
                }
                reason.push_str(
                    "Keep its existing signature working (add new parameters as optional), then finish again.",
                );
            }
            if let Some(a) = st.agents.get_mut(agent) {
                a.stats.gate_blocks += 1;
            }
            st.log(
                "stop",
                json!({ "agent": agent, "decision": "block", "reason": if !dups.is_empty() { "duplicate_symbol" } else { "contended" } }),
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
        if let Err(e) = Repo::commit_if_dirty(&wt, &format!("concord: merge {peer} into {agent} (conflicts resolved)"))
        {
            st.log("error", json!({ "agent": agent, "error": e.to_string() }));
        }
        let base = st.agents[agent].base.clone();
        let new_base = st.advance_base(&base, &head);
        if let Some(a) = st.agents.get_mut(agent) {
            a.base = new_base;
            a.merged_revs.insert(peer.clone(), head);
            a.pending_conflicts.clear();
            a.pending_peer = None;
        }
        st.log("peer_merge_resolved", json!({ "agent": agent, "peer": peer }));
        let _ = st.refresh_inner(agent, true);
    }

    // 2. Find a finished peer whose work conflicts textually with ours.
    let me = match Repo::snapshot_worktree(&wt) {
        Ok(c) => c,
        Err(e) => {
            st.log("error", json!({ "agent": agent, "error": e.to_string() }));
            return None;
        }
    };
    let order = crate::run::merge_order(st);
    let peers: Vec<(String, PathBuf, String)> = order
        .into_iter()
        .filter_map(|name| st.agents.get(&name))
        .filter(|p| p.name != agent && p.status == Status::Finished)
        .filter(|p| !st.agents[agent].merged_producers.contains(&p.name))
        .map(|p| (p.name.clone(), p.worktree.clone(), p.branch.clone()))
        .collect();
    for (peer, p_wt, p_branch) in peers {
        let _ = Repo::commit_if_dirty(&p_wt, &format!("agent {peer}: work committed by concord for merging"));
        let Ok(head) = st.repo.rev_parse(&p_branch) else {
            continue;
        };
        if st.repo.is_ancestor(&head, &me) {
            if let Some(a) = st.agents.get_mut(agent) {
                a.merged_producers.insert(peer.clone());
                a.merged_revs.insert(peer, head);
            }
            continue;
        }
        let Ok(m) = st.repo.merge_tree(&me, &head) else {
            continue;
        };
        if m.clean {
            continue;
        }
        // If the collision is a verified pure two-sided insertion union, keep
        // both insertions cleanly instead of dumping conflict markers!
        let _ = Repo::commit_if_dirty(&wt, &format!("concord: checkpoint of {agent} before merging {peer}"));
        if let Ok(u) = Repo::merge_into_union(
            &wt,
            &head,
            &format!("concord: union merge {peer} into {agent}"),
            &m.conflicted_files,
            true,
            &crate::regions::verify_union,
        ) && u.clean
        {
            let base = st.agents[agent].base.clone();
            let new_base = st.advance_base(&base, &head);
            if let Some(a) = st.agents.get_mut(agent) {
                a.base = new_base;
                a.merged_producers.insert(peer.clone());
                a.merged_revs.insert(peer.clone(), head);
                a.stats.merges.push(peer.clone());
            }
            st.log(
                "peer_merge",
                json!({ "agent": agent, "peer": peer, "clean": true, "union": true }),
            );
            let _ = st.refresh_inner(agent, true);
            continue;
        }
        match Repo::merge_keep_conflicts(&wt, &head, &format!("concord: merge {peer} into {agent}")) {
            Ok(out) => {
                if let Some(a) = st.agents.get_mut(agent) {
                    a.merged_producers.insert(peer.clone());
                    a.merged_revs.insert(peer.clone(), head.clone());
                    a.stats.merges.push(peer.clone());
                }
                st.log(
                    "peer_merge",
                    json!({ "agent": agent, "peer": peer, "clean": out.clean, "files": out.conflicted_files }),
                );
                if out.clean {
                    let base = st.agents[agent].base.clone();
                    let new_base = st.advance_base(&base, &head);
                    if let Some(a) = st.agents.get_mut(agent) {
                        a.base = new_base;
                    }
                    let _ = st.refresh_inner(agent, true);
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
    serve_with_auth(listener, shared, None);
}

/// Serve hook and IPC requests on `listener`, optionally requiring a shared
/// bearer token (`CONCORD_AUTH_TOKEN`) for remote/containerized agent sandboxes.
pub fn serve_with_auth(listener: TcpListener, shared: Shared, auth_token: Option<String>) {
    let token = std::sync::Arc::new(auth_token);
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let shared = shared.clone();
        let token = token.clone();
        std::thread::spawn(move || {
            let _ = serve_one(stream, &shared, token.as_deref());
        });
    }
}

fn serve_one(stream: TcpStream, shared: &Shared, expected_token: Option<&str>) -> Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let req: Value = serde_json::from_str(&line)?;
    if let Some(expected) = expected_token {
        let provided = req.get("token").and_then(Value::as_str).unwrap_or("");
        if provided != expected {
            let mut w = stream;
            writeln!(
                w,
                "{}",
                json!({ "stdout": "{\"error\":\"unauthorized\"}", "exit_code": 1 })
            )?;
            return Ok(());
        }
    }
    let agent = req.get("agent").and_then(Value::as_str).unwrap_or("");
    let event = req.get("event").and_then(Value::as_str).unwrap_or("");
    let input = req.get("input").cloned().unwrap_or(Value::Null);
    let (stdout, code) = handle(shared, agent, event, &input);
    let mut w = stream;
    writeln!(w, "{}", json!({ "stdout": stdout, "exit_code": code }))?;
    Ok(())
}

/// Send an IPC request (`Negotiate`, `Ack`, `Status`, `PostToolUse`, `Stop`) to
/// a running Concord daemon at `addr` and parse the JSON `stdout` payload.
pub fn daemon_request(addr: &str, agent: &str, event: &str, input: &Value) -> Result<Value> {
    let mut stream = TcpStream::connect(addr).with_context(|| format!("connecting to concord daemon at {addr}"))?;
    let token = std::env::var("CONCORD_AUTH_TOKEN").ok();
    let req = json!({ "agent": agent, "event": event, "input": input, "token": token });
    writeln!(stream, "{req}")?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let resp: Value = serde_json::from_str(&line)?;
    let stdout = resp.get("stdout").and_then(Value::as_str).unwrap_or("{}");
    let parsed: Value = serde_json::from_str(stdout).unwrap_or_else(|_| json!({ "raw": stdout }));
    Ok(parsed)
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
    let token = std::env::var("CONCORD_AUTH_TOKEN").ok();
    let req = json!({ "agent": agent, "event": event, "input": input, "token": token });
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
        NoticeKind::DuplicateSymbol { .. } => "duplicate_symbol",
        NoticeKind::Negotiation { .. } => "negotiation",
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
