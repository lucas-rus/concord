//! The repair pass after integration resumes every ticket whose acceptance
//! check still fails on the integrated tree, not only the first few.

use concord_cli::live::Mode;
use concord_cli::run::{self, AgentCli, RunConfig, Task};
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?}");
}

/// A stand-in for `claude -p`: the first turn writes a draft of the agent's
/// file; a turn that says the teammates' code is integrated writes the fix.
const FAKE_AGENT: &str = r#"#!/bin/sh
prompt="$2"; sid=""; prev=""
for a in "$@"; do
  if [ "$prev" = "--session-id" ] || [ "$prev" = "--resume" ]; then sid="$a"; fi
  prev="$a"
done
name=$(basename "$PWD")
case "$prompt" in
  *"integrated into your worktree"*) echo fixed > "$name.txt" ;;
  *) echo draft > "$name.txt" ;;
esac
printf '{"session_id":"%s","result":"done","is_error":false,"total_cost_usd":0}\n' "$sid"
"#;

#[test]
fn every_ticket_failing_after_integration_is_repaired() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("README"), "x\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let agent = tmp.path().join("fake-claude");
    fs::write(&agent, FAKE_AGENT).unwrap();
    Command::new("chmod").arg("+x").arg(&agent).status().unwrap();

    // Six tickets; each check needs every ticket's file (only true after
    // integration) and its own fix. The old pass repaired at most four.
    let names: Vec<String> = (1..=6).map(|i| format!("t{i}")).collect();
    let all = names
        .iter()
        .map(|n| format!("test -f {n}.txt"))
        .collect::<Vec<_>>()
        .join(" && ");
    let tasks = names
        .iter()
        .map(|n| Task {
            name: n.clone(),
            prompt: format!("write {n}.txt"),
            check: Some(format!("{all} && grep -q fixed {n}.txt")),
        })
        .collect();
    let cfg = RunConfig {
        repo: repo.clone(),
        base: "main".into(),
        tasks,
        model: "fake".into(),
        mode: Mode::Enforce,
        budget_usd: 1.0,
        test_cmd: "true".into(),
        allowed_tools: vec![],
        out_dir: tmp.path().join("runs"),
        max_wait: Duration::from_secs(5),
        peer_merge: false,
        contracts: true,
        regions: true,
        file_leases: false,
        overlap_warnings: false,
        all_contracts: false,
        deliver_notices: true,
        enforce_gate: true,
        publish_debounce: Duration::from_millis(0),
        speculative_secs: None,
        fails_together: false,
        lean: false,
        trim_outputs: false,
        fresh_repairs: false,
        gate_variant: None,
        merge_driver: None,
        resume_rounds: 1,
        claude: agent.to_string_lossy().into_owned(),
        agent_cli: AgentCli::Claude,
        opencode: "opencode".into(),
        concord_exe: env!("CARGO_BIN_EXE_concord").into(),
    };
    let report = run::run(&cfg).unwrap();
    assert!(report.integration.conflicts.is_empty());
    let iwt = report.run_dir.join("wt").join("_integration");
    for n in &names {
        assert_eq!(
            fs::read_to_string(iwt.join(format!("{n}.txt"))).unwrap(),
            "fixed\n",
            "{n} not repaired"
        );
    }
    let events = fs::read_to_string(report.run_dir.join("events.jsonl")).unwrap();
    assert_eq!(events.matches("\"post_merge_resume\"").count(), 6);
    assert_eq!(events.matches("\"kept\":true").count(), 6);
}
