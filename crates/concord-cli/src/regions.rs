//! Region coherence: textual overlaps between agents, coordinated while they
//! work (see docs/research/region-coherence.md).
//!
//! The contract layer serialises only signature changes; bodies are free. Two
//! agents that change the same lines (or insert at the same point) then
//! collide when their branches are merged. Region coherence extends the
//! single-writer rule of MESI to such regions, without merging anybody's work
//! into anybody else's worktree:
//!
//! - After every edit, the agent's worktree snapshot is merged in memory
//!   (`git merge-tree`) with every peer's latest snapshot (Crystal / WeCode).
//! - For each colliding pair, one agent owns the region: a finished agent
//!   owns it over a running one; otherwise the lower rank (earlier in the task
//!   list) owns it (CoAgent's fixed order, no livelock). The other agent
//!   *yields* and gets a notice with the owner's lines and new code.
//! - Git conflicts even when one side copies the other's change, so the only
//!   remedies are to move the code at least one unchanged line away, or to
//!   leave the lines alone until the owner has finished and then build on the
//!   owner's version, which Concord merges in cleanly (a hand-off).
//! - The finish gate validates the write set (Kung-Robinson): an agent may
//!   not finish while its work conflicts with a finished peer or with a
//!   running lower-ranked one.

use crate::live::{State, Status};
use concord_core::SymbolKind;
use concord_git::{Hunk, Repo};
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt::Write as _;

/// Most lines of the owner's code quoted per hunk in a notice.
const QUOTE_LINES: usize = 12;

/// One file in which two snapshots conflict, with the colliding hunks.
pub struct FileConflict {
    pub file: String,
    pub mine: Vec<Hunk>,
    pub theirs: Vec<Hunk>,
}

/// Conflicting files between two snapshots, with the hunks that collide.
pub fn conflicts(repo: &Repo, mine: &str, theirs: &str) -> Vec<FileConflict> {
    let Ok(m) = repo.merge_tree(mine, theirs) else {
        return vec![];
    };
    if m.clean {
        return vec![];
    }
    let base = repo.merge_base(mine, theirs).ok();
    m.conflicted_files
        .into_iter()
        .map(|file| {
            let (mut mh, mut th) = match &base {
                Some(b) => (
                    repo.hunks(b, mine, &file).unwrap_or_default(),
                    repo.hunks(b, theirs, &file).unwrap_or_default(),
                ),
                None => (vec![], vec![]),
            };
            let keep_m: Vec<bool> = mh.iter().map(|h| th.iter().any(|t| t.collides(h))).collect();
            let keep_t: Vec<bool> = th.iter().map(|t| mh.iter().any(|h| h.collides(t))).collect();
            if keep_m.iter().any(|k| *k) {
                let mut i = 0;
                mh.retain(|_| (keep_m[i], i += 1).0);
                let mut j = 0;
                th.retain(|_| (keep_t[j], j += 1).0);
            }
            FileConflict {
                file,
                mine: mh,
                theirs: th,
            }
        })
        .collect()
}

/// Whether the collision in this file is only two insertions at the same
/// point. Additions of members commute (Apel's semistructured merge), so
/// Concord resolves these by keeping both, the owner's first, instead of
/// asking anybody to move code: with many agents adding methods to one
/// class, free insertion points run out.
pub fn insert_only(f: &FileConflict) -> bool {
    !f.mine.is_empty()
        && !f.theirs.is_empty()
        && f.mine.iter().chain(&f.theirs).all(|h| h.old_count == 0)
        && f.mine
            .iter()
            .all(|m| f.theirs.iter().any(|t| t.old_start == m.old_start))
        && f.theirs
            .iter()
            .all(|t| f.mine.iter().any(|m| m.old_start == t.old_start))
}

fn symbol_def_count(display: &str) -> usize {
    if let Some((_, tail)) = display.rsplit_once("  (+")
        && let Some(num) = tail.strip_suffix(" more definitions)")
        && let Ok(extra) = num.parse::<usize>()
    {
        return 1 + extra;
    }
    1
}

