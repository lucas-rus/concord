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
use concord_git::{Hunk, Repo};
use serde_json::json;
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
    let (fa, fb) = (
        st.agents[a].status == Status::Finished,
        st.agents[b].status == Status::Finished,
    );
    let a_owns = match (fa, fb) {
        (true, false) => true,
        (false, true) => false,
        _ => st.agents[a].rank < st.agents[b].rank,
    };
    if a_owns { (a, b) } else { (b, a) }
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
        let owner_finished = st.agents[&owner].status == Status::Finished;
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

/// Outcome of the region part of the finish gate.
pub enum Gate {
    Clear,
    /// Conflicts with running owners: wait for them (names), or block with the text.
    Wait(Vec<String>, String),
    Block(String),
    /// Finished owners' work was handed over: progress, not a liveness block.
    Handoff(String),
}

/// The region part of the finish gate for `agent` (write-set validation).
pub fn gate(st: &mut State, agent: &str) -> Gate {
    let wt = st.agents[agent].worktree.clone();
    let me = match Repo::snapshot_worktree(&wt) {
        Ok(c) => c,
        Err(e) => {
            st.log("error", json!({ "agent": agent, "error": e.to_string() }));
            return Gate::Clear;
        }
    };
    let my_rank = st.agents[agent].rank;
    // Finished peers first: they can no longer move, so the finishing agent
    // must fit around them (or build on them).
    let finished: Vec<(String, std::path::PathBuf, String)> = st
        .agents
        .values()
        .filter(|p| p.name != agent && p.status == Status::Finished)
        .map(|p| (p.name.clone(), p.worktree.clone(), p.branch.clone()))
        .collect();
    let mut heads = Vec::new();
    for (peer, p_wt, p_branch) in finished {
        let _ = Repo::commit_all(&p_wt, &format!("agent {peer}: work committed by concord"));
        let Ok(head) = st.repo.rev_parse(&p_branch) else {
            continue;
        };
        let found = conflicts(&st.repo, &me, &head);
        if !found.is_empty() {
            st.log(
                "stop",
                json!({ "agent": agent, "decision": "block", "reason": "region_conflict", "owner": peer, "files": found.iter().map(|f| &f.file).collect::<Vec<_>>() }),
            );
            st.region_yields.insert((agent.to_owned(), peer.clone()));
            return Gate::Block(render(&peer, true, &found));
        }
        heads.push((peer, head));
    }
    // Pending hand-offs: this agent yielded lines to these finished peers, and
    // the merges are clean. Bring all their versions in at once so the agent
    // can build on them.
    let mut handed = Vec::new();
    for (peer, head) in heads {
        let yielded = st.region_yields.contains(&(agent.to_owned(), peer.clone()));
        if !yielded || st.agents[agent].merged_producers.contains(&peer) {
            continue;
        }
        let base = st.agents[agent].base.clone();
        let merged = Repo::commit_all(
            &wt,
            &format!("concord: checkpoint of {agent} before hand-off from {peer}"),
        )
        .and_then(|_| Repo::merge_into(&wt, &head, &format!("concord: hand-off of {peer}'s regions to {agent}")));
        match merged {
            Ok(m) if m.clean => {
                let new_base = st.repo.virtual_merge(&base, &head).ok().flatten().unwrap_or(head.clone());
                if let Some(a) = st.agents.get_mut(agent) {
                    a.base = new_base;
                    a.merged_producers.insert(peer.clone());
                    a.stats.merges.push(peer.clone());
                }
                st.log("region_handoff", json!({ "agent": agent, "owner": peer }));
                handed.push(format!("`{peer}`"));
            }
            Ok(m) => st.log(
                "error",
                json!({ "agent": agent, "error": format!("hand-off merge of {peer} conflicted: {:?}", m.conflicted_files) }),
            ),
            Err(e) => st.log("error", json!({ "agent": agent, "error": e.to_string() })),
        }
    }
    if !handed.is_empty() {
        let _ = st.refresh(agent);
        return Gate::Handoff(format!(
            "[concord] {} finished, and Concord merged their finished work into your worktree (clean merges: \
             you had left their lines alone). If your feature needs to change the lines they own, do it now, on top \
             of their version; otherwise nothing is left to do. Run the tests and finish.",
            handed.join(", ")
        ));
    }
    // Running owners (lower rank): wait for them.
    let running: Vec<(String, String)> = st
        .agents
        .values()
        .filter(|p| p.name != agent && p.status != Status::Finished && p.rank < my_rank)
        .filter_map(|p| p.snapshot.clone().map(|s| (p.name.clone(), s)))
        .collect();
    let mut owners = Vec::new();
    let mut text = String::new();
    for (peer, snap) in running {
        let found = conflicts(&st.repo, &me, &snap);
        if !found.is_empty() {
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
    if owners.is_empty() {
        Gate::Clear
    } else {
        Gate::Wait(owners, text)
    }
}
