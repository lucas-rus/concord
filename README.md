<h1 align="center">Concord</h1>

<p align="center">
  <strong>Contract-level coherence and smart merging for parallel AI coding agents.</strong>
</p>

<p align="center">
  <a href="https://github.com/lucas-rus/concord/actions/workflows/ci.yml"><img src="https://github.com/lucas-rus/concord/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
  <a href="https://github.com/lucas-rus/concord/releases/latest"><img src="https://img.shields.io/github/v/release/lucas-rus/concord?color=38bdf8&label=release" alt="Latest Release" /></a>
  <a href="#license"><img src="https://img.shields.io/badge/license-MIT%20%7C%20Apache--2.0-818cf8.svg" alt="License" /></a>
  <a href="#install-and-use-it-from-your-agent-mcp"><img src="https://img.shields.io/badge/MCP-12%20tools-34d399.svg" alt="12 MCP Tools" /></a>
  <a href="#benchmark-results"><img src="https://img.shields.io/badge/CommonMark-652%2F652%20(3.1x)-10b981.svg" alt="CommonMark 652/652" /></a>
  <a href="#benchmark-results"><img src="https://img.shields.io/badge/Dispecerat%20TS-48%2F48%20(2.2x)-10b981.svg" alt="Dispecerat 48/48" /></a>
</p>

<p align="center">
  <a href="#the-problem">The Problem</a> ·
  <a href="#the-idea">How It Works</a> ·
  <a href="#benchmark-results">Benchmark Results</a> ·
  <a href="#install-and-use-it-from-your-agent-mcp">Quick Install</a> ·
  <a href="#try-it-from-the-command-line">CLI</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="docs/roadmap.md">Roadmap</a> ·
  <a href="CHANGELOG.md">Changelog</a>
</p>

---

**Concord** is an orchestration engine and MCP server for running multiple AI coding agents (OpenCode, Claude Code, Cursor, ...) **at the same time on the same repository** without them silently breaking each other's work.

