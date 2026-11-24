//! Region coherence end to end, through the hook protocol: overlapping edits,
//! hand-offs, union hand-offs of insert/insert collisions, and the interaction
//! with the contract layer's waits.

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
fn colliding_appends_are_combined_when_the_second_agent_finishes() {
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

    // Both append a function at the end of the module: an insert/insert
    // collision. Nobody is asked to move code.
    let owner_text = format!("{BASE}\n\ndef c():\n    return 3\n");
    assert!(edit(&shared, "owner", owner_wt, &owner_text).is_empty());
    let out = edit(
        &shared,
        "yielder",
        yielder_wt,
        &format!("{BASE}\n\ndef d():\n    return 4\n"),
    );
    assert!(!out.contains("region conflict"), "{out}");

    // Nobody waits: the yielder may finish at once.
    assert_eq!(stop(&shared, "yielder"), None);
    // The owner finishes second: Concord combines the finished yielder's
    // insertion into its worktree (the earlier finisher's first), verified.
    let reason = stop(&shared, "owner").expect("hand-off");
    assert!(reason.contains("both additions are kept"), "{reason}");
    let merged = fs::read_to_string(owner_wt.join("m.py")).unwrap();
    assert!(merged.contains("def c():") && merged.contains("def d():"), "{merged}");
    assert!(
        merged.find("def d():") < merged.find("def c():"),
        "earlier finisher first: {merged}"
    );
    assert!(!merged.contains("<<<<<<<"));
    assert_eq!(stop(&shared, "owner"), None);

    // The owner's branch contains the yielder's: integration is clean.
    git(yielder_wt, &["add", "-A"]);
    git(yielder_wt, &["commit", "-q", "-m", "yielder", "--allow-empty"]);
    git(owner_wt, &["add", "-A"]);
    git(owner_wt, &["commit", "-q", "-m", "owner", "--allow-empty"]);
    assert!(repo.merge_tree("b/yielder", "b/owner").unwrap().clean);
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
        &format!(
            "{}\n\ndef c():\n    return g(3)\n",
            BASE2.replace("    return x\n", "    return x * 1\n")
        ),
    );
    read(&shared, "owner", &owner_wt);
    edit(
        &shared,
        "producer",
        &producer_wt,
        &BASE2
            .replace("def g(x):", "def g(x, y):")
            .replace("    return x\n", "    return x + y\n"),
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
    // Once its own call conforms, the owner may finish at once: holding a
    // conforming consumer for a producer that waits for it is a livelock.
    edit(
        &shared,
        "owner",
        &owner_wt,
        &format!(
            "{}\n\ndef c():\n    return g(3, 0)\n",
            BASE2.replace("    return x\n", "    return x * 1\n")
        ),
    );
    let t0 = std::time::Instant::now();
    assert_eq!(stop(&shared, "owner"), None, "{}", fs::read_to_string(&log).unwrap());
    assert!(t0.elapsed() < Duration::from_secs(2), "owner waited {:?}", t0.elapsed());
    let _ = waiter.join().unwrap();
}

/// A yielder that restored the contested lines may not finish before the
/// running owner: it must first receive the owner's version (the hand-off),
/// so that it can re-apply its own change to those lines on top.
#[test]
fn yielder_waits_for_the_hand_off_even_after_restoring() {
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
        Duration::from_millis(300),
    )
    .unwrap();
    st.regions = true;
    let mut wts = Vec::new();
    for name in ["owner", "yielder"] {
        let wt = tmp.path().join("wt").join(name);
        repo.worktree_add(&wt, &format!("b/{name}"), &base).unwrap();
        st.add_agent(name, wt.clone(), format!("b/{name}"), "task");
        wts.push(wt);
    }
    let shared: Shared = Arc::new(Mutex::new(st));
    let (owner_wt, yielder_wt) = (&wts[0], &wts[1]);
    // Both change the same line.
    edit(&shared, "owner", owner_wt, &BASE.replace("return 1", "return 10"));
    let out = edit(&shared, "yielder", yielder_wt, &BASE.replace("return 1", "return 11"));
    assert!(out.contains("region conflict"), "{out}");
    // The yielder restores the line and tries to finish while the owner runs.
    edit(&shared, "yielder", yielder_wt, BASE);
    let reason = stop(&shared, "yielder").expect("blocked");
    assert!(reason.contains("You yielded lines"), "{reason}");
    // The owner finishes; the yielder now gets the hand-off, then may finish.
    assert_eq!(stop(&shared, "owner"), None);
    let reason = stop(&shared, "yielder").expect("hand-off");
    assert!(reason.contains("merged their finished work"), "{reason}");
    assert!(
        fs::read_to_string(yielder_wt.join("m.py"))
            .unwrap()
            .contains("return 10")
    );
    assert_eq!(stop(&shared, "yielder"), None);
}

