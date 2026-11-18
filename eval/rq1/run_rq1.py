#!/usr/bin/env python3
"""RQ1: symbol-level analysis of CooperBench gold feature pairs.

For every CooperBench task, this script:

1. checks out the upstream repository at the task's base commit (blobless clone,
   cached across tasks);
2. creates one branch per feature by applying `feature<N>/feature.patch`;
3. runs `concord analyze --json` over all features of the task at once;
4. emits one JSON line per feature pair with textual and symbol-level findings.

It never executes CooperBench's scripts: it only reads `setup.sh` to find the
repository URL and base commit, and uses `git apply` on the patches.

Usage:
    python3 eval/rq1/run_rq1.py --cooperbench PATH/TO/CooperBench \
        --concord target/release/concord --cache /tmp/cb-repos \
        --repos pallets_click_task,samuelcolvin_dirty_equals_task \
        --out rq1.jsonl
"""

from __future__ import annotations

import argparse
import itertools
import json
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

PY_REPOS = [
    "dottxt_ai_outlines_task",
    "dspy_task",
    "huggingface_datasets_task",
    "llama_index_task",
    "openai_tiktoken_task",
    "pallets_click_task",
    "pallets_jinja_task",
    "pillow_task",
    "samuelcolvin_dirty_equals_task",
]

GIT_ID = ["-c", "user.name=concord-rq1", "-c", "user.email=rq1@localhost"]


def git(args: list[str], cwd: Path | None = None, check: bool = True) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], cwd=cwd, check=check, capture_output=True, text=True)


def parse_setup(setup: Path) -> tuple[str, str]:
    text = setup.read_text()
    variables = dict(re.findall(r'^(\w+)="([^"]*)"', text, re.M))
    m = re.search(r"git clone (\S+)", text)
    base = re.search(r'BASE_COMMIT="([0-9a-f]{7,40})"', text)
    if not m or not base:
        raise ValueError(f"cannot parse {setup}")
    url = re.sub(r"\$\{(\w+)\}", lambda v: variables.get(v.group(1), v.group(0)), m.group(1))
    return url, base.group(1)


def prepare_repo(cache: Path, url: str, base: str) -> Path:
    name = url.rstrip("/").removesuffix(".git").split("/")[-1]
    repo = cache / name
    if not repo.exists():
        print(f"  cloning {url} (blobless)", file=sys.stderr)
        git(["clone", "-q", "--filter=blob:none", "--no-checkout", url, str(repo)])
    if git(["cat-file", "-e", f"{base}^{{commit}}"], cwd=repo, check=False).returncode != 0:
        git(["fetch", "-q", "origin", base], cwd=repo)
    # Remove branches from previous tasks, then check out the base (fetches its blobs).
    for b in git(["for-each-ref", "--format=%(refname:short)", "refs/heads/cb-*"], cwd=repo).stdout.split():
        git(["branch", "-q", "-D", b], cwd=repo, check=False)
    git(["checkout", "-q", "-f", "--detach", base], cwd=repo)
    git(["clean", "-fdqx"], cwd=repo)
    return repo


def feature_dirs(task: Path) -> list[tuple[int, Path]]:
    out = []
    for d in task.iterdir():
        m = re.fullmatch(r"feature(\d+)", d.name)
        if m and (d / "feature.patch").exists():
            out.append((int(m.group(1)), d))
    return sorted(out)


def make_branches(repo: Path, base: str, features: list[tuple[int, Path]]) -> dict[int, str | None]:
    branches: dict[int, str | None] = {}
    for n, d in features:
        name = f"cb-f{n}"
        git(["checkout", "-q", "-f", "-B", name, base], cwd=repo)
        r = git(["apply", "--whitespace=nowarn", str((d / "feature.patch").resolve())], cwd=repo, check=False)
        if r.returncode != 0:
            print(f"    feature{n}: patch does not apply ({r.stderr.strip()[:120]})", file=sys.stderr)
            branches[n] = None
            continue
        git(["add", "-A"], cwd=repo)
        git([*GIT_ID, "commit", "-q", "--allow-empty", "-m", f"feature {n}"], cwd=repo)
        branches[n] = name
    git(["checkout", "-q", "-f", "--detach", base], cwd=repo)
    return branches


