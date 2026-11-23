//! Region coherence end to end, through the hook protocol: two agents append a
//! function at the same place; the higher-ranked one is told, waits for the
//! owner, moves its code, and receives the owner's finished work by a clean
//! hand-off. The final branches merge without conflict.

use concord_cli::live::{self, Mode, Shared, State};
use concord_git::Repo;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BASE: &str = "def a():\n    return 1\n\n\ndef b():\n    return 2\n";

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

fn edit(shared: &Shared, agent: &str, wt: &Path, text: &str) -> String {
    fs::write(wt.join("m.py"), text).unwrap();
    let input = json!({
        "session_id": format!("s-{agent}"),
        "tool_name": "Edit",
        "tool_input": { "file_path": wt.join("m.py").to_string_lossy() },
    });
    live::handle(shared, agent, "PostToolUse", &input).0
}

/// The finish gate's decision: None = allowed, Some(reason) = blocked.
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
fn colliding_appends_are_resolved_without_conflict_markers() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("m.py"), BASE).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let repo = Repo::open(&root).unwrap();
    let base = repo.rev_parse("HEAD").unwrap();

    let mut st = State::new(
        repo.clone(),
        base.clone(),
        Mode::Enforce,
        None,
        Duration::from_millis(500),
    )
    .unwrap();
    st.regions = true;
    let mut wts = Vec::new();
    for name in ["owner", "yielder"] {
        let wt = tmp.path().join("wt").join(name);
        let branch = format!("b/{name}");
        repo.worktree_add(&wt, &branch, &base).unwrap();
        st.add_agent(name, wt.clone(), branch, "task");
        wts.push(wt);
    }
    let shared: Shared = Arc::new(Mutex::new(st));
    let (owner_wt, yielder_wt) = (&wts[0], &wts[1]);

    // Both append a function at the end of the module: a collision.
    let owner_text = format!("{BASE}\n\ndef c():\n    return 3\n");
    assert!(edit(&shared, "owner", owner_wt, &owner_text).is_empty());
    let out = edit(
        &shared,
        "yielder",
        yielder_wt,
        &format!("{BASE}\n\ndef d():\n    return 4\n"),
    );
    assert!(out.contains("region conflict"), "{out}");
    assert!(out.contains("`owner`") && out.contains("def c():"), "{out}");
    // The same collision is not reported twice.
    assert!(
        !edit(
            &shared,
            "yielder",
            yielder_wt,
            &format!("{BASE}\n\ndef d():\n    return 44\n")
        )
        .contains("region")
    );

    // The yielder may not finish while the running owner holds the region.
    let reason = stop(&shared, "yielder").expect("blocked");
    assert!(reason.contains("region conflict"), "{reason}");

    // The owner (lower rank) is never held back by the yielder.
    assert_eq!(stop(&shared, "owner"), None);

    // Still conflicting with the now finished owner: blocked again.
    let reason = stop(&shared, "yielder").expect("blocked");
    assert!(reason.contains("bring their finished version"), "{reason}");

    // The yielder restores the end of the file and puts its code elsewhere.
    edit(
        &shared,
        "yielder",
        yielder_wt,
        &format!("def d():\n    return 4\n\n\n{BASE}"),
    );
    // Clean now: Concord hands the owner's finished work over...
    let reason = stop(&shared, "yielder").expect("hand-off");
    assert!(reason.contains("merged their finished work"), "{reason}");
    let merged = fs::read_to_string(yielder_wt.join("m.py")).unwrap();
    assert!(merged.contains("def c():") && merged.contains("def d():"), "{merged}");
    assert!(!merged.contains("<<<<<<<"));
    // ...and the yielder may finish.
    assert_eq!(stop(&shared, "yielder"), None);

    // The two branches integrate without conflict.
    git(yielder_wt, &["add", "-A"]);
    git(yielder_wt, &["commit", "-q", "-m", "yielder", "--allow-empty"]);
    git(owner_wt, &["add", "-A"]);
    git(owner_wt, &["commit", "-q", "-m", "owner", "--allow-empty"]);
    assert!(repo.merge_tree("b/owner", "b/yielder").unwrap().clean);
}

fn read(shared: &Shared, agent: &str, wt: &Path) {
    let input = json!({
        "session_id": format!("s-{agent}"),
        "tool_name": "Read",
        "tool_input": { "file_path": wt.join("m.py").to_string_lossy() },
    });
    live::handle(shared, agent, "PostToolUse", &input);
}

/// A contract wait (owner depends on the producer's new signature) and a region
/// wait (the producer yields a region to the owner) must not form a cycle: the
/// second wait is not entered, and the agent gets actionable instructions at
/// once instead of a timeout.
#[test]
fn contract_and_region_waits_never_form_a_cycle() {
    const BASE2: &str = "def g(x):\n    return x\n\n\ndef a():\n    return g(1)\n";
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("m.py"), BASE2).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let repo = Repo::open(&root).unwrap();
    let base = repo.rev_parse("HEAD").unwrap();
    let log = tmp.path().join("events.jsonl");
    let mut st = State::new(
        repo.clone(),
        base.clone(),
        Mode::Enforce,
        Some(&log),
        Duration::from_secs(4),
    )
    .unwrap();
    st.regions = true;
    let mut wts = Vec::new();
    for name in ["owner", "producer"] {
        let wt = tmp.path().join("wt").join(name);
        repo.worktree_add(&wt, &format!("b/{name}"), &base).unwrap();
        st.add_agent(name, wt.clone(), format!("b/{name}"), "task");
        wts.push(wt);
    }
    let shared: Shared = Arc::new(Mutex::new(st));
    let (owner_wt, producer_wt) = (wts[0].clone(), wts[1].clone());

    // The producer makes `g` take a required parameter and appends a function;
    // the owner appends another function at the same place.
    edit(
        &shared,
        "owner",
        &owner_wt,
        &format!("{BASE2}\n\ndef c():\n    return 3\n"),
    );
    read(&shared, "owner", &owner_wt);
    edit(
        &shared,
        "producer",
        &producer_wt,
        &format!(
            "{}\n\ndef d():\n    return 4\n",
            BASE2.replace("def g(x):", "def g(x, y):")
        ),
    );

    // The producer waits for the owner of the region...
    let s2 = shared.clone();
    let waiter = std::thread::spawn(move || stop(&s2, "producer"));
    std::thread::sleep(Duration::from_millis(600));
    // ...so the owner must not wait for the producer's contract.
    let t0 = std::time::Instant::now();
    let reason = stop(&shared, "owner").unwrap_or_else(|| panic!("{}", fs::read_to_string(&log).unwrap()));
    assert!(t0.elapsed() < Duration::from_secs(2), "owner waited {:?}", t0.elapsed());
    assert!(
        reason.contains("adapt your own call sites"),
        "{reason}\n{}",
        fs::read_to_string(&log).unwrap()
    );
    assert!(reason.contains("g(x, y)") || reason.contains("g"), "{reason}");
    let _ = waiter.join().unwrap();
}