/// Contract obligations cover only the call sites an agent wrote: a consumer
/// that touched a function containing a pre-existing call is not blocked on
/// it when another agent changes the callee's signature (guards the base-ref
/// exclusion in `footprint_and_index` and its text fallback in `conformance`).
#[test]
fn pre_existing_call_sites_are_not_the_consumers_obligation() {
    const BASE3: &str = "def g(x):\n    return x\n\n\ndef a():\n    v = 1\n    return g(v)\n";
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("m.py"), BASE3).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let repo = Repo::open(&root).unwrap();
    let base = repo.rev_parse("HEAD").unwrap();
    let log = tmp.path().join("events.jsonl");
    let st = State::new(
        repo.clone(),
        base.clone(),
        Mode::Enforce,
        Some(&log),
        Duration::from_millis(500),
    )
    .unwrap();
    let shared: Shared = Arc::new(Mutex::new(st));
    let mut wts = Vec::new();
    for name in ["producer", "consumer"] {
        let wt = tmp.path().join("wt").join(name);
        repo.worktree_add(&wt, &format!("b/{name}"), &base).unwrap();
        shared
            .lock()
            .unwrap()
            .add_agent(name, wt.clone(), format!("b/{name}"), "task");
        wts.push(wt);
    }
    let (producer_wt, consumer_wt) = (&wts[0], &wts[1]);
    // The consumer edits `a` (not its call) and reads the module.
    edit(&shared, "consumer", consumer_wt, &BASE3.replace("v = 1", "v = 2"));
    read(&shared, "consumer", consumer_wt);
    // The producer gives `g` a required parameter and finishes.
    edit(
        &shared,
        "producer",
        producer_wt,
        &BASE3.replace("def g(x):", "def g(x, y):"),
    );
    assert_eq!(stop(&shared, "producer"), None);
    // The consumer receives the producer's work, is asked to verify once, and
    // may then finish: `g(v)` was already there, it is not the consumer's.
    let mut blocks = 0;
    while let Some(reason) = stop(&shared, "consumer") {
        assert!(!reason.contains("does not conform"), "{reason}");
        blocks += 1;
        assert!(blocks < 3, "{reason}");
    }
    assert!(fs::read_to_string(&log).unwrap().contains("\"ack\""));
}

/// Hand-offs are batched (group commit): while an owner the agent yielded to
/// is still running, Concord waits for it instead of handing over the owners
/// that have already finished, so the agent re-tests once.
#[test]
fn hand_offs_are_batched_while_owners_are_running() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("m.py"), BASE).unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);
    let repo = Repo::open(&root).unwrap();
    let base = repo.rev_parse("HEAD").unwrap();
    let mut st = State::new(repo.clone(), base.clone(), Mode::Enforce, None, Duration::from_secs(5)).unwrap();
    st.regions = true;
    let mut wts = Vec::new();
    for name in ["o1", "o2", "y"] {
        let wt = tmp.path().join("wt").join(name);
        repo.worktree_add(&wt, &format!("b/{name}"), &base).unwrap();
        st.add_agent(name, wt.clone(), format!("b/{name}"), "task");
        wts.push(wt);
    }
    let shared: Shared = Arc::new(Mutex::new(st));
    edit(&shared, "o1", &wts[0], &BASE.replace("return 1", "return 10"));
    edit(&shared, "o2", &wts[1], &BASE.replace("return 2", "return 20"));
    let both = BASE.replace("return 1", "return 11").replace("return 2", "return 22");
    assert!(edit(&shared, "y", &wts[2], &both).contains("region conflict"));
    // The yielder restores both lines; o1 finishes.
    edit(&shared, "y", &wts[2], BASE);
    assert_eq!(stop(&shared, "o1"), None);
    // The yielder tries to finish while o2 runs: Concord waits for o2...
    let s2 = shared.clone();
    let waiter = std::thread::spawn(move || stop(&s2, "y"));
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(stop(&shared, "o2"), None);
    // ...and then hands over both owners at once.
    let reason = waiter.join().unwrap().expect("hand-off");
    assert!(reason.contains("`o1`") && reason.contains("`o2`"), "{reason}");
    let merged = fs::read_to_string(wts[2].join("m.py")).unwrap();
    assert!(merged.contains("return 10") && merged.contains("return 20"), "{merged}");
    assert_eq!(stop(&shared, "y"), None);
}

/// After a consumer has finished, a contract change resumes it only if its
/// own call sites violate the new contract; a conforming consumer is left
/// alone (integration merges both sides).
#[test]
fn finished_consumers_are_resumed_only_for_real_violations() {
    for (call, expect_resume) in [("g(3)", true), ("g(3, 0)", false)] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("m.py"), "def g(x, y=0):\n    return x\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "base"]);
        let repo = Repo::open(&root).unwrap();
        let base = repo.rev_parse("HEAD").unwrap();
        let st = State::new(
            repo.clone(),
            base.clone(),
            Mode::Enforce,
            None,
            Duration::from_millis(200),
        )
        .unwrap();
        let shared: Shared = Arc::new(Mutex::new(st));
        let mut wts = Vec::new();
        for name in ["producer", "consumer"] {
            let wt = tmp.path().join("wt").join(name);
            repo.worktree_add(&wt, &format!("b/{name}"), &base).unwrap();
            shared
                .lock()
                .unwrap()
                .add_agent(name, wt.clone(), format!("b/{name}"), "task");
            wts.push(wt);
        }
        // The consumer adds a caller in its own module and finishes.
        let user = wts[1].join("u.py");
        fs::write(&user, format!("from m import g\n\n\ndef c():\n    return {call}\n")).unwrap();
        live::handle(
            &shared,
            "consumer",
            "PostToolUse",
            &json!({ "session_id": "s-c", "tool_name": "Write", "tool_input": { "file_path": user.to_string_lossy() } }),
        );
        read(&shared, "consumer", &wts[1]);
        assert_eq!(stop(&shared, "consumer"), None);
        // The producer makes `y` required.
        edit(&shared, "producer", &wts[0], "def g(x, y):\n    return x\n");
        assert_eq!(stop(&shared, "producer"), None);
        let resume = shared.lock().unwrap().needs_resume("consumer");
        assert_eq!(resume.is_some(), expect_resume, "call {call}: {resume:?}");
    }
}
