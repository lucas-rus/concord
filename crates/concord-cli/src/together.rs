//! Fails-together check of the finish gate (docs/research/fails-together.md).
//!
//! When an agent finishes, its snapshot is merged in memory with every
//! finished peer's branch; the project's tests run on the agent's snapshot
//! ("alone") and on the merge ("together"), each in a temporary detached
//! worktree. A test that fails together but not alone is semantic
//! interference (SAM), and the agent is told before it may finish.

use concord_git::Repo;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Lines of test output quoted in a notice.
const TAIL_LINES: usize = 20;

#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// Nothing fails together (or there is no finished peer to merge with).
    Clean,
    /// Tests that fail on the merge but not alone.
    FailsTogether {
        tests: Vec<String>,
        peers: Vec<String>,
        tail: String,
    },
    /// The check could not run (e.g. textual conflict); the reason is logged.
    Skipped(String),
}

/// Failing test ids in pytest output (`FAILED a::b - msg`, `ERROR a::b`).
pub fn failing_tests(output: &str) -> BTreeSet<String> {
    output
        .lines()
        .filter_map(|l| l.strip_prefix("FAILED ").or_else(|| l.strip_prefix("ERROR ")))
        .map(|rest| rest.split(" - ").next().unwrap_or(rest).trim().to_owned())
        .filter(|t| !t.is_empty())
        .collect()
}

fn run_in(cmd: &str, dir: &Path) -> (bool, String) {
    match Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output() {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), text)
        }
        Err(e) => (false, e.to_string()),
    }
}

/// Run the tests at `rev` in a temporary detached worktree.
fn test_at(repo: &Repo, rev: &str, cmd: &str) -> Result<(bool, String), String> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir: PathBuf = std::env::temp_dir().join(format!(
        "concord-together-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let path = dir.to_string_lossy().into_owned();
    repo.git(&["worktree", "add", "--detach", "--quiet", &path, rev])
        .map_err(|e| e.to_string())?;
    let out = run_in(cmd, &dir);
    let _ = repo.git(&["worktree", "remove", "--force", &path]);
    let _ = std::fs::remove_dir_all(&dir);
    Ok(out)
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n")
}

/// The check for an agent whose snapshot is `me`, against finished peers
/// `(name, head)`.
pub fn check(repo: &Repo, test_cmd: &str, me: &str, peers: &[(String, String)]) -> Verdict {
    if peers.is_empty() {
        return Verdict::Clean;
    }
    let mut merged = me.to_owned();
    for (name, head) in peers {
        match repo.virtual_merge(&merged, head) {
            Ok(Some(m)) => merged = m,
            Ok(None) => return Verdict::Skipped(format!("textual conflict with {name}")),
            Err(e) => return Verdict::Skipped(e.to_string()),
        }
    }
    let together = match test_at(repo, &merged, test_cmd) {
        Ok(r) => r,
        Err(e) => return Verdict::Skipped(e),
    };
    if together.0 {
        return Verdict::Clean;
    }
    let alone = match test_at(repo, me, test_cmd) {
        Ok(r) => r,
        Err(e) => return Verdict::Skipped(e),
    };
    let names: Vec<String> = peers.iter().map(|(n, _)| n.clone()).collect();
    let (fails_t, fails_a) = (failing_tests(&together.1), failing_tests(&alone.1));
    let new: Vec<String> = fails_t.difference(&fails_a).cloned().collect();
    if !new.is_empty() {
        return Verdict::FailsTogether {
            tests: new,
            peers: names,
            tail: tail(&together.1),
        };
    }
    // Unknown output format: compare exit status only.
    if fails_t.is_empty() && alone.0 {
        return Verdict::FailsTogether {
            tests: vec!["(the test command fails on the merge and passes alone)".into()],
            peers: names,
            tail: tail(&together.1),
        };
    }
    Verdict::Clean
}

/// Notice for the agent, with each peer's diff stat against `base`.
pub fn render(repo: &Repo, base: &str, peers: &[(String, String)], v: &Verdict) -> String {
    let Verdict::FailsTogether { tests, tail, .. } = v else {
        return String::new();
    };
    let mut t = String::from(
        "[concord] fails together: your work passes its tests alone, but these tests fail once it is merged \
         with the work of the agents that have already finished:\n",
    );
    for name in tests.iter().take(15) {
        t.push_str(&format!("  {name}\n"));
    }
    if tests.len() > 15 {
        t.push_str(&format!("  ... and {} more\n", tests.len() - 15));
    }
    t.push_str("What the finished agents changed:\n");
    for (name, head) in peers {
        let stat = repo
            .git(&["diff", "--stat=72", "--stat-count=8", base, head])
            .unwrap_or_default();
        t.push_str(&format!("  `{name}`:\n"));
        for l in stat.lines() {
            t.push_str(&format!("    {}\n", l.trim_end()));
        }
    }
    t.push_str(&format!("Test output on the merge (tail):\n{tail}\n"));
    t.push_str(
        "Their work is final. Adapt your code so that it works together with theirs (you may read their \
         branches), run the tests, and finish again.",
    );
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pytest_failures() {
        let out =
            "....F\nFAILED tests/test_a.py::test_x - AssertionError: 1 != 2\nERROR tests/test_b.py::test_y\n1 failed";
        let f = failing_tests(out);
        assert!(f.contains("tests/test_a.py::test_x"));
        assert!(f.contains("tests/test_b.py::test_y"));
        assert_eq!(f.len(), 2);
    }
}
