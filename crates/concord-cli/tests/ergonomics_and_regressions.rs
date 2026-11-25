//! Regressions and product-ergonomics tests covering:
//! 1. Multi-peer union hand-offs without `a.base` clobbering or phantom contract notices;
//! 2. Live detection and gating of duplicate symbol definitions across non-overlapping lines;
//! 3. `Grep` read tracking, live contract `Negotiate` + explicit `Ack`, and `--file-leases` (B3);
//! 4. MCP tools (`concord_symbols`, `concord_diff_contracts`, `concord_worktrees`, `concord_smart_merge`)
//!    and binary bisection of failing branches.

use concord_cli::live::{self, Mode, Shared, State};
use concord_cli::mcp::{self, McpConfig};
use concord_git::Repo;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn post_edit(shared: &Shared, agent: &str, file: &Path) -> String {
    live::handle(
        shared,
        agent,
        "PostToolUse",
        &json!({
            "session_id": format!("s-{agent}"),
            "tool_name": "Edit",
            "tool_input": { "file_path": file.to_string_lossy() }
        }),
    )
    .0
}

fn stop(shared: &Shared, agent: &str) -> Option<String> {
    let (out, _) = live::handle(shared, agent, "Stop", &json!({ "session_id": format!("s-{agent}") }));
    if out.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["decision"], "block");
    Some(v["reason"].as_str().unwrap().to_owned())
}

#[test]
fn multi_peer_union_handoff_preserves_base_and_emits_zero_phantom_notices() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(
        root.join("notify.py"),
        "def send(to: str) -> bool:\n    return bool(to)\n",
    )
    .unwrap();
    fs::write(
        root.join("inlines.py"),
        "class Parser:\n    def base(self) -> int:\n        return 0\n",
    )
    .unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);

    let repo = Repo::open(&root).unwrap();
    let base = repo.rev_parse("HEAD").unwrap();
    let mut st = State::new(
        repo.clone(),
        base.clone(),
        Mode::Enforce,
        None,
        Duration::from_millis(200),
    )
    .unwrap();
    st.regions = true;

    let mut wts = std::collections::BTreeMap::new();
    for name in ["p1", "p2", "consumer", "observer"] {
        let wt = tmp.path().join("wt").join(name);
        repo.worktree_add(&wt, &format!("b/{name}"), &base).unwrap();
        st.add_agent(name, wt.clone(), format!("b/{name}"), "task");
        wts.insert(name, wt);
    }
    let shared: Shared = Arc::new(Mutex::new(st));

    // Observer reads `notify.py` so it subscribes to `notify.send`.
    live::handle(
        &shared,
        "observer",
        "PostToolUse",
        &json!({
            "session_id": "s-observer",
            "tool_name": "Read",
            "tool_input": { "file_path": wts["observer"].join("notify.py").to_string_lossy() }
        }),
    );

    // p1 changes `notify.send` compatibly AND appends `m1` to `inlines.py`, then finishes.
    fs::write(
        wts["p1"].join("notify.py"),
        "def send(to: str, urgent: bool = False) -> bool:\n    return bool(to)\n",
    )
    .unwrap();
    post_edit(&shared, "p1", &wts["p1"].join("notify.py"));
    fs::write(
        wts["p1"].join("inlines.py"),
        "class Parser:\n    def base(self) -> int:\n        return 0\n\n    def m1(self) -> int:\n        return 1\n",
    )
    .unwrap();
    post_edit(&shared, "p1", &wts["p1"].join("inlines.py"));
    assert_eq!(stop(&shared, "p1"), None);

    // Drain observer's initial notice from p1.
    let _ = live::handle(
        &shared,
        "observer",
        "PostToolUse",
        &json!({
            "session_id": "s-observer",
            "tool_name": "Read",
            "tool_input": { "file_path": wts["observer"].join("inlines.py").to_string_lossy() }
        }),
    );

    // p2 appends `m2` at the exact same end-of-file spot in `inlines.py`, then finishes
    // (receiving p1's `m1` via union hand-off).
    fs::write(
        wts["p2"].join("inlines.py"),
        "class Parser:\n    def base(self) -> int:\n        return 0\n\n    def m2(self) -> int:\n        return 2\n",
    )
    .unwrap();
    post_edit(&shared, "p2", &wts["p2"].join("inlines.py"));
    let p2_handoff = stop(&shared, "p2").expect("p2 receives p1 union hand-off");
    assert!(p2_handoff.contains("`p1`"), "{p2_handoff}");
    assert_eq!(stop(&shared, "p2"), None);

    // Now consumer appends `m3` at the exact same spot in `inlines.py` and stops:
    // Concord must merge p1 + p2 cleanly WITHOUT clobbering `a.base` or falsely
    // attributing `p1`'s `notify.send` contract change to `consumer`!
    fs::write(
        wts["consumer"].join("inlines.py"),
        "class Parser:\n    def base(self) -> int:\n        return 0\n\n    def m3(self) -> int:\n        return 3\n",
    )
    .unwrap();
    post_edit(&shared, "consumer", &wts["consumer"].join("inlines.py"));
    let c_handoff = stop(&shared, "consumer").expect("consumer receives union hand-off");
    assert!(c_handoff.contains("both additions are kept"), "{c_handoff}");
    assert_eq!(stop(&shared, "consumer"), None);

    let final_inlines = fs::read_to_string(wts["consumer"].join("inlines.py")).unwrap();
    assert!(
        final_inlines.contains("def m1") && final_inlines.contains("def m2") && final_inlines.contains("def m3"),
        "{final_inlines}"
    );

    // Observer must have ZERO phantom notices in its outbox!
    let obs_after = live::handle(
        &shared,
        "observer",
        "PostToolUse",
        &json!({
            "session_id": "s-observer",
            "tool_name": "Read",
            "tool_input": { "file_path": wts["observer"].join("inlines.py").to_string_lossy() }
        }),
    )
    .0;
    assert!(obs_after.is_empty(), "expected zero phantom notices, got: {obs_after}");
}

