---
name: concord-team
description: Coordinate parallel AI coding agents and contract-verified merges on a git repository using Concord. Use when splitting features across parallel agents, inspecting symbol contracts without reading whole files, checking live git worktrees, or merging branches safely at the contract level.
---

# Contract-Level Parallel Coordination & Merging with Concord

Concord operates at the **symbol/contract level** (functions, methods, classes/structs/traits/interfaces, types) across Python, TypeScript/JavaScript, Rust, and Go rather than raw files or line numbers.

## 1. Direct Contract Inspection & Smart Merge Tools
- **`concord_symbols`**: Inspect symbol signatures and line spans in files or directories without dumping function bodies (~10x fewer tokens than reading whole files).
- **`concord_diff_contracts`**: Compare two git revisions (or `HEAD` vs uncommitted working tree) at the contract level, highlighting breaking vs. compatible signature changes and affected callers.
- **`concord_worktrees`**: Inspect all active `git worktree` checkouts in memory (including uncommitted edits) for cross-worktree contract violations, duplicate definitions, and auto-unionable insertions.
- **`concord_analyze`**: Offline contract, duplicate-symbol, and textual conflict check across two or more git branches.
- **`concord_smart_merge`**: Merge branches in contract-dependency order (producers before consumers), automatically resolving verified pure two-sided insertions, checking post-merge AST call conformance, and bisecting test failures.
- **`concord_negotiate` / `concord_ack`**: Negotiate a leased symbol's signature with its live owner or explicitly acknowledge a peer's contract update.

## 2. Launching a Coordinated Parallel Agent Team
1. **Commit base state**: Agents branch from `HEAD` (or `base`); uncommitted changes are not inherited.
2. **Write self-contained tickets**: One feature per ticket (`{name, prompt, check?}`), specifying target modules, exact public signatures, and how to test. Tickets may touch the same files—Concord coordinates contract changes and region overlaps automatically.
3. **Start the team** with `concord_run` (default `coordination: "regions"` combines contract coherence + region coherence + verified insertion union).
4. **Monitor progress** with `concord_status` and **retrieve the integrated branch + SASE Merge-Readiness Pack (MRP)** with `concord_result`. Ask user approval before merging the integration branch into their working branch.
