#!/usr/bin/env python3
"""Summarize CooperBench agent runs (results.jsonl files from run_coop.py).

Prints, per mode and per repository:
  - pair success (both features' hidden tests pass on the merged code);
  - features passing;
  - merge conflicts;
  - cost.
It also runs an exact McNemar test on pair success for each pair of modes.
The test is paired because every pair is run under every mode.

Usage:
    python3 eval/coop/summarize.py results1.jsonl [results2.jsonl ...]
"""

from __future__ import annotations

import argparse
import json
from collections import defaultdict
from itertools import combinations
from math import comb
from pathlib import Path

MODES = ["off", "enforce", "peer"]


def mcnemar_exact(b: int, c: int) -> float:
    """Two-sided exact McNemar p-value for b vs. c discordant pairs."""
    n = b + c
    if n == 0:
        return 1.0
    k = min(b, c)
    p = 2 * sum(comb(n, i) for i in range(k + 1)) / 2**n
    return min(1.0, p)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("jsonl", nargs="+", type=Path)
    args = ap.parse_args()
    rows = [json.loads(line) for p in args.jsonl for line in p.read_text().splitlines() if line.strip()]
    modes = [m for m in MODES if any(r["mode"] == m for r in rows)]

    by_repo: dict[str, list[dict]] = defaultdict(list)
    for r in rows:
        by_repo[r["pair"].split(":")[0].removesuffix("_task")].append(r)
    by_repo["**all**"] = rows

    print("| repo | mode | pairs | pair success | features passing | merge conflicts | cost (USD) |")
    print("|---|---|---|---|---|---|---|")
    for repo, rs in by_repo.items():
        for m in modes:
            x = [r for r in rs if r["mode"] == m]
            if not x:
                continue
            n = len(x)
            ok = sum(r["success"] for r in x)
            feats = sum(r.get("f1_pass", False) + r.get("f2_pass", False) for r in x)
            conf = sum(r["merge_conflict"] for r in x)
            cost = sum(r["cost_usd"] for r in x)
            print(
                f"| {repo} | {m} | {n} | {ok} ({100 * ok / n:.0f}%) | {feats}/{2 * n} | "
                f"{conf} ({100 * conf / n:.0f}%) | {cost:.2f} |"
            )

    # Paired comparison on pairs run under both modes.
    success = {(r["pair"], r["mode"]): r["success"] for r in rows}
    pairs = sorted({r["pair"] for r in rows})
    print("\n| comparison | pairs | only A succeeds | only B succeeds | exact McNemar p |")
    print("|---|---|---|---|---|")
    for a, b in combinations(modes, 2):
        both = [p for p in pairs if (p, a) in success and (p, b) in success]
        only_a = sum(success[(p, a)] and not success[(p, b)] for p in both)
        only_b = sum(success[(p, b)] and not success[(p, a)] for p in both)
        print(f"| {a} vs {b} | {len(both)} | {only_a} | {only_b} | {mcnemar_exact(only_a, only_b):.4f} |")


if __name__ == "__main__":
    main()
