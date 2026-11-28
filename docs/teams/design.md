# Concord Teams: design draft

> Status: ideation on branch `teams` (2026-11-28). This is not part of the thesis scope: the thesis stays about parallel agents on one machine. Nothing here changes `main` or the binaries the benchmarks use.

## 1. The idea

Two or more people build the same product at the same time. Each has their own AI tool and subscription (Claude Code, Cursor, OpenCode, Antigravity), their own laptop and their own branch. Today they find out that their work clashes only when they merge or open a PR.

Concord Teams runs Concord's coherence protocol across their machines:

- a small local client reports each person's work in progress to a team server;
- the server finds contract breaks and overlapping edits between people while they work;
- each person's AI gets a targeted notice in its own context;
- the people see everything on a live team page.

One-line pitch: *vibecode together without stepping on each other.*

## 2. What changes from one machine to a team

| | `concord run` today | Concord Teams |
|---|---|---|
| Where the work is | worktrees of one repo on one disk; the daemon reads them | each person's clone on their own laptop; the server sees only what the clients send |
| Base | one common base commit | each branch has its own base, and `main` moves as PRs merge |
| Lifecycle | Concord starts the agents, gates them and merges at the end | people start and stop whenever they like, and merge through PRs on GitHub |
| Who may change a worktree | Concord merges peers into worktrees and commits for them | only the owner's own client, and only with their consent |
| Owner of a contested region | the lower task rank | merged work first, then whoever touched it first; leases expire when idle |
| Who edits | agents with hooks | agents with hooks, agents without hooks, and humans in an editor |
| Enforcement | the finish gate holds agents | advice by default; the gate is opt-in per person or project |
| Duration | about 30 minutes | days, so the server must persist state and survive restarts |
| Security | 127.0.0.1 with an optional shared token | internet-facing: TLS, a token per user, several teams per server |

## 3. Architecture

```text
 each member's laptop                                     team server (Romarg VPS)
+-----------------------------------------+              +--------------------------------------+
| AI tool --hooks / plugin / MCP--+       |              | Caddy (TLS) --> concord-server        |
| editor (human edits) --fs events+       |    HTTPS     |   per project:                        |
|                                 v       |   bundles,   |    bare mirror repo (+ WIP refs)      |
|                       concord team up --+--events--->  |    team engine (analyzer / live       |
|                       (local relay)  <--+--SSE notices-|      protocol over snapshot commits)  |
| git clone (own branch) <-- sync (opt-in)|              |    SQLite: users, tokens, sessions,   |
+-----------------------------------------+              |      events, notices                  |
                                                         |   team page (watch.html, extended)    |
 GitHub --webhook: push to main, PR opened/merged------> |   GitHub check runs, Slack/Discord    |
                                                         +--------------------------------------+
```

### 3.1 Local relay: `concord team up`

A small background process per clone. The first hook starts it, the same way a run starts its daemon today.

- **Same hook protocol as today.** It listens on 127.0.0.1 and speaks the protocol of `concord hook --addr` (`live.rs:2315`). Claude Code hooks and the OpenCode plugin (`opencode_plugin.js`) work unchanged; only the address points at the relay instead of a run daemon.
- **It never slows the agent down.** A hook answers at once with the notices already in the local inbox, and snapshots upload in the background. If the server is down, the agent works undisturbed (fail-open, as today).
- **It sees human edits too.** A file watcher on the working tree takes a snapshot about 2 s after the last change (`Repo::snapshot_worktree`, `concord-git/src/lib.rs:326`). So Cursor without hooks, a human in VS Code, and any other tool all count.
- **It sends** snapshot commits (as git bundles, see 3.3), read footprints (file and line ranges of `Read`/`Grep`), heartbeats, and finish-gate questions from `Stop`.
- **It receives** notices over one SSE connection and delivers each one three ways:
  - into the AI's context, as `additionalContext` on the next hook, as today;
  - through a new MCP tool `concord_inbox`, for tools that cannot inject context;
  - as a desktop notification for the human.
- **It acts only on its own clone, and only with consent.** `concord team sync` merges `main` or a teammate's finished work, using the verified insertion-union merge and the conflict-marker flow that already exist.

### 3.2 Team server: `concord-server`

A new crate, so the CLI stays synchronous and light: Tokio and axum for HTTPS and SSE, rusqlite for storage. For each project the server keeps:

- a bare mirror of the GitHub repo (read through a deploy key, later a GitHub App), fetched on every webhook and once a minute;
- work-in-progress snapshots, stored as refs `refs/concord/s/<user>/<session>`;
- a **team engine** that runs the existing analysis over commits (see 3.6);
- a SQLite database: users, tokens, projects, sessions, notices (delivered and acknowledged), and the event log that is `events.jsonl` today.