/// AST-backed symbol definition counts per `(SymbolKind, SymbolId)` for any
/// supported language (Python, TypeScript/JavaScript, Rust, Go). Nested helper
/// functions inside function bodies are ignored by the AST indexers, avoiding
/// false-positive collisions on local closures.
fn definitions(path: &str, text: &str) -> BTreeMap<(SymbolKind, String), usize> {
    let mut out = BTreeMap::new();
    let Some(Ok(idx)) = concord_lang::index_file(path, text) else {
        return out;
    };
    for s in idx.symbols {
        if matches!(s.kind, SymbolKind::Function | SymbolKind::Method | SymbolKind::Class) {
            out.insert((s.kind, s.id.0), symbol_def_count(&s.contract.display));
        }
    }
    out
}

/// Symbols newly defined more times in `merged` than in either `ours` or `theirs`.
pub fn duplicate_definitions(path: &str, merged: &str, ours: &str, theirs: &str) -> Vec<String> {
    let (m, o, t) = (
        definitions(path, merged),
        definitions(path, ours),
        definitions(path, theirs),
    );
    m.into_iter()
        .filter(|(k, n)| *n > 1 && *n > o.get(k).copied().unwrap_or(0).max(t.get(k).copied().unwrap_or(0)))
        .map(|((_, id), _)| id)
        .collect()
}

/// Verification of a union of insertions: the merged file parses, and it
/// does not define anything twice that neither side defined twice (two
/// agents implementing the same method at different places).
pub fn verify_union(path: &str, merged: &str, ours: &str, theirs: &str) -> bool {
    parses(path, merged) && duplicate_definitions(path, merged, ours, theirs).is_empty()
}

/// Whether a file's content still parses (Python, TypeScript/JavaScript, Rust, Go);
/// languages Concord does not parse are accepted.
pub fn parses(path: &str, content: &str) -> bool {
    match concord_lang::index_file(path, content) {
        None => true,
        Some(Ok(f)) => !f.has_errors,
        Some(Err(_)) => false,
    }
}

fn span(h: &Hunk) -> String {
    if h.old_count == 0 {
        format!("inserted after line {}", h.old_start)
    } else if h.old_count == 1 {
        format!("line {}", h.old_start)
    } else {
        format!("lines {}-{}", h.old_start, h.old_start + h.old_count - 1)
    }
}

/// Notice for the yielding agent. `owner_finished` decides the remedy offered.
pub fn render(owner: &str, owner_finished: bool, files: &[FileConflict]) -> String {
    let mut t = format!(
        "[concord] region conflict: your changes overlap code that agent `{owner}` has changed in the same place. \
         Git cannot merge two different changes to the same lines, or two insertions at the same point (not even \
         if one copies the other), so `{owner}` keeps these lines and you must not change them.\n"
    );
    for f in files {
        let _ = writeln!(t, "  {}:", f.file);
        if f.mine.is_empty() && f.theirs.is_empty() {
            let _ = writeln!(t, "    both of you created or deleted this file");
            continue;
        }
        for h in &f.theirs {
            let _ = writeln!(
                t,
                "    `{owner}` changed {} (line numbers of the original file):",
                span(h)
            );
            for l in h.new_lines.iter().take(QUOTE_LINES) {
                let _ = writeln!(t, "      | {l}");
            }
            if h.new_lines.len() > QUOTE_LINES {
                let _ = writeln!(t, "      | ... ({} more lines)", h.new_lines.len() - QUOTE_LINES);
            }
        }
        let mine: Vec<String> = f.mine.iter().map(span).collect();
        let _ = writeln!(t, "    you changed {}", mine.join(", "));
    }
    t.push_str(
        "  What to do: restore those lines of yours to the original text, and put your code at least one \
         unchanged line away from their change (for example after another function or at a different place in \
         the list).",
    );
    if owner_finished {
        t.push_str(
            " If your feature must change these very lines, restore them first and finish again: Concord will then \
             bring their finished version into your worktree and you add your change on top of it.\n",
        );
    } else {
        t.push_str(
            " If your feature must change these very lines, leave them for now and do the rest of your work: \
             when they finish, Concord brings their version into your worktree and you add your change on top.\n",
        );
    }
    t
}