def pair_rows(repo_name: str, task_id: str, analysis: dict, names: dict[str, int]) -> list[dict]:
    rows: dict[tuple[int, int], dict] = {}

    def row(a: str, b: str) -> dict:
        x, y = sorted((names[a], names[b]))
        return rows.setdefault(
            (x, y),
            {
                "repo": repo_name,
                "task_id": task_id,
                "f1": x,
                "f2": y,
                "textual_conflict": None,
                "conflicted_files": [],
                "shared_files": 0,
                "shared_files_symbol_disjoint": 0,
                "same_symbol": 0,
                "same_symbol_body_only": 0,
                "contract_deps": 0,
                "contract_deps_file_disjoint": 0,
                "breaking_deps": 0,
                "breaking_deps_file_disjoint": 0,
                "duplicates": 0,
                "contract_dep_symbols": [],
            },
        )

    branch_names = list(names)
    for a, b in itertools.combinations(branch_names, 2):
        row(a, b)
    for t in analysis["textual"]:
        r = row(t["a"], t["b"])
        r["textual_conflict"] = not t["clean"]
        r["conflicted_files"] = t["conflicted_files"]
    for f in analysis["findings"]:
        kind = f["type"]
        if kind == "file_overlap":
            r = row(f["a"], f["b"])
            r["shared_files"] += 1
            if not f["shared_symbols"]:
                r["shared_files_symbol_disjoint"] += 1
        elif kind == "same_symbol":
            r = row(f["a"], f["b"])
            r["same_symbol"] += 1
            if f["a_change"]["kind"] == "body_changed" and f["b_change"]["kind"] == "body_changed":
                r["same_symbol_body_only"] += 1
        elif kind == "contract_dependency":
            r = row(f["producer"], f["consumer"])
            r["contract_deps"] += 1
            if f["file_disjoint"]:
                r["contract_deps_file_disjoint"] += 1
            if f["breaking"]:
                r["breaking_deps"] += 1
                r["breaking_deps_file_disjoint"] += f["file_disjoint"]
            r["contract_dep_symbols"].append({"symbol": f["symbol"], "breaking": f["breaking"], "reasons": f.get("reasons", [])[:3]})
        elif kind == "duplicate_definition":
            row(f["a"], f["b"])["duplicates"] += 1
    return list(rows.values())


def summarize(rows: list[dict], gold: dict[tuple, bool]) -> dict:
    n = len(rows)
    c = Counter()
    for r in rows:
        textual = bool(r["textual_conflict"])
        c["textual_conflict"] += textual
        c["shared_file"] += r["shared_files"] > 0
        # Every shared file is touched at disjoint symbols: file locks serialise needlessly.
        c["file_overlap_all_symbol_disjoint"] += r["shared_files"] > 0 and r["shared_files_symbol_disjoint"] == r["shared_files"]
        c["same_symbol"] += r["same_symbol"] > 0
        c["contract_coupled"] += r["contract_deps"] > 0
        c["contract_coupled_file_disjoint"] += r["contract_deps_file_disjoint"] > 0
        c["contract_coupled_but_textually_clean"] += r["contract_deps"] > 0 and not textual
        c["breaking_coupled"] += r["breaking_deps"] > 0
        c["breaking_coupled_file_disjoint"] += r["breaking_deps_file_disjoint"] > 0
        c["breaking_coupled_but_textually_clean"] += r["breaking_deps"] > 0 and not textual
        c["textual_conflict_without_same_symbol"] += textual and r["same_symbol"] == 0
        key = (r["repo"], int(r["task_id"]), r["f1"], r["f2"])
        if key in gold:
            c["gold_compared"] += 1
            c["gold_agree"] += gold[key] == textual
    return {"pairs": n, **dict(c)}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cooperbench", required=True, type=Path)
    ap.add_argument("--concord", default="target/release/concord")
    ap.add_argument("--cache", required=True, type=Path)
    ap.add_argument("--repos", default=",".join(PY_REPOS))
    ap.add_argument("--out", required=True, type=Path)
    args = ap.parse_args()

    dataset = args.cooperbench / "dataset"
    gold_report = json.loads((dataset / "gold_conflict_report.json").read_text())
    gold = {
        (r["repo"], int(r["task_id"]), min(r["f1"], r["f2"]), max(r["f1"], r["f2"])): bool(r["has_conflict"])
        for r in gold_report["all_results"]
        if not r.get("error")
    }
    args.cache.mkdir(parents=True, exist_ok=True)
    concord = str(Path(args.concord).resolve())

    all_rows: list[dict] = []
    with args.out.open("w") as out:
        for repo_name in [r for r in args.repos.split(",") if r]:
            for task in sorted((dataset / repo_name).glob("task*")):
                task_id = task.name.removeprefix("task")
                url, base = parse_setup(task / "setup.sh")
                print(f"{repo_name}/{task.name}: {url} @ {base[:10]}", file=sys.stderr)
                try:
                    repo = prepare_repo(args.cache, url, base)
                    branches = make_branches(repo, base, feature_dirs(task))
                except subprocess.CalledProcessError as e:
                    # One broken task (network, huge checkout) must not stop the run.
                    print(f"  skipped: {' '.join(e.cmd)} -> {(e.stderr or '').strip()[:300]}", file=sys.stderr)
                    continue
                names = {b: n for n, b in branches.items() if b}
                if len(names) < 2:
                    continue
                r = subprocess.run(
                    [concord, "analyze", "--repo", str(repo), "--base", base, *names, "--json"],
                    capture_output=True,
                    text=True,
                )
                if r.returncode != 0:
                    print(f"  concord failed: {r.stderr.strip()[:300]}", file=sys.stderr)
                    continue
                analysis = json.loads(r.stdout)["analysis"]
                for w in analysis.get("parse_warnings", []):
                    print(f"  warning: {w}", file=sys.stderr)
                rows = pair_rows(repo_name, task_id, analysis, names)
                for row in rows:
                    out.write(json.dumps(row) + "\n")
                out.flush()
                all_rows.extend(rows)

    print(json.dumps(summarize(all_rows, gold), indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
