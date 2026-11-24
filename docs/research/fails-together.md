# Fails-together check: design

Status: design and opt-in implementation, 2026-11-24 (`concord run --fails-together`). Not part of the frozen study build. Background: [literature-review.md](literature-review.md).

## Problem

Region coherence and the contract layer make branches merge *textually*, and keep call sites *syntactically* conformant. Neither sees behaviour. Two branches can each pass their tests and still break each other once merged:
- Crystal reports that 33% of textually clean merges break the build or the tests;
- "Passes Alone, Fails Together" (Xia et al.) names exactly this failure for agents.

The proposal planned a speculative integration step (Alg. 2) to catch it. It was never built.

## Grounding

- **SAM** (Da Silva et al.) defines semantic interference as a test that passes on one side and fails on the merge. That definition is executable with the project's own tests.
- **Crystal** merges and tests speculatively, and tells the developer who has to resolve the result.
- **Palantír** found that awareness alone gets ignored, so a gate has to back it.
- **Kung–Robinson validation** says where the check belongs: when a transaction (an agent) commits (finishes), against the transactions already committed (finished peers).
- **Xia et al.** found that a short message describing the other side's completed change recovers 82–98% of interference. The notice therefore names the peers and summarises their changes.

## Mechanism

When an agent passes every other part of the finish gate:

1. **Speculative merge.** Concord builds the merge of the agent's snapshot with every finished peer's branch, in memory (`git merge-tree`). Peers that are still running are left out: their work is not final, and they are checked when they finish (validation is always against committed transactions).
   - If the merge is not textually clean, the check is skipped; region coherence and the integration step handle that case.
2. **Tests.** It runs the project's test command on the agent's snapshot ("alone") and on the merge ("together"), each in a temporary detached worktree. Nobody's worktree is touched.
3. **Verdict.** A test fails together if it fails on the merge but not alone.
   - That includes a peer's test broken by the agent's change. The agent is the later finisher, and as with regions the finisher fits around what is already finished.
   - Failures that already exist alone are not the merge's fault and are ignored.
   - Test names are parsed from pytest output (`FAILED`/`ERROR` lines). With unknown output formats, only the exit status is compared.
4. **Gate.** If any test fails together, the agent is blocked once with a notice listing:
   - the failing tests and the tail of the output;
   - the finished peers in the merge, with each peer's diff stat (Xia: describe the completed change).
5. **Cost and liveness.**
   - The tests run without holding Concord's state lock, so other agents' hooks are not delayed.
   - A verdict is cached per (agent snapshot, peer set), so an unchanged retry does not rerun the tests.
   - The block counts toward the gate block limit like any other.

## Limits

- The check is only as good as the tests: like SAM, it finds interference that some test exercises.
- Interference with peers that finish *later* is checked when *they* finish, against this agent's branch. A pair is therefore always checked once, by whichever agent finishes second.
- Three-way interactions among peers that were already finished are not attributed to the finishing agent beyond "fails on the merge, passes alone".
- A failing test is not attributed to one peer when several are in the merge. Bisecting over peers would cost one test run per peer.

## Evaluation plan

Measure it on top of `parallel-regions` with a real test command (CommonMark: the project's own test suite; CooperBench: the features' visible tests):
- test runs and the time they add;
- blocks raised, and how many of them are later fixed by the agent;
- the change in integrated pass rate.
