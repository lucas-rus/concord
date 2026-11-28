//! Fails-together check of the finish gate (docs/research/fails-together.md).
//!
//! When an agent finishes, its snapshot is merged in memory (including
//! verified insertion-union merges) with every finished peer's branch; the
//! project's tests run on the agent's snapshot ("alone") and on the merge
//! ("together"), each in a temporary detached worktree. A test that fails
//! together but not alone is semantic interference (SAM). When more than one
//! peer has finished, Concord bisects the peer set in O(log k) runs to
//! attribute the failure to the exact conflicting peer (`Attribute(t, G)`).

use concord_git::Repo;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
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
        /// Bisected minimal culprit peer(s) whose merge triggers the failure.
        culprits: Vec<String>,
        tail: String,
    },
    /// The check could not run (e.g. textual conflict); the reason is logged.
    Skipped(String),
}

/// Failing test identifiers across pytest, cargo test, go test, and jest/vitest.
pub fn failing_tests(output: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in output.lines() {
        let trimmed = line.trim();
        // pytest: `FAILED a::b - msg` or `ERROR a::b`
        if let Some(rest) = trimmed
            .strip_prefix("FAILED ")
            .or_else(|| trimmed.strip_prefix("ERROR "))
        {
            let id = rest.split(" - ").next().unwrap_or(rest).trim();
            if !id.is_empty() {
                out.insert(id.to_owned());
            }
            continue;
        }
        // cargo test: `test module::case ... FAILED`
        if let Some(rest) = trimmed.strip_prefix("test ")
            && let Some(id) = rest.strip_suffix(" ... FAILED")
        {
            let id = id.trim();
            if !id.is_empty() {
                out.insert(id.to_owned());
            }
            continue;
        }
        // go test: `--- FAIL: TestName (0.00s)`
        if let Some(rest) = trimmed.strip_prefix("--- FAIL: ") {
            let id = rest.split_whitespace().next().unwrap_or("").trim();
            if !id.is_empty() {
                out.insert(id.to_owned());
            }
            continue;
        }
        // jest / vitest: `FAIL src/foo.test.ts`
        if let Some(rest) = trimmed.strip_prefix("FAIL ")
            && (rest.contains(".test.") || rest.contains(".spec."))
        {
            let id = rest.split_whitespace().next().unwrap_or("").trim();
            if !id.is_empty() {
                out.insert(id.to_owned());
            }
            continue;
        }
        // node --test (spec reporter): `✖ test name (1.23ms)`
        if let Some(rest) = trimmed.strip_prefix("✖ ").or_else(|| trimmed.strip_prefix("× "))
            && !rest.starts_with("failing tests:")
        {
            let id = rest.rsplit_once(" (").map(|(n, _)| n).unwrap_or(rest).trim();
            if !id.is_empty() {
                out.insert(id.to_owned());
            }
            continue;
        }
        // node --test (TAP): `not ok 3 - test name`
        if let Some(rest) = trimmed.strip_prefix("not ok ") {
            let id = rest.split_once(" - ").map(|(_, n)| n).unwrap_or(rest).trim();
            if !id.is_empty() {
                out.insert(id.to_owned());
            }
            continue;
        }
        // TypeScript `tsc --noEmit`: `lib/foo.ts(12,5): error TS2345: ...` or `lib/foo.ts:12:5 - error TS2345: ...`
        if let Some((loc, err_part)) = trimmed.split_once("error TS") {
            let code: String = err_part.chars().take_while(|c| c.is_ascii_digit()).collect();
            let loc = loc.trim().trim_end_matches([':', '-']).trim();
            if !loc.is_empty() && !code.is_empty() {
                out.insert(format!("{loc}:TS{code}"));
            }
        }
    }
    out
}

/// Substitute `{affected_tests}` in `test_cmd` if present; if `affected` is
/// empty when `{affected_tests}` is requested, returns `None` (no affected tests).
pub fn resolve_test_cmd(test_cmd: &str, affected: Option<&BTreeSet<String>>) -> Option<String> {
    if !test_cmd.contains("{affected_tests}") {
        return Some(test_cmd.to_owned());
    }
    let files = affected?;
    if files.is_empty() {
        return None;
    }
    let joined: Vec<String> = files.iter().map(|f| format!("'{f}'")).collect();
    Some(test_cmd.replace("{affected_tests}", &joined.join(" ")))
}

fn run_in(cmd: &str, dir: &Path) -> (bool, String) {
    match crate::shell::run_shell_timeout(cmd, dir, crate::shell::check_timeout()) {
        Ok(o) => (o.ok, o.output),
        Err(e) => (false, e.to_string()),
    }
}

