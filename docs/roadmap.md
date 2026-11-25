# Roadmap

Aligned with the BSc thesis work plan and open-source product roadmap.

## M0: Symbol engine, protocol core, offline analyzer ✅

- [x] Rust workspace, CI (`cargo fmt`, `cargo clippy -- -D warnings`, `cargo test --all`)
- [x] `concord-lang`: Python symbols, normalised contracts, body digests, imports, reference resolution (tree-sitter)
- [x] `concord-core`: contract/body/rename classification; coherence directory (leases, heads-ups, read misses, duplicate-symbol detection, contract negotiation, notices, finish gate); pairwise conflict analysis
- [x] `concord-git`: tree reading (`cat-file --batch`), in-memory `merge-tree`, patience/diff3 verified insertion-union (`union_text`, `virtual_merge_union`), dirty-worktree snapshots, `list_worktrees`, `commit_if_dirty`
- [x] `concord analyze --notices` on the running example, with end-to-end tests

## M1: RQ1 analysis on CooperBench ✅

- [x] Breaking vs. compatible contract changes (structured signatures, `compat::classify`, `check_call`)
- [x] `eval/rq1/run_rq1.py`: turn every CooperBench feature pair (`feature<N>/feature.patch`) into branches on the task's base commit and run `concord analyze --json` (textual results match CooperBench's gold conflict report 560/560)
- [x] Report on all 560 Python gold pairs ([eval/rq1/results.md](../eval/rq1/results.md)): 24% symbol-disjoint file overlaps, 12% contract-coupled, 1% silent call-site violations
- [x] Test-based validation (`validate_pairs.py`): precision refinements (attribute resolution, call-site conformance, own call sites only) reduce false-positive silent flags from 15 to 1

## M2: Multi-language front-ends & live coordination engine ✅

- [x] **All 4 CooperBench languages implemented in `concord-lang`**:
  - [x] Python (`.py`, `.pyi`)
  - [x] TypeScript / JavaScript (`.ts`, `.tsx`, `.mts`, `.cts`, `.js`, `.jsx`, `.mjs`, `.cjs`)
  - [x] Rust (`.rs`: functions, structs, enums, traits, `impl` methods, `use` trees, field-init & call conformance)
  - [x] Go (`.go`: functions, receiver methods, structs, interfaces, cross-package imports & selector calls)
- [x] `concord run`: parallel Claude Code (`claude -p`) and OpenCode (`opencode run`) agents in isolated git worktrees, with automatic OpenCode server startup backoff and network transport session resume
- [x] Live daemon (`live.rs`): `PostToolUse` (`Read`, `Grep`, `Edit`, `Write`, `MultiEdit`, `Bash`), `Stop` finish gate, `Negotiate`, `Ack`, `Status`, publish debounce (`--publish-debounce-ms`), structural file-existence conflict alerts
- [x] Region coherence (`regions.rs`): after-edit `merge-tree` region ownership by task rank, AST-verified insertion-union merges (zero nested-closure false positives, zero duplicate symbol definitions), topological multi-peer hand-offs, and `merged_revs` footprint filtering
- [x] Baselines & ablations built into `concord run` and `concord-bench`:
  - [x] Baselines: `sequential`, `parallel-off`, `parallel-integrator`, B3 `--file-leases`, B4 `--overlap-warnings`
  - [x] Ablations: A1 `--all-contracts` (body-as-contract), A2 `--no-notices` (gate-only), A3 default vs. `--fails-together`, A4 `--no-gate` (notices-only), `--no-contracts` (peer-merge-only)

## M3: Speculative verification, Smart Merge, SASE MRPs & MCP Suite ✅

- [x] Symbol-graph affected-test selection (`RepoIndex::affected_tests`)
- [x] `fails-together` verification with $O(\log k)$ binary peer bisection (`together.rs`)
- [x] Continuous background speculative integration (`--speculative-secs`)
- [x] Standalone `concord worktrees`, `concord merge` (contract-ordered smart merge + verified union + post-merge AST conformance + test failure bisection), and `concord mrp`
- [x] Automatic SASE **Merge-Readiness Pack** (`mrp.json` + `mrp.md`) generation covering Functional Completeness, Sound Verification, SE Hygiene, Rationale, and Auditability
- [x] 11-tool MCP server (`concord mcp`): `concord_symbols`, `concord_diff_contracts`, `concord_analyze`, `concord_worktrees`, `concord_smart_merge`, `concord_negotiate`, `concord_ack`, `concord_run`, `concord_status`, `concord_result`, `concord_cancel`

## M4: Post-Thesis & Open-Source v1.0 Roadmap (Future Use Cases)

- [ ] **Compiler / LSP / SCIP Precision Layer (Opt-in Tier 2)**: Optional `ty` / `basedpyright` (Python), `typescript-language-server` (TS), `rust-analyzer` (Rust), and `gopls` (Go) enrichment for polymorphic receiver types and inferred return types while keeping sub-50ms Tree-sitter Tier 1 default
- [ ] **GitHub App & CI Cross-PR Action (`concord-action`)**: Run `concord analyze` + `concord merge --dry-run` automatically across all concurrently open PRs in a repository (human + Devin/Codex/Dependabot/Claude PRs), posting a live SASE Merge-Readiness Pack comment before any PR merges
- [ ] **Schema & Pseudo-Symbol Contracts**: Extend `concord-lang` to OpenAPI/Swagger specs, GraphQL schemas, Prisma/SQL migrations, Protobufs, and HTTP route tables so backend and frontend agents coordinate API contracts automatically
- [ ] **Conflict-Aware Task Partitioner (`concord plan`)**: Pre-execution task splitter (inspired by Co-Coder) using `RepoIndex` symbol-coupling centrality to group tightly coupled symbols into the same ticket and maximize parallel speedup
- [ ] **Distributed Cloud / Container Daemon**: Authenticated Unix-socket / mTLS HTTP transport + SQLite WAL event persistence so remote containerized agents (SWE-bench / OpenHands sandboxes / Kubernetes runners) share one live coherence directory
- [ ] **Interactive IDE / Agentic Cockpit Extension**: VS Code / Cursor / Zed panel + `ratatui` TUI visualizing live symbol leases, contract diffs, and one-click MRP approval
- [ ] **Formal Protocol Verification**: TLA+ / Stateright model checking of the combined Contract + Region + Waits-For rank-acyclic state machine
