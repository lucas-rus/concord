//! # concord-git
//!
//! Thin, well-tested wrappers around the `git` CLI (>= 2.38).
//!
//! The CLI is used deliberately for the first milestone: it is the reference
//! implementation (`git merge-tree --write-tree` is what CooperBench-style
//! evaluations effectively do) and it handles dirty worktrees through a
//! temporary index. Moving hot paths to `gix` (gitoxide, which now supports
//! in-memory tree merges) is planned once profiling shows a need.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("failed to run git: {0}")]
    Io(#[from] std::io::Error),
    #[error("git {args} failed: {stderr}")]
    Command { args: String, stderr: String },
    #[error("unexpected git output: {0}")]
    Parse(String),
}

pub type Result<T> = std::result::Result<T, GitError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub status: FileStatus,
    pub path: String,
}

/// Result of an in-memory three-way merge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeOutcome {
    pub clean: bool,
    /// Tree object id of the merge result (with conflict markers if not clean).
    pub tree: String,
    pub conflicted_files: Vec<String>,
}

/// One change of a file between two revisions, in the coordinates of the
/// old (base) side: lines `old_start .. old_start + old_count` were replaced
/// by `new_lines`. `old_count == 0` is an insertion after line `old_start`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_lines: Vec<String>,
}

impl Hunk {
    /// The part of the base this hunk occupies, as a closed interval of
    /// "slots": line `n` is slot `2n`, the gap after line `n` is slot `2n + 1`.
    /// An insertion occupies only its gap; a modification occupies its lines
    /// and the gaps on both sides (git also conflicts on adjacent changes).
    fn slots(&self) -> (u32, u32) {
        if self.old_count == 0 {
            (2 * self.old_start + 1, 2 * self.old_start + 1)
        } else {
            (2 * self.old_start - 1, 2 * (self.old_start + self.old_count - 1) + 1)
        }
    }

    /// Whether git's three-way merge would see `self` and `other` (both
    /// against the same base) as one conflicting region: they overlap or touch
    /// without an unchanged base line between them, or insert at the same point.
    pub fn collides(&self, other: &Hunk) -> bool {
        let (a0, a1) = self.slots();
        let (b0, b1) = other.slots();
        a0 <= b1 && b0 <= a1
    }
}

/// Parse `git diff -U0` output for one file.
pub fn parse_hunks(diff: &str) -> Vec<Hunk> {
    let mut hunks: Vec<Hunk> = Vec::new();
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("@@ -") {
            let old = rest.split_whitespace().next().unwrap_or("0");
            let mut it = old.split(',');
            let start: u32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            let count: u32 = it.next().and_then(|x| x.parse().ok()).unwrap_or(1);
            hunks.push(Hunk {
                old_start: start,
                old_count: count,
                new_lines: Vec::new(),
            });
        } else if let Some(added) = line.strip_prefix('+')
            && !line.starts_with("+++")
            && let Some(h) = hunks.last_mut()
        {
            h.new_lines.push(added.to_owned());
        }
    }
    hunks
}

#[derive(Clone, Debug)]
pub struct Repo {
    root: PathBuf,
}