### 3.3 Getting work in progress to the server: git bundles

1. The client points a local ref at its snapshot and runs `git bundle create - refs/concord/snap ^<commit the server has>`.
2. It uploads the bundle, and the server runs `git fetch <bundle>` into the project's mirror.

A bundle with a few minutes of changes is a few kB. This needs one HTTPS endpoint with token auth and no git server, and it passes through any reverse proxy. If the server lacks a commit the bundle builds on (for example, local commits not yet pushed), it says so and the client resends a wider range. Later we could switch to git smart-HTTP through `git http-backend`.

### 3.4 `main` is a teammate

A merged PR is a finished agent whose work is in `main`. When `main` moves (webhook), the server compares every live session with the new `main`:

- If a contract the session uses changed, the person gets a notice with their own call sites: "PR #42 by Ana merged. It changed `parse_link`, which your branch calls in 3 places. Run `concord team sync`."
- If the session's edits conflict textually with the new `main`, the person is warned before the rebase becomes painful (Crystal's BEHIND/TEXTUAL relationships).

Each session's footprint is measured from its own base, `merge-base(snapshot, main)`. `AgentInfo.base` is already per agent (`live.rs:93`).

Two sessions can have different bases. A textual conflict between them is reported only if the colliding lines on the other side are that person's own changes. Lines that came from `main` belong to the `main` check.

### 3.5 Who owns a region between people

Today the lower task rank owns a contested region (`regions.rs:211`). People have no task order, so:

1. merged work owns over work in progress, and finished work (PR open) over work still going on;
2. otherwise, whoever touched the region first owns it;
3. a lease goes soft after N minutes without a heartbeat ("Ana, idle 2 h"), and then the other side may take it;
4. a person can hand a region over, or ask the owner for it.

### 3.6 Two engines: stateless first, then the live protocol

**Engine A, stateless (the MVP).** On every snapshot, compare it with every other session's latest snapshot and with `main`.

- Tools: the offline analyzer behind `concord analyze` (`analyze_branches`, `lib.rs:182`; `conflict::analyze`, `concord-core/src/conflict.rs:71`), plus `regions::conflicts` for the colliding merge-tree hunks.
- Each finding is sent once, deduplicated by (pair, symbol or file, contract digest).
- With no protocol state to rebuild, it survives restarts and copes with different bases.
- It only advises.

**Engine B, the live protocol (phase 2).** The `live::State` and `Directory` daemon, run over snapshot commits.

- What it adds: contract leases, versions, acknowledgements, read footprints, the waits-for graph and the finish gate. These are needed for enforcement, and for clearing a notice once the person has adapted.
- It needs one seam: split `refresh_inner` (`live.rs:358`) into "get a snapshot commit" and "observe a snapshot". Locally the commit comes from `snapshot_worktree` (line 363); remotely it is the sha just received.
- The finish gate's steps that change worktrees (merging peers in, committing for a peer: `live.rs:1291`, `1709`, `2152` and the hand-offs in `regions.rs`) become **advice** in team mode, carried out by the owner's relay.

### 3.7 Advice first, enforcement by opt-in

Holding someone's AI because of a teammate is more intrusive than holding one's own agents. But Palantír found that people ignored some notifications, so advice needs a gate behind it. So:

- **by default:** notices, the team page and a PR check (everyone sees the soft gate on GitHub);
- **opt-in, per person or project:** the `Stop` hook asks the server's gate and holds the agent while it still uses a teammate's changed contract the wrong way. This is the same finish gate, now across people.

### 3.8 Privacy

- **Full sync (default when self-hosted).** Work-in-progress code goes to the team's own server. That is what makes the merge-tree checks, the verified union and the call-site checks exact.
- **Contracts only (later, for the hosted product or sensitive repos).** The client computes its footprint locally (the engine already runs on the client) and sends only symbol names, signatures, call-site lines, and hunk ranges with hashes. Contract notices still work. Textual overlap becomes approximate: by line ranges, without a verified merge.

## 4. What we reuse and what we build

