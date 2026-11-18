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
