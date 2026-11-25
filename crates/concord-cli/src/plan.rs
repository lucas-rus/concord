//! Conflict-aware task partitioner (`concord plan`).
//!
//! Analyses a repository's symbol dependency graph (`RepoIndex`) to identify:
//! - Shared contract hubs (files with high cross-module fan-in, e.g. `types.ts`)
//! - Coupled module clusters (strongly/connected non-hub files that should be
//!   co-assigned or ordered producer-first)
//! - A topological recommended rank order for parallel tickets.

use crate::index_revision;
use anyhow::Result;
use concord_git::Repo;
use concord_lang::RepoIndex;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize)]
pub struct HubFile {
    pub path: String,
    pub symbol_count: usize,
    pub incoming_file_refs: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModuleCluster {
    pub rank: usize,
    pub files: Vec<String>,
    pub symbol_count: usize,
    pub depends_on_clusters: Vec<usize>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PartitionPlan {
    pub rev: String,
    pub total_files: usize,
    pub total_symbols: usize,
    pub shared_hubs: Vec<HubFile>,
    pub clusters: Vec<ModuleCluster>,
}

pub fn build_plan(repo: &Repo, rev: &str, max_clusters: usize) -> Result<PartitionPlan> {
    let mut warnings = Vec::new();
    let files = index_revision(repo, rev, &mut warnings)?;
    let mut index = RepoIndex::default();
    let mut sym_to_file: BTreeMap<String, String> = BTreeMap::new();
    let mut file_sym_counts: BTreeMap<String, usize> = BTreeMap::new();

    for (path, fi) in &files {
        if concord_lang::is_test_path(path) {
            continue;
        }
        file_sym_counts.insert(path.clone(), fi.symbols.len());
        for s in &fi.symbols {
            sym_to_file.insert(s.id.0.clone(), path.clone());
        }
        index.upsert(fi.clone());
    }

    // Build file -> set of dependency files it calls/references.
    let mut file_deps: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut fan_in: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for path in file_sym_counts.keys() {
        file_deps.entry(path.clone()).or_default();
        fan_in.entry(path.clone()).or_default();
    }

    for (path, fi) in &files {
        if concord_lang::is_test_path(path) {
            continue;
        }
        let from_syms: BTreeSet<concord_core::SymbolId> = fi.symbols.iter().map(|s| s.id.clone()).collect();
        let mod_files: BTreeSet<String> = [path.clone()].into_iter().collect();
        let mut deps = BTreeSet::new();
        for target_id in index.uses_from(&from_syms, &mod_files).keys() {
            if let Some(tpath) = sym_to_file.get(&target_id.0)
                && tpath != path
            {
                deps.insert(tpath.clone());
            }
        }
        for d in &deps {
            fan_in.entry(d.clone()).or_default().insert(path.clone());
        }
        file_deps.insert(path.clone(), deps);
    }

    // Identify shared hubs: files referenced by >= 3 other files (or top 15% fan-in when >= 2).
    let hub_threshold = if file_sym_counts.len() >= 6 { 3 } else { 2 };
    let mut hubs: Vec<HubFile> = fan_in
        .iter()
        .filter(|(_, callers)| callers.len() >= hub_threshold)
        .map(|(path, callers)| HubFile {
            path: path.clone(),
            symbol_count: *file_sym_counts.get(path).unwrap_or(&0),
            incoming_file_refs: callers.len(),
        })
        .collect();
    hubs.sort_by(|a, b| {
        b.incoming_file_refs
            .cmp(&a.incoming_file_refs)
            .then(a.path.cmp(&b.path))
    });
    let hub_set: BTreeSet<String> = hubs.iter().map(|h| h.path.clone()).collect();

    // Group non-hub files into directory/module clusters, splitting or merging to `max_clusters`.
    let non_hub_files: Vec<String> = file_sym_counts
        .keys()
        .filter(|p| !hub_set.contains(*p))
        .cloned()
        .collect();
    let mut raw_groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in &non_hub_files {
        let key = p.rsplit_once('/').map(|(d, _)| d).unwrap_or("root").to_owned();
        raw_groups.entry(key).or_default().push(p.clone());
    }

    // If few directories exist, split by individual files so parallel tickets are visible.
    let groups: Vec<Vec<String>> = if raw_groups.len() <= 2 && non_hub_files.len() > 2 {
        non_hub_files.iter().map(|f| vec![f.clone()]).collect()
    } else {
        raw_groups.into_values().collect()
    };

    let cap = max_clusters.max(1);
    let mut merged_groups: Vec<Vec<String>> = Vec::new();
    for (idx, g) in groups.into_iter().enumerate() {
        if idx < cap {
            merged_groups.push(g);
        } else {
            merged_groups[idx % cap].extend(g);
        }
    }

    // Compute inter-cluster dependencies and sort producers before consumers.
    let mut file_to_cluster: BTreeMap<String, usize> = BTreeMap::new();
    for (ci, g) in merged_groups.iter().enumerate() {
        for f in g {
            file_to_cluster.insert(f.clone(), ci);
        }
    }
    let mut cluster_deps: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); merged_groups.len()];
    for (ci, g) in merged_groups.iter().enumerate() {
        for f in g {
            if let Some(deps) = file_deps.get(f) {
                for d in deps {
                    if let Some(&dc) = file_to_cluster.get(d)
                        && dc != ci
                    {
                        cluster_deps[ci].insert(dc);
                    }
                }
            }
        }
    }

