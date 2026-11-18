use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use concord_cli::{analyze_branches, report, simulate};
use concord_core::diff_symbols;
use concord_git::Repo;
use concord_lang::python;
use std::path::PathBuf;

/// Concord: contract-level coherence for parallel AI coding agents.
#[derive(Parser)]
#[command(name = "concord", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List the symbols of Python files with their contracts and digests.
    Symbols {
        files: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Classify symbol changes between two versions of a Python file.
    Diff {
        before: PathBuf,
        after: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Analyse branches developed concurrently from a common base: contract
    /// dependencies, file and symbol overlaps, textual merges, and the notices
    /// the coherence protocol would send.
    Analyze {
        /// Common base revision of the branches.
        #[arg(long)]
        base: String,
        /// Branches (or any revisions) to analyse.
        #[arg(required = true)]
        branches: Vec<String>,
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Skip pairwise `git merge-tree` checks.
        #[arg(long)]
        no_textual: bool,
        /// Also replay the branches through the coherence protocol.
        #[arg(long)]
        notices: bool,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Symbols { files, json } => {
            let mut all = Vec::new();
            for f in &files {
                let src = std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
                let idx = python::index_file(&f.to_string_lossy(), &src)?;
                if json {
                    all.push(idx);
                } else {
                    print!("{}", report::symbols(&idx.path, &idx.symbols));
                }
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&all)?);
            }
        }
        Command::Diff { before, after, json } => {
            let path = after.to_string_lossy().into_owned();
            let b = python::index_file(&path, &std::fs::read_to_string(&before)?)?;
            let a = python::index_file(&path, &std::fs::read_to_string(&after)?)?;
            let changes = diff_symbols(&b.symbols, &a.symbols);
            if json {
                println!("{}", serde_json::to_string_pretty(&changes)?);
            } else if changes.is_empty() {
                println!("no symbol changes");
            } else {
                for c in &changes {
                    let tag = if c.is_contract_change() { "CONTRACT" } else { "body    " };
                    println!("{tag} {:?} {}", c.kind, c.symbol);
                    if c.is_contract_change() {
                        println!(
                            "         before: {}\n         after : {}",
                            c.before.as_ref().map(|x| x.display.as_str()).unwrap_or("-"),
                            c.after.as_ref().map(|x| x.display.as_str()).unwrap_or("(removed)")
                        );
                    }
                }
            }
        }
        Command::Analyze {
            base,
            branches,
            repo,
            no_textual,
            notices,
            json,
        } => {
            if branches.len() < 2 && !notices {
                bail!("give at least two branches to compare");
            }
            let repo = Repo::open(&repo)?;
            let analysis = analyze_branches(&repo, &base, &branches, !no_textual)?;
            let sim = notices.then(|| simulate(&analysis.branches));
            if json {
                let v = serde_json::json!({ "analysis": analysis, "simulation": sim });
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                print!("{}", report::analysis(&analysis, sim.as_ref()));
            }
        }
    }
    Ok(())
}
