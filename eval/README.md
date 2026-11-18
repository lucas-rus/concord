# Evaluation plan

The evaluation answers the thesis research questions on [CooperBench](https://github.com/cooperbench/CooperBench) (652 two-feature tasks, 12 repos; Python repos first) and on the interference tasks of [Passes Alone, Fails Together](https://arxiv.org/abs/2609.25396).

## RQ1: where do parallel agents break each other? (offline, no LLM calls)

Each CooperBench task directory looks like `dataset/<repo>_task/task<id>/`, with `feature<N>/feature.patch`, `tests.patch`, `combined.patch`, `setup.sh` and `run_tests.sh`. For every feature pair:

1. Check out the task's base commit and create one branch per feature by applying `feature.patch`.
2. Run `concord analyze --base <base> feat-1 feat-2 --json`.
3. Aggregate the `summary` fields:
   - `file_overlaps_symbol_disjoint / file_overlaps`: how often file-level coordination would serialise needlessly;
   - `contract_dependencies_file_disjoint`: conflicts invisible to git and file locks;
   - `same_symbol_both_body_only`: overlaps a structured merge could resolve.

The same pipeline is then applied to the *agents'* patches from baseline runs, to classify failures.

## RQ2–RQ4: controlled experiments

| Condition | Description |
|---|---|
| B0 Solo | one agent implements both features (speedup reference) |
| B1 Isolated | parallel agents, no communication |
| B2 Chat | CooperBench default free-form messaging |
| B3 File claims | exclusive file claims (AgentRoom / agent-teams convention) |
| B4 Overlap warnings | Clash/Grove-style textual overlap warnings |
| **C Concord** | protocol + speculative integration + queue |
| A1–A4 | Concord without: contract/body split, notices, speculative integration, finish gate |

**Metrics:**
- joint success (merged result passes both features' tests);
- integration success;
- interference rate (pass alone / fail together);
- stale-contract defects;
- serialisation ratio and wall-clock speedup;
- tokens and cost;
- notice count and precision;
- detection latency.

**Integration with CooperBench:**
- Concord registers as an external agent adapter (`COOPERBENCH_EXTERNAL_AGENTS`), based on the existing `claude_code` adapter, which runs `claude --print --settings <file>`.
- Each agent runs in its own container. The Concord daemon runs on the host and receives hook events over HTTP at `host.docker.internal`.
- Notices go back through `additionalContext`, so the comparison with B2, which uses CooperBench's Redis message channel, uses comparable delivery.
- The CooperBench commit and dataset revision are pinned for reproducibility, because the harness has recently been re-graded.

**Statistics:**
- paired design per task;
- McNemar's test for success;
- bootstrap confidence intervals clustered by feature pool;
- up to 3 seeds;
- a pilot run first to fix subset sizes within the API budget.
