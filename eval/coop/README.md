# CooperBench with real agents (RQ2/RQ3)

`run_coop.py` runs two Claude Code agents (Haiku by default) on CooperBench feature pairs through `concord run`, and checks the result with CooperBench's hidden tests. `summarize.py` turns the results into tables and paired significance tests.

## Protocol

For each pair `repo:task:f1:f2`:

1. **Setup.** Both agents start from the task's base commit, each in its own git worktree, at the same time. Each one's prompt is its feature's `feature.md`, plus the command that runs the repository's existing tests.
2. **Run.** The agents run under one of these modes:

   | mode | Concord |
   |---|---|
   | `off` | none: plain parallel agents, the baseline |
   | `enforce` | notices after tool calls, plus the contract finish gate (merges finished producers whose contracts the agent depends on) |
   | `peer` | `enforce` plus `--peer-merge`: at its finish gate, an agent also merges finished peers whose work conflicts textually with its own, and resolves the conflicts itself |

3. **Integrate.** After both agents finish, their branches are merged into an integration worktree in dependency order.
4. **Hidden tests.** Each feature's hidden tests (`tests.patch`) run separately on the merged code, as in CooperBench:
   - the test files are first reset to the base revision, because agents often write their own tests into the same files;
   - the pair **succeeds** only if the merge is clean and both features' tests pass.

The agents never see the hidden tests.

## Differences from the official CooperBench harness

These must be disclosed in the thesis:

- **Environment.** Agents run locally in git worktrees with a per-repository `uv` virtual environment, not in CooperBench's Docker images.
- **Agent.** Agents are Claude Code (`claude -p`), not OpenHands.
- **Communication.** In the `off` baseline the agents have no communication channel at all (CooperBench's "no-comm" setting).

## Running

```sh
C=$HOME/BSc/eval-cache           # durable cache: NOT under /tmp
git clone --depth 1 https://github.com/cooperbench/CooperBench $C/cooperbench
python3 -I eval/coop/run_coop.py --cooperbench $C/cooperbench --cache $C/repos \
    --concord target/release/concord --pairs "$(sed -n 1p eval/coop/data/sample-2026-11-23.txt)" \
    --out $C/runs/jinja
python3 eval/coop/summarize.py $C/runs/*/results.jsonl
```

Keep the cache out of `/tmp`: macOS's periodic cleanup empties files there that have not been accessed for a few days. This silently broke a run during development.

Each repository uses a different cache checkout, so repositories can run in parallel, with one `run_coop.py` per line of the sample file. A pair costs about $0.03–0.10 per mode with Haiku.

## Data

| file | content |
|---|---|
| `data/pilot-2026-11-22.jsonl` | Pilot: 6 pairs (jinja, click, dirty-equals) × 3 modes |
| `data/sample-2026-11-23.txt` | Main sample: 8 pairs per repository (jinja, click, dirty-equals, dspy), drawn with seed `20261123`, excluding the pilot pairs |

## Results columns

| column | meaning |
|---|---|
| `success` | merge clean and both features' hidden tests pass |
| `f1_pass`, `f2_pass` | each feature's hidden tests, on the (possibly partially) merged code |
| `merge_conflict` | integration hit a textual conflict |
| `gate_blocks` | times Concord's finish gate blocked an agent |
| `notices` | notices delivered to agents |
| `merges` | producer and peer merges Concord performed |
| `cost_usd` | total cost for both agents |
| `wall_seconds` | wall time of the whole run |