| Needed | Already in Concord | Work |
|---|---|---|
| Hooks or plugins in each AI tool | Claude Code hooks (`run.rs:243`), OpenCode plugin, MCP server with 12 tools, `concord setup` | point them at the relay; add Cursor hooks (`afterFileEdit`, `stop`) and Antigravity; MCP tools `concord_inbox` and `concord_team` |
| Snapshots of uncommitted work | `Repo::snapshot_worktree` | bundle upload, file watcher |
| Contract breaks between branches | `conflict::analyze`, `compat::classify`, `check_call`, the notice renderer | run on each snapshot, deduplicate, route to people |
| Overlapping edits | `regions::conflicts`, verified union (`virtual_merge_union`) | the ownership rules of 3.5 |
| Live protocol | `live::State`, `Directory` | the snapshot seam, gate actions as advice, a base per session, a size limit on the revision cache (`live.rs:330`, unbounded today: fine for a 30-minute run, a memory leak for a server) |
| Team view | `watch.rs`, `web.rs:34`, the coordination graph in `watch.html` | people and sessions, SSE instead of polling, login |
| Remote access | `serve_with_auth` (`live.rs:2260`) | not safe on the internet as is: plain TCP and one shared token compared with `!=` (`live.rs:2279`). Replace with HTTPS behind Caddy, a token per user, constant-time comparison, size limits |
| PR check | the cross-PR analysis in `action.yml` | a GitHub App check run that also knows live work in progress |
| Merging a teammate's work | `merge_into_union`, the conflict-marker flow | `concord team sync` |

## 5. What the team sees (the multiplayer part)

1. **Live presence, per function.** The team page and `concord team status` show who (a person and their agent) is in which files and functions right now, and which contracts are changing. Like cursors in Figma, but per function.
2. **A team brief when an AI session starts.** A SessionStart or first-prompt hook gives the AI a short brief: "Ana is changing `Session.create` in auth/session.py. Bob's billing API merged 10 minutes ago (new route POST /invoices)." Crystal shows that the cheapest moment to avoid a conflict is before the edit that creates it.
3. **Targeted notices across people.** "Ana's agent changed `send_email(to, body)` to `send_email(to, body, *, user_id)`. Your branch calls it at auth/reset.py:11." They go only to people whose code depends on the change, with their call sites, and are never broadcast (WeCode: report only conflicts that were actually found).
4. **AI-to-AI negotiation across people.** `concord_negotiate` sends a proposed signature change to the owner's agent and to the owner on the team page; their answer is broadcast. None of the tools in `docs/research/related-work.md` do this across people.
5. **Claims and a plan board.** `concord plan` splits upcoming work into producer-first clusters, people claim them, and claims become soft leases that every AI sees.
6. **Sync in one command.** "Bring Ana's finished `parse_link` change into my branch": a verified union merge, with conflict markers only where they are needed.
7. **A PR check that knows live work.** "This PR breaks 2 call sites in Bob's work in progress."
8. **Slack, Discord and desktop notifications** for breaking changes and overlaps between people.

## 6. Plan

### Phase 0: decisions and the server (no code)

- [ ] Check that the Romarg plan meets section 7, point a subdomain at it and install the base stack.
- [ ] Pick the repo to dogfood on: a real project of yours and your friend's.
- [ ] Decide the server's licence (open like the rest of Concord, or source-available for a hosted product), and who owns what between you and your friend, before outside code arrives.

### Phase 1: MVP, awareness only (engine A)

- [ ] `concord-server` crate: projects, a token per user, bundle upload, read events, heartbeat, SSE inbox, team state as JSON, GitHub webhook.
- [ ] Engine A over snapshots, plus `main` tracking (3.4, 3.6), deduplication, and notice wording for people.
- [ ] Client commands:
  - `concord team join <url> --project <p> --token <t>`: writes `.concord/team.toml` and installs hooks and MCP for Claude Code, Cursor, OpenCode and Antigravity;
  - `concord team up`: the relay and the file watcher;
  - `concord team status` and `concord team sync`.
- [ ] Team page: `watch.html` with people and sessions, served over HTTPS with a login.
- [ ] Tests: two clones in temporary directories talking to an in-process server, in the style of `crates/concord-cli/tests/regions.rs`.
- [ ] Deploy to Romarg and dogfood for a week with your friend. Log every notice as useful, noise or missed.

### Phase 2: interaction and opt-in enforcement (engine B)

- [ ] Snapshot seam in `live.rs`, a base per session, gate actions as advice, the region ownership rules of 3.5, lease expiry, a size limit on the revision cache.
- [ ] Opt-in `Stop` gate across people; negotiation on the team page; claims and the plan board.
- [ ] GitHub App check runs; Slack and Discord webhooks; desktop notifications.

### Phase 3: product

- [ ] A hosted version for many teams, GitHub OAuth or SSO, billing per seat.
- [ ] The contracts-only privacy mode, encryption at rest, an audit log.
- [ ] One worker process per project; optional sandboxed test runs on the server (fails-together across people).

## 7. Hosting on the Romarg server