/// Run the tests at `rev` in a temporary detached worktree.
pub(crate) fn test_at(repo: &Repo, rev: &str, cmd: &str) -> Result<(bool, String), String> {
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

fn merge_subset(repo: &Repo, me: &str, peers: &[(String, String)]) -> Result<String, String> {
    let mut merged = me.to_owned();
    for (name, head) in peers {
        match repo.virtual_merge_union(&merged, head, &crate::regions::verify_union) {
            Ok(Some(m)) => merged = m,
            Ok(None) => return Err(format!("textual conflict with {name}")),
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(merged)
}

fn subset_fails(
    repo: &Repo,
    test_cmd: &str,
    me: &str,
    alone_ok: bool,
    fails_a: &BTreeSet<String>,
    subset: &[(String, String)],
) -> bool {
    let Ok(merged) = merge_subset(repo, me, subset) else {
        return false;
    };
    let Ok((ok, out)) = test_at(repo, &merged, test_cmd) else {
        return false;
    };
    if ok {
        return false;
    }
    let fails_t = failing_tests(&out);
    if !fails_t.is_empty() {
        fails_t.difference(fails_a).next().is_some()
    } else {
        alone_ok
    }
}

/// Binary bisection (`Attribute(t, G)`) to isolate a culprit peer in O(log k)
/// test runs when multiple peers are merged together.
fn bisect_culprits(
    repo: &Repo,
    test_cmd: &str,
    me: &str,
    alone_ok: bool,
    fails_a: &BTreeSet<String>,
    peers: &[(String, String)],
) -> Vec<String> {
    if peers.len() <= 1 {
        return peers.iter().map(|(n, _)| n.clone()).collect();
    }
    let mut cur: Vec<(String, String)> = peers.to_vec();
    while cur.len() > 1 {
        let mid = cur.len() / 2;
        let (left, right) = (&cur[..mid], &cur[mid..]);
        if subset_fails(repo, test_cmd, me, alone_ok, fails_a, left) {
            cur = left.to_vec();
        } else if subset_fails(repo, test_cmd, me, alone_ok, fails_a, right) {
            cur = right.to_vec();
        } else {
            // Multi-way interaction across both halves: keep the full minimal span.
            break;
        }
    }
    cur.into_iter().map(|(n, _)| n).collect()
}

/// The check for an agent whose snapshot is `me`, against finished peers
/// `(name, head)`.
pub fn check(repo: &Repo, test_cmd: &str, me: &str, peers: &[(String, String)]) -> Verdict {
    check_with_affected(repo, test_cmd, me, peers, None)
}

/// Like [`check`], optionally narrowing `{affected_tests}` in `test_cmd` to
/// symbol-graph affected test files.
pub fn check_with_affected(
    repo: &Repo,
    test_cmd: &str,
    me: &str,
    peers: &[(String, String)],
    affected: Option<&BTreeSet<String>>,
) -> Verdict {
    if peers.is_empty() {
        return Verdict::Clean;
    }
    let Some(effective_cmd) = resolve_test_cmd(test_cmd, affected) else {
        return Verdict::Clean;
    };
    let merged = match merge_subset(repo, me, peers) {
        Ok(m) => m,
        Err(why) => return Verdict::Skipped(why),
    };
    let together = match test_at(repo, &merged, &effective_cmd) {
        Ok(r) => r,
        Err(e) => return Verdict::Skipped(e),
    };
    if together.0 {
        return Verdict::Clean;
    }
    let alone = match test_at(repo, me, &effective_cmd) {
        Ok(r) => r,
        Err(e) => return Verdict::Skipped(e),
    };
    let names: Vec<String> = peers.iter().map(|(n, _)| n.clone()).collect();
    let (fails_t, fails_a) = (failing_tests(&together.1), failing_tests(&alone.1));
    let new: Vec<String> = fails_t.difference(&fails_a).cloned().collect();
    if !new.is_empty() {
        let culprits = bisect_culprits(repo, &effective_cmd, me, alone.0, &fails_a, peers);
        return Verdict::FailsTogether {
            tests: new,
            peers: names,
            culprits,
            tail: tail(&together.1),
        };
    }
    // Unknown output format: compare exit status only.
    if fails_t.is_empty() && alone.0 {
        let culprits = bisect_culprits(repo, &effective_cmd, me, alone.0, &fails_a, peers);
        return Verdict::FailsTogether {
            tests: vec!["(the test command fails on the merge and passes alone)".into()],
            peers: names,
            culprits,
            tail: tail(&together.1),
        };
    }
    Verdict::Clean
}

/// Notice for the agent, highlighting bisected culprit peer(s) and their diff stat.
pub fn render(repo: &Repo, base: &str, peers: &[(String, String)], v: &Verdict) -> String {
    let Verdict::FailsTogether {
        tests, culprits, tail, ..
    } = v
    else {
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
    if !culprits.is_empty() && culprits.len() < peers.len() {
        t.push_str(&format!(
            "Bisected conflicting peer(s): {}\n",
            culprits.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(", ")
        ));
    }
    let focus: Vec<&(String, String)> = if !culprits.is_empty() {
        peers.iter().filter(|(n, _)| culprits.contains(n)).collect()
    } else {
        peers.iter().collect()
    };
    t.push_str("What the conflicting finished agent(s) changed:\n");
    for (name, head) in focus {
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
    fn parses_pytest_cargo_go_and_jest_failures() {
        let out = concat!(
            "....F\n",
            "FAILED tests/test_a.py::test_x - AssertionError: 1 != 2\n",
            "ERROR tests/test_b.py::test_y\n",
            "test auth::tests::rejects_bad_token ... FAILED\n",
            "--- FAIL: TestSendEmail (0.01s)\n",
            "FAIL src/auth.spec.ts\n",
        );
        let f = failing_tests(out);
        assert!(f.contains("tests/test_a.py::test_x"));
        assert!(f.contains("tests/test_b.py::test_y"));
        assert!(f.contains("auth::tests::rejects_bad_token"));
        assert!(f.contains("TestSendEmail"));
        assert!(f.contains("src/auth.spec.ts"));
        assert_eq!(f.len(), 5);
    }

    #[test]
    fn resolves_affected_test_placeholders() {
        let mut set = BTreeSet::new();
        assert_eq!(resolve_test_cmd("pytest {affected_tests}", Some(&set)), None);
        set.insert("tests/test_a.py".into());
        assert_eq!(
            resolve_test_cmd("pytest {affected_tests}", Some(&set)).as_deref(),
            Some("pytest 'tests/test_a.py'")
        );
    }
}
