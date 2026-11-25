//! Fails-together check of the finish gate, end to end through the hooks: a
//! test that passes on the agent's own work but fails once it is merged with a
//! finished peer blocks the agent, until it adapts.

use concord_cli::live::{self, Mode, Shared, State};
use concord_git::Repo;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Runs every tests/*.sh and reports failures like pytest does.
const TEST_CMD: &str =
    "fail=0; for t in tests/*.sh; do sh \"$t\" >/dev/null 2>&1 || { echo \"FAILED $t\"; fail=1; }; done; exit $fail";

fn git(dir: &Path, args: &[&str]) {
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
}

fn write(shared: &Shared, agent: &str, wt: &Path, file: &str, text: &str) {
    let path = wt.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, text).unwrap();
    let input = json!({
        "session_id": format!("s-{agent}"),
        "tool_name": "Write",
        "tool_input": { "file_path": path.to_string_lossy() },
    });
    live::handle(shared, agent, "PostToolUse", &input);
}

fn stop(shared: &Shared, agent: &str) -> Option<String> {
    let (out, _) = live::handle(shared, agent, "Stop", &json!({ "session_id": format!("s-{agent}") }));
    if out.is_empty() {
        return None;
    }
    let v: Value = serde_json::from_str(&out).unwrap();
    Some(v["reason"].as_str().unwrap().to_owned())
}

#[test]
fn a_test_that_fails_together_blocks_until_fixed() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(root.join("tests")).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("value.txt"), "1\n").unwrap();
    fs::write(root.join("tests/base.sh"), "test -f value.txt\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let repo = Repo::open(&root).unwrap();
    let base = repo.rev_parse("HEAD").unwrap();

    let mut st = State::new(
        repo.clone(),
        base.clone(),
        Mode::Enforce,
        None,
        Duration::from_millis(300),
    )
    .unwrap();
    st.regions = true;
    st.together = Some(TEST_CMD.to_owned());
    let mut wts = Vec::new();
    for name in ["a", "b"] {
        let wt = tmp.path().join("wt").join(name);
        repo.worktree_add(&wt, &format!("b/{name}"), &base).unwrap();
        st.add_agent(name, wt.clone(), format!("b/{name}"), "task");
        wts.push(wt);
    }
    let shared: Shared = Arc::new(Mutex::new(st));

    // A changes the value (and tests the new one); B relies on the old value.
    write(&shared, "a", &wts[0], "value.txt", "2\n");
    write(&shared, "a", &wts[0], "tests/a.sh", "grep -qx 2 value.txt\n");
    write(&shared, "b", &wts[1], "tests/b.sh", "grep -qx 1 value.txt\n");

    // A finishes first: nothing finished to merge with.
    assert_eq!(stop(&shared, "a"), None);
    // B passes alone but fails together with A: blocked, with the test named.
    let reason = stop(&shared, "b").expect("fails together");
    assert!(
        reason.contains("fails together") && reason.contains("tests/b.sh"),
        "{reason}"
    );
    assert!(reason.contains("`a`") && reason.contains("value.txt"), "{reason}");
    // B adapts to A's change and may finish.
    write(&shared, "b", &wts[1], "tests/b.sh", "grep -qx 2 value.txt\n");
    assert_eq!(stop(&shared, "b"), None);
}

/// The ticket's acceptance check, also without coordination: the agent is
/// sent back while the check fails and it makes progress, and may finish when
/// the check passes or stops improving.
#[test]
fn acceptance_check_sends_the_agent_back_while_it_makes_progress() {
    const CHECK: &str = "fail=0; for f in a b; do [ -f $f ] || { echo \"FAILED t::$f\"; fail=1; }; done; exit $fail";
    for finish_all in [true, false] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("README"), "x\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "base"]);
        let repo = Repo::open(&root).unwrap();
        let base = repo.rev_parse("HEAD").unwrap();
        let mut st = State::new(repo.clone(), base.clone(), Mode::Off, None, Duration::from_millis(200)).unwrap();
        let wt = tmp.path().join("wt").join("a");
        repo.worktree_add(&wt, "b/a", &base).unwrap();
        st.add_agent("a", wt.clone(), "b/a".into(), "task");
        st.agents.get_mut("a").unwrap().check = Some(CHECK.to_owned());
        let shared: Shared = Arc::new(Mutex::new(st));

        // Two failures: sent back, with the failing checks named.
        let reason = stop(&shared, "a").expect("check fails");
        assert!(reason.contains("t::a") && reason.contains("t::b"), "{reason}");
        // One fixed: progress, sent back again.
        write(&shared, "a", &wt, "a", "");
        assert!(stop(&shared, "a").is_some());
        if finish_all {
            // All fixed: may finish.
            write(&shared, "a", &wt, "b", "");
        }
        // Passing, or no progress since the last attempt: may finish.
        assert_eq!(stop(&shared, "a"), None, "finish_all={finish_all}");
    }
}

/// Reproduces the runtime handler-table overwrite seen on CommonMark (`links`
/// overwriting `images-autolinks`'s `"!"` handler without `--fails-together`):
/// `peer_check_gate` detects that `links` regressed `images`'s passing `check`,
/// merges `images` into `links`'s worktree, and blocks until both pass.
#[test]
fn peer_check_gate_catches_cross_agent_runtime_regression_and_merges_peer() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    // Two separate hook slots so `images` and `links` have ZERO textual or symbol conflict!
    fs::write(
        root.join("dispatch.py"),
        concat!(
            "HANDLERS = {}\n\n",
            "def setup_a():\n    pass\n\n\n",
            "def setup_b():\n    pass\n\n\n",
            "def register_all():\n    setup_a()\n    setup_b()\n",
        ),
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

    let wt_img = tmp.path().join("wt/images");
    let wt_lnk = tmp.path().join("wt/links");
    repo.worktree_add(&wt_img, "b/images", &base).unwrap();
    repo.worktree_add(&wt_lnk, "b/links", &base).unwrap();
    st.add_agent("images", wt_img.clone(), "b/images".into(), "images");
    st.add_agent("links", wt_lnk.clone(), "b/links".into(), "links");
    st.agents.get_mut("images").unwrap().check = Some(
        "python3 -c 'import dispatch; dispatch.register_all(); assert dispatch.HANDLERS.get(\"!\") == \"image\"' || { echo 'FAILED test_spec[Images-572]'; exit 1; }".into(),
    );
    st.agents.get_mut("links").unwrap().check = Some(
        "python3 -c 'import dispatch; dispatch.register_all(); assert dispatch.HANDLERS.get(\"[\") == \"link\"' || { echo 'FAILED test_spec[Links-500]'; exit 1; }".into(),
    );
    let shared: Shared = Arc::new(Mutex::new(st));

    // `images` edits `setup_a` (lines 3-4) and finishes cleanly with 0 failures.
    write(
        &shared,
        "images",
        &wt_img,
        "dispatch.py",
        concat!(
            "HANDLERS = {}\n\n",
            "def setup_a():\n    HANDLERS.setdefault('!', 'image')\n\n\n",
            "def setup_b():\n    pass\n\n\n",
            "def register_all():\n    setup_a()\n    setup_b()\n",
        ),
    );
    assert_eq!(stop(&shared, "images"), None);

    // `links` edits `setup_b` (disjoint lines & disjoint symbol!) and overwrites `HANDLERS['!'] = 'bang'`.
    write(
        &shared,
        "links",
        &wt_lnk,
        "dispatch.py",
        concat!(
            "HANDLERS = {}\n\n",
            "def setup_a():\n    pass\n\n\n",
            "def setup_b():\n    HANDLERS['['] = 'link'\n    HANDLERS['!'] = 'bang'\n\n\n",
            "def register_all():\n    setup_a()\n    setup_b()\n",
        ),
    );

    // When `links` tries to finish: its own check passes, git merge is 100% clean,
    // contracts are unchanged, yet `peer_check_gate` catches `Images-572` regressing
    // AND merges `images` into `wt_lnk`!
    let block = stop(&shared, "links").expect("peer_check_gate must block regression");
    assert!(
        block.contains("peer regression") && block.contains("`images`") && block.contains("Images-572"),
        "{block}"
    );
    let merged_src = fs::read_to_string(wt_lnk.join("dispatch.py")).unwrap();
    assert!(
        merged_src.contains("HANDLERS.setdefault('!', 'image')"),
        "expected `images`'s code merged into `links` worktree: {merged_src}"
    );

    // `links` updates `HANDLERS['!']` -> `HANDLERS.setdefault('!', 'bang')` on top of `images` and finishes!
    write(
        &shared,
        "links",
        &wt_lnk,
        "dispatch.py",
        concat!(
            "HANDLERS = {}\n\n",
            "def setup_a():\n    HANDLERS.setdefault('!', 'image')\n\n\n",
            "def setup_b():\n    HANDLERS['['] = 'link'\n    HANDLERS.setdefault('!', 'bang')\n\n\n",
            "def register_all():\n    setup_a()\n    setup_b()\n",
        ),
    );
    assert_eq!(stop(&shared, "links"), None);
}
