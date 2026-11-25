//! Contract-verified Smart Merge, Live Worktree Coherence Inspection, and
//! SASE Merge-Readiness Pack (MRP) generation.
//!
//! Grounded in Hassan et al. (2025/2026, *Agentic Software Engineering*,
//! arXiv:2509.06216): an MRP elevates merge review from line-by-line git diffs
//! to structured evidence of (1) Functional Completeness, (2) Sound
//! Verification, (3) SE Hygiene, (4) Clear Rationale, and (5) Auditability.

use crate::run::RunReport;
use crate::{Analysis, TextualMerge, analyze_branches, index_revision};
use anyhow::{Context, Result, bail};
use concord_core::{ChangeKind, Finding, Summary, check_call};
use concord_git::{Repo, WorktreeEntry};
use concord_lang::RepoIndex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

/// Result of inspecting all active `git worktree` checkouts in a repository.
#[derive(Clone, Debug, Serialize)]
pub struct WorktreeAnalysis {
    pub base: String,
    pub base_commit: String,
    pub worktrees: Vec<WorktreeStatus>,
    pub analysis: Analysis,
    /// Conflicting pairs that Concord's verified insertion-union resolves cleanly.
    pub auto_union_pairs: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct WorktreeStatus {
    pub label: String,
    pub path: String,
    pub branch: Option<String>,
    pub head: String,
    pub snapshot: String,
    pub dirty: bool,
    pub changed_files: usize,
    pub symbol_changes: usize,
}

/// Discover all active worktrees in `repo`, snapshot their live working trees
/// in memory (without mutating git refs or working files), and analyse
/// cross-worktree contract dependencies, duplicate definitions, and textual
/// overlaps against `base`.
pub fn analyze_worktrees(repo: &Repo, base: &str, include_clean: bool) -> Result<WorktreeAnalysis> {
    let base_commit = repo
        .rev_parse(base)
        .with_context(|| format!("unknown base revision {base}"))?;
    let raw_wts: Vec<WorktreeEntry> = repo.list_worktrees()?;

    let mut snaps: Vec<String> = Vec::new();
    let mut label_by_snap: BTreeMap<String, String> = BTreeMap::new();
    let mut statuses: Vec<WorktreeStatus> = Vec::new();
    let mut used_labels: BTreeSet<String> = BTreeSet::new();

    for wt in raw_wts {
        let path_str = wt.path.to_string_lossy().into_owned();
        if path_str.contains("concord-together-") || path_str.ends_with("/_integration") {
            continue;
        }
        let Ok(snap) = Repo::snapshot_worktree(&wt.path) else {
            continue;
        };
        let dirty = snap != wt.head;
        let changed = repo.changed_files(&base_commit, &snap).unwrap_or_default();
        if !include_clean && changed.is_empty() {
            continue;
        }
        let base_label = wt.branch.clone().unwrap_or_else(|| {
            wt.path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_else(|| wt.head[..wt.head.len().min(8)].to_owned())
        });
        let mut label = base_label.clone();
        let mut seq = 2usize;
        while !used_labels.insert(label.clone()) {
            label = format!("{base_label}-{seq}");
            seq += 1;
        }
        label_by_snap.insert(snap.clone(), label.clone());
        snaps.push(snap.clone());
        statuses.push(WorktreeStatus {
            label,
            path: path_str,
            branch: wt.branch,
            head: wt.head,
            snapshot: snap,
            dirty,
            changed_files: changed.len(),
            symbol_changes: 0,
        });
    }

    let mut analysis = analyze_branches(repo, &base_commit, &snaps, true)?;
    let map_name = |s: &str| label_by_snap.get(s).cloned().unwrap_or_else(|| s.to_owned());

    // Rewrite snapshot SHAs to human-readable worktree labels.
    for fp in &mut analysis.branches {
        fp.name = map_name(&fp.name);
        if let Some(ws) = statuses.iter_mut().find(|w| w.label == fp.name) {
            ws.symbol_changes = fp.changes.len();
        }
    }
    for f in &mut analysis.findings {
        match f {
            Finding::FileOverlap { a, b, .. }
            | Finding::SameSymbol { a, b, .. }
            | Finding::DuplicateDefinition { a, b, .. } => {
                *a = map_name(a);
                *b = map_name(b);
            }
            Finding::ContractDependency { producer, consumer, .. } => {
                *producer = map_name(producer);
                *consumer = map_name(consumer);
            }
        }
    }

    let mut auto_union_pairs = Vec::new();
    let mut mapped_textual = Vec::new();
    for tm in analysis.textual {
        let la = map_name(&tm.a);
        let lb = map_name(&tm.b);
        if !tm.clean
            && repo
                .virtual_merge_union(&tm.a, &tm.b, &crate::regions::verify_union)
                .ok()
                .flatten()
                .is_some()
        {
            auto_union_pairs.push((la.clone(), lb.clone()));
        }
        mapped_textual.push(TextualMerge {
            a: la,
            b: lb,
            clean: tm.clean,
            conflicted_files: tm.conflicted_files,
        });
    }
    analysis.textual = mapped_textual;

    Ok(WorktreeAnalysis {
        base: base.to_owned(),
        base_commit,
        worktrees: statuses,
        analysis,
        auto_union_pairs,
    })
}

pub fn render_worktrees(w: &WorktreeAnalysis) -> String {
    let mut s = format!(
        "Concord live worktree check against base {} ({})\n",
        w.base,
        &w.base_commit[..w.base_commit.len().min(10)]
    );
    if w.worktrees.is_empty() {
        s.push_str("  No active worktrees have changes relative to base.\n");
        return s;
    }
    s.push_str(&format!("  Active modified worktrees ({}):\n", w.worktrees.len()));
    for wt in &w.worktrees {
        let state = if wt.dirty { "uncommitted edits" } else { "committed" };
        let _ = writeln!(
            s,
            "    - {:<24} ({state}, {} files, {} symbol changes) [{}]",
            wt.label, wt.changed_files, wt.symbol_changes, wt.path
        );
    }
    s.push('\n');
    s.push_str(&crate::report::analysis(&w.analysis, None));
    if !w.auto_union_pairs.is_empty() {
        s.push_str("\nVerified auto-unionable insertion overlaps:\n");
        for (a, b) in &w.auto_union_pairs {
            let _ = writeln!(s, "  - {a} + {b}: pure two-sided insertions (Concord merges cleanly)");
        }
    }
    s
}

/// Configuration for standalone contract-verified `concord merge`.
#[derive(Clone, Debug)]
pub struct SmartMergeConfig {
    pub repo: std::path::PathBuf,
    pub base: String,
    pub branches: Vec<String>,
    /// Optional target branch name to create or update (if `None`, merges in a
    /// temporary detached worktree and returns the resulting commit SHA).
    pub into_branch: Option<String>,
    pub test_cmd: Option<String>,
    pub merge_driver: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SmartMergeReport {
    pub base: String,
    pub base_commit: String,
    pub order: Vec<String>,
    pub merged_commit: Option<String>,
    pub into_branch: Option<String>,
    pub clean_merges: Vec<String>,
    pub union_merges: BTreeMap<String, Vec<String>>,
    pub conflicts: BTreeMap<String, Vec<String>>,
    pub post_merge_violations: Vec<String>,
    pub parse_errors: Vec<String>,
    pub tests_passed: Option<bool>,
    pub tests_output_tail: String,
    /// If `tests_passed == Some(false)`, binary bisection identifies the first
    /// branch prefix where the test suite started failing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bisected_culprit: Option<String>,
    pub rationale: Vec<String>,
    pub analysis_summary: Summary,
}

/// Order branches so contract producers precede their consumers.
pub fn contract_merge_order(branches: &[String], findings: &[Finding]) -> (Vec<String>, Vec<String>) {
    let mut deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut rationale = Vec::new();
    for b in branches {
        deps.entry(b.clone()).or_default();
    }
    for f in findings {
        if let Finding::ContractDependency {
            producer,
            consumer,
            symbol,
            ..
        } = f
            && producer != consumer
            && deps.contains_key(consumer)
            && deps.contains_key(producer)
            && deps.entry(consumer.clone()).or_default().insert(producer.clone())
        {
            rationale.push(format!(
                "`{producer}` ordered before `{consumer}` (modifies contract of `{symbol}`)"
            ));
        }
    }
    let mut order = Vec::new();
    let mut left: Vec<String> = branches.to_vec();
    while !left.is_empty() {
        let pos = left
            .iter()
            .position(|b| deps[b].iter().all(|p| order.contains(p)))
            .unwrap_or(0);
        order.push(left.remove(pos));
    }
    (order, rationale)
}

/// Verify post-merge AST parse health and intra-repository call conformance on
/// all files changed between `base_commit` and `merged_commit`.
pub fn verify_merged_contracts(repo: &Repo, base_commit: &str, merged_commit: &str) -> (Vec<String>, Vec<String>) {
    let mut parse_errors = Vec::new();
    let Ok(files) = index_revision(repo, merged_commit, &mut parse_errors) else {
        return (parse_errors, Vec::new());
    };
    let changed: BTreeSet<String> = repo
        .changed_files(base_commit, merged_commit)
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.path)
        .collect();
    let index = RepoIndex::new(files.values().cloned());
    let mut violations = Vec::new();
    for path in &changed {
        let Some(f) = files.get(path) else { continue };
        for r in &f.refs {
            if !r.is_call {
                continue;
            }
            let Some(call) = &r.args else { continue };
            let Some(target) = index.resolve(f, r) else { continue };
            if target == r.from {
                continue;
            }
            // Only flag hard post-merge violations when the receiver/callee is
            // deterministically bound (`self`/`this`/`cls`, an import, or a
            // top-level/class symbol of the current module), avoiding false
            // positives on untyped local receivers like `regex.match(s, pos)`.
            if r.chain.len() > 1 {
                let head = &r.chain[0];
                let bound = matches!(head.as_str(), "self" | "cls" | "this" | "Self")
                    || f.imports.iter().any(|b| &b.local == head)
                    || f.symbols.iter().any(|s| s.id.name() == head);
                if !bound {
                    continue;
                }
            }
            let Some(sym) = index.symbol(&target) else { continue };
            let Some(sig) = &sym.contract.signature else { continue };
            let is_static = sig.decorators.iter().any(|d| d.contains("staticmethod"));
            let bound = sym.kind == concord_core::SymbolKind::Method && !is_static;
            for p in check_call(sig, call, bound) {
                violations.push(format!("{}:{}  {}  -> `{}`: {p}", f.path, r.line, r.text, target));
            }
        }
    }
    violations.sort();
    violations.dedup();
    (parse_errors, violations)
}

fn run_shell(cmd: &str, dir: &Path) -> (bool, String) {
    match Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output() {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            let lines: Vec<&str> = text.lines().collect();
            let tail = lines[lines.len().saturating_sub(25)..].join("\n");
            (o.status.success(), tail)
        }
        Err(e) => (false, e.to_string()),
    }
}