    let mut order = Vec::new();
    let mut remaining: Vec<usize> = (0..merged_groups.len()).collect();
    while !remaining.is_empty() {
        let pos = remaining
            .iter()
            .position(|&c| cluster_deps[c].iter().all(|d| order.contains(d)))
            .unwrap_or(0);
        order.push(remaining.remove(pos));
    }

    let mut old_to_rank = BTreeMap::new();
    for (rank_idx, &old_idx) in order.iter().enumerate() {
        old_to_rank.insert(old_idx, rank_idx + 1);
    }

    let clusters = order
        .into_iter()
        .enumerate()
        .map(|(rank_idx, old_idx)| {
            let mut files_in = merged_groups[old_idx].clone();
            files_in.sort();
            let symbol_count = files_in.iter().map(|f| *file_sym_counts.get(f).unwrap_or(&0)).sum();
            let depends_on_clusters = cluster_deps[old_idx]
                .iter()
                .filter_map(|d| old_to_rank.get(d).copied())
                .collect();
            ModuleCluster {
                rank: rank_idx + 1,
                files: files_in,
                symbol_count,
                depends_on_clusters,
            }
        })
        .collect();

    Ok(PartitionPlan {
        rev: rev.to_owned(),
        total_files: file_sym_counts.len(),
        total_symbols: file_sym_counts.values().sum(),
        shared_hubs: hubs,
        clusters,
    })
}

pub fn render_plan(p: &PartitionPlan) -> String {
    let mut out = format!(
        "Concord Task Partition Plan ({} files, {} symbols at {})\n",
        p.total_files,
        p.total_symbols,
        &p.rev[..p.rev.len().min(10)]
    );
    if !p.shared_hubs.is_empty() {
        out.push_str("\nShared contract hubs (expect Concord contract/region coherence here):\n");
        for h in &p.shared_hubs {
            out.push_str(&format!(
                "  - {} ({} symbols, {} incoming file refs)\n",
                h.path, h.symbol_count, h.incoming_file_refs
            ));
        }
    }
    out.push_str("\nRecommended parallel ticket clusters (producer -> consumer order):\n");
    for c in &p.clusters {
        let deps = if c.depends_on_clusters.is_empty() {
            "independent".to_owned()
        } else {
            format!("depends on {:?}", c.depends_on_clusters)
        };
        out.push_str(&format!(
            "  #{}: {} ({} symbols, {})\n",
            c.rank,
            c.files.join(", "),
            c.symbol_count,
            deps
        ));
    }
    out
}
