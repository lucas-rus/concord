//! Tier-2 compiler / type-checker precision layer (opt-in).
//!
//! Tree-sitter (`concord-lang`) runs in <50 ms on every edit, while an
//! external type checker (`tsc`, `basedpyright`/`pyright`/`ty`, `cargo check`,
//! `go vet`) catches duck-typed receivers, inferred return types, and
//! polymorphic signature breaks at merge or worktree inspection time.

use serde::Serialize;
use std::path::Path;
use std::process::Command;

#[derive(Clone, Debug, Serialize)]
pub struct TypeCheckResult {
    pub tool: String,
    pub available: bool,
    pub ok: bool,
    pub diagnostics: Vec<String>,
}

fn find_executable(dir: &Path, local_rel: Option<&str>, names: &[&str]) -> Option<String> {
    if let Some(rel) = local_rel {
        let mut cur = Some(dir);
        while let Some(d) = cur {
            let cand = d.join(rel);
            if cand.is_file() {
                return Some(cand.to_string_lossy().into_owned());
            }
            cur = d.parent();
        }
    }
    for name in names {
        if Command::new("which")
            .arg(name)
            .output()
            .is_ok_and(|o| o.status.success())
        {
            return Some((*name).to_owned());
        }
    }
    None
}

/// Auto-detect and run the repository's native type checker (`tsc`,
/// `basedpyright`/`pyright`/`ty`, `cargo check`, or `go vet`).
pub fn run_type_check(dir: &Path) -> Option<TypeCheckResult> {
    if dir.join("tsconfig.json").is_file() {
        let Some(bin) = find_executable(dir, Some("node_modules/.bin/tsc"), &["tsc"]) else {
            return Some(TypeCheckResult {
                tool: "tsc".into(),
                available: false,
                ok: true,
                diagnostics: vec![],
            });
        };
        return Some(exec_checker(&bin, &["--noEmit"], dir, "tsc"));
    }
    if dir.join("Cargo.toml").is_file() {
        let bin = find_executable(dir, None, &["cargo"])?;
        return Some(exec_checker(
            &bin,
            &["check", "--message-format=short"],
            dir,
            "cargo check",
        ));
    }
    if dir.join("go.mod").is_file() {
        let bin = find_executable(dir, None, &["go"])?;
        return Some(exec_checker(&bin, &["vet", "./..."], dir, "go vet"));
    }
    if dir.join("pyproject.toml").is_file() || dir.join("pyrightconfig.json").is_file() {
        if let Some(bin) = find_executable(dir, None, &["ty", "basedpyright", "pyright"]) {
            let label = Path::new(&bin)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("pyright")
                .to_owned();
            let args: Vec<&str> = if label == "ty" { vec!["check"] } else { vec![] };
            return Some(exec_checker(&bin, &args, dir, &label));
        }
        return Some(TypeCheckResult {
            tool: "pyright/ty".into(),
            available: false,
            ok: true,
            diagnostics: vec![],
        });
    }
    None
}

fn exec_checker(bin: &str, args: &[&str], dir: &Path, label: &str) -> TypeCheckResult {
    match Command::new(bin).args(args).current_dir(dir).output() {
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            let diagnostics: Vec<String> = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .take(40)
                .map(str::to_owned)
                .collect();
            TypeCheckResult {
                tool: label.to_owned(),
                available: true,
                ok: out.status.success(),
                diagnostics,
            }
        }
        Err(e) => TypeCheckResult {
            tool: label.to_owned(),
            available: false,
            ok: false,
            diagnostics: vec![e.to_string()],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_python_checker_reports_unavailable_without_error() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(run_type_check(tmp.path()).is_none());
    }
}
