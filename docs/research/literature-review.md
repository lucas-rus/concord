# Literature review: what the bibliography says about improving Concord

This file records what the papers in [`../bibliography/references.txt`](../bibliography/references.txt) say about Concord's design: mechanisms, numbers, and baselines. It is the working memory of the project. Read it before designing or changing a mechanism, a baseline or an experiment.

Reviewed on 2026-11-23: sections 1, 2, 3, 6 and 7. Sections 4–5 are added when their review finishes. Numbers come from the full texts unless marked *(abstract only)*.

## 0. Concord against its own proposal (audit, 2026-11-23)

The thesis proposal ([`thesis-proposal/cap3.tex`](../../../thesis-proposal/cap3.tex)) designs four layers. What is implemented:

| Proposal component | Status |
|---|---|
| Observed footprints, tree-sitter symbol graph, contract/body split | implemented (`concord-lang`, `concord-core/change.rs`) |
| Coherence directory: leases, heads-up, LeaseHeld, read misses, stale state, notices with call sites | implemented (`concord-core/coherence.rs`) |
| Finish gate | implemented (`concord-cli/live.rs`) |
| Producer-first integration order | implemented (`run.rs::merge_order`) |
| Peer merge at the finish gate (not in the proposal; added in development) | implemented (`live.rs`) |
| **Speculative integration while agents work** (Alg. 2: background `merge-tree` of all worktrees, TextualConflict notice to both agents) | **missing** |
| Type check of the speculative merge, affected tests, "fails together" notices | **missing** |
| B state: same-symbol body overlap checked by structured merge | **missing** |
| Mergiraf at integration; residual conflicts routed back to the agent | **missing** |
| Publish debounce; `concord_negotiate`; batch-and-bisect merge queue | **missing** |

The proposal itself says that the protocol "does not see behavioural interference or textual overlap inside the same function", and gives this job to speculative integration. In the measurements, contracts-only failed on CooperBench because of exactly that: 29 of 32 runs ended in a textual merge conflict at integration. **What we measured as "contracts-only" lacked its own safety net.**

## 1. Measurements on our own data (2026-11-23)

CooperBench main sample, 32 pairs, Haiku, contracts-only (`enforce`): 29 runs had a textual merge conflict. They involved 40 source files and 20 test files; only one run conflicted on test files alone.

Overlapping hunks between the two branches, classified with `git diff -U0` against the base (141 in total):

| Kind | Hunks |
|---|---|
| Both sides insert at the same place | 71 |
| Both sides modify the same lines | 48, often the same signature or the `__init__` export list |
| One side inserts, the other modifies | 22 |

Re-merged with **Mergiraf 0.20** (syntax-aware merge): **3/29** became clean. Files left in conflict fell from 60 to about 40; Mergiraf removed most conflicts in CHANGES.rst and in tests. The source-code collisions are real same-region edits, so a better merge driver cannot fix them after the fact. They must be coordinated while the agents work.

## 2. Problem papers (section 1)

- **AgenticFlict** (Ogenrwot & Businge): 27.67% of 107,026 agent PRs conflict, 4.36 files and 11.36 regions per conflicting PR. The conflict rate grows with churn: about 9.9% at 2 changed lines, about 30% at 25 lines.
  - Accioly et al. (cited there): conflicts concentrate inside shared method bodies, which is exactly Concord's free class of body edits.
- **Agent PRs** (Xu et al., v2 title "...Merge Conflict Rates"):
  - same-agent pairs conflict 19.8%, cross-agent pairs 41.7%;
  - conflict types: 57.6% content, 26.8% modify/delete, 15.1% add/add. About 42% are structural (file existence), so **contracts should also cover file creation and deletion**.
  - Method: one pair per repository; Wilson confidence intervals.
- **Campos & Murta:**
  - Agents resolving conflicts keep their own side 79.7% of the time (humans 45.1%) and rarely write combined code (3.9% vs 11.4%). After any agent-side resolution, **check that every peer hunk survives**.
  - Conflicts are small: a median of 2 chunks, with sides of about 2–3 lines of code.