**Needs:**

- a VPS or dedicated server with root SSH; shared cPanel hosting will not work, because it allows no long-running process and no SSE;
- Ubuntu 24.04 or Debian 12, whose packaged git (2.43 and 2.39) is new enough for `merge-tree --write-tree` (2.38 or later);
- 2 vCPU, 4 GB RAM and 40 GB SSD, plenty for a few teams: indexing and merge-tree are the cost, and phase 1 runs nobody's tests;
- a public IPv4 address with ports 22, 80 and 443 open;
- a subdomain, for example `teams.example.com`.

**Setup (once, as root):**

```bash
apt update && apt install -y git caddy sqlite3 ufw   # or Caddy from its own apt repo
git --version                                        # must be >= 2.38
adduser --system --group --home /var/lib/concord concord
ufw allow OpenSSH && ufw allow 80,443/tcp && ufw enable
```

`/etc/caddy/Caddyfile` (Caddy fetches the TLS certificate by itself):

```text
teams.example.com {
    reverse_proxy 127.0.0.1:7420
}
```

`/etc/systemd/system/concord-server.service`:

```ini
[Unit]
Description=Concord Teams server
After=network.target

[Service]
User=concord
ExecStart=/usr/local/bin/concord-server --data /var/lib/concord --listen 127.0.0.1:7420
Restart=always

[Install]
WantedBy=multi-user.target
```

- **Backups:** `/var/lib/concord` (SQLite and the mirrors), nightly.
- **Per project:** a read-only deploy key in the GitHub repo, and a webhook to `https://teams.example.com/v1/github/webhook`.

## 8. Risks and open questions

- **Notices into Cursor.** Cursor has hooks (`afterFileEdit`, `stop`), but a [forum thread](https://forum.cursor.com/t/native-posttooluse-hooks-accept-and-log-additional-context-successfully-but-the-injected-context-is-not-surfaced-to-the-model/155689) reports that context injected after a tool call does not reach the model. Fallbacks:
  - the `stop` hook's follow-up message;
  - the `concord_inbox` MCP tool;
  - a rule file asking the agent to check the inbox.

  All of these must be tested.
- **Noise.** People mute noisy tools quickly. Keep Concord's rule: only real dependents, only real conflicts, each reported once. Measure the noise while dogfooding.
- **Different bases and a moving `main`** are the hardest technical part (3.4). Start with engine A: it recomputes everything from scratch, so it cannot drift.
- **Server load.** Comparing every pair of sessions costs O(sessions²) per snapshot. With the revision cache that is fine up to about 20 live sessions per project. Beyond that, compare only pairs that share files or symbols.
- **Security.**
  - Bundles are untrusted: limit their size, disable hooks in the server's repos, validate refs.
  - Give each user their own token.
  - Never run teammates' code on the server in phase 1.
- **Thesis safety.** The thesis benchmarks measure `live.rs`. Phase-2 changes stay on `teams` until the thesis evaluation is frozen, and the benchmark binaries are never built from this branch.

## 9. Grounding in the literature

- **Palantír** (Sarma et al., TSE 2012): workspace awareness of uncommitted changes across developers, with direct conflicts (same artifact) and indirect ones (signatures). Users found far more conflicts with it: 31/39 indirect conflicts vs 5/39 without.
  - Concord Teams is Palantír's setting, with Concord's precision (contract vs body, breaking vs compatible, call sites) and AI agents as recipients.
  - Its finding that some notifications get ignored motivates the opt-in gate (3.7).
- **Crystal** (Brun et al., FSE 2011): speculative merges of collaborators' states. 93% of textual conflicts grow from a clean state at one specific change, so the time to act is that edit. This motivates snapshots on every edit and the brief at session start.
- **WeCode** (Guimarães & Silva, ICSE 2012): continuous merging of uncommitted changes, reporting only conflicts actually found, without touching anyone's workspace. That is engine A.
- **Optimistic concurrency control** (Kung & Robinson): the opt-in gate is the validation phase, and the PR check is a soft validation everyone sees.
- **Semistructured merge** (Apel et al., FSE 2011): insertions of different declarations at the same place are not real conflicts. The verified union keeps these silent between people too.

## 10. How we would evaluate it

Run the same benchmarks, with each agent acting as a separate person:

- 2 to 4 clones in separate containers, each with a different AI CLI (Claude Code, OpenCode, Cursor CLI);
- all talking to a team server over HTTPS, and merging through PRs.

Compare three setups: PRs without Concord, Concord Teams with advice only, and Concord Teams with the opt-in gate. Use the existing measures: examples passed, integration loss, conflicts, time and tokens.