#[test]
fn duplicate_symbol_definitions_across_non_overlapping_lines_are_gated() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    let base_src = concat!(
        "class InlineParser:\n",
        "    def parse_text(self):\n",
        "        return 't'\n",
        "\n",
        "    def parse_Middle(self):\n",
        "        return 'm'\n",
        "\n",
        "    def parse_end(self):\n",
        "        return 'e'\n",
    );
    fs::write(root.join("inlines.py"), base_src).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);

    let repo = Repo::open(&root).unwrap();
    let base = repo.rev_parse("HEAD").unwrap();
    let mut st = State::new(
        repo.clone(),
        base.clone(),
        Mode::Enforce,
        None,
        Duration::from_millis(100),
    )
    .unwrap();
    st.regions = true;

    let wt1 = tmp.path().join("wt/escapes");
    let wt2 = tmp.path().join("wt/images");
    repo.worktree_add(&wt1, "b/escapes", &base).unwrap();
    repo.worktree_add(&wt2, "b/images", &base).unwrap();
    st.add_agent("escapes", wt1.clone(), "b/escapes".into(), "escapes");
    st.add_agent("images", wt2.clone(), "b/images".into(), "images");
    let shared: Shared = Arc::new(Mutex::new(st));

    // `escapes` inserts `parse_backslash` near the top; `images` inserts `parse_backslash` at the bottom.
    // Textually, git merge-tree would merge these cleanly and produce duplicate methods!
    let src1 = base_src.replace(
        "    def parse_text(self):\n        return 't'\n",
        "    def parse_text(self):\n        return 't'\n\n    def parse_backslash(self):\n        return '\\\\'\n",
    );
    fs::write(wt1.join("inlines.py"), &src1).unwrap();
    assert!(post_edit(&shared, "escapes", &wt1.join("inlines.py")).is_empty());
    assert_eq!(stop(&shared, "escapes"), None);

    let src2 = format!("{base_src}\n    def parse_backslash(self):\n        return 'dup'\n");
    fs::write(wt2.join("inlines.py"), &src2).unwrap();
    let notice = post_edit(&shared, "images", &wt2.join("inlines.py"));
    assert!(
        notice.contains("duplicate symbol") && notice.contains("InlineParser.parse_backslash"),
        "expected immediate duplicate symbol notice, got: {notice}"
    );

    let block = stop(&shared, "images").expect("must block duplicate symbol at finish gate");
    assert!(block.contains("InlineParser.parse_backslash"), "{block}");

    // Renaming `images`'s helper clears the contention immediately.
    let src2_fixed = format!("{base_src}\n    def parse_image_escape(self):\n        return 'ok'\n");
    fs::write(wt2.join("inlines.py"), &src2_fixed).unwrap();
    post_edit(&shared, "images", &wt2.join("inlines.py"));
    assert_eq!(stop(&shared, "images"), None);
}

