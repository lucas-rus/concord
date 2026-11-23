#!/usr/bin/env python3
"""Summarize project-benchmark runs (results.jsonl from run_project.py).

Per mode, it reports:
  - hidden acceptance tests passed per milestone and in total (out of 33),
    with mean and range over runs;
  - milestones fully completed;
  - merge conflicts;
  - Concord activity (notices, gate blocks, merges);
  - cost.

Usage:
    python3 eval/project/summarize.py DIR/*/results.jsonl
"""

from __future__ import annotations

import argparse
import json
from collections import defaultdict
from pathlib import Path
from statistics import mean

MODES = ["off", "enforce", "peeronly", "peer"]
TOTAL = {1: 18, 2: 9, 3: 6}


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("jsonl", nargs="+", type=Path)
    args = ap.parse_args()
    rows = [json.loads(l) for p in args.jsonl for l in p.read_text().splitlines() if l.strip()]
    runs: dict[tuple[str, int], dict[int, dict]] = defaultdict(dict)
    for r in rows:
        runs[(r["mode"], r["run"])][r["milestone"]] = r

    print("| mode | runs | M1 (/18) | M2 (/9) | M3 (/6) | total (/33) | milestones complete | merge conflicts | notices | gate blocks | merges | cost/run |")
    print("|---|---|---|---|---|---|---|---|---|---|---|---|")
    for mode in [m for m in MODES if any(k[0] == m for k in runs)]:
        rs = [v for k, v in runs.items() if k[0] == mode and len(v) == 3]
        if not rs:
            continue
        per = {m: [r[m]["hidden_passed"] for r in rs] for m in (1, 2, 3)}
        tot = [sum(r[m]["hidden_passed"] for m in (1, 2, 3)) for r in rs]
        cell = lambda xs: f"{mean(xs):.1f} ({min(xs)}-{max(xs)})"  # noqa: E731
        complete = sum(r[m]["milestone_ok"] for r in rs for m in (1, 2, 3))
        conflicts = sum(r[m]["merge_conflict"] for r in rs for m in (1, 2, 3))
        s = lambda k: sum(r[m][k] for r in rs for m in (1, 2, 3))  # noqa: E731
        cost = mean(sum(r[m]["cost_usd"] for m in (1, 2, 3)) for r in rs)
        print(
            f"| {mode} | {len(rs)} | {cell(per[1])} | {cell(per[2])} | {cell(per[3])} | {cell(tot)} | "
            f"{complete}/{3 * len(rs)} | {conflicts}/{3 * len(rs)} | {s('notices')} | {s('gate_blocks')} | {s('merges')} | ${cost:.2f} |"
        )


if __name__ == "__main__":
    main()
