# Roadmap

Aligned with the thesis work plan (defence: summer 2027).

## M0: Symbol engine, protocol core, offline analyzer (Nov 2026) ✅

- [x] Rust workspace, CI (fmt, clippy, tests)
- [x] `concord-lang`: Python symbols, normalised contracts, body digests, imports, reference resolution (tree-sitter)
- [x] `concord-core`: contract/body/rename classification; coherence directory (leases, heads-ups, read misses, notices, finish gate); pairwise conflict analysis
- [x] `concord-git`: tree reading (`cat-file --batch`), in-memory `merge-tree`, dirty-worktree snapshots
- [x] `concord analyze --notices` on the running example, with an end-to-end test

## M1: RQ1 analysis on CooperBench (Dec 2026)

- [x] Breaking vs. compatible contract changes (structured signatures, `compat::classify`)
- [x] `eval/rq1/run_rq1.py`: turn every CooperBench feature pair (`feature<N>/feature.patch`) into branches on the task's base commit and run `concord analyze --json` (textual results match CooperBench's gold conflict report)
- [x] Report on all 560 Python gold pairs ([eval/rq1/results.md](../eval/rq1/results.md)): 24% symbol-disjoint file overlaps, 47% contract-coupled but only 13% breaking, 15 silent breaking couplings
- [ ] Validate the 15 silent breaking couplings by running both features' tests on the clean merge (CooperBench Docker images)
- [ ] Resolver quality: precision/recall of `RepoIndex` uses against scip-python / basedpyright on the 9 Python repos

## M2: Live daemon and Claude Code integration ✅ (pulled forward, Nov 2026)

- [x] `concord run`: parallel Claude Code agents (headless, Haiku) in worktrees; hooks; integration + tests ([eval/live](../eval/live/README.md))
- [x] Daemon: PostToolUse → reads/footprints/notices via `additionalContext`; Stop → finish gate with producer merge and call-site conformance
- [x] Resume of agents that became stale after finishing
- [x] Shop scenario: integrated tests pass 3/3 with Concord vs. 0/3 without

## M2 (original plan, remaining items)

- [ ] `concord-daemon` (tokio): per-agent worktree registry, hook endpoint (HTTP + `concord hook` command shim), SQLite event log
- [ ] PostToolUse/PostToolBatch: `Read`/`Grep` → `observe_reads`; `Edit`/`Write`/`Bash` → snapshot + diff → `observe_footprint`
- [ ] Notices via `additionalContext`; finish gate via `Stop` hook; debounce/publish policy
- [ ] Automatic acknowledgement when the listed use sites change and type-check
- [ ] `concord run`: spawn N Claude Code agents in worktrees with hooks installed

## M3: Precise resolution and speculative integration (Feb–Mar 2027)

- [ ] ty LSP client (`references`, `callHierarchy`) with basedpyright fallback; merge with tree-sitter results
- [ ] Background speculative integration: snapshot all worktrees → merge → `ty check` diff vs. base → affected tests (selected via the symbol graph) → "fails together" reports
- [ ] Integration queue in contract-dependency order; Mergiraf as merge driver; batch testing with bisection
- [ ] MCP server (`rmcp`): `concord_status`, `concord_negotiate`, `concord_ack`
- [ ] Baselines B3 (file claims) and B4 (overlap warnings) as Concord modes

## M4: Experiments (Apr 2027)

- [ ] CooperBench external adapter (host daemon reachable from agent containers)
- [ ] Conditions B0–B4, Concord, ablations A1–A4 (see [eval/README.md](../eval/README.md))
- [ ] Pilot → subset sizes; main runs; statistics (McNemar, clustered bootstrap)

## M5: Stretch (May 2027)

- [ ] TypeScript front-end (tree-sitter-typescript; `tsc` for type-checking)
- [ ] Pseudo-symbols: config keys, routes, DB schemas
- [ ] Conflict-aware dispatch (predicted footprints)
- [ ] TLA+ model of the protocol; `ratatui` dashboard
