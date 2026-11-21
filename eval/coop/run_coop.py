#!/usr/bin/env python3
"""Run real Claude Code agents on CooperBench feature pairs with `concord run`.

For each selected pair (repo, task, f1, f2), two agents start from the task's
base commit, each receiving its feature description (`feature.md`) as the
prompt. They run in parallel git worktrees, with or without Concord (`--modes`).
Afterwards their branches are merged, and both features' hidden tests
(`tests.patch`) are run on the merged code. This is CooperBench's success
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


def tests_apply_together(repo: Path, base: str, t1: Path, t2: Path, scratch: Path) -> bool:
    if scratch.exists():
        git(["worktree", "remove", "--force", str(scratch)], cwd=repo, check=False)
    git(["worktree", "add", "-q", "--detach", str(scratch), base], cwd=repo)
    ok = all(
        git(["apply", "--whitespace=nowarn", str(t.resolve())], cwd=scratch, check=False).returncode == 0
        for t in (t1, t2)
    )
    git(["worktree", "remove", "--force", str(scratch)], cwd=repo, check=False)
    return ok


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cooperbench", required=True, type=Path)
    ap.add_argument("--cache", required=True, type=Path)
    ap.add_argument("--concord", required=True)
    ap.add_argument("--pairs", required=True, help="comma-separated repo:task:f1:f2")
    ap.add_argument("--modes", default="off,enforce,peer", help="off | observe | enforce | peer (enforce + --peer-merge)")
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
        if not tests_apply_together(repo, base, t1, t2, args.cache / f"{repo.name}-check"):
            print(f"{spec}: hidden test patches conflict with each other; skipped", file=sys.stderr)
            continue
        files = " ".join(test_files(t1) + test_files(t2))
        # Agents may have edited or created the hidden test files themselves;
        # as in CooperBench, reset them to the base revision before applying.
        reset = " ".join(
            f"{{ git cat-file -e {base}:{f} 2>/dev/null && git checkout -q {base} -- {f} || rm -f {f}; }};"
            for f in files.split()
        )
        hidden = (
            f"{reset} git apply --whitespace=nowarn {t1.resolve()} && git apply --whitespace=nowarn {t2.resolve()} && "
            f"{test_cmd_bin} -q {files}"
        )
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
                    "--mode", "enforce" if mode == "peer" else mode, *(["--peer-merge"] if mode == "peer" else []),
                    "--model", args.model, "--budget-usd", args.budget_usd,
                    "--test-cmd", hidden, "--allow", f"Bash({test_cmd_bin} *)", "--allow", f"Bash({test_cmd_bin})",
                    "--out", str(run_out),
                ],
                capture_output=True,
                text=True,
            )
            reports = sorted(run_out.glob("*/report.json"))
            if r.returncode != 0 or not reports:
                print(f"  concord run failed: {r.stderr.strip()[-400:]}", file=sys.stderr)
                continue
            rep = json.loads(reports[-1].read_text())
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
                "tail": rep["integration"]["tests_output_tail"].splitlines()[-1:] if rep["integration"]["tests_output_tail"] else [],
            }
            print(f"  -> success={row['success']} conflict={row['merge_conflict']} cost=${row['cost_usd']:.3f}", file=sys.stderr)
            with results.open("a") as fh:
                fh.write(json.dumps(row) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
