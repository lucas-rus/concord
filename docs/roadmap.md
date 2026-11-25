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

- [x] **Compiler / Type-Checker Precision Layer (`concord typecheck` & `concord merge --type-check`)**: Auto-detect and run `tsc --noEmit`, `ty`/`basedpyright`/`pyright`, `cargo check`, or `go vet` both standalone and as a merge-readiness gate in `concord merge` / `concord_smart_merge` (optional LSP/SCIP sidecar remains future work)
- [x] **GitHub Cross-PR Action (`action.yml`)**: Reusable composite GitHub Action running `concord analyze` across all concurrently open PRs targeting `main`
- [x] **Schema & Pseudo-Symbol Contracts (`concord-lang::schema`)**: SQL DDL (`.sql`), Prisma (`.prisma`), Protocol Buffers (`.proto`), and OpenAPI/Swagger specs (`.json`/`.yaml`/`.yml`) indexed into the shared symbol graph (`db.*`, `prisma.*`, `proto.*`, `api.*`)
- [x] **Conflict-Aware Task Partitioner (`concord plan` + `concord_plan` MCP tool)**: Dependency-graph partitioner identifying shared contract hubs and recommending producer-first parallel ticket clusters
- [x] **Authenticated Remote / Container Daemon (`live::serve_with_auth` + `CONCORD_AUTH_TOKEN`)**: Bearer-token authenticated TCP IPC for containerized/remote agent sandboxes (SQLite WAL persistence & mTLS remain future work)
- [ ] **Interactive IDE / Agentic Cockpit Extension**: VS Code / Cursor / Zed panel + `ratatui` TUI visualizing live symbol leases, contract diffs, and one-click MRP approval
- [x] **Formal Protocol Verification (`crates/concord-core/tests/protocol_model_check.rs`)**: Bounded exhaustive BFS state-space model checking of Concord's Coherence Directory proving Lease Exclusivity, Finish-Gate Soundness, Duplicate-Symbol Safety, and Deadlock-Freedom across all reachable interleavings