/// Binary bisection over prefix commits `prefix_commits = [(branch_name, integrated_sha), ...]`
/// to find the first branch whose integration broke `test_cmd`.
pub fn bisect_prefix_failure(repo: &Repo, prefix_commits: &[(String, String)], test_cmd: &str) -> Option<String> {
    if prefix_commits.is_empty() || test_cmd.trim() == "true" {
        return None;
    }
    if prefix_commits.len() == 1 {
        return Some(prefix_commits[0].0.clone());
    }
    let mut lo = 0usize;
    let mut hi = prefix_commits.len() - 1;
    while lo < hi {
        let mid = (lo + hi) / 2;
        let rev = &prefix_commits[mid].1;
        let tmp = std::env::temp_dir().join(format!("concord-bisect-{}-{mid}", std::process::id()));
        let tmp_s = tmp.to_string_lossy().into_owned();
        if repo
            .git(&["worktree", "add", "--detach", "--quiet", &tmp_s, rev])
            .is_err()
        {
            return None;
        }
        let (ok, _) = run_shell(test_cmd, &tmp);
        let _ = repo.git(&["worktree", "remove", "--force", &tmp_s]);
        let _ = std::fs::remove_dir_all(&tmp);
        if ok {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Some(prefix_commits[lo].0.clone())
}

pub fn smart_merge(cfg: &SmartMergeConfig) -> Result<SmartMergeReport> {
    if cfg.branches.is_empty() {
        bail!("at least one branch is required");
    }
    let repo = Repo::open(&cfg.repo)?;
    let base_commit = repo.rev_parse(&cfg.base)?;
    if let Some(cmd) = &cfg.merge_driver {
        let driver = format!("{cmd} merge --git %O %A %B -s %S -x %X %Y -p %P -l %L");
        repo.set_merge_driver(
            "concord-structured",
            &driver,
            &["*.py", "*.js", "*.ts", "*.tsx", "*.rs", "*.go", "*.json", "*.toml"],
        )?;
    }

    let analysis = analyze_branches(&repo, &base_commit, &cfg.branches, true)?;
    let (order, rationale) = contract_merge_order(&cfg.branches, &analysis.findings);

    let wt = std::env::temp_dir().join(format!(
        "concord-smart-merge-{}-{}",
        std::process::id(),
        crate::live::now_millis()
    ));
    let wt_s = wt.to_string_lossy().into_owned();
    repo.git(&["worktree", "add", "--detach", "--quiet", &wt_s, &base_commit])?;

    let mut clean_merges = Vec::new();
    let mut union_merges = BTreeMap::new();
    let mut conflicts = BTreeMap::new();
    let mut prefix_commits: Vec<(String, String)> = Vec::new();

    for b in &order {
        let head = repo.rev_parse(b)?;
        let msg = format!("concord merge: integrate {b}");
        let mut m = Repo::merge_into(&wt, &head, &msg)?;
        if !m.clean {
            let files = m.conflicted_files.clone();
            let u = Repo::merge_into_union(&wt, &head, &msg, &files, false, &crate::regions::verify_union)?;
            if u.clean {
                union_merges.insert(b.clone(), files);
                m = u;
            }
        } else {
            clean_merges.push(b.clone());
        }
        if m.clean {
            if let Ok(wt_repo) = Repo::open(&wt)
                && let Ok(sha) = wt_repo.rev_parse("HEAD")
            {
                prefix_commits.push((b.clone(), sha));
            }
        } else {
            conflicts.insert(b.clone(), m.conflicted_files);
        }
    }

    let merged_commit = Repo::open(&wt).and_then(|r| r.rev_parse("HEAD")).ok();
    let (parse_errors, post_merge_violations) = match &merged_commit {
        Some(mc) => verify_merged_contracts(&repo, &base_commit, mc),
        None => (Vec::new(), Vec::new()),
    };

    let (tests_passed, tests_output_tail, bisected_culprit) = match &cfg.test_cmd {
        Some(cmd) => {
            let (ok, tail) = run_shell(cmd, &wt);
            let culprit = if !ok && conflicts.is_empty() && prefix_commits.len() > 1 {
                bisect_prefix_failure(&repo, &prefix_commits, cmd)
            } else if !ok && prefix_commits.len() == 1 {
                Some(prefix_commits[0].0.clone())
            } else {
                None
            };
            (Some(ok && conflicts.is_empty()), tail, culprit)
        }
        None => (None, String::new(), None),
    };

    if let (Some(target), Some(mc)) = (&cfg.into_branch, &merged_commit)
        && conflicts.is_empty()
    {
        repo.git(&["branch", "-f", target, mc])?;
    }

    let _ = repo.git(&["worktree", "remove", "--force", &wt_s]);
    let _ = std::fs::remove_dir_all(&wt);

    Ok(SmartMergeReport {
        base: cfg.base.clone(),
        base_commit,
        order,
        merged_commit,
        into_branch: cfg.into_branch.clone(),
        clean_merges,
        union_merges,
        conflicts,
        post_merge_violations,
        parse_errors,
        tests_passed,
        tests_output_tail,
        bisected_culprit,
        rationale,
        analysis_summary: analysis.summary,
    })
}

pub fn render_smart_merge(r: &SmartMergeReport) -> String {
    let mut s = format!(
        "Concord Smart Merge against base {} ({})\n",
        r.base,
        &r.base_commit[..r.base_commit.len().min(10)]
    );
    let _ = writeln!(s, "  Order: {}", r.order.join(" -> "));
    if !r.rationale.is_empty() {
        s.push_str("  Contract ordering rationale:\n");
        for line in &r.rationale {
            let _ = writeln!(s, "    - {line}");
        }
    }
    if !r.union_merges.is_empty() {
        s.push_str("  Verified insertion-union merges:\n");
        for (b, files) in &r.union_merges {
            let _ = writeln!(s, "    - {b}: {}", files.join(", "));
        }
    }
    if !r.conflicts.is_empty() {
        s.push_str("  Unresolved conflicts:\n");
        for (b, files) in &r.conflicts {
            let _ = writeln!(s, "    - {b}: {}", files.join(", "));
        }
    }
    if !r.post_merge_violations.is_empty() {
        s.push_str("  Post-merge contract violations:\n");
        for v in &r.post_merge_violations {
            let _ = writeln!(s, "    - {v}");
        }
    } else {
        s.push_str("  Post-merge contract conformance: PASS (0 broken call sites)\n");
    }
    if let Some(ok) = r.tests_passed {
        let _ = writeln!(s, "  Test suite: {}", if ok { "PASS" } else { "FAIL" });
        if let Some(c) = &r.bisected_culprit {
            let _ = writeln!(s, "  Bisected culprit branch: `{c}`");
        }
    }
    if let Some(mc) = &r.merged_commit {
        let _ = writeln!(s, "  Merged commit: {mc}");
    }
    if let Some(into) = &r.into_branch
        && r.conflicts.is_empty()
    {
        let _ = writeln!(s, "  Updated branch: `{into}`");
    }
    s
}

/// SASE Merge-Readiness Pack (MRP) covering all 5 structured evidence pillars.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MergeReadinessPack {
    pub schema_version: String,
    pub run_id: String,
    pub ready_to_merge: bool,
    pub functional_completeness: FunctionalCompleteness,
    pub sound_verification: SoundVerification,
    pub se_hygiene: SeHygiene,
    pub rationale: RationaleSection,
    pub auditability: AuditabilitySection,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FunctionalCompleteness {
    pub agents_total: usize,
    pub agents_succeeded: usize,
    pub branches: Vec<BranchSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BranchSummary {
    pub name: String,
    pub changed_files: Vec<String>,
    pub symbols_added: usize,
    pub contracts_changed: usize,
    pub bodies_changed: usize,
    pub tests_alone_passed: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SoundVerification {
    pub integration_order: Vec<String>,
    pub merge_clean: bool,
    pub union_merged_branches: BTreeMap<String, Vec<String>>,
    pub conflicted_branches: BTreeMap<String, Vec<String>>,
    pub post_merge_contract_violations: Vec<String>,
    pub tests_passed: bool,
    pub bisected_culprit: Option<String>,
    pub tests_output_tail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SeHygiene {
    pub parse_errors: Vec<String>,
    pub duplicate_definitions: Vec<String>,
    pub breaking_contract_dependencies: usize,
    pub compatible_contract_dependencies: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RationaleSection {
    pub ordering_edges: Vec<String>,
    pub handoffs_performed: BTreeMap<String, Vec<String>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditabilitySection {
    pub base_commit: String,
    pub integrated_commit: Option<String>,
    pub branch_commits: BTreeMap<String, String>,
    pub mode: String,
    pub model: String,
    pub wall_seconds: f64,
    pub total_cost_usd: f64,
    pub total_notices_delivered: u32,
    pub total_gate_blocks: u32,
}

/// Build a complete SASE Merge-Readiness Pack at the end of `concord run`.
pub fn build_mrp_for_run(
    repo: &Repo,
    report: &RunReport,
    integrated_commit: Option<String>,
    bisected_culprit: Option<String>,
) -> MergeReadinessPack {
    let branch_names: Vec<String> = report
        .agents
        .iter()
        .map(|a| format!("concord/{}/{}", report.run_id, a.name))
        .collect();
    let mut branch_commits = BTreeMap::new();
    for a in &report.agents {
        let br = format!("concord/{}/{}", report.run_id, a.name);
        if let Ok(sha) = repo.rev_parse(&br) {
            branch_commits.insert(a.name.clone(), sha);
        }
    }

    let analysis = analyze_branches(repo, &report.base, &branch_names, false).ok();
    let (_, ordering_edges) = match &analysis {
        Some(an) => contract_merge_order(&branch_names, &an.findings),
        None => (Vec::new(), Vec::new()),
    };

    let mut branches = Vec::new();
    for a in &report.agents {
        let br_ref = format!("concord/{}/{}", report.run_id, a.name);
        let fp = analysis
            .as_ref()
            .and_then(|an| an.branches.iter().find(|b| b.name == br_ref));
        let (changed_files, symbols_added, contracts_changed, bodies_changed) = match fp {
            Some(f) => (
                f.changed_files.iter().cloned().collect(),
                f.changes.iter().filter(|c| c.kind == ChangeKind::Added).count(),
                f.changes
                    .iter()
                    .filter(|c| c.kind == ChangeKind::ContractChanged)
                    .count(),
                f.changes.iter().filter(|c| c.kind == ChangeKind::BodyChanged).count(),
            ),
            None => (Vec::new(), 0, 0, 0),
        };
        branches.push(BranchSummary {
            name: a.name.clone(),
            changed_files,
            symbols_added,
            contracts_changed,
            bodies_changed,
            tests_alone_passed: a.tests_alone_passed,
        });
    }

    let (parse_errors, post_merge_contract_violations) = match &integrated_commit {
        Some(mc) => verify_merged_contracts(repo, &report.base, mc),
        None => (Vec::new(), Vec::new()),
    };

    // Check duplicate definitions directly on the integrated commit (against
    // `base`) rather than pairwise across post-handoff branches that legitimately
    // share merged producer commits!
    let duplicate_definitions = match &integrated_commit {
        Some(mc) => {
            let changed: Vec<String> = repo
                .changed_files(&report.base, mc)
                .unwrap_or_default()
                .into_iter()
                .map(|c| c.path)
                .filter(|p| concord_lang::is_supported_path(p))
                .collect();
            let base_contents = repo.read_files(&report.base, &changed).unwrap_or_default();
            let merged_contents = repo.read_files(mc, &changed).unwrap_or_default();
            let mut dups = Vec::new();
            for p in &changed {
                if let Some(m_src) = merged_contents.get(p) {
                    let b_src = base_contents.get(p).map(String::as_str).unwrap_or("");
                    dups.extend(crate::regions::duplicate_definitions(p, m_src, b_src, b_src));
                }
            }
            dups.sort();
            dups.dedup();
            dups
        }
        None => Vec::new(),
    };

    let (breaking_deps, compat_deps) = match &analysis {
        Some(an) => (
            an.summary.breaking_contract_dependencies,
            an.summary
                .contract_dependencies
                .saturating_sub(an.summary.breaking_contract_dependencies),
        ),
        None => (0, 0),
    };

    let handoffs_performed: BTreeMap<String, Vec<String>> = report
        .agents
        .iter()
        .filter(|a| !a.merges.is_empty())
        .map(|a| (a.name.clone(), a.merges.clone()))
        .collect();

    let merge_clean = report.integration.conflicts.is_empty();
    let ready_to_merge = merge_clean
        && report.integration.tests_passed
        && parse_errors.is_empty()
        && post_merge_contract_violations.is_empty();

    MergeReadinessPack {
        schema_version: "1.0".into(),
        run_id: report.run_id.clone(),
        ready_to_merge,
        functional_completeness: FunctionalCompleteness {
            agents_total: report.agents.len(),
            agents_succeeded: report.agents.iter().filter(|a| !a.is_error).count(),
            branches,
        },
        sound_verification: SoundVerification {
            integration_order: report.integration.order.clone(),
            merge_clean,
            union_merged_branches: report.integration.unions.clone(),
            conflicted_branches: report.integration.conflicts.clone(),
            post_merge_contract_violations,
            tests_passed: report.integration.tests_passed,
            bisected_culprit,
            tests_output_tail: report.integration.tests_output_tail.clone(),
        },
        se_hygiene: SeHygiene {
            parse_errors,
            duplicate_definitions,
            breaking_contract_dependencies: breaking_deps,
            compatible_contract_dependencies: compat_deps,
        },
        rationale: RationaleSection {
            ordering_edges,
            handoffs_performed,
        },
        auditability: AuditabilitySection {
            base_commit: report.base.clone(),
            integrated_commit,
            branch_commits,
            mode: format!("{:?}", report.mode).to_lowercase(),
            model: report.model.clone(),
            wall_seconds: report.wall_seconds,
            total_cost_usd: report.total_cost_usd,
            total_notices_delivered: report.agents.iter().map(|a| a.notices_delivered).sum(),
            total_gate_blocks: report.agents.iter().map(|a| a.gate_blocks).sum(),
        },
    }
}

pub fn render_mrp_markdown(mrp: &MergeReadinessPack) -> String {
    let verdict = if mrp.ready_to_merge {
        "READY TO MERGE"
    } else {
        "NEEDS ATTENTION"
    };
    let mut s = format!("# Merge-Readiness Pack: `{}` — **{verdict}**\n\n", mrp.run_id);

    s.push_str("## 1. Functional Completeness\n\n");
    let _ = writeln!(
        s,
        "- Agents completed without terminal error: **{}/{}**\n",
        mrp.functional_completeness.agents_succeeded, mrp.functional_completeness.agents_total
    );
    s.push_str(
        "| Branch / Agent | Files | +Symbols | Contract Δ | Body Δ | Tests Alone |\n|---|---|---|---|---|---|\n",
    );
    for b in &mrp.functional_completeness.branches {
        let alone = match b.tests_alone_passed {
            Some(true) => "pass",
            Some(false) => "FAIL",
            None => "-",
        };
        let _ = writeln!(
            s,
            "| `{}` | {} | {} | {} | {} | {} |",
            b.name,
            b.changed_files.len(),
            b.symbols_added,
            b.contracts_changed,
            b.bodies_changed,
            alone
        );
    }

    s.push_str("\n## 2. Sound Verification\n\n");
    let _ = writeln!(
        s,
        "- **Integration order**: `{}`",
        mrp.sound_verification.integration_order.join(" -> ")
    );
    let _ = writeln!(
        s,
        "- **Textual / verified-union merge clean**: `{}`",
        mrp.sound_verification.merge_clean
    );
    let _ = writeln!(
        s,
        "- **Post-merge contract violations**: **{}**",
        mrp.sound_verification.post_merge_contract_violations.len()
    );
    let _ = writeln!(
        s,
        "- **Integrated test suite**: **{}**",
        if mrp.sound_verification.tests_passed {
            "PASS"
        } else {
            "FAIL"
        }
    );
    if let Some(c) = &mrp.sound_verification.bisected_culprit {
        let _ = writeln!(s, "- **Bisected culprit branch**: `{c}`");
    }
    for v in &mrp.sound_verification.post_merge_contract_violations {
        let _ = writeln!(s, "  - `{v}`");
    }

    s.push_str("\n## 3. Software Engineering Hygiene\n\n");
    let _ = writeln!(
        s,
        "- Syntax / AST parse errors: **{}**",
        mrp.se_hygiene.parse_errors.len()
    );
    let _ = writeln!(
        s,
        "- Unresolved duplicate symbol definitions: **{}**",
        mrp.se_hygiene.duplicate_definitions.len()
    );
    let _ = writeln!(
        s,
        "- Cross-branch contract dependencies: **{} breaking**, **{} compatible**",
        mrp.se_hygiene.breaking_contract_dependencies, mrp.se_hygiene.compatible_contract_dependencies
    );

    s.push_str("\n## 4. Integration Rationale\n\n");
    if mrp.rationale.ordering_edges.is_empty() && mrp.rationale.handoffs_performed.is_empty() {
        s.push_str("- All branches had independent contracts; topological order followed task rank.\n");
    } else {
        for e in &mrp.rationale.ordering_edges {
            let _ = writeln!(s, "- {e}");
        }
        for (agent, peers) in &mrp.rationale.handoffs_performed {
            let _ = writeln!(
                s,
                "- `{agent}` incorporated finished peer(s) live: {}",
                peers.join(", ")
            );
        }
    }

    s.push_str("\n## 5. Auditability\n\n");
    let _ = writeln!(s, "- **Base commit**: `{}`", mrp.auditability.base_commit);
    if let Some(ic) = &mrp.auditability.integrated_commit {
        let _ = writeln!(s, "- **Integrated commit**: `{ic}`");
    }
    let _ = writeln!(
        s,
        "- **Mode / Model**: `{}` / `{}`",
        mrp.auditability.mode, mrp.auditability.model
    );
    let _ = writeln!(
        s,
        "- **Telemetry**: {:.1}s wall time, ${:.4} cost, {} notices delivered, {} finish-gate blocks",
        mrp.auditability.wall_seconds,
        mrp.auditability.total_cost_usd,
        mrp.auditability.total_notices_delivered,
        mrp.auditability.total_gate_blocks
    );
    s
}
