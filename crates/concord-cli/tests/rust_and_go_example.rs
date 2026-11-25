//! End-to-end tests of Rust and Go cross-file contract coherence: git sees
//! clean merges across disjoint files, while Concord catches the broken call
//! contracts both offline (`analyze_branches`) and live (`PostToolUse` / `Stop`).

use concord_cli::analyze_branches;
use concord_cli::live::{self, Mode, Shared, State};
use concord_core::Finding;
use concord_git::Repo;
use serde_json::{Value, json};
use std::fs;
use std::path::Path;
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

#[test]
fn finds_the_cross_file_contract_conflict_in_rust() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(root.join("src/notify")).unwrap();
    fs::create_dir_all(root.join("src/auth")).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("src/lib.rs"), "pub mod notify;\npub mod auth;\n").unwrap();
    fs::write(
        root.join("src/notify/mod.rs"),
        "pub fn send_email(to: &str, body: &str) -> bool { !to.is_empty() && !body.is_empty() }\n",
    )
    .unwrap();
    fs::write(root.join("src/auth/mod.rs"), "pub fn ping() -> bool { true }\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "-q", "-m", "base"]);

    let repo = Repo::open(&root).unwrap();
    let base = repo.rev_parse("HEAD").unwrap();

    // Producer branch adds a required `user_id: u64` parameter to `send_email`.
    git(&root, &["checkout", "-q", "-b", "b/audit", &base]);
    fs::write(
        root.join("src/notify/mod.rs"),
        "pub fn send_email(to: &str, body: &str, user_id: u64) -> bool { user_id > 0 && !to.is_empty() && !body.is_empty() }\n",
    )
    .unwrap();
    git(&root, &["commit", "-q", "-a", "-m", "audit"]);

    // Consumer branch calls `send_email` with the old 2-arg signature in a disjoint file.
    git(&root, &["checkout", "-q", "-b", "b/reset", &base]);
    fs::write(
        root.join("src/auth/mod.rs"),
        "use crate::notify::send_email;\npub fn reset_password(addr: &str) -> bool { send_email(addr, \"tok\") }\n",
    )
    .unwrap();
    git(&root, &["commit", "-q", "-a", "-m", "reset"]);
    git(&root, &["checkout", "-q", "main"]);

    // Git thinks the merge is 100% clean!
    assert!(repo.merge_tree("b/audit", "b/reset").unwrap().clean);

    // Concord catches the breaking cross-file contract violation.
    let a = analyze_branches(&repo, &base, &["b/audit".into(), "b/reset".into()], true).unwrap();
    assert_eq!(a.summary.breaking_contract_dependencies, 1);
    assert_eq!(a.summary.breaking_contract_dependencies_file_disjoint, 1);
    let dep = a
        .findings
        .iter()
        .find_map(|f| match f {
            Finding::ContractDependency {
                producer,
                consumer,
                breaking,
                violations,
                ..
            } => Some((producer, consumer, *breaking, violations)),
            _ => None,
        })
        .expect("contract dependency");
    assert_eq!(dep.0, "b/audit");
    assert_eq!(dep.1, "b/reset");
    assert!(dep.2);
    assert!(!dep.3.is_empty(), "expected call arity violation: {:?}", dep.3);
}

#[test]
fn finds_the_cross_package_contract_conflict_in_go_and_gates_live() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(root.join("pkg/notify")).unwrap();
    fs::create_dir_all(root.join("pkg/auth")).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("go.mod"), "module github.com/acme/service\n\ngo 1.22\n").unwrap();
    fs::write(
        root.join("pkg/notify/email.go"),
        "package notify\n\nfunc SendEmail(to string, body string) error {\n\treturn nil\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("pkg/auth/reset.go"),
        "package auth\n\nfunc Ready() bool { return true }\n",
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
    let wt_audit = tmp.path().join("wt/audit");
    let wt_reset = tmp.path().join("wt/reset");
    repo.worktree_add(&wt_audit, "b/audit", &base).unwrap();
    repo.worktree_add(&wt_reset, "b/reset", &base).unwrap();
    st.add_agent("audit", wt_audit.clone(), "b/audit".into(), "add audit userID");
    st.add_agent("reset", wt_reset.clone(), "b/reset".into(), "password reset");
    let shared: Shared = Arc::new(Mutex::new(st));

    // Reset agent writes a 2-arg call to `notify.SendEmail`.
    fs::write(
        wt_reset.join("pkg/auth/reset.go"),
        "package auth\n\nimport \"github.com/acme/service/pkg/notify\"\n\nfunc RequestReset(addr string) error {\n\treturn notify.SendEmail(addr, \"tok\")\n}\n",
    )
    .unwrap();
    live::handle(
        &shared,
        "reset",
        "PostToolUse",
        &json!({
            "session_id": "s-reset",
            "tool_name": "Edit",
            "tool_input": { "file_path": wt_reset.join("pkg/auth/reset.go").to_string_lossy() }
        }),
    );

    // Audit agent adds a 3rd required parameter `userID string` to `SendEmail` and finishes.
    fs::write(
        wt_audit.join("pkg/notify/email.go"),
        "package notify\n\nfunc SendEmail(to string, body string, userID string) error {\n\treturn nil\n}\n",
    )
    .unwrap();
    live::handle(
        &shared,
        "audit",
        "PostToolUse",
        &json!({
            "session_id": "s-audit",
            "tool_name": "Edit",
            "tool_input": { "file_path": wt_audit.join("pkg/notify/email.go").to_string_lossy() }
        }),
    );
    assert_eq!(
        live::handle(&shared, "audit", "Stop", &json!({ "session_id": "s-audit" })).0,
        ""
    );

    // Reset agent tries to stop with its stale 2-arg call: Concord merges `audit` and blocks `reset`!
    let (out, _) = live::handle(&shared, "reset", "Stop", &json!({ "session_id": "s-reset" }));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["decision"], "block");
    let reason = v["reason"].as_str().unwrap();
    assert!(reason.contains("pkg.notify.SendEmail"), "{reason}");

    // Reset agent updates its call to pass 3 arguments, then finishes cleanly.
    fs::write(
        wt_reset.join("pkg/auth/reset.go"),
        "package auth\n\nimport \"github.com/acme/service/pkg/notify\"\n\nfunc RequestReset(addr string) error {\n\treturn notify.SendEmail(addr, \"tok\", \"u-1\")\n}\n",
    )
    .unwrap();
    assert_eq!(
        live::handle(&shared, "reset", "Stop", &json!({ "session_id": "s-reset" })).0,
        ""
    );
}
