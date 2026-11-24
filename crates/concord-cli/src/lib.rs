//! Library part of the `concord` CLI: branch analysis and protocol simulation.

pub mod live;
pub mod mcp;
pub mod regions;
pub mod report;
pub mod run;
pub mod together;
pub mod watch;

use anyhow::{Context, Result};
use concord_core::{
    AgentId, BranchFootprint, Directory, Finding, Gate, Notice, Summary, SymbolChange, SymbolId, analyze, diff_symbols,
    summarize,
};
use concord_git::{FileStatus, Repo};
use concord_lang::{FileIndex, LangError, RepoIndex};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize)]
pub struct TextualMerge {
    pub a: String,
    pub b: String,
    pub clean: bool,
    pub conflicted_files: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Analysis {
    pub base: String,
    pub base_commit: String,
    pub branches: Vec<BranchFootprint>,
    pub findings: Vec<Finding>,
    pub summary: Summary,
    pub textual: Vec<TextualMerge>,
    pub parse_warnings: Vec<String>,
}

/// Index one source file with the front-end for its extension. Callers
/// filter with [`concord_lang::is_supported_path`] first.
fn index_source(path: &str, src: &str) -> Result<FileIndex, LangError> {
    concord_lang::index_file(path, src)
        .unwrap_or_else(|| Err(LangError::Parse(format!("{path}: unsupported language"))))
}

/// Index every source file of `rev` in a supported language (Python,
/// TypeScript/JavaScript).
pub fn index_revision(repo: &Repo, rev: &str, warnings: &mut Vec<String>) -> Result<BTreeMap<String, FileIndex>> {
    let paths: Vec<String> = repo
        .list_files(rev)?
        .into_iter()
        .filter(|p| concord_lang::is_supported_path(p))
        .collect();
    let contents = repo.read_files(rev, &paths)?;
    let mut out = BTreeMap::new();
    for (path, src) in contents {
        match index_source(&path, &src) {
            Ok(f) => {
                if f.has_errors {
                    warnings.push(format!("{rev}:{path}: syntax errors (partial index)"));
                }
                out.insert(path, f);
            }
            Err(e) => warnings.push(format!("{rev}:{path}: {e}")),
        }
    }
    Ok(out)
}

/// Symbol-level footprint of `branch` relative to `base`.
pub fn branch_footprint(
    repo: &Repo,
    base: &str,
    branch: &str,
    base_files: &BTreeMap<String, FileIndex>,
    base_index: &RepoIndex,
    warnings: &mut Vec<String>,
) -> Result<BranchFootprint> {
    Ok(footprint_and_index(repo, base, branch, base_files, base_index, warnings)?.0)
}

/// Like [`branch_footprint`], also returning the symbol index of the branch.
pub fn footprint_and_index(
    repo: &Repo,
    base: &str,
    branch: &str,
    base_files: &BTreeMap<String, FileIndex>,
    base_index: &RepoIndex,
    warnings: &mut Vec<String>,
) -> Result<(BranchFootprint, RepoIndex)> {
    let changed = repo.changed_files(base, branch)?;
    let changed_files: BTreeSet<String> = changed.iter().map(|c| c.path.clone()).collect();
    let py: Vec<&concord_git::ChangedFile> = changed
        .iter()
        .filter(|c| concord_lang::is_supported_path(&c.path))
        .collect();
    let to_read: Vec<String> = py
        .iter()
        .filter(|c| c.status != FileStatus::Deleted)
        .map(|c| c.path.clone())
        .collect();
    let contents = repo.read_files(branch, &to_read)?;

    let mut index = base_index.clone();
    let mut new_files: BTreeMap<String, FileIndex> = BTreeMap::new();
    for c in &py {
        if c.status == FileStatus::Deleted {
            index.remove(&c.path);
            continue;
        }
        let Some(src) = contents.get(&c.path) else { continue };
        match index_source(&c.path, src) {
            Ok(f) => {
                if f.has_errors {
                    warnings.push(format!("{branch}:{}: syntax errors (partial index)", c.path));
                }
                new_files.insert(c.path.clone(), f);
            }
            Err(e) => warnings.push(format!("{branch}:{}: {e}", c.path)),
        }
    }
    for f in new_files.values() {
        index.upsert(f.clone());
    }

    let mut changes: Vec<SymbolChange> = Vec::new();
    for c in &py {
        let before = base_files.get(&c.path).map(|f| f.symbols.as_slice()).unwrap_or(&[]);
        let after = new_files.get(&c.path).map(|f| f.symbols.as_slice()).unwrap_or(&[]);
        changes.extend(diff_symbols(before, after));
    }
    changes.sort_by(|a, b| a.symbol.cmp(&b.symbol));

    let from: BTreeSet<SymbolId> = changes
        .iter()
        .filter_map(|c| match &c.kind {
            concord_core::ChangeKind::Removed => None,
            concord_core::ChangeKind::Renamed { to } => Some(to.clone()),
            _ => Some(c.symbol.clone()),
        })
        .collect();
    let module_level: BTreeSet<String> = new_files.keys().cloned().collect();
    let mut uses = index.uses_from(&from, &module_level);
    // Only use sites the branch wrote or changed are its own risk. A call that
    // already existed at the base is updated by whoever changes the contract
    // (and if both edit the same lines, git reports a textual conflict).
    let existing: BTreeSet<(String, String, String)> = new_files
        .keys()
        .filter_map(|p| base_files.get(p))
        .flat_map(|f| {
            f.refs
                .iter()
                .map(move |r| (f.path.clone(), r.from.0.clone(), r.text.clone()))
        })
        .collect();
    for sites in uses.values_mut() {
        sites.retain(|s| !existing.contains(&(s.path.clone(), s.from.0.clone(), s.text.clone())));
    }
    uses.retain(|_, sites| !sites.is_empty());

    Ok((
        BranchFootprint {
            name: branch.to_owned(),
            changed_files,
            changes,
            uses,
        },
        index,
    ))
}

/// Analyse several branches developed concurrently from `base`.
pub fn analyze_branches(repo: &Repo, base: &str, branches: &[String], textual: bool) -> Result<Analysis> {
    let base_commit = repo
        .rev_parse(base)
        .with_context(|| format!("unknown base revision {base}"))?;
    let mut warnings = Vec::new();
    let base_files = index_revision(repo, &base_commit, &mut warnings)?;
    let base_index = RepoIndex::new(base_files.values().cloned());

    let mut footprints = Vec::new();
    for b in branches {
        repo.rev_parse(b).with_context(|| format!("unknown branch {b}"))?;
        footprints.push(branch_footprint(
            repo,
            &base_commit,
            b,
            &base_files,
            &base_index,
            &mut warnings,
        )?);
    }
    let findings = analyze(&footprints);
    let summary = summarize(&findings);

    let mut merges = Vec::new();
    if textual {
        for i in 0..branches.len() {
            for j in (i + 1)..branches.len() {
                let m = repo.merge_tree(&branches[i], &branches[j])?;
                merges.push(TextualMerge {
                    a: branches[i].clone(),
                    b: branches[j].clone(),
                    clean: m.clean,
                    conflicted_files: m.conflicted_files,
                });
            }
        }
    }
    Ok(Analysis {
        base: base.to_owned(),
        base_commit,
        branches: footprints,
        findings,
        summary,
        textual: merges,
        parse_warnings: warnings,
    })
}

#[derive(Clone, Debug, Serialize)]
pub struct Simulation {
    pub notices: Vec<Notice>,
    pub gates: BTreeMap<String, Gate>,
}

/// Replay the branches through the coherence protocol as if each branch were
/// an agent working concurrently: every footprint is observed, then every
/// agent publishes its contract changes. Returns the notices each agent would
/// have received and whether it could finish.
pub fn simulate(footprints: &[BranchFootprint]) -> Simulation {
    let mut dir = Directory::new();
    let ids: Vec<AgentId> = footprints.iter().map(|f| AgentId::new(f.name.clone())).collect();
    for id in &ids {
        dir.register_agent(id, None);
    }
    let mut notices = Vec::new();
    for (id, f) in ids.iter().zip(footprints) {
        notices.extend(dir.observe_footprint(id, &f.changes, f.uses.clone()));
    }
    for id in &ids {
        notices.extend(dir.publish_all(id));
    }
    let gates = ids.iter().map(|id| (id.0.clone(), dir.finish_gate(id))).collect();
    Simulation { notices, gates }
}
