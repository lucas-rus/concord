# Design

This document summarises the model and protocol implemented in `concord-core` and how they map to the code. The thesis has the full motivation and the formal treatment.

## Model

- **Symbol** `σ`: a function, method, class or module/class-level variable, identified by its qualified name (`notify.email.send_email`). Code: `concord_core::Symbol`.
- **Contract** `κ(σ)`: the normalised externally visible shape across all four front-ends (`concord_lang::{python, typescript, rust_lang, go_lang}`).
  - For functions/methods: decorators/attributes, `async`, receiver, name, type parameters, ordered parameters with annotations and defaults, and the return annotation.
  - For classes/structs/traits/interfaces: attributes/decorators, name, bases, annotated fields (Python dataclasses/Pydantic, TypeScript constructors/interfaces, Rust struct fields, Go structs/interfaces) and constructor signature.
  - For variables/constants/type aliases: name and type annotation.
- **Body** `β(σ)`: everything else. For classes, only class-level statements count; methods are separate symbols, so a method edit never changes its class.
- **Canonical digests.** Contracts and bodies are hashed (BLAKE3) over the tree-sitter token stream: leaves joined by single spaces, comments removed, string literals kept verbatim. Formatting and comments therefore never produce a change.
- **Change classification.** Code: `concord_core::diff_symbols`.
  - Contract change: the digests differ, or the symbol was removed, or it was renamed (a removed and an added symbol with an identical body in the same file).
  - Body change: same contract, different body.
  - Added symbol.
