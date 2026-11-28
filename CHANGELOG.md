# Changelog

All notable changes to **Concord** are documented in this file.

## [0.2.0] - 2026-11-28
- **Lean gate is the default** in enforce mode (`--no-lean` turns it off; `--lean` is still accepted). A hand-off whose merge breaks none of the agent's acceptance-check tests (run before and after the merge) needs no agent turn; the check sends an agent back at most once; the repair pass after integration covers the rest.
- **`--trim-outputs`** (OpenCode agents): command and search outputs above 8,000 characters keep their head and tail in the context; the full output is saved to a file the agent can search. File reads, edits and fetched web pages are never trimmed.
- **Conflicting region hand-offs leave git's conflict markers** in the agent's worktree and hold it until they are resolved (as peer merge does), instead of asking it to reconcile with a version it cannot see.
- **Repair pass after integration** re-evaluates every ticket's check each round and resumes every still-failing ticket at once (was: the first four of the initial list).
- **Agents that exit without finishing are never waited for**; OpenCode agents get enough turns for every block the gate may give.
- **Bounded checks**: acceptance checks and test commands run with a timeout in their own process group (`CONCORD_CHECK_TIMEOUT_SECS`, default 120 s), so a hanging test cannot stall a run.
- **`concord watch`**: each agent's latest action from its own stream, a Claude Code–style terminal view sized to the terminal with start, now and elapsed time, `--web` for a browser view with a live coordination graph, and following the newest run under a parent directory.

## [0.1.7] - 2026-11-25
- `concord setup --client antigravity` registers the MCP server in Antigravity's global `~/.gemini/config/mcp_config.json`; `auto` detects an Antigravity installation.

## [0.1.6] - 2026-11-25
- **C / C++ front-end** (`tree-sitter-cpp`): `.c`, `.h`, `.cc`, `.cpp`, `.cxx`, `.hh`, `.hpp`, `.hxx` with header prototypes, namespaces, class/out-of-line methods, `#include` bindings, and call-conformance checks.
- **Cross-language HTTP route contracts**: links FastAPI/Flask, Express/Hono/Fastify, and Next.js App Router handlers (`http.<METHOD>.<path>`) directly to frontend `fetch()` and `axios` calls.
- **Multi-wave milestone runner**: `concord run --milestones waves.json` chains parallel agent waves automatically.
- **Tier-2 type-check merge gate**: `concord merge --type-check` and `concord_smart_merge` (`type_check: true`).
- **Authenticated container daemon IPC**: `serve_with_auth` + `CONCORD_AUTH_TOKEN`.
- **Formal protocol verification**: bounded exhaustive BFS model checker verifying Lease Exclusivity, Soundness, Duplicate-Symbol Safety, and Deadlock-Freedom.

## [0.1.5] — 2026-11-25
- Added one-command `concord setup` auto-configurator for OpenCode, Claude Code, and Cursor, plus `install.sh --setup`.
- Fixed single-line SQL `CREATE TABLE (...)` inline column extraction.

## [0.1.4] — 2026-11-25
- Added **Schema & API contracts** (SQL DDL, Prisma, Protobuf, OpenAPI/Swagger).
- Added conflict-aware task partitioner (`concord plan` CLI + `concord_plan` 12th MCP tool).
- Added `concord typecheck` auto-detecting `tsc`, `ty`/`basedpyright`/`pyright`, `cargo check`, and `go vet`.
- Added reusable GitHub Cross-PR CI Action (`action.yml`).

## [0.1.3] — 2026-11-25
- Added Rust & Go front-ends, TypeScript `@/` & `~/` root path-alias resolution, zero-config `--test-cmd` fallback, SASE Merge-Readiness Packs (`concord mrp`), `concord worktrees`, `concord merge`, and 11-tool MCP server.
- Achieved **652/652 (100.0%)** on 12-agent Python CommonMark and **48/48 (100.0%)** on 6-agent TypeScript `dispecerat` (and `Orchestrator + Concord`).
