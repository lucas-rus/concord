---
name: concord-team
description: Split a larger piece of work into parallel tickets and have a team of coding agents implement them at the same time with Concord, then merge the integrated result. Use when the user asks to parallelise work, run several agents at once, or build several features simultaneously.
---

# Running a coordinated agent team with Concord

Concord's MCP tools start a team of coding agents that work in parallel on this repository, each in its own git worktree, and coordinate them while they work: an agent is told when another agent changes a function signature it uses, and when it finishes, finished teammates' overlapping work is merged into its worktree so that it resolves the conflict itself. At the end, all branches are merged into one integration branch and the tests run on it.

1. **Make sure the work is committed.** The agents start from a commit (default `HEAD`); uncommitted changes are invisible to them.
2. **Write the tickets.** One feature per ticket, each self-contained: what to build, in which files or modules, the exact API (function names, signatures, return types), and how to test it. Tickets may touch the same code; Concord handles that. Do not mention the other agents.
3. **Start the team** with `concord_run`: `tasks` (a list of `{name, prompt}`), and `test_cmd`, the command that tests the whole project (e.g. `python -m pytest -q`).
4. **Follow progress** with `concord_status` every few minutes; a team typically needs 5-15 minutes.
5. **Get the result** with `concord_result`. Report to the user which tickets succeeded, whether the integrated tests pass, and the integration branch. Suggest `git merge <branch>`; do not merge without the user's approval.

To check existing branches (for example from pull requests) for conflicts and broken function contracts without running agents, use `concord_analyze`.