/// Which of two agents owns a contested region: a finished agent over a
/// running one, otherwise the lower rank.
fn owner_of<'a>(st: &'a State, a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    let (fa, fb) = (st.agents[a].status.done(), st.agents[b].status.done());
    let a_owns = match (fa, fb) {
        (true, false) => true,
        (false, true) => false,
        _ => st.agents[a].rank < st.agents[b].rank,
    };
    if a_owns { (a, b) } else { (b, a) }
}

/// Whether two snapshots can be combined cleanly via Concord's verified
/// insertion-union merge (including zero duplicate definitions).
fn can_union_cleanly(repo: &Repo, a: &str, b: &str) -> bool {
    repo.virtual_merge_union(a, b, &verify_union).ok().flatten().is_some()
}

/// After an edit of `agent`: check its latest snapshot against every peer's,
/// and queue a notice for the yielding side of every new collision.
pub fn after_edit(st: &mut State, agent: &str) {
    let Some(me) = st.agents.get(agent).and_then(|a| a.snapshot.clone()) else {
        return;
    };
    let peers: Vec<(String, String)> = st
        .agents
        .values()
        .filter(|p| p.name != agent)
        .filter_map(|p| p.snapshot.clone().map(|s| (p.name.clone(), s)))
        .collect();
    for (peer, snap) in peers {
        if st.repo.is_ancestor(&snap, &me) || st.repo.is_ancestor(&me, &snap) {
            continue;
        }
        let found = conflicts(&st.repo, &me, &snap);
        let (owner, yielder) = owner_of(st, agent, &peer);
        let (owner, yielder) = (owner.to_owned(), yielder.to_owned());
        // Forget alerts of this pair that no longer conflict, so a new
        // collision in the same file is reported again.
        let live: Vec<&str> = found.iter().map(|f| f.file.as_str()).collect();
        st.region_alerted.retain(|(y, o, f)| {
            !(((y == agent && o == &peer) || (y == &peer && o == agent)) && !live.contains(&f.as_str()))
        });
        let new: Vec<FileConflict> = found
            .into_iter()
            .filter(|f| {
                !st.region_alerted
                    .contains(&(yielder.clone(), owner.clone(), f.file.clone()))
            })
            .collect();
        if new.is_empty() {
            continue;
        }
        // Insert/insert collisions that pass verified union merge need no
        // action at all: integration keeps both insertions (union merge).
        let union_ok = new.iter().all(insert_only) && can_union_cleanly(&st.repo, &me, &snap);
        if union_ok {
            for f in &new {
                st.region_alerted
                    .insert((yielder.clone(), owner.clone(), f.file.clone()));
            }
            st.log(
                "region_union",
                json!({ "yielder": yielder, "owner": owner, "files": new.iter().map(|f| &f.file).collect::<Vec<_>>() }),
            );
            continue;
        }
        let new: Vec<FileConflict> = new
            .into_iter()
            .filter(|f| !insert_only(f) || !can_union_cleanly(&st.repo, &me, &snap))
            .collect();
        if new.is_empty() {
            continue;
        }
        // Hunks are oriented from `agent`'s side; flip them for the notice if
        // the peer is the one who yields.
        let new: Vec<FileConflict> = if yielder == agent {
            new
        } else {
            new.into_iter()
                .map(|f| FileConflict {
                    file: f.file,
                    mine: f.theirs,
                    theirs: f.mine,
                })
                .collect()
        };
        for f in &new {
            st.region_alerted
                .insert((yielder.clone(), owner.clone(), f.file.clone()));
        }
        st.region_yields.insert((yielder.clone(), owner.clone()));
        let owner_finished = st.agents[&owner].status.done();
        let text = render(&owner, owner_finished, &new);
        st.log(
            "region_conflict",
            json!({ "yielder": yielder, "owner": owner, "detected_by": agent, "files": new.iter().map(|f| &f.file).collect::<Vec<_>>() }),
        );
        if let Some(a) = st.agents.get_mut(&yielder) {
            a.region_notes.push(text);
            a.stats.region_conflicts += 1;
        }
    }
}

