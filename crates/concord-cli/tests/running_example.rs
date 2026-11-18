//! End-to-end test on the running example of the thesis
//! (examples/running-example/setup.sh).

use concord_cli::{analyze_branches, simulate};
use concord_core::{Finding, NoticeKind};
use concord_git::Repo;
use std::path::Path;
use std::process::Command;

fn setup() -> (tempfile::TempDir, Repo) {
    let tmp = tempfile::tempdir().unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/running-example/setup.sh");
    let status = Command::new("bash")
        .arg(&script)
        .arg(tmp.path().join("repo"))
        .stdout(std::process::Stdio::null())
        .status()
        .expect("run setup.sh");
    assert!(status.success());
    let repo = Repo::open(tmp.path().join("repo")).unwrap();
    (tmp, repo)
}

#[test]
fn finds_the_cross_file_contract_conflict_that_git_misses() {
    let (_tmp, repo) = setup();
    let branches = vec!["agent-a".to_string(), "agent-b".to_string(), "agent-c".to_string()];
    let a = analyze_branches(&repo, "main", &branches, true).unwrap();

    // Every pairwise textual merge is clean...
    assert!(a.textual.iter().all(|t| t.clean), "{:?}", a.textual);

    // ...but agent-b depends on the contract agent-a changed, across disjoint files.
    let deps: Vec<&Finding> = a
        .findings
        .iter()
        .filter(|f| matches!(f, Finding::ContractDependency { .. }))
        .collect();
    assert_eq!(deps.len(), 1, "{:#?}", a.findings);
    match deps[0] {
        Finding::ContractDependency {
            producer,
            consumer,
            symbol,
            sites,
            file_disjoint,
            after,
            ..
        } => {
            assert_eq!(producer, "agent-a");
            assert_eq!(consumer, "agent-b");
            assert_eq!(symbol.as_str(), "notify.email.send_email");
            assert!(*file_disjoint);
            assert_eq!(after.as_ref().unwrap().display, "def send_email(to, body, *, user_id)");
            assert!(
                sites
                    .iter()
                    .any(|s| s.path == "auth/reset.py" && s.text.starts_with("send_email(user.email"))
            );
        }
        _ => unreachable!(),
    }

    // agent-a and agent-c share a file but no symbol: file locks would serialise them needlessly.
    assert_eq!(a.summary.file_overlaps, 1);
    assert_eq!(a.summary.file_overlaps_symbol_disjoint, 1);
    assert_eq!(a.summary.same_symbol, 0);

    // The protocol notifies exactly agent-b and gates only agent-b.
    let sim = simulate(&a.branches);
    let contract_notices: Vec<_> = sim
        .notices
        .iter()
        .filter(|n| matches!(n.kind, NoticeKind::ContractChanged { .. }))
        .collect();
    assert_eq!(contract_notices.len(), 1);
    assert_eq!(contract_notices[0].to.0, "agent-b");
    assert!(!sim.gates["agent-b"].is_open());
    assert!(sim.gates["agent-a"].is_open());
    assert!(sim.gates["agent-c"].is_open());
}
