# Region coherence: design

Status: design, 2026-11-23. Background: [literature-review.md](literature-review.md).

## Problem

Concord's contract layer coordinates *signatures*. Body edits are free ("never block"), which departs from MESI's single-writer rule. On CooperBench, 29 of 32 contracts-only runs then failed on a textual merge conflict at integration:
- 71 of the 141 overlapping hunks were inserts at the same place;
- 48 were the same lines;
- 22 were an insert next to a modification.

Mergiraf fixes only 3 of the 29. The proposal planned speculative integration to catch this (Alg. 2), but it was never built.

## Facts that constrain the design

1. **Worktrees are write-back** (Gray & Cheriton). A region an agent has modified stays in its branch until integration. A lease timeout therefore cannot prevent a conflict; ownership must last until the owner's work is handed over.
2. **Git conflicts on adjacent changes, and also on a "superset" copy of the other side's change.** Tested on git 2.48 (theirs inserts T at a point):

   | Our change at the same point | Result |
   |---|---|
   | insert T then M | conflict |
   | insert M then T | conflict |
   | insert M one unchanged line away | clean |
   | insert M elsewhere | clean |

   So "copy the peer's code" is **not** a remedy. Exactly two remedies produce a clean merge:
   - **move:** put your code at least one unchanged base line away from the peer's region;
   - **hand-off:** leave the region untouched until the owner finishes, receive the owner's finished version through a *clean* merge, then build on top.
3. **Conflicts appear at one specific edit** (Crystal: 93% grow out of a clean state). The cheapest moment to act is right after that edit, while the agent still has the context (SWE-agent: feedback in the edit result).
4. **Awareness alone gets ignored** (Palantír), so a gate is needed behind the notice.
5. **Two-way "both adapt" livelocks; a fixed rank does not** (CoAgent). Locking and abort-and-retry lose almost all the speedup.
6. **Structured merge removes ordering-only conflicts** (Apel; Mergiraf and Aura drivers). Used as the merge driver, it also acts as a classifier: an overlap that structured merge resolves needs no coordination.

## Mechanism

Each agent has a fixed **rank**, its position in the task list. For every pair of agents, the lower-ranked one (first in the list) **owns** any region where their changes collide, and the higher-ranked one **yields**.

1. **Detection after every edit** (Crystal/WeCode, in-memory only).
   - Concord already snapshots the agent's worktree after each edit; it keeps the latest snapshot of every agent.
   - It runs `git merge-tree` of the edited snapshot against each peer's latest snapshot. If a structured merge driver is configured, merge-tree uses it.
   - A conflict that appears between a pair produces a **region notice** to the yielding agent. The notice names:
     - the file;
     - the owner's lines (base coordinates) and the owner's new code;
     - the agent's own overlapping lines;
     - the instruction: *undo your change to these lines and put your code at least one unchanged line away from them. If your feature must change these very lines, finish the rest first: when the owner finishes, Concord brings its version into your worktree and you add your change on top.*
   - The notice is delivered in the result of the edit tool when the yielding agent made the edit, and at its next tool call otherwise.
2. **Finish gate with write-set validation** (Kung–Robinson condition 3).
   - An agent may finish only if its snapshot merges cleanly with:
     - every **finished** peer;
     - every **running lower-ranked** peer.
   - Against a running owner: wait, as for producers today, up to `max_wait`, then block with the region notice.
   - Against a finished owner:
     - first try a **clean hand-off merge** of the owner's branch (the existing `merge_into`, which aborts on conflict). If it is clean, merge it and ask the agent to re-test and add its change on top;
     - if it is not clean, block with the region notice. The agent must restore the contested lines (move or hand-off), then finish again.
   - A higher-ranked running peer never blocks a lower-ranked agent, so waiting cannot form a cycle (CoAgent).
3. **Structured merge driver** (optional, `--merge-driver`): Mergiraf, registered in `.git/config` and `.git/info/attributes`. It applies to detection, to the gate and to final integration. Under fair benchmarking it is enabled in every condition, or in none.

No conflict markers are ever written into an agent's worktree, and no agent resolves another agent's conflict: this is not peer merge. The only merges are clean hand-offs of finished owners, the same thing the contract layer already does for producers.

## Properties (to be argued in the thesis)

- **Clean integration.** If every agent passes the gate, then for every pair one of the two finished second, and its finish was checked against the other's final snapshot. Pairwise-clean merges, integrated in rank order, are clean except for three-way interactions, which the integration step still reports.
- **No deadlock.** Waiting goes only from higher rank to lower rank, so there are no cycles. Waits are bounded by `max_wait` and `MAX_GATE_BLOCKS`.
- **Serialisation only where needed.** Only colliding regions are handed off; everything else stays parallel (MESI: exclusivity costs nothing when a region is not shared).

## Evaluation

- **CooperBench** main sample, 32 pairs. Modes: off, contracts (old), contracts plus regions (new), peer. Naive merge, as current CooperBench does. Report the fraction serialised (waits) and the time.
- **concord-bench** (CommonMark): new condition `parallel-regions`.
- Ablations: regions without contracts; with and without the merge driver.

## Lessons from the first runs (2026-11-23/24)

Each fix below came from a failure observed with real agents (Haiku), and has a regression test in `crates/concord-cli/tests/regions.rs` unless noted.

1. **Cross-layer deadlock.** Contract waits (a consumer waits for a running producer) and region waits (a yielder waits for a running owner) formed a cycle. On CooperBench jinja 1465:7:8 the owner waited for the producer's new signature while the producer waited for the owner's region, about 30 minutes per pair.
   - Fix: one waits-for graph across both layers. In a would-be cycle, the wait toward the lower rank is kept, and the lower-ranked agent stops waiting for a higher-ranked one that waits for it. Every cycle has an edge toward a higher rank, so no cycle survives, and the region owner goes first.
2. **Agents waiting on their own.** A gate block that said "wait a moment and finish again" led an agent to call ScheduleWakeup and poll in 20-minute loops.
   - Fix: the gate never asks the agent to wait. Agents are denied ScheduleWakeup, Cron, Monitor and RemoteTrigger, and the system note says that Concord does all waiting. (No test; the settings are checked by inspection.)
3. **Lost features: the yielder finished before the hand-off.** When both features must change the same lines (both add a parameter to `groupby`), the yielder restored its lines as told. The owner was still running, so the gate saw no conflict and let the yielder finish without ever re-applying its change. The hidden tests of the second feature failed in 4 of 6 pairs.
   - Fix: a yielder with an outstanding yield waits for the owner's hand-off before it may finish. "No conflict" is not "no lost work".
4. **Hand-offs counted as liveness blocks.** With 12 agents, the last-ranked agents received one hand-off per finish attempt and hit the gate block limit (5 of 12 agents). One finished with an unresolved conflict (CommonMark 567/652).
   - Fix: pending hand-offs are merged in one batch and do not count toward the limit. Each finished owner hands over at most once, so this is bounded progress.
   - With fixes 1–4: 644/652, 0 conflicts, no agent at the block limit, 5 contract notices (CommonMark, 12 agents, 10.5 min, $2.83).

Contract-layer fixes found on the same runs (full Concord, 513 notices):
- `re.match(...)` was resolved to the repository's unique `InlineParser.match`. A receiver bound by an import that does not resolve in the repository is now never matched by name.
- Agents were blocked on pre-existing call sites (in their base) of a symbol another agent removed. Conformance now covers only call sites the agent wrote; existing callers are the changing agent's job (as in any refactoring).