#[test]
fn grep_reads_negotiation_ack_and_file_leases_work_live() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(
        root.join("api.py"),
        "def charge_card(amount: int) -> bool:\n    return amount > 0\n",
    )
    .unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);

    let repo = Repo::open(&root).unwrap();
    let base = repo.rev_parse("HEAD").unwrap();
    let mut st = State::new(
        repo.clone(),
        base.clone(),
        Mode::Enforce,
        None,
        Duration::from_millis(50),
    )
    .unwrap();
    st.file_leases = true;
    let wt_a = tmp.path().join("wt/a");
    let wt_b = tmp.path().join("wt/b");
    repo.worktree_add(&wt_a, "b/a", &base).unwrap();
    repo.worktree_add(&wt_b, "b/b", &base).unwrap();
    st.add_agent("a", wt_a.clone(), "b/a".into(), "billing");
    st.add_agent("b", wt_b.clone(), "b/b".into(), "checkout");
    let shared: Shared = Arc::new(Mutex::new(st));

    // Agent `b` discovers `charge_card` via Grep (without Read!).
    live::handle(
        &shared,
        "b",
        "PostToolUse",
        &json!({
            "session_id": "s-b",
            "tool_name": "Grep",
            "tool_input": { "pattern": "charge_card" }
        }),
    );

    // Agent `a` modifies `charge_card` signature -> acquires contract lease + file lease on `api.py`.
    fs::write(
        wt_a.join("api.py"),
        "def charge_card(amount: int, currency: str) -> bool:\n    return amount > 0 and bool(currency)\n",
    )
    .unwrap();
    post_edit(&shared, "a", &wt_a.join("api.py"));

    // Agent `b` sends a contract negotiation proposal to `a` and gets routed cleanly.
    let (neg_out, code) = live::handle(
        &shared,
        "b",
        "Negotiate",
        &json!({ "symbol": "api.charge_card", "proposal": "default currency='USD' so callers stay compatible" }),
    );
    assert_eq!(code, 0);
    let neg_v: Value = serde_json::from_str(&neg_out).unwrap();
    assert_eq!(neg_v["routed_to"], "a");

    // Agent `a` receives `b`'s negotiation proposal on its next tool call!
    let a_ctx = live::handle(
        &shared,
        "a",
        "PostToolUse",
        &json!({
            "session_id": "s-a",
            "tool_name": "Read",
            "tool_input": { "file_path": wt_a.join("api.py").to_string_lossy() }
        }),
    )
    .0;
    assert!(
        a_ctx.contains("contract negotiation") && a_ctx.contains("currency='USD'"),
        "{a_ctx}"
    );

    // Because `file_leases` is enabled, if `b` also edits `api.py` while `a` holds the lease, `b` is warned & gated.
    fs::write(
        wt_b.join("api.py"),
        "def charge_card(amount: int) -> bool:\n    return amount >= 1\n",
    )
    .unwrap();
    let b_warn = post_edit(&shared, "b", &wt_b.join("api.py"));
    assert!(b_warn.contains("file lease held"), "{b_warn}");
    let b_block = stop(&shared, "b").expect("blocked by exclusive file lease");
    assert!(b_block.contains("Exclusive file leases"), "{b_block}");

    // Explicit Ack clears stale state on `b`.
    let (ack_out, ack_code) = live::handle(&shared, "b", "Ack", &json!({ "symbol": "api.charge_card" }));
    assert_eq!(ack_code, 0);
    assert!(ack_out.contains("acknowledged"));
}