- **Breaking vs. compatible.** Code: `concord_core::compat::classify`. Front-ends also extract a structured **signature**:
  - for functions: parameters with kind (positional-only, positional-or-keyword, `*args`, keyword-only, `**kwargs`), annotation and default; return annotation; decorators; `async`;
  - for classes: the constructor, taken from `__init__` without `self`, or synthesised from annotated fields; plus bases.

  A contract change is **breaking** if a call site valid against the old signature may be invalid against the new one. Examples: a removed or renamed parameter, a new required parameter, a parameter that became keyword-only, reordered positional parameters, a default removed, a changed annotation or return type, an added or removed decorator, a removed base. Otherwise it is **compatible**: new optional parameters, `*args`/`**kwargs`, relaxed parameter kinds, new bases. Without a signature (e.g. overloads), the rules are conservative and count the change as breaking. Compatible changes still produce informational notices but never close the finish gate. This is similar in spirit to API-breakage checkers such as [Griffe](https://github.com/mkdocstrings/griffe) and [cargo-semver-checks](https://github.com/obi1kenobi/cargo-semver-checks), but applied to in-flight agent work and to call sites in other agents' branches.
- **Uses.** `(τ, σ)` means the declaration or body of `τ` references `σ`: a call, attribute chain, name, import, decorator, base class or annotation. Code: `concord_lang::RepoIndex::resolve`. Resolution order:
  1. `self.x` / `cls.x` resolve to the enclosing class;
  2. import bindings, including `import a.b` chains, aliases, relative imports and `*`;
  3. module-level names;
  4. attribute calls on unknown receivers resolve to a callable of that name, but only if it is unique in the repository.

  This is a cheap over-approximation for the common cases. An LSP-backed resolver (ty / basedpyright) will refine it (see [tech-stack.md](tech-stack.md)). Its precision and recall will be measured against Pyright as part of the thesis.
- **Footprint** of agent `a`, relative to the common base:
  - `W_a`: symbols changed or added;
  - `U_a`: symbols the agent's changed code uses (with use sites);
  - `R_a`: symbols the agent has read, from `Read`/`Grep` tool calls mapped through `RepoIndex::symbols_in_lines`.

## Protocol

Per (agent, symbol) state, with its MESI analogue:

| State | MESI | Meaning | Effect |
|---|---|---|---|
| none | Invalid | no interest | – |
| `Shared` | Shared | reads/uses σ, has seen its latest contract | – |
| `Stale` | Invalid + notice | another agent published a newer, **breaking** contract | notice sent; finish gate closed if σ ∈ U ∪ W |
| `Body` | Modified (body) | changed only σ's body (or added it) | never blocks |
| `Owner` | Modified/Exclusive | holds the contract lease of σ | other contract edits of σ get `LeaseHeld`; dependents get a heads-up |

These are the operations of `concord_core::Directory`, which is pure and deterministic and does no I/O:

- `observe_footprint(agent, changes, uses)` (Algorithm 1 in the thesis):
  - It takes the agent's current footprint. Leases on reverted contract changes are released.
  - The first contract change of σ takes the lease and sends a **heads-up** to the agents that depend on σ. A contract change of σ while another agent holds its lease produces `LeaseHeld` and blocks the finish gate.
  - Symbols that newly enter the footprint are checked for a **read miss**. If another agent already published a newer contract, the agent is told immediately. Its own worktree still has the old code, so without this it would build on stale code.
- `observe_reads(agent, symbols)`: read-only interest (from `Read` line spans and `Grep` symbol matches). The agent is notified of changes but never gated by them.
- **Duplicate-symbol guard**: the first agent to add a new symbol `σ` (`ChangeKind::Added`) claims `defined_by(σ)` even after finishing; any second agent independently adding `σ` receives `NoticeKind::DuplicateSymbol` and is gated (`Blocker::Contended`) until it renames its helper or removes the duplicate.
- `negotiate(from, σ, proposal)`: routes a `NoticeKind::Negotiation` proposal directly to the live lease holder or definer of `σ`.
- `publish(agent, σ)` / `publish_all(agent)` bump `σ`'s version and send **contract-change notices** to every interested agent, with the recipient's own use sites, and mark them `Stale`. Supports immediate or debounced (`--publish-debounce-ms`) publication, flushing immediately on `Bash` test runs or `Stop`.
- `acknowledge(agent, σ)`: the agent has incorporated the new contract (automatically verified via `check_call` or explicitly via `concord_ack`).
- `finish_gate(agent)` lists blockers: stale dependencies (U ∪ W), contended contracts, and duplicate symbol definitions.
- `release(agent)` drops active edit leases while retaining `defined_by` ownership of newly added symbols.

**Stale-contract freedom (to be proven in the thesis).** Assume:
- (i) uses are over-approximated;
- (ii) every contract change is observed before its author completes;
- (iii) every agent passes the finish gate.

Then no integrated program contains a call site written against an outdated contract by an agent that was never informed. Behavioural ("rule") changes inside bodies are out of the protocol's scope and are left to speculative integration.

## Offline analysis

`concord_core::conflict::analyze` compares branch footprints pairwise and reports these findings:

- `ContractDependency` (with `file_disjoint` when the branches share no file, i.e. invisible to git and to file locks);
- `FileOverlap` (with the shared symbols; none means a file lock would serialise needlessly);
- `SameSymbol`;
- `DuplicateDefinition`.

`concord analyze` builds the footprints from git revisions. `--notices` replays them through the protocol (`concord_cli::simulate`). This is the instrument for RQ1 (where do parallel agents break each other?), applied to CooperBench gold patches.

## Live operation & integration (`concord-cli`)

```text
agent tool call --(Claude Code hook / OpenCode plugin)--> live::serve (TCP)
   PostToolUse(Read/Grep)        -> observe_reads
   PostToolUse(Edit/Write/Bash)  -> snapshot worktree -> filter merged_revs -> observe_footprint
                                 -> region coherence check (git merge-tree + verified union)
   notices                       -> hookSpecificOutput.additionalContext / plugin tool result
   Negotiate / Ack (MCP / IPC)   -> route contract proposal / explicit ack
   Stop hook / controller turn   -> finish_gate:
                                      1. contract conformance & producer merge
                                      2. region hand-off (topological order + verified insertion union)
                                      3. per-ticket acceptance check (progress-bounded)
                                      4. optional fails-together check with O(log k) peer bisection
background (--speculative-secs)  -> virtual union merge -> AST & contract conformance check
integration & MRP                -> topological merge + verified union + test bisection -> mrp.{json,md}
```