- **CooperBench** (Khatua et al.), the most important paper:
  - **The paper's evaluation** merged in three steps: naive merge, then union merge, then a learned resolver (Qwen2.5-Coder-0.5B). For Claude 4.5 without communication, merge success went from 12.27% naive to 26.84% after union to 27.30% after the resolver (Table 6).
  - **Correction (verified in the code, CooperBench @63b9d44):** commit 32ad650 (2026-05-21) **removed the union fallback**; see `src/cooperbench/eval/sandbox.py:220`, "No union fallback — union resolves conflicts by concatenating both sides". If the naive merge conflicts, only the first agent's patch is tested against both test suites. **Our naive-merge scoring matches current CooperBench.** The paper's numbers were scored with union, and the thesis must say so when comparing.
  - Communication reduces conflicts but not success. Conflicts are spatial; success needs semantic agreement on values and defaults.
  - A plan in the first turn lowers the conflict rate from 51.5% to 29.4%. Specific line numbers and file paths also help.
  - Failure symptoms: work overlap 33.2%, divergent architecture 29.7%, repetition 14.7%, unresponsiveness 8.7%. Root causes: expectation 42%, commitment 32%, communication 26%.
  - Recommended (§6.3, §8): "pasted signatures, explicit insertion-point contracts, integration checks before declaring safety". Successful pairs divide work by role and by line ranges ("I will INSERT between lines 14 and 15").
  - App. I (Jinja `groupby`): both agents needed the same signature hunk and disagreed on a default value. A notice carrying the exact final signature would have avoided it.
  - Scaling: 68.6%, 46.5% and 30.0% success with 2, 3 and 4 agents. Retention (cooperative vs solo performance) 0.59.
- **Passes Alone, Fails Together** (Xia et al.): an oracle message of about 130 tokens describing the completed change recovers 82% (real-derived tasks) to 98% (synthetic) of interference.
  - Agents told about a required-argument change sometimes **edit the definition itself and create a textual conflict**. Notices must say "adapt your call sites; do not edit the definition".
  - Recovery from messages sent during the work is untested; Concord can be the first to measure it.
  - Method: Δblind on a common test set; discard agent test edits; count a pair only if each patch passes alone.
- **SE 3.0** (Hassan et al.): its Merge-Readiness Pack is the conceptual twin of the finish gate.
- **SWE-agent:** a guardrail at edit time (reject edits that fail lint, with concise feedback) is worth 3.0 points. **Feedback belongs in the edit tool's result.**
- **SWE-bench:** report "% applies" (cleanly merged) next to "% resolved"; keep notices short, since long context hurts.
- **Agentless:** hierarchical localisation before editing; a "correct location" metric can score footprint coverage.
- **MetaGPT / ChatDev:** coordination by roles (an Architect writes the file list and interface definitions; a Project Manager assigns tasks), but executed **sequentially**. Each paper wins on its own benchmark (they contradict each other), so a coordinator baseline must be re-run in our harness, never cited.
- **OpenHands:** delegation adds little (WebArena 15.3 vs 15.5).

## 3. Coordination mechanisms (section 2)

