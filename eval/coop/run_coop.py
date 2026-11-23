#!/usr/bin/env python3
"""Run real Claude Code agents on CooperBench feature pairs with `concord run`.

For each selected pair (repo, task, f1, f2), two agents start from the task's
base commit, each receiving its feature description (`feature.md`) as the
prompt. They run in parallel git worktrees, with or without Concord (`--modes`).
Afterwards their branches are merged, and each feature's hidden tests
(`tests.patch`) are run, separately, on the merged code. This is CooperBench's success
criterion.

Deviation from the official harness (to be disclosed): the agents run locally
in git worktrees with a per-repository Python environment, not in CooperBench's
Docker containers, and they use Claude Code rather than OpenHands. Agents never
see the hidden tests.

Usage:
    python3 eval/coop/run_coop.py --cooperbench PATH --cache DIR --concord target/release/concord \
        --pairs pallets_jinja_task:1559:1:2,... --modes off,enforce --out results/
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "rq1"))
from run_rq1 import git, parse_setup, prepare_repo  # noqa: E402
from validate_pairs import ensure_env, install, test_files  # noqa: E402


def wrapper(cache: Path, repo: Path, py: Path) -> Path:
    """A test command the agents may run: local sources first, repo venv."""
    bindir = cache / "bin"
    bindir.mkdir(exist_ok=True)
    w = bindir / f"{repo.name}-pytest"
    w.write_text(
        "#!/bin/sh\n"
        'PYTHONPATH="$PWD/src:$PWD${PYTHONPATH:+:$PYTHONPATH}" '
        f'exec "{py}" -m pytest -p no:cacheprovider "$@"\n'
    )
    w.chmod(0o755)
    return w


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cooperbench", required=True, type=Path)
    ap.add_argument("--cache", required=True, type=Path)
    ap.add_argument("--concord", required=True)
    ap.add_argument("--pairs", required=True, help="comma-separated repo:task:f1:f2")
    ap.add_argument("--modes", default="off,enforce,peer", help="off | observe | enforce | peer (enforce + --peer-merge) | peeronly (peer merge without the contract layer)")
    ap.add_argument("--model", default="claude-haiku-5-5")
    ap.add_argument("--budget-usd", default="0.75")
    ap.add_argument("--python", default="3.12")
    ap.add_argument("--out", required=True, type=Path)
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    concord = str(Path(args.concord).resolve())
    results = args.out / "results.jsonl"

    for spec in args.pairs.split(","):
        repo_name, task_id, f1, f2 = spec.split(":")
        task = args.cooperbench / "dataset" / repo_name / f"task{task_id}"
        url, base = parse_setup(task / "setup.sh")
        repo = prepare_repo(args.cache, url, base)
        py = ensure_env(args.cache, repo, args.python)
        install(py, repo)
        test_cmd_bin = wrapper(args.cache, repo, py)
        t1, t2 = task / f"feature{f1}" / "tests.patch", task / f"feature{f2}" / "tests.patch"
        # Each feature's hidden tests run separately on the merged code (as in
        # CooperBench), so test patches that conflict with each other are fine.
        # Agents may have edited or created the hidden test files themselves;
        # reset them to the base revision before applying each patch.
        all_files = sorted(set(test_files(t1) + test_files(t2)))
        reset = " ".join(
            f"{{ git cat-file -e {base}:{f} 2>/dev/null && git checkout -q {base} -- {f} || rm -f {f}; }};"
            for f in all_files
        )
        steps = []
        for i, t in ((1, t1), (2, t2)):
            steps.append(
                f'{reset} echo "== feature {i} =="; '
                f"if git apply --whitespace=nowarn {t.resolve()}; then {test_cmd_bin} -q --tb=line {' '.join(test_files(t))}; "
                f'r{i}=$?; else echo "tests.patch does not apply"; r{i}=1; fi; echo "feature {i} rc=$r{i}";'
            )
        hidden = " ".join(steps) + f' {reset} echo "HIDDEN f1=$r1 f2=$r2"; test "$r1" = 0 && test "$r2" = 0'
        tasks = []
        for f in (f1, f2):
            desc = (task / f"feature{f}" / "feature.md").read_text()
            tasks.append(
                {
                    "name": f"f{f}",
                    "prompt": f"Implement the following feature in this repository.\n\n{desc}\n\n"
                    f"Run the existing tests with `{test_cmd_bin} -q -x tests` (add your own tests too) before you finish.",
                }
            )
        tasks_file = args.out / f"{repo_name}-{task_id}-{f1}-{f2}.tasks.json"
        tasks_file.write_text(json.dumps(tasks, indent=2))
        for mode in args.modes.split(","):
            run_out = args.out / "runs" / f"{repo_name}-{task_id}-{f1}-{f2}" / mode
            run_out.mkdir(parents=True, exist_ok=True)
            print(f"{spec} [{mode}] ...", file=sys.stderr, flush=True)
            r = subprocess.run(
                [
                    concord, "run", "--repo", str(repo), "--base", base, "--tasks", str(tasks_file),
                    "--mode", "enforce" if mode in ("peer", "peeronly") else mode,
                    *(["--peer-merge"] if mode in ("peer", "peeronly") else []),
                    *(["--no-contracts"] if mode == "peeronly" else []),
                    "--model", args.model, "--budget-usd", args.budget_usd,
                    "--test-cmd", hidden, "--allow", f"Bash({test_cmd_bin} *)", "--allow", f"Bash({test_cmd_bin})",
                    "--out", str(run_out), "--max-wait-secs", "300",
                ],
                capture_output=True,
                text=True,
            )
            reports = sorted(run_out.glob("*/report.json"))
            if r.returncode != 0 or not reports:
                print(f"  concord run failed: {r.stderr.strip()[-400:]}", file=sys.stderr)
                continue
            rep = json.loads(reports[-1].read_text())
            hm = re.search(r"HIDDEN f1=(\d+) f2=(\d+)", rep["integration"]["tests_output_tail"])
            row = {
                "pair": spec,
                "mode": mode,
                "success": rep["integration"]["tests_passed"],
                "merge_conflict": bool(rep["integration"]["conflicts"]),
                "cost_usd": rep["total_cost_usd"],
                "wall_seconds": rep["wall_seconds"],
                "gate_blocks": sum(a["gate_blocks"] for a in rep["agents"]),
                "notices": sum(a["notices_delivered"] for a in rep["agents"]),
                "merges": sum(len(a["merges"]) for a in rep["agents"]),
                "f1_pass": bool(hm) and hm.group(1) == "0",
                "f2_pass": bool(hm) and hm.group(2) == "0",
                "tail": rep["integration"]["tests_output_tail"].splitlines()[-1:] if rep["integration"]["tests_output_tail"] else [],
            }
            print(f"  -> success={row['success']} conflict={row['merge_conflict']} cost=${row['cost_usd']:.3f}", file=sys.stderr)
            with results.open("a") as fh:
                fh.write(json.dumps(row) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
