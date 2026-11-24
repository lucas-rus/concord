//! The running example of the thesis (see running_example.rs), written in
//! TypeScript: a contract change and a new caller in disjoint files.

use concord_cli::{analyze_branches, simulate};
use concord_core::{Finding, NoticeKind};
use concord_git::Repo;
use std::fs;
use std::path::Path;
use std::process::Command;

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

fn write(root: &Path, path: &str, text: &str) {
    let p = root.join(path);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

const EMAIL: &str = r#"import nodemailer from 'nodemailer';

export function formatSubject(subject: string): string {
  return subject.trim();
}

export function sendEmail(to: string, body: string): void {
  nodemailer.send('noreply@example.com', [to], body);
}
"#;

fn setup() -> (tempfile::TempDir, Repo) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    write(&root, "src/notify/email.ts", EMAIL);
    write(
        &root,
        "src/auth/tokens.ts",
        "export function issue(user: User): string {\n  return `${user.id}-${Math.random()}`;\n}\n",
    );
    write(
        &root,
        "src/app.ts",
        "import { sendEmail } from './notify/email';\n\nexport function welcome(user: User) {\n  sendEmail(user.email, 'Welcome!');\n}\n",
    );
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "base"]);

    // agent-a: rate-limit e-mails per user (contract change).
    git(&root, &["checkout", "-q", "-b", "agent-a", "main"]);
    write(
        &root,
        "src/notify/email.ts",
        &(EMAIL.replace(
            "export function sendEmail(to: string, body: string): void {\n",
            "export function sendEmail(to: string, body: string, userId: number): void {\n  if ((SENT.get(userId) ?? 0) >= LIMIT) throw new Error('rate limit');\n  SENT.set(userId, (SENT.get(userId) ?? 0) + 1);\n",
        ) + "\nconst LIMIT = 5;\nconst SENT = new Map<number, number>();\n"),
    );
    write(
        &root,
        "src/app.ts",
        "import { sendEmail } from './notify/email';\n\nexport function welcome(user: User) {\n  sendEmail(user.email, 'Welcome!', user.id);\n}\n",
    );
    git(&root, &["commit", "-q", "-am", "agent A"]);

    // agent-b: password reset (new code calling sendEmail the old way).
    git(&root, &["checkout", "-q", "-b", "agent-b", "main"]);
    write(
        &root,
        "src/auth/reset.ts",
        "import * as tokens from './tokens';\nimport { sendEmail } from '../notify/email';\n\nexport function resetMsg(token: string): string {\n  return `Reset your password: https://example.com/reset/${token}`;\n}\n\nexport function requestReset(user: User): void {\n  const token = tokens.issue(user);\n  sendEmail(user.email, resetMsg(token));\n}\n",
    );
    git(&root, &["add", "-A"]);
    git(&root, &["commit", "-q", "-m", "agent B"]);

    // agent-c: body-only change in agent-a's file.
    git(&root, &["checkout", "-q", "-b", "agent-c", "main"]);
    write(
        &root,
        "src/notify/email.ts",
        &EMAIL.replace(
            "return subject.trim();",
            "return subject.split(/\\s+/).join(' ').trim();",
        ),
    );
    git(&root, &["commit", "-q", "-am", "agent C"]);
    git(&root, &["checkout", "-q", "main"]);
    let repo = Repo::open(&root).unwrap();
    (tmp, repo)
}

#[test]
fn finds_the_cross_file_contract_conflict_in_typescript() {
    let (_tmp, repo) = setup();
    let branches = vec!["agent-a".to_string(), "agent-b".to_string(), "agent-c".to_string()];
    let a = analyze_branches(&repo, "main", &branches, true).unwrap();
    assert!(a.parse_warnings.is_empty(), "{:?}", a.parse_warnings);

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
            assert_eq!(symbol.as_str(), "src.notify.email.sendEmail");
            assert!(*file_disjoint);
            assert_eq!(
                after.as_ref().unwrap().display,
                "export function sendEmail(to: string, body: string, userId: number): void"
            );
            assert!(
                sites
                    .iter()
                    .any(|s| s.path == "src/auth/reset.ts" && s.text.starts_with("sendEmail(user.email"))
            );
        }
        _ => unreachable!(),
    }

    // agent-a and agent-c share a file but no symbol.
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