impl Repo {
    /// Open the repository containing `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let out = run(path.as_ref(), &["rev-parse", "--show-toplevel"], None)?;
        Ok(Repo {
            root: PathBuf::from(out.trim()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn git(&self, args: &[&str]) -> Result<String> {
        run(&self.root, args, None)
    }

    pub fn rev_parse(&self, rev: &str) -> Result<String> {
        Ok(self
            .git(&["rev-parse", "--verify", &format!("{rev}^{{commit}}")])?
            .trim()
            .to_owned())
    }

    pub fn merge_base(&self, a: &str, b: &str) -> Result<String> {
        Ok(self.git(&["merge-base", a, b])?.trim().to_owned())
    }

    /// Files changed between two revisions (no rename detection: a rename is a
    /// delete plus an add, which is what symbol-level analysis wants).
    pub fn changed_files(&self, base: &str, rev: &str) -> Result<Vec<ChangedFile>> {
        let out = self.git(&["diff", "--name-status", "--no-renames", "-z", base, rev])?;
        parse_name_status_z(&out)
    }

    /// All file paths in a revision.
    pub fn list_files(&self, rev: &str) -> Result<Vec<String>> {
        let out = self.git(&["ls-tree", "-r", "-z", "--name-only", rev])?;
        Ok(out.split('\0').filter(|s| !s.is_empty()).map(str::to_owned).collect())
    }

    /// Contents of the given paths at `rev` (missing paths are omitted), read in
    /// one `git cat-file --batch` process.
    pub fn read_files(&self, rev: &str, paths: &[String]) -> Result<BTreeMap<String, String>> {
        let mut child = Command::new("git")
            .current_dir(&self.root)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("stdin");
        let requests: Vec<String> = paths.iter().map(|p| format!("{rev}:{p}\n")).collect();
        let writer = std::thread::spawn(move || -> std::io::Result<()> {
            for r in requests {
                stdin.write_all(r.as_bytes())?;
            }
            Ok(())
        });
        let mut reader = BufReader::new(child.stdout.take().expect("stdout"));
        let mut out = BTreeMap::new();
        for p in paths {
            let mut header = String::new();
            reader.read_line(&mut header)?;
            let header = header.trim_end();
            if header.ends_with(" missing") || header.is_empty() {
                continue;
            }
            let size: usize = header
                .rsplit(' ')
                .next()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| GitError::Parse(header.to_owned()))?;
            let mut buf = vec![0u8; size];
            reader.read_exact(&mut buf)?;
            let mut nl = [0u8; 1];
            reader.read_exact(&mut nl)?;
            if header.contains(" blob ") {
                out.insert(p.clone(), String::from_utf8_lossy(&buf).into_owned());
            }
        }
        writer.join().expect("writer thread")?;
        child.wait()?;
        Ok(out)
    }

    /// In-memory merge of two commits (`git merge-tree --write-tree`); never
    /// touches any worktree.
    pub fn merge_tree(&self, a: &str, b: &str) -> Result<MergeOutcome> {
        let out = Command::new("git")
            .current_dir(&self.root)
            .args(["merge-tree", "--write-tree", "--name-only", "--no-messages", "-z", a, b])
            .output()?;
        let code = out.status.code().unwrap_or(-1);
        if code != 0 && code != 1 {
            return Err(GitError::Command {
                args: format!("merge-tree {a} {b}"),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut parts = stdout.split('\0').filter(|s| !s.is_empty());
        let tree = parts
            .next()
            .ok_or_else(|| GitError::Parse("empty merge-tree output".into()))?
            .trim()
            .to_owned();
        let mut conflicted: Vec<String> = parts.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect();
        conflicted.dedup();
        Ok(MergeOutcome {
            clean: code == 0,
            tree,
            conflicted_files: conflicted,
        })
    }

    /// Snapshot a (possibly dirty) worktree as a commit whose parent is the
    /// worktree's HEAD, without touching its index or files. Untracked files
    /// that are not ignored are included.
    pub fn snapshot_worktree(worktree: impl AsRef<Path>) -> Result<String> {
        let wt = worktree.as_ref();
        let tmp_index = std::env::temp_dir().join(format!(
            "concord-index-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let env = [("GIT_INDEX_FILE", tmp_index.to_string_lossy().into_owned())];
        let result = (|| {
            run_env(wt, &["read-tree", "HEAD"], &env)?;
            run_env(wt, &["add", "-A", "."], &env)?;
            let tree = run_env(wt, &["write-tree"], &env)?.trim().to_owned();
            let head = run(wt, &["rev-parse", "HEAD"], None)?.trim().to_owned();
            let commit = run_env(
                wt,
                &[
                    "-c",
                    "user.name=concord",
                    "-c",
                    "user.email=concord@localhost",
                    "commit-tree",
                    &tree,
                    "-p",
                    &head,
                    "-m",
                    "concord snapshot",
                ],
                &env,
            )?;
            Ok(commit.trim().to_owned())
        })();
        let _ = std::fs::remove_file(&tmp_index);
        result
    }
}

/// Outcome of merging a revision into a worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeIntoOutcome {
    pub clean: bool,
    pub conflicted_files: Vec<String>,
}

const IDENTITY: [&str; 4] = ["-c", "user.name=concord", "-c", "user.email=concord@localhost"];

impl Repo {
    /// `git worktree add -b <branch> <path> <start>`.
    pub fn worktree_add(&self, path: &Path, branch: &str, start: &str) -> Result<()> {
        let p = path.to_string_lossy();
        self.git(&["worktree", "add", "-q", "-b", branch, &p, start])?;
        Ok(())
    }

    /// Commit everything in a worktree (including untracked files); returns HEAD.
    pub fn commit_all(worktree: &Path, message: &str) -> Result<String> {
        run(worktree, &["add", "-A", "."], None)?;
        let mut args: Vec<&str> = IDENTITY.to_vec();
        args.extend(["commit", "-q", "--allow-empty", "--no-verify", "-m", message]);
        run(worktree, &args, None)?;
        Ok(run(worktree, &["rev-parse", "HEAD"], None)?.trim().to_owned())
    }

    /// Merge `rev` into the worktree's current branch. On conflict the merge is
    /// aborted and the conflicted files are reported.
    pub fn merge_into(worktree: &Path, rev: &str, message: &str) -> Result<MergeIntoOutcome> {
        let mut args: Vec<&str> = IDENTITY.to_vec();
        args.extend(["merge", "--no-ff", "--no-edit", "-m", message, rev]);
        match run(worktree, &args, None) {
            Ok(_) => Ok(MergeIntoOutcome {
                clean: true,
                conflicted_files: vec![],
            }),
            Err(GitError::Command { .. }) => {
                let files = run(worktree, &["diff", "--name-only", "--diff-filter=U"], None).unwrap_or_default();
                let _ = run(worktree, &["merge", "--abort"], None);
                Ok(MergeIntoOutcome {
                    clean: false,
                    conflicted_files: files.lines().map(str::to_owned).collect(),
                })
            }
            Err(e) => Err(e),
        }
    }

    /// Merge `rev` into the worktree, leaving conflict markers in place for the
    /// agent to resolve (no abort). Returns the conflicted files.
    pub fn merge_keep_conflicts(worktree: &Path, rev: &str, message: &str) -> Result<MergeIntoOutcome> {
        let mut args: Vec<&str> = IDENTITY.to_vec();
        args.extend(["merge", "--no-ff", "--no-edit", "-m", message, rev]);
        match run(worktree, &args, None) {
            Ok(_) => Ok(MergeIntoOutcome {
                clean: true,
                conflicted_files: vec![],
            }),
            Err(GitError::Command { .. }) => {
                let files = run(worktree, &["diff", "--name-only", "--diff-filter=U"], None)?;
                Ok(MergeIntoOutcome {
                    clean: false,
                    conflicted_files: files.lines().map(str::to_owned).collect(),
                })
            }
            Err(e) => Err(e),
        }
    }

    /// The hunks of `path` between `base` and `rev` (`git diff -U0`).
    pub fn hunks(&self, base: &str, rev: &str, path: &str) -> Result<Vec<Hunk>> {
        let diff = self.git(&["diff", "-U0", "--no-color", "--no-ext-diff", base, rev, "--", path])?;
        Ok(parse_hunks(&diff))
    }

    /// Register a structured merge driver (e.g. Mergiraf) for the given file
    /// patterns, repository-wide (`.git/config` and `.git/info/attributes`,
    /// so it never shows up in agents' diffs). `git merge`, `git merge-tree`
    /// and every linked worktree then use it.
    pub fn set_merge_driver(&self, name: &str, command: &str, patterns: &[&str]) -> Result<()> {
        self.git(&[
            "config",
            &format!("merge.{name}.name"),
            &format!("{name} structured merge"),
        ])?;
        self.git(&["config", &format!("merge.{name}.driver"), command])?;
        let common = self.git(&["rev-parse", "--git-common-dir"])?;
        let common = self.root.join(common.trim());
        let info = common.join("info");
        std::fs::create_dir_all(&info)?;
        let path = info.join("attributes");
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        for p in patterns {
            let line = format!("{p} merge={name}");
            if !text.lines().any(|l| l == line) {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&line);
                text.push('\n');
            }
        }
        std::fs::write(&path, text)?;
        Ok(())
    }

    /// A commit whose tree is the clean merge of `a` and `b` (parents a, b),
    /// without touching any worktree. `None` if the merge conflicts.
    pub fn virtual_merge(&self, a: &str, b: &str) -> Result<Option<String>> {
        let m = self.merge_tree(a, b)?;
        if !m.clean {
            return Ok(None);
        }
        let mut args: Vec<&str> = IDENTITY.to_vec();
        args.extend(["commit-tree", &m.tree, "-p", a, "-p", b, "-m", "concord virtual base"]);
        Ok(Some(self.git(&args)?.trim().to_owned()))
    }
}

fn run(dir: &Path, args: &[&str], stdin: Option<&str>) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    if let Some(input) = stdin {
        child.stdin.take().expect("stdin").write_all(input.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(GitError::Command {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run_env(dir: &Path, args: &[&str], env: &[(&str, String)]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(GitError::Command {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_name_status_z(out: &str) -> Result<Vec<ChangedFile>> {
    let mut items = out.split('\0').filter(|s| !s.is_empty());
    let mut files = Vec::new();
    while let Some(status) = items.next() {
        let path = items
            .next()
            .ok_or_else(|| GitError::Parse(format!("missing path after status {status}")))?;
        let status = match status.chars().next() {
            Some('A') => FileStatus::Added,
            Some('D') => FileStatus::Deleted,
            _ => FileStatus::Modified,
        };
        files.push(ChangedFile {
            status,
            path: path.to_owned(),
        });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sh(dir: &Path, args: &[&str]) -> String {
        run(dir, args, None).unwrap()
    }

    fn init_repo() -> (tempfile::TempDir, Repo) {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        sh(d, &["init", "-q", "-b", "main"]);
        sh(d, &["config", "user.email", "t@example.com"]);
        sh(d, &["config", "user.name", "t"]);
        fs::write(d.join("a.py"), "def f():\n    return 1\n").unwrap();
        fs::write(d.join("b.py"), "x = 1\n").unwrap();
        sh(d, &["add", "."]);
        sh(d, &["commit", "-q", "-m", "base"]);
        let repo = Repo::open(d).unwrap();
        (tmp, repo)
    }

    #[test]
    fn changed_files_read_files_and_merge_tree() {
        let (tmp, repo) = init_repo();
        let d = tmp.path();
        sh(d, &["checkout", "-q", "-b", "one"]);
        fs::write(d.join("a.py"), "def f():\n    return 2\n").unwrap();
        fs::write(d.join("c.py"), "y = 2\n").unwrap();
        sh(d, &["add", "."]);
        sh(d, &["commit", "-q", "-m", "one"]);
        sh(d, &["checkout", "-q", "main"]);
        sh(d, &["checkout", "-q", "-b", "two"]);
        fs::write(d.join("a.py"), "def f():\n    return 3\n").unwrap();
        sh(d, &["commit", "-q", "-am", "two"]);

        let changed = repo.changed_files("main", "one").unwrap();
        assert_eq!(changed.len(), 2);
        assert!(
            changed
                .iter()
                .any(|c| c.path == "c.py" && c.status == FileStatus::Added)
        );

        let files = repo
            .read_files("one", &["a.py".into(), "nope.py".into(), "c.py".into()])
            .unwrap();
        assert_eq!(files["a.py"], "def f():\n    return 2\n");
        assert!(!files.contains_key("nope.py"));

        let m = repo.merge_tree("one", "two").unwrap();
        assert!(!m.clean);
        assert_eq!(m.conflicted_files, vec!["a.py".to_string()]);

        let m = repo.merge_tree("one", "main").unwrap();
        assert!(m.clean);
    }

    #[test]
    fn merge_keep_conflicts_leaves_markers() {
        let (tmp, _repo) = init_repo();
        let d = tmp.path();
        sh(d, &["checkout", "-q", "-b", "other"]);
        fs::write(d.join("a.py"), "def f():\n    return 'other'\n").unwrap();
        sh(d, &["commit", "-q", "-am", "other"]);
        sh(d, &["checkout", "-q", "main"]);
        fs::write(d.join("a.py"), "def f():\n    return 'mine'\n").unwrap();
        sh(d, &["commit", "-q", "-am", "mine"]);
        let out = Repo::merge_keep_conflicts(d, "other", "merge").unwrap();
        assert!(!out.clean);
        assert_eq!(out.conflicted_files, vec!["a.py".to_string()]);
        assert!(fs::read_to_string(d.join("a.py")).unwrap().contains("<<<<<<<"));
        // Resolving and committing concludes the merge.
        fs::write(d.join("a.py"), "def f():\n    return 'both'\n").unwrap();
        Repo::commit_all(d, "resolved").unwrap();
        assert_eq!(
            sh(d, &["rev-list", "--parents", "-n", "1", "HEAD"])
                .split_whitespace()
                .count(),
            3
        );
    }

    #[test]
    fn hunk_collisions_follow_git() {
        // Base "a b c"; theirs inserts after b. Checked against git merge-tree.
        let (tmp, repo) = init_repo();
        let d = tmp.path();
        fs::write(d.join("f.txt"), "a\nb\nc\n").unwrap();
        sh(d, &["add", "."]);
        sh(d, &["commit", "-q", "-m", "f"]);
        let base = sh(d, &["rev-parse", "HEAD"]).trim().to_owned();
        let branch = |name: &str, text: &str| {
            sh(d, &["checkout", "-q", "-B", name, &base]);
            fs::write(d.join("f.txt"), text).unwrap();
            sh(d, &["commit", "-q", "-am", name]);
        };
        branch("theirs", "a\nb\nT\nc\n");
        let cases = [
            ("same_point", "a\nb\nM\nc\n", true),
            ("superset", "a\nb\nT\nM\nc\n", true),
            ("modify_next_line", "a\nb\nC\n", true),
            ("modify_prev_line", "a\nB\nc\n", true),
            ("one_line_away", "a\nb\nc\nM\n", false),
            ("elsewhere", "M\na\nb\nc\n", false),
        ];
        // Adjacent modifications conflict; one unchanged line between does not.
        branch("theirs_mod", "A\nb\nc\n");
        let tm = repo.hunks(&base, "theirs_mod", "f.txt").unwrap();
        for (name, text, expect) in [
            ("mod_adjacent", "a\nB\nc\n", true),
            ("insert_after_modified", "a\nX\nb\nc\n", true),
            ("mod_one_line_away", "a\nb\nC\n", false),
        ] {
            branch(name, text);
            let o = repo.hunks(&base, name, "f.txt").unwrap();
            assert_eq!(
                !repo.merge_tree("theirs_mod", name).unwrap().clean,
                expect,
                "git, case {name}"
            );
            assert_eq!(
                o.iter().any(|h| tm.iter().any(|x| x.collides(h))),
                expect,
                "prediction, case {name}"
            );
        }
        let t = repo.hunks(&base, "theirs", "f.txt").unwrap();
        assert_eq!(
            t,
            vec![Hunk {
                old_start: 2,
                old_count: 0,
                new_lines: vec!["T".into()]
            }]
        );
        for (name, text, expect) in cases {
            branch(name, text);
            let o = repo.hunks(&base, name, "f.txt").unwrap();
            let predicted = o.iter().any(|h| t.iter().any(|x| x.collides(h)));
            let actual = !repo.merge_tree("theirs", name).unwrap().clean;
            assert_eq!(actual, expect, "git, case {name}");
            assert_eq!(predicted, expect, "prediction, case {name}");
        }
    }

    #[test]
    fn merge_driver_is_used_by_merge_tree() {
        let (tmp, repo) = init_repo();
        let d = tmp.path();
        sh(d, &["checkout", "-q", "-b", "one"]);
        fs::write(d.join("b.py"), "x = 'one'\n").unwrap();
        sh(d, &["commit", "-q", "-am", "one"]);
        sh(d, &["checkout", "-q", "main"]);
        fs::write(d.join("b.py"), "x = 'two'\n").unwrap();
        sh(d, &["commit", "-q", "-am", "two"]);
        assert!(!repo.merge_tree("one", "main").unwrap().clean);
        // A "driver" that always succeeds by keeping our side.
        repo.set_merge_driver("keepours", "true", &["*.py"]).unwrap();
        repo.set_merge_driver("keepours", "true", &["*.py"]).unwrap(); // idempotent
        assert!(repo.merge_tree("one", "main").unwrap().clean);
        let attrs = fs::read_to_string(d.join(".git/info/attributes")).unwrap();
        assert_eq!(attrs.matches("*.py merge=keepours").count(), 1);
    }

    #[test]
    fn snapshot_of_dirty_worktree_includes_untracked_files() {
        let (tmp, repo) = init_repo();
        let d = tmp.path();
        fs::write(d.join("a.py"), "def f():\n    return 42\n").unwrap();
        fs::write(d.join("new.py"), "z = 3\n").unwrap();
        let snap = Repo::snapshot_worktree(d).unwrap();
        let changed = repo.changed_files("HEAD", &snap).unwrap();
        let paths: Vec<&str> = changed.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, vec!["a.py", "new.py"]);
        // The real index is untouched.
        assert_eq!(sh(d, &["diff", "--cached", "--name-only"]).trim(), "");
    }
}