| Paper | Granularity | Declared or observed | Mechanism | Key result |
|---|---|---|---|---|
| CodeCRDT | character (shared document) | observed | CRDT; TODO claims | 0 textual failures by construction; 5–10% semantic conflicts; 13% slower |
| AgentRoom | file | declared (MCP `room_claim`) | advisory claims, deterministic conflict error | 0.669 vs parallel-merge 0.456 (p=0.003); structured claims 7/7 vs chat claims 1/2. Unclaimed writes correlate *positively* with quality, so a hard block needs an escape path |
| Grite | task | declared | git-native leases plus shared completion state | leases alone are not enough (duplicate work 0.64); leases plus shared state give 0.00 |
| ATM | atom / virtual atom (enclosing block or line range) | declared intents | 7-gate admission; same file with disjoint regions goes through a deterministic composer; split suggestions; compare-and-swap at apply | virtual atoms prevent 8 false-safe rows (ablation) |
| **Claim Plane** | file / region / symbol / contract | declared by an LLM planner | static and dynamic admission | CooperBench, 30 pairs: parallel 23.3%, **static 50.0% but 96.7% serialised**, dynamic 22.2%. Dynamic failed closed in 46/90 runs, 45 of them in a declared file outside the declared region and 15 within 10 lines of it. **Observed footprints fix this.** |
| **CoAgent** | object | declared per tool call | fixed rank; one-way notify-and-repair (lower rank to higher rank) | 93% correct at 1.43× speedup and 1.15× cost vs 2PL (1.04×) and OCC (0.93×, 1.83× cost); not code tasks |
| S-Bus | shard | **observed reads** | validation at commit with a retryable 409 | 0 corruptions; declared read sets over-claim by 32–49%. A shared unit needs a hand-off, not concurrent merge. AutoGen supervisor 88–90% vs 70–74%, but it serialises implicitly |
| Token Coherence | document | declared | real MESI, single writer, TLA+ | single-writer invariant; a 30 s lease timeout reverted valid writes, so **no wall-clock timeouts** |
| **Co-Coder** | file | LLM blueprint | dependency-graph partition, hubs get one owner, dataflow scheduling | 68.1% vs sequential 56.8%, at 0.55× the time; **Claude Code Agent Teams (an LLM lead) was lowest**: 54.1% and 16.3% |