/// A hand-off just merged into an agent's worktree.
pub struct Handoff {
    /// The message for the agent.
    pub text: String,
    /// The agent's commit just before the first merge of the batch.
    pub before: Option<String>,
    /// The agent had yielded lines to one of these owners, so it may still
    /// have to rewrite them on top of the owner's version.
    pub yielded: bool,
}

/// Outcome of the region part of the finish gate.
pub enum Gate {
    Clear,
    /// Conflicts with running owners: wait for them (names), or block with the text.
    Wait(Vec<String>, String),
    Block(String),
    /// Finished owners' work was handed over: progress, not a liveness block.
    Handoff(Handoff),
}

/// The region part of the finish gate for `agent` (write-set validation).
///
/// `may_wait`: the agent is still within its wait budget. Hand-offs are then
/// deferred while owners it yielded to are still running, so that all of them
/// are handed over in one batch (group commit, Gray): every hand-off costs the
/// agent a re-test turn on its whole context.
pub fn gate(st: &mut State, agent: &str, may_wait: bool) -> Gate {
    let wt = st.agents[agent].worktree.clone();
    let me = match Repo::snapshot_worktree(&wt) {
        Ok(c) => c,
        Err(e) => {
            st.log("error", json!({ "agent": agent, "error": e.to_string() }));
            return Gate::Clear;
        }
    };
    let my_rank = st.agents[agent].rank;
    // Order finished peers topologically (producers before consumers) so
    // transitive hand-offs compose cleanly without ordering conflicts.
    let order = crate::run::merge_order(st);
    // Lean: a peer waiting at its own finish gate has done its work and only
    // waits to receive others' code, so its regions are released to the
    // agents waiting on it now (early release, cf. altruistic locking), not
    // when it finishes. Its later changes reach them through its branch, of
    // which the released commit is an ancestor.
    let released =
        |p: &crate::live::AgentInfo| st.lean && p.status == Status::Running && st.waiting.contains_key(&p.name);
    let finished: Vec<(String, std::path::PathBuf, String, bool)> = order
        .into_iter()
        .filter_map(|name| st.agents.get(&name))
        .filter(|p| p.name != agent && (p.status.done() || released(p)))
        .map(|p| (p.name.clone(), p.worktree.clone(), p.branch.clone(), released(p)))
        .collect();
    let mut heads = Vec::new();
    for (peer, p_wt, p_branch, early) in finished {
        let _ = Repo::commit_if_dirty(&p_wt, &format!("agent {peer}: work committed by concord"));
        let Ok(head) = st.repo.rev_parse(&p_branch) else {
            continue;
        };
        if st.repo.is_ancestor(&head, &me) {
            if let Some(a) = st.agents.get_mut(agent) {
                a.merged_producers.insert(peer.clone());
                a.merged_revs.entry(peer).or_insert(head);
            }
            continue;
        }
        if early {
            st.log("region_early_release", json!({ "agent": agent, "owner": peer }));
        }
        let found = conflicts(&st.repo, &me, &head);
        if !found.is_empty() && !can_union_cleanly(&st.repo, &me, &head) {
            st.log(
                "stop",
                json!({ "agent": agent, "decision": "block", "reason": "region_conflict", "owner": peer, "files": found.iter().map(|f| &f.file).collect::<Vec<_>>() }),
            );
            st.region_yields.insert((agent.to_owned(), peer.clone()));
            let real: Vec<FileConflict> = if found.iter().any(|f| !insert_only(f)) {
                found.into_iter().filter(|f| !insert_only(f)).collect()
            } else {
                found
            };
            return Gate::Block(render(&peer, true, &real));
        }
        let union: Vec<String> = found.into_iter().map(|f| f.file).collect();
        heads.push((peer, head, union));
    }
    // Running owners (lower rank): wait for them.
    let running: Vec<(String, String)> = st
        .agents
        .values()
        .filter(|p| p.name != agent && p.status == Status::Running && p.rank < my_rank)
        .filter(|p| !(st.lean && st.waiting.contains_key(&p.name)))
        .filter_map(|p| p.snapshot.clone().map(|s| (p.name.clone(), s)))
        .collect();
    let mut owners = Vec::new();
    let mut text = String::new();
    for (peer, snap) in running {
        if st.repo.is_ancestor(&snap, &me) {
            continue;
        }
        let found = conflicts(&st.repo, &me, &snap);
        if !found.is_empty() && found.iter().all(insert_only) && can_union_cleanly(&st.repo, &me, &snap) {
            // Verified pure insertions at the same points: integration combines them.
            continue;
        } else if !found.is_empty() {
            for f in &found {
                st.region_alerted
                    .insert((agent.to_owned(), peer.clone(), f.file.clone()));
            }
            st.region_yields.insert((agent.to_owned(), peer.clone()));
            text.push_str(&render(&peer, false, &found));
            owners.push(peer);
        } else if st.region_yields.contains(&(agent.to_owned(), peer.clone()))
            && !st.agents[agent].merged_producers.contains(&peer)
        {
            // The agent left the owner's lines alone, possibly restoring its
            // own change to them: it must receive the owner's finished version
            // (and re-apply its change on top) before it may finish.
            let _ = writeln!(
                text,
                "[concord] You yielded lines to agent `{peer}`, which is still working. Do not wait, poll or \
                 schedule anything: finish the rest of your work and try to finish again. When `{peer}` finishes, \
                 Concord merges their version into your worktree, and you then add any change your feature needs \
                 to those lines on top of it."
            );
            owners.push(peer);
        }
    }
    if !owners.is_empty() && may_wait {
        return Gate::Wait(owners, text);
    }
    // Pending hand-offs: this agent yielded lines to these finished peers, or
    // shares insert-only points with them. Bring their versions in sequentially,
    // re-checking against the updated worktree at each step so multi-peer
    // unions never conflict or leak unmerged state.
    let mut handed = Vec::new();
    let mut combined = false;
    let mut before: Option<String> = None;
    let mut yielded_any = false;
    for (peer, head, initial_union) in heads {
        let yielded = st.region_yields.contains(&(agent.to_owned(), peer.clone()));
        if (!yielded && initial_union.is_empty()) || st.agents[agent].merged_producers.contains(&peer) {
            continue;
        }
        let _ = Repo::commit_if_dirty(
            &wt,
            &format!("concord: checkpoint of {agent} before hand-off from {peer}"),
        );
        let cur_wt = Repo::open(&wt).and_then(|r| r.rev_parse("HEAD")).unwrap_or_default();
        if !cur_wt.is_empty() && st.repo.is_ancestor(&head, &cur_wt) {
            let new_base = st.advance_base(&st.agents[agent].base.clone(), &head);
            if let Some(a) = st.agents.get_mut(agent) {
                a.base = new_base;
                a.merged_producers.insert(peer.clone());
                a.merged_revs.insert(peer, head);
            }
            continue;
        }
        if before.is_none() && !cur_wt.is_empty() {
            before = Some(cur_wt.clone());
        }
        let msg = format!("concord: hand-off of {peer}'s regions to {agent}");
        let mut used_union = false;
        let merged = match Repo::merge_into(&wt, &head, &msg) {
            Ok(m) if m.clean => Ok(m),
            Ok(m) => {
                used_union = true;
                Repo::merge_into_union(&wt, &head, &msg, &m.conflicted_files, true, &verify_union)
            }
            Err(e) => Err(e),
        };
        match merged {
            Ok(m) if m.clean => {
                let new_base = st.advance_base(&st.agents[agent].base.clone(), &head);
                if let Some(a) = st.agents.get_mut(agent) {
                    a.base = new_base;
                    a.merged_producers.insert(peer.clone());
                    a.merged_revs.insert(peer.clone(), head.clone());
                    a.stats.merges.push(peer.clone());
                }
                st.log(
                    "region_handoff",
                    json!({ "agent": agent, "owner": peer, "combined": used_union }),
                );
                combined |= used_union;
                yielded_any |= yielded;
                handed.push(format!("`{peer}`"));
            }
            Ok(m) => {
                st.log(
                    "region_handoff_conflict",
                    json!({ "agent": agent, "owner": peer, "files": m.conflicted_files }),
                );
                // If earlier peers were already handed over in this batch, let
                // the agent digest and test those first; otherwise block on
                // this conflict directly (never fall through to Gate::Clear!).
                if !handed.is_empty() {
                    break;
                }
                let latest = Repo::snapshot_worktree(&wt).unwrap_or(me);
                let found = conflicts(&st.repo, &latest, &head);
                st.region_yields.insert((agent.to_owned(), peer.clone()));
                return Gate::Block(render(&peer, true, &found));
            }
            Err(e) => {
                st.log("error", json!({ "agent": agent, "error": e.to_string() }));
                if !handed.is_empty() {
                    break;
                }
                return Gate::Block(format!(
                    "[concord] Could not merge finished agent `{peer}` into your worktree ({e}). \
                     Check `git status`, ensure your worktree is clean, run tests, and finish again."
                ));
            }
        }
    }
    if !handed.is_empty() {
        let _ = st.refresh(agent);
        let how = if combined {
            "Where you and they added code at the same places, both additions are kept (theirs first)."
        } else {
            "The merges were clean: you had left their lines alone."
        };
        return Gate::Handoff(Handoff {
            text: format!(
                "[concord] {} finished, and Concord merged their finished work into your worktree. {how} If your \
                 feature needs to change the lines they own, do it now, on top of their version. Run the tests and \
                 finish.",
                handed.join(", ")
            ),
            before,
            yielded: yielded_any,
        });
    }
    if owners.is_empty() {
        Gate::Clear
    } else {
        Gate::Wait(owners, text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_definitions_are_counted_per_class_and_ignore_nested_closures() {
        let text = "class A:\n    def f(self):\n        pass\n\nclass B:\n    def f(self):\n        pass\n";
        let d = definitions("m.py", text);
        assert_eq!(d[&(SymbolKind::Method, "m.A.f".to_owned())], 1);
        assert_eq!(d[&(SymbolKind::Method, "m.B.f".to_owned())], 1);
        let twice = format!("{text}    def f(self):\n        return 2\n");
        assert!(!verify_union("m.py", &twice, text, text));
        assert_eq!(
            duplicate_definitions("m.py", &twice, text, text),
            vec!["m.B.f".to_owned()]
        );
        assert!(verify_union("m.py", text, text, text));

        // Two functions defining identically-named nested closures must NOT be
        // rejected as duplicate module-level definitions.
        let ours = "def alpha():\n    def helper():\n        return 1\n    return helper()\n";
        let theirs = "def beta():\n    def helper():\n        return 2\n    return helper()\n";
        let merged = format!("{ours}\n{theirs}");
        assert!(verify_union("m.py", &merged, ours, theirs));
    }

    #[test]
    fn verify_union_supports_rust_and_go() {
        let rs_a = "pub fn a() -> i32 { 1 }\n";
        let rs_b = "pub fn b() -> i32 { 2 }\n";
        let rs_dup = "pub fn a() -> i32 { 1 }\npub fn a() -> i32 { 2 }\n";
        assert!(verify_union("src/lib.rs", &format!("{rs_a}{rs_b}"), rs_a, rs_b));
        assert!(!verify_union("src/lib.rs", rs_dup, rs_a, rs_a));

        let go_a = "package p\nfunc A() int { return 1 }\n";
        let go_b = "package p\nfunc B() int { return 2 }\n";
        let go_dup = "package p\nfunc A() int { return 1 }\nfunc A() int { return 2 }\n";
        assert!(verify_union(
            "p/x.go",
            &format!("{go_a}\nfunc B() int {{ return 2 }}\n"),
            go_a,
            go_b
        ));
        assert!(!verify_union("p/x.go", go_dup, go_a, go_a));
    }
}
