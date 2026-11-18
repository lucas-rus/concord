# Tech stack and design decisions

Versions were verified on crates.io, PyPI, npm and GitHub on 2026-11-18. Pin exact versions where the upstream API is unstable.

## Summary

| Concern | Choice | Version | Why / how |
|---|---|---|---|
| Core language | **Rust** (edition 2024) | MSRV 1.90 | ADR-0001 |
| Parsing, contracts, bodies | `tree-sitter` + `tree-sitter-python` | 0.27.2 / 0.25.0 | Stable API, incremental, multi-language (TypeScript later via `tree-sitter-typescript` 0.23.2). The bundled `tags.scm` has no qualified names or parameters, so Concord walks the syntax tree itself. |
| Precise references, type-checking | **ty** (`ty server` over LSP, `ty check`) | 0.0.x (beta, pin) | ADR-0002. LSP `references`, `callHierarchy/incomingCalls`, diagnostics. Fallbacks: basedpyright 1.40.x, pyrefly 1.4.x (stable). |
| LSP client | `async-lsp` + `lsp-types` | 0.2.4 / 0.97 | Drive ty/basedpyright as subprocesses. |
| Git | `git` CLI (≥ 2.38) now, **gitoxide** `gix` later | gix 0.90 (`merge`, `status`, `blob-diff`) | `git merge-tree --write-tree` is the reference behaviour. `gix::Repository::merge_trees` gives in-memory merges without subprocesses. Dirty worktrees are snapshotted with a temporary `GIT_INDEX_FILE`. |
| Structured merge | **Mergiraf** as a subprocess / git merge driver only | 0.20 | GPL-3.0 and not a library, so it stays out of process. |
| Daemon | `tokio` + an HTTP endpoint for hooks | tokio 1.54 | ADR-0003 |
| Event log / state | SQLite via `rusqlite` | 0.43 | Every observation and notice is logged, so runs can be replayed and analysed for the thesis. |
| Agent tool server | MCP via `rmcp` (stdio) | **=3.11.0** (pin; majors churned in 2026) | `concord_status`, `concord_negotiate`, `concord_ack`. |
| Dashboard | `ratatui` TUI (agent × symbol coherence matrix) | 0.30 | For the demo and debugging. |
| Hashing | `blake3` | 1.8 | Contract/body digests. |
| CLI | `clap` 4 | 4.6 | |
| Evaluation harness | Python ≥ 3.12 with `uv` | – | CooperBench is Python. Concord registers as an external CooperBench agent adapter (ADR-0003). |

## ADR-0001: Rust (not C++, not Python) for the core

**Decision:** the engine is written in Rust. The evaluation harness is written in Python.

**Context:** the engine is a long-running daemon on the critical path of every agent tool call (hooks are synchronous). It parses code incrementally, keeps a per-worktree symbol graph, merges trees and serves several agents concurrently.

**Why Rust:**
- **Ecosystem fit.** Every building block exists as a maintained Rust crate:
  - tree-sitter (written in C, with first-class Rust bindings);
  - gitoxide (in-memory merges);
  - `rmcp` (the official MCP SDK);
  - tokio, ratatui and rusqlite.
  - The closest prior art is also Rust: Clash (gix + notify), Grove (gix + tree-sitter) and Mergiraf.
- **Safety and concurrency.** The daemon is a concurrent state machine shared by many agents, and Rust rules out data races at compile time. The protocol core (`concord-core`) is pure and deterministic, so it can be tested exhaustively and later model-checked.
- **Distribution.** A single static binary installs with `cargo install`, so it can be dropped into agent containers.
- **Hook latency.** A `command` hook spawns a process on every tool call. Rust starts in milliseconds; Python adds interpreter start-up to every call.

**Why not C++:** it would match Rust's performance. However:
- there is no maintained MCP SDK, and no gitoxide equivalent (libgit2 has no in-memory `merge-tree --write-tree` equivalent with the same semantics);
- dependency management is weaker;
- memory-safety bugs are a real risk in a long-running daemon that parses untrusted code;
- for a one-person thesis project, the build and tooling overhead buys nothing.

**Why not Python for the core:**
- start-up cost on every hook;
- weaker concurrency;
- harder distribution into containers.

Python stays for the evaluation, where CooperBench and the data analysis already live.

## ADR-0002: layered symbol analysis (tree-sitter first, LSP for precision)

**Decision:** contracts, bodies and a first, cheap resolution of references come from tree-sitter, in process. Precise references and type-checking come from **ty** over LSP (subprocess), with basedpyright or pyrefly as fallback.

**Rationale:**
- The contract/body split needs exact syntax and normalisation, which tree-sitter provides with a stable API.
- Name resolution in Python is hard: dynamic attributes, re-exports, `*` imports.
- Static resolution misses dependencies. Assumption (i) of the protocol needs an over-approximation, so precision matters more than speed for the final check.
- ty is fast and exposes `references`, `callHierarchy` and diagnostics. It is officially beta, so its version is pinned and merged-tree diagnostics are compared against base diagnostics, not used as raw counts.

**Alternatives considered:**
- stack-graphs: **archived** 2025.
- `ruff_python_*` / `ty_*` crates as libraries: published weekly as 0.0.x and explicitly unstable; `ty_server` is not published.
- scip-python: batch-only. It will be used as an *offline oracle* to measure the precision and recall of the resolver.
- Jedi: slower and less precise.

## ADR-0003: host daemon with HTTP hooks (fits CooperBench's one-container-per-agent model)

**Decision:** Concord runs as a host daemon. Agents report tool events through Claude Code hooks:
- `http` hooks to `host.docker.internal` when agents run in containers;
- `command` hooks locally.

The daemon replies with notices through `hookSpecificOutput.additionalContext` and enforces the finish gate in the `Stop` hook. It returns `decision: block` with a reason. The Stop-hook continuation cap is configurable through `CLAUDE_CODE_STOP_HOOK_BLOCK_CAP`.

**Context:**
- CooperBench runs each agent in its **own container**, with no shared filesystem. Its Claude Code adapter runs `claude --print --settings <file>`, so hooks can be injected through that settings file.
- `TaskCompleted` does not fire at the end of headless `-p` runs, so `Stop` is the gate.
- `MultiEdit` is no longer in the Claude Code tool reference. Edits made through `Bash` (sed, heredocs) never reach the Edit/Write hooks, so every write is also confirmed by diffing a worktree snapshot.

**Consequences:**
- The daemon needs each agent's tree. It gets it from hook payloads plus periodic snapshots pushed by a tiny in-container helper, or from CooperBench's `--git` shared remote.
- An alternative runner that puts all agents in one container with worktrees would be simpler, but it deviates from the official protocol. If used, this must be disclosed.

## Licensing

Concord is MIT OR Apache-2.0. Dependencies must be permissive. GPL tools (Mergiraf, the Serena application) may only be invoked as separate programs.
