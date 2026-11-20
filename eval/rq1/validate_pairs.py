#!/usr/bin/env python3
"""Do Concord's breaking-coupling flags predict "passes alone, fails together"?

For every CooperBench gold pair that merges cleanly (from rq1 JSONL), this script:

1. builds both feature branches (gold `feature.patch`);
2. runs each feature's own tests (`tests.patch`) on its branch (passes alone?);
3. merges the two branches, applies both tests patches, and runs both test sets
   (fails together?);
4. reports interference against Concord's `breaking_deps` flag.

It needs a Python environment per repository; it creates one with `uv` in the
cache directory (`--python` selects the interpreter version).

Usage:
    python3 eval/rq1/validate_pairs.py --cooperbench PATH --cache DIR \
        --rq1 eval/rq1/data/gold-pairs-2026-11-18.jsonl --repo pallets_jinja_task --out val.jsonl
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from run_rq1 import GIT_ID, feature_dirs, git, make_branches, parse_setup, prepare_repo  # noqa: E402


def test_files(patch: Path) -> list[str]:
    return sorted(set(re.findall(r"^\+\+\+ b/(\S+)", patch.read_text(), re.M)))


def ensure_env(cache: Path, repo: Path, python: str) -> Path:
    env = cache / f"{repo.name}-venv"
    py = env / "bin" / "python"
    if not py.exists():
        subprocess.run(["uv", "venv", "-q", "--python", python, str(env)], check=True)
    return py


def install(py: Path, worktree: Path) -> None:
    subprocess.run(
        ["uv", "pip", "install", "-q", "--python", str(py), "-e", str(worktree), "pytest"],
        check=True,
        capture_output=True,
    )


def run_pytest(py: Path, worktree: Path, files: list[str]) -> tuple[bool, str]:
    files = [f for f in files if (worktree / f).exists()]
    if not files:
        return False, "no test files"
    r = subprocess.run(
        [str(py), "-m", "pytest", "-q", "-x", "-p", "no:cacheprovider", *files],
        cwd=worktree,
        capture_output=True,
        text=True,
        timeout=600,
    )
    tail = "\n".join((r.stdout + r.stderr).strip().splitlines()[-6:])
    return r.returncode == 0, tail


def worktree_at(repo: Path, path: Path, rev: str) -> None:
    if path.exists():
        git(["worktree", "remove", "--force", str(path)], cwd=repo, check=False)
        shutil.rmtree(path, ignore_errors=True)
    git(["worktree", "add", "-q", "--detach", str(path), rev], cwd=repo)


def apply(worktree: Path, patch: Path) -> bool:
    return git(["apply", "--whitespace=nowarn", str(patch.resolve())], cwd=worktree, check=False).returncode == 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cooperbench", required=True, type=Path)
    ap.add_argument("--cache", required=True, type=Path)
    ap.add_argument("--rq1", required=True, type=Path)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--python", default="3.12")
    ap.add_argument("--out", required=True, type=Path)
    args = ap.parse_args()

    pairs = [json.loads(line) for line in args.rq1.read_text().splitlines() if line.strip()]
    pairs = [p for p in pairs if p["repo"] == args.repo and not p["textual_conflict"]]
    by_task: dict[str, list[dict]] = {}
    for p in pairs:
        by_task.setdefault(str(p["task_id"]), []).append(p)

    dataset = args.cooperbench / "dataset" / args.repo
    out = args.out.open("w")
    for task_id, task_pairs in sorted(by_task.items()):
        task = dataset / f"task{task_id}"
        url, base = parse_setup(task / "setup.sh")
        repo = prepare_repo(args.cache, url, base)
        feats = dict(feature_dirs(task))
        branches = make_branches(repo, base, sorted(feats.items()))
        py = ensure_env(args.cache, repo, args.python)
        wt = args.cache / f"{repo.name}-validate"
        alone: dict[int, tuple[bool, str]] = {}

        def run_alone(n: int) -> tuple[bool, str]:
            if n not in alone:
                worktree_at(repo, wt, branches[n])
                install(py, wt)
                ok = apply(wt, feats[n] / "tests.patch")
                alone[n] = run_pytest(py, wt, test_files(feats[n] / "tests.patch")) if ok else (False, "tests.patch failed")
            return alone[n]

        for p in task_pairs:
            f1, f2 = p["f1"], p["f2"]
            a1, a2 = run_alone(f1), run_alone(f2)
            worktree_at(repo, wt, branches[f1])
            merged_ok = git([*GIT_ID, "merge", "-q", "--no-edit", branches[f2]], cwd=wt, check=False).returncode == 0
            together, tail = (False, "merge failed")
            if merged_ok:
                install(py, wt)
                t1 = apply(wt, feats[f1] / "tests.patch")
                t2 = apply(wt, feats[f2] / "tests.patch")
                if t1 and t2:
                    together, tail = run_pytest(
                        py, wt, test_files(feats[f1] / "tests.patch") + test_files(feats[f2] / "tests.patch")
                    )
                else:
                    tail = "tests patches do not apply together"
            row = {
                "repo": args.repo,
                "task_id": task_id,
                "f1": f1,
                "f2": f2,
                "breaking_flag": p["breaking_deps"] > 0,
                "contract_flag": p["contract_deps"] > 0,
                "alone_1": a1[0],
                "alone_2": a2[0],
                "together": together,
                "interference": a1[0] and a2[0] and not together,
                "tail": tail,
            }
            print(
                f"task{task_id} f{f1}+f{f2}: flag={row['breaking_flag']!s:5} alone={a1[0]!s:5}/{a2[0]!s:5} together={together!s:5}",
                file=sys.stderr,
            )
            out.write(json.dumps(row) + "\n")
            out.flush()
        git(["worktree", "remove", "--force", str(wt)], cwd=repo, check=False)
    return 0


if __name__ == "__main__":
    sys.exit(main())
