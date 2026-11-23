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