#[test]
fn mcp_tools_worktrees_and_smart_merge_end_to_end() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(
        root.join("calc.py"),
        "def add(x: int, y: int) -> int:\n    return x + y\n",
    )
    .unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);

    let repo = Repo::open(&root).unwrap();
    let base = repo.rev_parse("HEAD").unwrap();

    // Create two worktrees that both append new functions to `calc.py`, where
    // `wt_b` breaks a test (`assert sub(5, 3) == 2`) so smart_merge bisects `b/sub`!
    let wt_mul = tmp.path().join("wt_mul");
    let wt_sub = tmp.path().join("wt_sub");
    repo.worktree_add(&wt_mul, "b/mul", &base).unwrap();
    repo.worktree_add(&wt_sub, "b/sub", &base).unwrap();

    fs::write(
        wt_mul.join("calc.py"),
        "def add(x: int, y: int) -> int:\n    return x + y\n\n\ndef mul(x: int, y: int) -> int:\n    return x * y\n",
    )
    .unwrap();
    fs::write(
        wt_sub.join("calc.py"),
        "def add(x: int, y: int) -> int:\n    return x + y\n\n\ndef sub(x: int, y: int) -> int:\n    return x + y\n",
    )
    .unwrap();

    let cfg = McpConfig {
        agent_cli: "opencode".into(),
        model: "test".into(),
        concord_exe: PathBuf::from("concord"),
    };

    // 1. `concord_symbols` inspects contracts concisely without bodies.
    let syms = mcp::dispatch(&cfg, &root, "concord_symbols", &json!({ "paths": ["calc.py"] })).unwrap();
    assert!(
        syms.contains("calc.add") && syms.contains("def add(x: int, y: int) -> int"),
        "{syms}"
    );

    // 2. `concord_worktrees` snapshots dirty uncommitted worktrees in memory and reports auto-unionability!
    let wts_out = mcp::dispatch(&cfg, &root, "concord_worktrees", &json!({ "base": "HEAD" })).unwrap();
    assert!(wts_out.contains("b/mul") && wts_out.contains("b/sub"), "{wts_out}");
    assert!(wts_out.contains("auto-unionable"), "{wts_out}");

    // Commit both worktrees and run `concord_diff_contracts` + `concord_smart_merge`.
    git(&wt_mul, &["commit", "-q", "-a", "-m", "mul"]);
    git(&wt_sub, &["commit", "-q", "-a", "-m", "sub"]);

    let diff_out = mcp::dispatch(
        &cfg,
        &root,
        "concord_diff_contracts",
        &json!({ "base": "main", "head": "b/mul" }),
    )
    .unwrap();
    assert!(
        diff_out.contains("ADDED") && diff_out.contains("calc.mul"),
        "{diff_out}"
    );

    // 3. `concord_smart_merge` union-merges `b/mul` and `b/sub` cleanly and bisects `b/sub` as the test culprit!
    let merge_out = mcp::dispatch(
        &cfg,
        &root,
        "concord_smart_merge",
        &json!({
            "base": "main",
            "branches": ["b/mul", "b/sub"],
            "into_branch": "merged-out",
            "test_cmd": "python3 -c 'import calc; assert calc.mul(2, 3) == 6; assert (not hasattr(calc, \"sub\")) or calc.sub(5, 3) == 2'"
        }),
    )
    .unwrap();
    assert!(merge_out.contains("Verified insertion-union merges"), "{merge_out}");
    assert!(
        merge_out.contains("Post-merge contract conformance: PASS"),
        "{merge_out}"
    );
    assert!(merge_out.contains("Bisected culprit branch: `b/sub`"), "{merge_out}");
}
