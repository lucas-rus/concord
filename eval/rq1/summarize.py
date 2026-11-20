#!/usr/bin/env python3
"""Turn rq1.jsonl (from run_rq1.py) into a Markdown results table per repository."""

from __future__ import annotations

import argparse
import json
from collections import defaultdict
from pathlib import Path

COLUMNS = [
    ("pairs", "pairs", None),
    ("textual_conflict", "textual conflict", lambda r: bool(r["textual_conflict"])),
    ("same_symbol", "same symbol edited", lambda r: r["same_symbol"] > 0),
    (
        "symbol_disjoint",
        "file overlap, symbols disjoint",
        lambda r: r["shared_files"] > 0 and r["shared_files_symbol_disjoint"] == r["shared_files"],
    ),
    ("contract", "contract-coupled", lambda r: r["contract_deps"] > 0),
    ("breaking", "breaking-coupled", lambda r: r["breaking_deps"] > 0),
    ("violating", "call sites violate new contract", lambda r: r.get("violating_deps", 0) > 0),
    ("violating_clean", "violating, textually clean", lambda r: r.get("violating_deps", 0) > 0 and not r["textual_conflict"]),
    ("breaking_clean", "breaking-coupled, textually clean", lambda r: r["breaking_deps"] > 0 and not r["textual_conflict"]),
    ("contract_clean", "contract-coupled, textually clean", lambda r: r["contract_deps"] > 0 and not r["textual_conflict"]),
]


def pct(n: int, d: int) -> str:
    return f"{n} ({100 * n / d:.0f}%)" if d else "0"


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("jsonl", type=Path)
    args = ap.parse_args()
    rows = [json.loads(line) for line in args.jsonl.read_text().splitlines() if line.strip()]
    groups: dict[str, list[dict]] = defaultdict(list)
    for r in rows:
        groups[r["repo"].removesuffix("_task")].append(r)
    groups["**all**"] = rows

    header = "| repo | " + " | ".join(c[1] for c in COLUMNS) + " |"
    print(header)
    print("|" + "---|" * (len(COLUMNS) + 1))
    for repo, rs in groups.items():
        n = len(rs)
        cells = []
        for key, _, pred in COLUMNS:
            cells.append(str(n) if pred is None else pct(sum(1 for r in rs if pred(r)), n))
        print(f"| {repo} | " + " | ".join(cells) + " |")


if __name__ == "__main__":
    main()
