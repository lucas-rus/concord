# Live experiments: real Claude Code agents coordinated by Concord

`concord run` starts several Claude Code agents (headless `claude -p`) in parallel git worktrees. It installs Concord's hooks, coordinates the agents live, merges their branches and runs the test suite on the integrated result.

## Shop scenario (`examples/shop`)

A small Python package (catalog, pricing, orders, notifications; unittest suite) and three tickets ([tasks.json](../../examples/shop/tasks.json)) written like real issues. None of them mentions the other agents.

| Agent | Ticket | Effect on code |
|---|---|---|
| `currency` | multi-currency support | makes `currency` a **required** keyword-only argument of `shop.pricing.price_order` (breaking contract change) |
| `invoices` | plain-text invoices | new module whose code **calls `price_order`** |
| `discount` | rounding bug in `apply_discount` | body-only change in the **same file** as `currency` |

```bash
cargo build --release
examples/shop/setup.sh /tmp/shop
./target/release/concord run --repo /tmp/shop --tasks examples/shop/tasks.json --mode enforce   # or --mode off
```

## Results (2026-11-18, model `claude-haiku-5-5`, 3 runs per condition)

| Condition | Integrated tests pass | Cost per run (3 agents) | Wall time |
|---|---|---|---|
| **off** (isolated worktrees, no coordination) | **0 / 3** | ~$0.02 | 31–43 s |
| **enforce** (Concord) | **3 / 3** | $0.024–0.032 | 43–56 s |

- **Baseline failure.** In every baseline run, each agent's own tests pass ("passes alone"). Git merges the three branches without conflicts. Yet the integrated suite fails with `TypeError: price_order() missing 1 required keyword-only argument: 'currency'` ("fails together").
- **What Concord did.** With Concord, `invoices` received a heads-up and a breaking-change notice. At its finish gate, Concord committed and merged the finished `currency` work into the `invoices` worktree. It then checked the call sites against the new signature and blocked until `invoices` called `price_order(order, currency='EUR')` and re-ran the tests on the merged code.
- **When `invoices` finished first.** If `invoices` finished before `currency` changed the contract, Concord resumed its session afterwards (`resumed=1`).
- **The `discount` agent** edited the same file as `currency` and was never blocked.

Per-run reports and event logs are in [2026-11-18/](2026-11-18/).

## Lessons from the first runs

1. **Notices must be verifiable.** In a smoke test, Haiku ignored an injected note that contradicted the file it had just read, treating it as a possible prompt injection. In an early buggy run, Concord claimed it had merged a change that was not actually in the worktree (the producer's work was uncommitted). The agent checked `git log` and the file, and refused to make the change until it could verify it. Concord therefore merges real code into the agent's worktree before asking it to conform, and tells the agent so in a system-prompt note.
2. **Calls hide in f-strings.** `f"Total: {price_order(order)} EUR"` was invisible to the first resolver; interpolations are now scanned.
3. **Scale is still small.** One scenario and three runs per condition: this demonstrates the mechanism, not a statistical result. CooperBench runs follow (see the roadmap).
