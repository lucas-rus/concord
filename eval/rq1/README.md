# RQ1: where do parallel features interfere?

`run_rq1.py` analyses the **gold** feature patches of CooperBench at symbol level. It is offline: it makes no LLM calls and never runs CooperBench's scripts.

For each task in the selected repositories it does the following:

1. Read the upstream URL and base commit from `setup.sh` and check the base out in a blobless clone.
2. Create one branch per feature by applying `feature<N>/feature.patch`.
3. Run `concord analyze --json` over all features of the task. Every pair of features in a task is a CooperBench pair.
4. Write one JSON line per pair and print an aggregate summary.

```bash
cargo build --release
git clone https://github.com/cooperbench/CooperBench /tmp/CooperBench
python3 eval/rq1/run_rq1.py --cooperbench /tmp/CooperBench \
  --concord target/release/concord --cache /tmp/cb-repos --out rq1.jsonl
```

## Per-pair fields

| Field | Meaning |
|---|---|
| `textual_conflict` | `git merge-tree` reports a conflict. Cross-checked against CooperBench's `gold_conflict_report.json` (`gold_agree`). |
| `shared_files`, `shared_files_symbol_disjoint` | Files changed by both features, and how many of them are touched at disjoint symbols. |
| `same_symbol`, `same_symbol_body_only` | Symbols changed by both features; how many of these are body-only on both sides. |
| `contract_deps`, `breaking_deps` | One feature changes the contract of a symbol the other feature's code uses (any change / breaking change). |
| `*_file_disjoint` | The same, between features that share no file, i.e. invisible to textual merge and to file locks. |

## Caveats

- These are **gold** patches written to be mergeable by construction, so they are a lower bound on agent behaviour. The same pipeline is applied to agent patches from the baseline runs (M4).
- `uses` come from the tree-sitter resolver, an over-approximation for common cases. Its precision and recall against an LSP oracle is a separate M1 item.
- Breaking/compatible is decided syntactically (`concord_core::compat`) and is conservative for decorators and annotations.

## Results

See [results.md](results.md).