> [!NOTE]
> **Jump straight to the [Benchmark Results & Tests below](#benchmark-results):** **652/652 in all 5 runs** on 12-agent Python CommonMark (`3.1×` faster than one agent, `1.6×` faster than an AI integrator, `0` conflicts) and **48/48 (100.0%)** on 6-agent TypeScript `dispecerat` (`2.20×` speedup, `0` conflicts, zero per-ticket test hints).

### Highlights

<table>
  <tr>
    <td width="50%">
      <strong>Contract-Level Coherence</strong><br/>
      Separates symbol <em>contracts</em> (signatures, types, exports, schemas) from <em>bodies</em>. Body edits never block; breaking contract edits trigger surgical notices only to affected callers.
    </td>
    <td width="50%">
      <strong>Region Coherence &amp; Verified Union</strong><br/>
      Detects overlapping edits live, resolves pure two-sided insertions via patience/diff3 + AST verification, and eliminates duplicate symbol definitions before merge.
    </td>
  </tr>
  <tr>
    <td width="50%">
      <strong>Polyglot &amp; Full-Stack Contracts</strong><br/>
      Python, TypeScript/JS (incl. <code>@/</code> aliases), Rust, Go, C/C++, SQL/Prisma/Protobuf/OpenAPI schemas, and cross-language HTTP routes (FastAPI/Express/Next.js ↔ <code>fetch</code>/<code>axios</code>).
    </td>
    <td width="50%">
      <strong>12-Tool MCP Server &amp; One-Line Setup</strong><br/>
      Connects to OpenCode, Claude Code, Cursor, and Antigravity with <code>concord setup</code>. Exposes <code>concord_plan</code>, <code>concord_symbols</code>, <code>concord_worktrees</code>, <code>concord_smart_merge</code>, and <code>concord_run</code>.
    </td>
  </tr>
  <tr>
    <td width="50%">
      <strong>SASE Merge-Readiness Packs &amp; Tier-2 Typecheck</strong><br/>
      Emits auditable <code>mrp.md</code> / <code>mrp.json</code> reports, runs <code>tsc</code>/<code>pyright</code>/<code>cargo check</code>/<code>go vet</code> via <code>--type-check</code>, and bisects test regressions in <em>O</em>(log <em>k</em>).
    </td>
    <td width="50%">
      <strong>Formally Verified Protocol</strong><br/>
      Exhaustive BFS model checking proves Lease Exclusivity, Finish-Gate Soundness, Duplicate-Symbol Safety, and Deadlock-Freedom across reachable agent interleavings.
    </td>
  </tr>
</table>

---

## The problem

Today's parallel-agent setups give each agent its own git worktree. That stops agents from overwriting each other's files, but not from **breaking each other's code**:

- 28% of AI-agent pull requests on GitHub hit merge conflicts ([AgenticFlict](https://arxiv.org/abs/2604.03551)).
- Two cooperating agents succeed about 30% less often than one agent doing both tasks ([CooperBench](https://arxiv.org/abs/2601.13295)).
- The worst failures are invisible to git: patches that ["pass alone but fail together"](https://arxiv.org/abs/2609.25396).

```python
# notify/email.py   (agent A: rate-limit e-mails)     # auth/reset.py   (agent B: password reset)
-def send_email(to, body):                            +def request_reset(user):
+def send_email(to, body, *, user_id):                +    token = tokens.issue(user)
     ...                                              +    send_email(user.email, reset_msg(token))
```

The two agents touch different files, so git merges cleanly, file locks see no conflict, and both branches pass their own tests. The merged program crashes with `TypeError: send_email() missing 1 required keyword-only argument: 'user_id'`. Meanwhile, a third agent that only rewrites the *body* of another function in `notify/email.py` would be blocked by a file lock for no reason.

## The idea

Files are the wrong unit of coordination. Agents break each other through **contracts**: the signatures, types, exported names, routes, and schemas that other code depends on. Concord treats every agent's worktree like a CPU cache and every symbol like a cache line, and runs a **coherence protocol** over them:

1. **Infer, don't ask.** It watches what each agent actually reads and edits (hooks, diffs) and maps it onto a symbol graph, giving a *footprint* per agent: symbols read, written, and used by the agent's new code.
2. **Contract vs. body, breaking vs. compatible.** Every symbol is split into its contract and its body. Body edits never block anyone. Contract changes are classified from structured signatures:
   - **breaking**, e.g. a new required parameter;
   - **compatible**, e.g. a new optional parameter, which only produces an informational notice.

   Only concurrent edits to the *same contract* are serialised, through a short lease.
3. **Precise notices + finish gate.** When a contract changes, Concord sends a short, generated notice only to the agents whose footprint depends on it. The notice has the old and new contract and the recipient's exact call sites. An agent cannot finish while it depends on a stale contract.
4. **Speculative integration.** In the background, all branches are merged in memory, type-checked, and the affected tests are run, to catch whatever the contract model misses.

---

## Benchmark results

### 1. Python — 12 Agents Building a Full CommonMark 0.31.2 Parser ([`concord-bench`](https://github.com/lucas-rus/concord-bench))

Twelve parallel OpenCode agents (`opencode-go/hy4-flash-free`) implement all sections of the CommonMark 0.31.2 Markdown→HTML specification from a minimal paragraph-only scaffold, judged by the official **652-example** CommonMark specification suite. Main study (pre-registered in [PROTOCOL.md](https://github.com/lucas-rus/concord-bench/blob/main/PROTOCOL.md)): `opencode-go/hy4-flash-free`, runs interleaved by round, mean ± SD over n valid runs. Full report with bootstrap CIs, tests and figures: [`concord-bench/results/hy4m`](https://github.com/lucas-rus/concord-bench/blob/main/results/hy4m/report/report.md).

| Condition | n | Spec examples passed (of 652) | Integration loss | Merge conflicts (of 12 branches) | Wall-clock time |
|---|---|---|---|---|---|
| `sequential` (1 agent, ticket by ticket) | 5 | 651.2 ± 0.8 (650–652) | 0 | 0 | 65.1 ± 5.9 min (1.0×) |
| `parallel-off` (12 agents, uncoordinated) | 5 | 199.0 ± 30.1 (161–236) | 301.8 ± 45.8 | 9.0 ± 1.4 | 13.7 ± 1.9 min |
| `parallel-integrator` (12 agents + 13th integrator agent) | 5 | 652 in every run | 0 | 9.4 ± 1.1, resolved by the integrator | 34.2 ± 4.8 min (1.9×) |
| Concord, peer merge (`parallel-concord`: `--mode enforce --peer-merge`) | 3 | 652 in every run | 0 | 0 | 27.1 ± 3.5 min (2.4×) |
| **Concord, region coherence (`parallel-regions`: `--mode enforce --regions`)** | **5** | **652 in every run** | **0** | **0** | **20.8 ± 1.9 min (3.1×)** |

Pre-registered tests (Mann–Whitney U, Holm-corrected, Cliff's δ), region coherence vs: `parallel-off` on examples passed (H1) and integration loss (H2), and `sequential` on wall time (H3): δ = ±1.00, p = 0.03 each. Exploratory: region coherence is faster than the integrator in every pairing (δ = −1.00, p = 0.008, uncorrected). A replication with `opencode-go/deepseek-v4.1-flash` (n = 3 per condition) is running.

### 2. TypeScript 5 (`strict`) — 6 Agents Building `dispecerat` with Zero Per-Ticket Test Hints ([`concord-bench`](https://github.com/lucas-rus/concord-bench))

Exploratory, n = 1 per condition (rescored after a fix to the evaluator's fixtures, see [PROTOCOL.md](https://github.com/lucas-rus/concord-bench/blob/main/PROTOCOL.md)). Six parallel OpenCode agents (`opencode-go/hy4-flash-free`) implement coupled features sharing `lib/types.ts`, `DemoBackend`, and `@/lib/...` tsconfig aliases with **no pre-written feature tests** and **no per-ticket `check` commands** (only `--test-cmd ./scripts/run-tests.sh`), judged by a held-out **48-test** suite (42 per-ticket + 6 cross-ticket runtime synergy tests):

| Condition | Held-out spec (of 48) | Cross-ticket synergy | Integration loss | Merge conflicts | Wall-clock time |
|---|---|---|---|---|---|
| `sequential` (1 agent, ticket-by-ticket) | 48 / 48 (100.0%) | 6 / 6 | 0 | 0 / 6 | 13.3 min (1.0×) |
| `parallel-off` (6 agents, uncoordinated) | 21 / 48 (43.8%) | 0 / 6 | 21 | 3 / 6 | 6.0 min |
| `parallel-fileleases` (6 agents, file locks) | 14 / 48 (29.2%) | 0 / 6 | 25 | 4 / 6 | 7.7 min |
| `parallel-integrator` (6 agents + 7th integrator) | 48 / 48 (100.0%) | 6 / 6 | 0 | 0 / 6 (7th agent) | 11.7 min (1.13×) |
| **`Concord` (`--mode enforce --regions`)** | **48 / 48 (100.0%)** | **6 / 6** | **0** | **0 / 6** | **6.0 min (2.20×)** |

---

## Install and use it from your agent (MCP)

Concord runs locally, next to your repository, as an [MCP](https://modelcontextprotocol.io) server. Your coding agent (OpenCode, Claude Code, Cursor, or any MCP client) gets 12 tools to plan, coordinate, verify, and merge parallel work automatically.

**1. Install & auto-connect** (macOS, Linux):

```bash
# Homebrew (macOS & Linux):
brew install lucas-rus/tap/concordmcp && concord setup

# Or curl installer:
curl -fsSL https://raw.githubusercontent.com/lucas-rus/concord/main/install.sh | sh -s -- --setup
```

*(From source with Rust ≥ 1.90: `cargo install --git https://github.com/lucas-rus/concord concord-cli && concord setup`.)*

**2. Or connect manually:**

OpenCode (one-line CLI, no JSON editing):

```bash
opencode mcp add --global concord -- concord mcp --agent-cli opencode
```

Antigravity (writes `~/.gemini/config/mcp_config.json`; restart Antigravity afterwards):

```bash
concord setup --client antigravity
```

or add it by hand to `~/.gemini/config/mcp_config.json`:

```json
{ "mcpServers": { "concord": { "command": "concord", "args": ["mcp", "--agent-cli", "opencode"] } } }
```

All planning, inspection, and merge tools run directly inside Antigravity. `concord_run` launches its parallel workers with OpenCode or Claude Code (`--agent-cli opencode|claude`), so one of them must be installed to use team runs.

Claude Code (plugin with the MCP server and a `concord-team` skill):

```text
/plugin marketplace add lucas-rus/concord
/plugin install concord@lucas-rus-concord
```

**MCP Tools (12):**
- **Planning, contract inspection & smart merging (for any agent workflow):**
  - `concord_plan` — partition the repository symbol graph into shared contract hubs and producer-first parallel ticket clusters
  - `concord_symbols` — list symbol signatures and line spans without function bodies (~10× fewer tokens than reading whole files; Python, TS/JS, Rust, Go, C/C++, SQL, Prisma, Protobuf, OpenAPI)
  - `concord_diff_contracts` — contract-level diff between revisions or working tree, with impacted callers
  - `concord_worktrees` — zero-mutation live check across all active `git worktree` checkouts (Claude Squad, Cursor worktrees, manual worktrees)
  - `concord_analyze` — check concurrent branches for cross-file contract violations, duplicate symbols, and textual overlaps
  - `concord_smart_merge` — merge branches producer-first with AST-verified insertion union, post-merge call conformance, optional `--type-check`, and $O(\log k)$ test failure bisection
  - `concord_negotiate` / `concord_ack` — propose a signature adjustment to a symbol's live owner or acknowledge a peer's contract change
- **Parallel team orchestration:**
  - `concord_run`, `concord_status`, `concord_result` (includes SASE Merge-Readiness Pack), `concord_cancel`

---

## Try it from the command line

Requires Rust ≥ 1.90 and git ≥ 2.38.

```bash
git clone https://github.com/lucas-rus/concord && cd concord
cargo build --release

# Build the running example: branches agent-a, agent-b, agent-c from a common base.
examples/running-example/setup.sh /tmp/concord-demo

./target/release/concord analyze --repo /tmp/concord-demo --base main agent-a agent-b agent-c --notices
```

<details>
<summary><strong>View example CLI output (cross-file breaking parameter detection)</strong></summary>

```text
Findings
  BREAKING   agent-a -> agent-b: notify.email.send_email   (no shared files: invisible to textual merge and file locks)
             now: def send_email(to, body, *, user_id)
             what: new required parameter `user_id`
             auth/reset.py:11  request_reset: send_email(user.email, reset_msg(token))
  FILE       agent-a & agent-c: notify/email.py   disjoint symbols (a file lock would serialise this needlessly)

Textual merge (git merge-tree)
  agent-a & agent-b: clean
  agent-a & agent-c: clean
  agent-b & agent-c: clean

Protocol simulation (notices each agent would receive)
  -> to agent-b
     [concord] BREAKING contract change: notify.email.send_email   v0 -> v1   (by agent-a)
       before: def send_email(to, body)
       after : def send_email(to, body, *, user_id)
       what  : new required parameter `user_id`
       your code that depends on it:
         auth/reset.py:11  request_reset: send_email(user.email, reset_msg(token))
  Finish gates:
     agent-a: open
     agent-b: BLOCKED (1 unresolved)
     agent-c: open
```

</details>

Other commands:

```bash
concord setup                                   # auto-connect OpenCode, Claude Code, Cursor, and Antigravity MCP
concord plan --rev HEAD                         # identify shared hubs & producer-first parallel ticket clusters
concord symbols src/                            # symbol contracts & spans (Py, TS/JS, Rust, Go, C/C++, SQL/Prisma/Proto/OpenAPI)
concord diff old.py new.py                      # contract vs. body changes between two versions
concord worktrees --base main                   # inspect all active git worktrees (incl. uncommitted edits)
concord typecheck --repo .                      # auto-detect & run Tier-2 checker (tsc, pyright/ty, cargo check, go vet)
concord merge --base main b1 b2 --type-check    # contract-ordered smart merge + verified union + conformance + type check
concord mrp .concord/runs/<run-id>              # render SASE Merge-Readiness Pack (markdown or --json)
concord analyze --base main b1 b2 --json        # machine-readable output (for CI & experiments)
concord run --tasks tasks.json --mode enforce --regions --test-cmd "npm test"
concord run --milestones waves.json --mode enforce --regions --test-cmd "npm test"
```

> [!NOTE]
> **Zero-config acceptance gating:** `tasks.json` only needs `[{"name": "...", "prompt": "..."}]` — when per-ticket `check` commands are omitted, Concord automatically uses `--test-cmd` for finish-gate verification, cross-agent regression gating, and $O(\log k)$ post-merge failure bisection.

### GitHub Cross-PR CI Action

Drop [`action.yml`](action.yml) into any workflow to cross-check concurrently open PRs for silent contract breaks:

```yaml
- uses: lucas-rus/concord@main
  with:
    base: main
```

### Watch a team live

`concord watch` follows a run as it happens: what each agent is doing, who waits for whom, and every coordination step Concord takes.

```bash
concord watch runs/            # the most recent run under the --out of `concord run`
concord watch runs/ --once     # one snapshot, e.g. for a log
```

```text
concord  1795518603-enforce-463d82  1/12 finished  03:07 · running

  agent             state                              edits notices blocks hand-off combined merges
  code-spans        finished                               4       0      0        0        0      0
  emphasis          working                                8       0      0        0        0      0
  images-autolinks  working                                2       0      0        0        1      0
  lists             working                                5       0      0        0        1      0
  ...

  coordination
  02:26  region    raw-html-breaks yields src/cmark/inlines.py to code-spans
  02:36  combine   lists + headings add at the same place in src/cmark/blocks.py
  02:59  done      code-spans finished
```

---

## First results (RQ1, CooperBench gold patches)

On all **560** Python feature pairs of [CooperBench](https://github.com/cooperbench/CooperBench), Concord's textual merge results agree with CooperBench's own gold conflict report on 560/560 pairs. At symbol level:

- **24%** of pairs share files but touch disjoint symbols, so a file lock would serialise them needlessly.
- **12%** are contract-coupled, and only **1%** have call sites that actually violate the new contract.
- Test runs confirmed that interference between *gold* patches is rare: 0 of 48 evaluable jinja pairs fail together. The value of Concord shows on *agent* patches (see the live results above).

Details: [eval/rq1/results.md](eval/rq1/results.md).

---

## Architecture

```mermaid
flowchart LR
  subgraph Agents
    A1[Agent 1<br/>worktree w1]
    A2[Agent 2<br/>worktree w2]
    An[Agent n<br/>worktree wn]
  end
  subgraph Coherence layer
    FT[Footprint tracker]
    SG[Symbol graph<br/>contracts / bodies / uses]
    CD[Coherence directory<br/>leases, versions, notices]
  end
  SI[Speculative integrator<br/>merge-tree, type-check, tests]
  IQ[Integration queue]
  A1 & A2 & An -- hook events, diffs --> FT --> SG --> CD
  CD -. notices, finish gate .-> A1 & A2 & An
  FT --> SI --> CD
  CD --> IQ --> main[(main)]
```

| Crate | Role | Status |
|---|---|---|
| [`concord-core`](crates/concord-core) | Symbols, contract/body change classification, coherence protocol (leases, duplicate-symbol guard, negotiation, finish gate), pairwise conflict analysis, exhaustive BFS formal model checker | Complete |
| [`concord-lang`](crates/concord-lang) | Tree-sitter & schema front-ends: symbols, normalised contracts, body digests, imports, reference resolution, HTTP route linking, affected-test graph | Python, TS/JS, Rust, Go, C/C++, Schemas |
| [`concord-git`](crates/concord-git) | Git plumbing: tree reading, in-memory `merge-tree`, patience/diff3 verified insertion-union, worktree snapshots & listing | Complete |
| [`concord-cli`](crates/concord-cli) | `concord` binary: `setup`, `plan`, `symbols`, `diff`, `analyze`, `worktrees`, `typecheck`, `merge`, `mrp`, `run`, `watch`, `mcp` (12 MCP tools + authenticated TCP daemon) | Complete |

**Languages.** Concord's symbol and contract engine covers **Python** (`.py`, `.pyi`), **TypeScript/JavaScript** (`.ts`, `.tsx`, `.mts`, `.cts`, `.js`, `.jsx`, `.mjs`, `.cjs`, including `@/...` and `~/...` tsconfig root path aliases), **Rust** (`.rs`), **Go** (`.go`), **C/C++** (`.c`, `.h`, `.cc`, `.cpp`, `.cxx`, `.hh`, `.hpp`, `.hxx`), **schema/API contracts** (`.sql`, `.prisma`, `.proto`, OpenAPI `.json`/`.yaml`), and **cross-language HTTP routes** (FastAPI/Flask/Express/Next.js App Router ↔ `fetch`/`axios` calls). Files in other languages still participate in textual/structural merge checks and file-existence guards.

Design: [docs/design.md](docs/design.md) · Tech stack: [docs/tech-stack.md](docs/tech-stack.md) · Related work: [docs/research/related-work.md](docs/research/related-work.md) · Contributing: [CONTRIBUTING.md](CONTRIBUTING.md) · Security: [SECURITY.md](SECURITY.md)

## How it differs from existing tools

| | Unit of coordination | Footprint | Cross-file contract dependencies | Proactive, targeted info |
|---|---|---|---|---|
| Claude Code agent teams | tasks; files by convention | declared by lead | no | free-form messages |
| [Clash](https://github.com/clash-sh/clash) | file (textual merge) | observed diffs | no | pre-edit warning |
| [Grove](https://github.com/NathanDrake2406/grove) | file, hunk, symbol, import | observed diffs | imports/exports | advisory score |
| [Weave](https://github.com/Ataraxy-Labs/weave) / [Aura](https://github.com/Naridon-Inc/aura) | AST entities | declared MCP claims | no (same-file entity merge) | entity claim conflict |
| [Claim Plane](https://github.com/SkeinRank/claim-plane) | declared intents/regions | declared by planner | partially | admission (96.7% serialised) |
| [Gas Town](https://github.com/gastownhall/gastown) Refinery | merge queue | – | at merge time | no |
| **Concord** | **symbols; contract vs. body + regions** | **inferred at run time** | **yes, live + post-merge + HTTP/schema** | **surgical notices + finish gate + MRP** |

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
