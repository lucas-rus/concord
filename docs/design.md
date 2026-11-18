# Design

This document summarises the model and protocol implemented in `concord-core` and how they map to the code. The thesis has the full motivation and the formal treatment.

## Model

- **Symbol** `σ`: a function, method, class or module/class-level variable, identified by its qualified name (`notify.email.send_email`). Code: `concord_core::Symbol`.
- **Contract** `κ(σ)`: the normalised externally visible shape. Code: `concord_lang::python`.
  - For functions: decorators, `async`, name, type parameters, ordered parameters with annotations and defaults, and the return annotation.
  - For classes: decorators, name, bases, annotated fields (dataclass/attrs/pydantic style) and the `__init__` signature.
  - For variables: name and annotation.
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
- `observe_reads(agent, symbols)`: read-only interest. The agent is notified of changes but never gated by them.
- `publish(agent, σ)` / `publish_all(agent)` bump σ's version and send **contract-change notices** to every interested agent, with the recipient's own use sites, and mark them `Stale`. The daemon will call this after a debounce or when the author runs its tests.
- `acknowledge(agent, σ)`: the agent has incorporated the new contract.
- `finish_gate(agent)` lists blockers: stale dependencies (U ∪ W) and contended contracts.
- `release(agent)` drops leases.

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

## Live operation (next milestones)

```text
agent tool call --(Claude Code http/command hook)--> concord-daemon
   PostToolUse(Read/Grep)  -> observe_reads(symbols_in_lines)
   PostToolUse(Edit/Write/Bash) -> snapshot worktree -> diff vs base -> observe_footprint
   debounce / test run      -> publish
   notices                  -> hookSpecificOutput.additionalContext (next tool result)
   Stop hook                -> finish_gate (block with reason while not open)
background: snapshot all worktrees -> merge-tree -> type-check (ty) -> affected tests
```

CooperBench runs each agent in its own container, so the daemon runs on the host. Agents reach it through `http` hooks at `host.docker.internal` (see [eval/README.md](../eval/README.md)).
