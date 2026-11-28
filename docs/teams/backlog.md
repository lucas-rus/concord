# Concord Teams: ideas and known gaps

Status as of concord-team 0.2.0 (2026-11-28). This list is honest on purpose: what each idea is worth, what it costs, and what could go wrong. Ordered roughly by value for the first real team test.

## Ideas

### 1. Shared plans: declare what you are *about to* change

**Idea (from the user).** AI tools increasingly propose a plan or a set of options, and the person picks one (Claude Code's plan mode, "which approach do you want?" questions, todo lists). The chosen plan is the best available statement of intent. Share it with teammates and with Concord *before* the edits start: "Ana's AI plans to change `parse_link` and `auth/session.py`".

**Why it is worth it.**
- It is the earliest possible moment to avoid a clash. Crystal's finding that most conflicts grow from one specific change argues for acting before that change; a plan comes even before the first edit.
- It turns the AI's own planning step into coordination, with no extra work for the person.
- It fills the biggest blind spot today: Concord only sees edits that already happened.

**How it could work.**
- Claude Code: a hook on the plan-approval step (`ExitPlanMode`) and on todo-list updates captures the approved plan text automatically.
- Match the plan to the repo: identifiers and paths in the text are looked up in Concord's symbol index (`concord symbols`), giving a list of intended files and functions.
- Publish it as a *soft intent* with an expiry, next to the real footprint. A teammate's AI then gets "Ana plans to change `parse_link`, which you call" at the start of its own planning, and its plan can avoid it or agree on it first (`concord-team say`).
- When edits arrive, intent turns into real footprint; parts of the plan that never got touched expire.
- Other tools: a manual `concord-team plan "billing: invoices table + POST /invoices"`, and an MCP tool `concord_plan_share` so any agent can declare its plan.

**Honest caveats.**
- Plans are often wrong. AIs change course, and plans name things that end up untouched. So intent must never block anyone or count as "first" in the verdict on its own; it is a heads-up only, until real edits confirm it.
- Matching plain-language text to code is fuzzy. Short or common names (`get`, `utils`) will give false matches, so only show matches that resolve to a specific symbol or file.
- Plan text may contain things a person would not share. Show them what is shared, and allow turning it off.
- Only Claude Code exposes the plan step through hooks today, as far as I know. Other tools need the manual or MCP route.

**Verdict.** Agree. This is the most distinctive idea on the list: nobody else coordinates *intent* across people's AIs. Build it after the first test confirms that edit-level notices are useful and not noisy.

### 2. Check that a teammate joined in the right repo

The invite carries no repo identity. If someone joins in a different repo, their `up` only says "could not read lucas's work". Put a fingerprint in the invite (the root commit of `main`) and make `join` refuse with a clear message. Small, cheap, clearly worth it. **Do before the first test.**

### 3. Bring a teammate's finished work in (`concord-team sync`)

When a verdict says "you adapt to Ana's change", the person often needs Ana's code locally to build on it. In the real Claude Code test, Claude adapted its call to Ana's new signature but noted that its own copy still had the old one. A command (and an AI tool) that merges a teammate's latest snapshot or branch using Concord's verified union merge closes that gap. Risk: merging unfinished work brings in half-done code, so do it only on request and from a commit the teammate has marked ready.

### 4. Talk to the Discord bot in plain language

Today the bot understands only `/team ...` commands. Reading normal messages needs an always-connected bot (Discord gateway), not just the HTTP endpoint. Understanding "what is Ana working on?" also needs a language model call (a cost per message), unless the bot simply answers in a fixed format. Nice to have, not core.

### 5. Messages inside Cursor's chat (and other tools)

Edits from any tool are detected, but only Claude Code shows clashes *inside the AI's chat*. Cursor has hooks (`afterFileEdit`, `stop`). A forum report says text injected after a tool call does not reach its model, so the likely route is the `stop` hook's follow-up message plus an MCP inbox tool. Needs real testing in Cursor. OpenCode can reuse Concord's existing plugin.

### 6. Track files people only read

`concord run` records reads (`Read`/`Grep`), but team mode records only edits. With reads, Concord could warn "Ana changed a function you were reading" before you write code against it. Useful, but noisier. Make it opt-in.

### 7. A web team page

`concord watch --web` already draws a coordination graph for agents. A team version on the relay would show people, functions and clashes live, with the same buttons as Discord. Good for showing the product; less important than Discord and the terminal for daily use.

### 8. Pull-request checks that know live work

A GitHub check on each PR: "this PR breaks 2 call sites in Bob's unfinished work". The relay already has everyone's snapshots; it would need a GitHub App and the `concord` binary on the server.

### 9. Opt-in finish gate across people

Concord's main strength with agents is that an agent cannot finish while its code no longer fits. Across people this is intrusive, so it should be opt-in: Claude Code's `Stop` hook holds *your own* AI until a clash assigned to you is fixed. Needs the live protocol (engine B in `design.md`), not just the stateless check.

### 10. Native Windows

Today Windows needs WSL. The tool itself is nearly portable (file locking is the main Unix-only part), but Concord has no Windows build in its releases.

## Known gaps in what exists (0.2.0)

Honest list of weaknesses in the shipped version.

- **No automated tests for concord-team.** Everything was tested by hand: two clones on one Mac, the Railway relay, a real Claude Code session, and Discord's own endpoint check. It has never run between two different computers. It needs a test suite like `crates/concord-cli/tests` before it grows much more.
- **One relay key opens every team on the relay.** The key travels in invites, so anyone with an invite can reach all teams on that relay (teams are separated by name plus a hash of their team key, not by permissions). Fine for one team; a hosted product needs a key per team.
- **"Who was first" uses each laptop's clock.** Clocks a few seconds apart can flip a near-tie. A tie within a few seconds should count as a tie (decided by name).
- **First-touch times live in the clone.** Deleting `.concord/` or re-cloning resets them, so that person looks "later" than they really were.
- **Big repos are untested.** A snapshot every 2 seconds is cheap on small repos; on a large monorepo it may cost noticeable CPU. It should snapshot on file-change events instead of polling.
- **The AI hears about clashes only on its next action.** Notices arrive with the next tool call or prompt, not instantly while the AI is thinking.
- **Homebrew install needs up-to-date Xcode Command Line Tools,** because the formula has no prebuilt bottle, and newer Homebrew asks you to trust the tap once. The curl installer avoids both.
- **Names must be unique.** Two clones joined with the same name confuse each other; `join` should refuse a name already in use.
- **The terminal view cuts long function lists mid-word.**
- **The Discord bot cannot start private conversations,** so automatic alerts belong in a server channel; in a private chat you ask with `/team clashes`.