**Separate worktrees (important).** A peer's body edit stays in the peer's branch until integration, so a lock held only for one edit call cannot prevent a textual conflict. A region lease must last until the holder's branch is integrated, or be handed over together with the holder's text. Git also reports adjacent lines as a conflict, so region checks need a margin of one line (Claim Plane's `_regions_overlap` has no margin).

## 4. LLM coordinator baselines: what the evidence says

- Negative in coding:
  - Co-Coder: Agent Teams lowest.
  - AgentRoom: the ChatDev-style pipeline scored 0.333; 7 of 13 runs crashed in the orchestrator.
  - Claim Plane: LLM-planned regions were too coarse and serialised almost everything.
- Positive: S-Bus, where the AutoGen supervisor wins, but by serialising.
- A fair LLM-coordinator baseline must report both success **and** how much work it serialised (Claim Plane's "always serial" arm).
- Design: a plan in MetaGPT style (file and region ownership, exact shared signatures, insertion points, shared defaults), filled into the plan slot, then parallel agents. Same model; count the coordinator's tokens.

## 5. Ideas for Concord's contracts layer, with evidence

1. **Implement the proposal's speculative integration** (Crystal, Palantir, WeCode; proposal Alg. 2): snapshot the live worktrees, `merge-tree` them pairwise during the work, and send a TextualConflict notice with the peer's hunk to both agents.
2. **Region coherence (the B state made real):** observed region leases on body edits. Use the enclosing symbol or a virtual atom (ATM), with a one-line margin, held until the holder is integrated (Token Coherence: no TTL). On overlap, **one-way ordering by rank** (CoAgent): the higher-ranked agent builds on the holder's hunk or moves its code elsewhere. Feedback goes in the edit tool's result (SWE-agent, AgentRoom).
3. **Shared-surface rule:** export lists, registries, option lists and file ends take an exclusive lease and a fixed order (ATM Def. 5, Co-Coder hubs, S-Bus Remark 2).
4. **Finish gate with a dry-run merge:** block until `merge-tree` against finished peers is clean, without merging (S-Bus 409, ATM compare-and-swap, CooperBench "integration checks before declaring safety").
5. **Notices carry the completed change plus the action:** the exact new signature, defaults and parameter order; "update your call sites, do not edit the definition" (Passes, CooperBench App. I).
6. **Files as contract objects:** creation and deletion (AgentPRs: 42% structural).
7. **Mergiraf at integration** for every condition (proposal §spec). Measured effect alone: 3/29.
8. **Evaluation:** naive merge, as current CooperBench does; also report "after structured merge" separately; paired McNemar tests; Wilson intervals; per-repository concentration; fraction serialised; "% applies" vs "% resolved".

## 6. Tools (sections 6–7), read from source

Sources: clash @2ac931c, grove @0fce00c, mergiraf @0151074, gastown @94cc822, aura @8ced4ea, vibe-kanban @3f095f0, CooperBench @63b9d44, and the agent-teams and hooks docs (read 2026-11-23).

- **`git merge-tree --write-tree` runs custom merge drivers** that are configured in `.git/config` plus `.git/info/attributes` (tested with git 2.48). So a structured driver plugs into Concord's `Repo::merge_tree` and `merge_keep_conflicts` without any code change. Use `.git/info/attributes`, not `.gitattributes`, so the setting does not show up in agents' diffs.
- **Clash:** an in-memory merge of **committed HEADs only**; uncommitted work counts only as a file-level "active changes" flag. A PreToolUse hook returns `ask`, which a headless run turns into a deny, so Clash works as a file lock on any file a peer touched.
- **Grove:** includes dirty work. It has five layers (file, hunk within 5 lines, symbol, schema, dependency) and a 500 ms watcher. Two bugs:
  - the hunk layer compares **new-side** line numbers across branches, which are not comparable; base coordinates are needed;
  - its PostToolUse hook prints plain stdout, which Claude Code does not inject; it needs JSON `additionalContext`.
- **Aura** (now Apache-2.0, `Naridon-Inc/aura`; update the thesis text, which calls it a "commercial beta"):
  - awareness at file level, with a 90 s throttle;
  - claims are declared through MCP (`aura_zone_claim`, `aura_snapshot`);
  - its AST merge driver (`src/merge_driver_engine.rs`) keeps both sides when the two sides add **different new top-level named definitions at the same place**. It does not handle top-level assignments or imports, or two methods appended inside the same class.
- **Mergiraf, Python:**
  - Commutative merging covers imports, class members (methods and attributes), `from x import a, b`, keyword arguments, sets and `__all__`.
  - It does **not** resolve: top-level functions appended at the same place (no module group for function definitions; Aura covers exactly this case), top-level constants, statements inserted at the same point of a body, `elif` branches, dict entries, list elements, or new parameters in a `def`.
  - It line-merges first and solves only the conflicting hunks; 10 s timeout per file.
  - Mergiraf and Aura complement each other. Measured on our data, Mergiraf alone fixes 3 of 29.
- **Claude Code agent teams:**
  - shared working tree (overwrites, not merge conflicts);
  - no code-level conflict handling; the docs say "break the work so each teammate owns a different set of files";
  - **does not run in `-p` / headless mode**, so it is not a clean benchmark baseline.
- **Gas Town:**
  - the Mayor is an LLM dispatcher ("file it, sling it");
  - conflicts are found only at merge time, by the Refinery's batch queue, and handed back to the worker (`assign_back`). `auto_rebase` is configured but not implemented;
  - needs tmux, Dolt and beads, so it is not headless. Use it for qualitative related work only.
- **The fair LLM-coordinator baseline is CooperBench `--setting team`:**
  - a lead plus members, a task list, a PLAN.md with disjoint regions;
  - the lead applies the members' patches and resolves the conflicts;
  - headless `claude --print`, with ablation flags (`--team-no-task-list` and others).
  - The lead integrates, so the fair comparison is Concord with peer merge. For concord-bench (OpenCode, Big Pickle) we build the equivalent condition ourselves.
- **Other ideas:**
  - `asyncRewake` hooks: wake an agent with a warning between tool calls;
  - insertion-point arbitration for appends (end of module, class or list): the later agent takes a different anchor;
  - merge-semantics' `duplicate-definition` and `shared-registry-collision` checks.
