#!/usr/bin/env python3
"""Have a team of three parallel agents build the `conta` project (examples/conta)
from its scaffold through three milestones, with or without Concord.

Per run:
- a fresh git repository is created from `examples/conta/scaffold`;
- for each milestone, the three tasks in `examples/conta/milestones/mN.json`
  run in parallel through `concord run`;
- the integrated result (the integration branch, including any partial merge)
  becomes the base of the next milestone, as in a real team;
- after each milestone, the hidden acceptance tests of that milestone
  (`eval/project/hidden/test_mN.py`, never shown to the agents) run on the
  integrated code.

Modes (as in eval/coop): off | enforce | peeronly | peer.

Usage:
    python3 eval/project/run_project.py --concord target/release/concord --modes off,enforce \
        --runs 3 --python /path/to/venv/bin/python --out DIR
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HIDDEN = ROOT / "eval" / "project" / "hidden"
# project -> (scaffold, [(milestone tasks file, hidden test file)])
PROJECTS = {
    "conta": (
        ROOT / "examples" / "conta" / "scaffold",
        [(ROOT / "examples" / "conta" / "milestones" / f"m{m}.json", HIDDEN / f"test_m{m}.py") for m in (1, 2, 3)],
    ),
    # The v2 sprint: 12 parallel tickets on top of the finished conta v1.
    "v2": (
        ROOT / "examples" / "conta" / "v2" / "base",
        [(ROOT / "examples" / "conta" / "v2" / "tasks.json", HIDDEN / "test_v2.py")],
    ),
}
GIT_ID = ["-c", "user.name=concord-eval", "-c", "user.email=concord-eval@localhost"]


def git(args: list[str], cwd: Path) -> str:
    return subprocess.run(["git", *GIT_ID, *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


def mode_flags(mode: str) -> list[str]:
    return {
        "off": ["--mode", "off"],
        "enforce": ["--mode", "enforce"],
        "peeronly": ["--mode", "enforce", "--peer-merge", "--no-contracts"],
        "peer": ["--mode", "enforce", "--peer-merge"],
    }[mode]


def counts(tail: str) -> tuple[int, int]:
    """(passed, failed+errors) from pytest's summary line."""
    last = next((l for l in reversed(tail.splitlines()) if " in " in l and re.search(r"\d+ (passed|failed|error)", l)), "")
    num = lambda k: sum(int(n) for n in re.findall(rf"(\d+) {k}", last))  # noqa: E731
    return num("passed"), num("failed") + num("error")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--concord", required=True)
    ap.add_argument("--modes", default="off,enforce,peeronly,peer")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--first-run", type=int, default=1)
    ap.add_argument("--python", required=True, help="interpreter with pytest installed")
    ap.add_argument("--project", choices=sorted(PROJECTS), default="conta")
    ap.add_argument("--agent-cli", choices=["claude", "opencode"], default="claude")
    ap.add_argument("--model", default="claude-haiku-5-5")
    ap.add_argument("--budget-usd", default="0.75")
    ap.add_argument("--out", required=True, type=Path)
    args = ap.parse_args()
    concord = str(Path(args.concord).resolve())
    args.out.mkdir(parents=True, exist_ok=True)
    results = args.out / "results.jsonl"

    bindir = args.out / "bin"
    bindir.mkdir(exist_ok=True)
    tester = bindir / "conta-pytest"
    tester.write_text(
        f'#!/bin/sh\nPYTHONPATH="$PWD/src${{PYTHONPATH:+:$PYTHONPATH}}" exec "{args.python}" -m pytest -p no:cacheprovider "$@"\n'
    )
    tester.chmod(0o755)

    for run in range(args.first_run, args.first_run + args.runs):
        for mode in args.modes.split(","):
            work = args.out / f"run{run}" / mode
            if work.exists():
                shutil.rmtree(work)
            repo = work / "repo"
            scaffold, milestones = PROJECTS[args.project]
            shutil.copytree(scaffold, repo, ignore=shutil.ignore_patterns("__pycache__"))
            git(["init", "-q", "-b", "main"], repo)
            git(["add", "-A"], repo)
            git(["commit", "-q", "-m", "scaffold"], repo)
            base = git(["rev-parse", "HEAD"], repo)
            for m, (tasks_path, hidden_file) in enumerate(milestones, 1):
                tasks = json.loads(tasks_path.read_text())
                for t in tasks:
                    t["prompt"] += (
                        f"\n\nThe package is in `src/conta`. Run the test suite with `{tester} -q tests` "
                        "(add your own tests under `tests/`) before you finish."
                    )
                tasks_file = work / f"m{m}.tasks.json"
                tasks_file.write_text(json.dumps(tasks, indent=2))
                hidden = (
                    f'PYTHONPATH="$PWD/src" "{args.python}" -m pytest -q -rA --tb=no -p no:cacheprovider '
                    f"--rootdir {HIDDEN} {hidden_file}"
                )
                out = work / f"m{m}"
                print(f"run{run} [{mode}] milestone {m} ...", file=sys.stderr, flush=True)
                r = subprocess.run(
                    [
                        concord, "run", "--repo", str(repo), "--base", base, "--tasks", str(tasks_file),
                        *mode_flags(mode), "--agent-cli", args.agent_cli, "--model", args.model,
                        "--budget-usd", args.budget_usd,
                        "--test-cmd", hidden, "--allow", f"Bash({tester} *)", "--allow", f"Bash({tester})",
                        "--out", str(out), "--max-wait-secs", "300",
                    ],
                    capture_output=True,
                    text=True,
                )
                reports = sorted(out.glob("*/report.json"))
                if r.returncode != 0 or not reports:
                    print(f"  concord run failed: {r.stderr.strip()[-600:]}", file=sys.stderr)
                    break
                rep = json.loads(reports[-1].read_text())
                passed, failed = counts(rep["integration"]["tests_output_tail"])
                row = {
                    "run": run,
                    "mode": mode,
                    "milestone": m,
                    "hidden_passed": passed,
                    "hidden_failed": failed,
                    "milestone_ok": rep["integration"]["tests_passed"],
                    "merge_conflict": bool(rep["integration"]["conflicts"]),
                    "conflicts": rep["integration"]["conflicts"],
                    "cost_usd": rep["total_cost_usd"],
                    "wall_seconds": rep["wall_seconds"],
                    "notices": sum(a["notices_delivered"] for a in rep["agents"]),
                    "gate_blocks": sum(a["gate_blocks"] for a in rep["agents"]),
                    "merges": sum(len(a["merges"]) for a in rep["agents"]),
                    "resumed": sum(a.get("resumed", 0) for a in rep["agents"]),
                    "agent_cli": args.agent_cli,
                    "model": args.model,
                    "project": args.project,
                    "agents": len(rep["agents"]),
                    "agent_errors": sum(bool(a.get("is_error")) for a in rep["agents"]),
                    "tests": dict(
                        (name.split("::")[-1], status == "PASSED")
                        for status, name in re.findall(r"^(PASSED|FAILED|ERROR) (\S+)", rep["integration"]["tests_output_tail"], re.M)
                    ),
                }
                print(
                    f"  -> hidden {passed} passed / {failed} failed, conflict={row['merge_conflict']}, "
                    f"cost=${row['cost_usd']:.3f}, notices={row['notices']}, blocks={row['gate_blocks']}",
                    file=sys.stderr,
                    flush=True,
                )
                with results.open("a") as fh:
                    fh.write(json.dumps(row) + "\n")
                # Carry the integrated state forward.
                base = git(["rev-parse", f"concord/{rep['run_id']}/integration"], repo)
    return 0


if __name__ == "__main__":
    sys.exit(main())
