use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use concord_cli::live::{self, Mode};
use concord_cli::run::{self, RunConfig, Task};
use concord_cli::{analyze_branches, report, simulate};
use concord_core::diff_symbols;
use concord_git::Repo;
use concord_lang::python;
use std::path::PathBuf;
use std::time::Duration;

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
    /// Launch several Claude Code agents in parallel worktrees, coordinate them
    /// live, integrate their work and run the tests.
    Run {
        /// JSON file with `[{"name": "...", "prompt": "..."}, ...]`.
        #[arg(long)]
        tasks: PathBuf,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long, default_value = "HEAD")]
        base: String,
        #[arg(long, default_value = "claude-haiku-5-5")]
        model: String,
        /// off (no hooks) | observe (log only) | enforce (notices + finish gate).
        #[arg(long, default_value = "enforce")]
        mode: Mode,
        /// Spending cap per agent session, in USD.
        #[arg(long, default_value_t = 1.0)]
        budget_usd: f64,
        /// Test command run in each worktree and on the integrated result.
        #[arg(long, default_value = "python3 -m unittest discover -s tests -t .")]
        test_cmd: String,
        /// Tools the agents may use without asking (besides file edits).
        #[arg(long, default_values_t = ["Bash(python3 -m unittest *)".to_string(), "Bash(python3 -m pytest *)".to_string()])]
        allow: Vec<String>,
        /// Where runs are stored (default: <repo>/.concord/runs).
        #[arg(long)]
        out: Option<PathBuf>,
        /// How long a finishing agent may wait for producers it depends on.
        #[arg(long, default_value_t = 900)]
        max_wait_secs: u64,
        /// Finish gate also merges finished peers whose work conflicts textually,
        /// so the finishing agent resolves the conflict (enforce mode).
        #[arg(long)]
        peer_merge: bool,
        /// Disable the contract layer (no notices, no contract gate, no resumes).
        /// With --peer-merge this is the peer-merge-only ablation.
        #[arg(long)]
        no_contracts: bool,
        /// Region coherence (enforce mode): textual overlaps between agents are
        /// detected after every edit; the higher-ranked agent (later in the task
        /// list) moves its code or takes over the owner's finished version, and
        /// cannot finish while its work conflicts with a finished or
        /// lower-ranked peer. No conflict markers, no peer merge.
        #[arg(long)]
        regions: bool,
        /// Structured merge driver for merges and conflict checks, e.g. the
        /// path of `mergiraf` (called as `<CMD> merge --git %O %A %B ...`).
        #[arg(long)]
        merge_driver: Option<String>,
        /// Rounds of resuming agents that became stale after finishing.
        #[arg(long, default_value_t = 2)]
        resume_rounds: u32,
        /// Claude Code executable.
        #[arg(long, default_value = "claude")]
        claude: String,
        /// Coding agent that runs the tasks: claude (Claude Code) or opencode.
        #[arg(long, value_enum, default_value = "claude")]
        agent_cli: run::AgentCli,
        /// OpenCode executable (with --agent-cli opencode). The model is given
        /// with --model in OpenCode's provider/model form, e.g. opencode/qwen4-max-free.
        #[arg(long, default_value = "opencode")]
        opencode: String,
    },
    /// Serve Concord as an MCP server on stdin/stdout, for MCP clients such as
    /// OpenCode or Claude Code. Its tools start and follow coordinated agent teams
    /// on the client's repository.
    Mcp {
        /// Coding agent for the team: opencode or claude.
        #[arg(long, default_value = "opencode")]
        agent_cli: String,
        /// Default model for the team's agents.
        #[arg(long, default_value = "opencode/qwen4-max-free")]
        model: String,
    },
    /// Live view of a coordinated run: each agent's state (working, waiting for
    /// whom, held by the finish gate, finished) and Concord's coordination
    /// events (notices, region hand-offs, combined insertions, merges).
    Watch {
        /// A run directory (with events.jsonl), or a directory of runs (the
        /// most recent one is shown), e.g. the `--out` of `concord run`.
        dir: PathBuf,
        /// Print one snapshot and exit.
        #[arg(long)]
        once: bool,
        /// Refresh interval in milliseconds.
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
    },
    /// Hook entry point (configured by `concord run`): forwards the hook input
    /// on stdin to the daemon and prints its answer.
    Hook {
        #[arg(long)]
        addr: String,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        event: String,
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
        Command::Run {
            tasks,
            repo,
            base,
            model,
            mode,
            budget_usd,
            test_cmd,
            allow,
            out,
            max_wait_secs,
            peer_merge,
            no_contracts,
            regions,
            merge_driver,
            resume_rounds,
            claude,
            agent_cli,
            opencode,
        } => {
            let tasks: Vec<Task> = serde_json::from_str(&std::fs::read_to_string(&tasks)?)?;
            let repo_root = Repo::open(&repo)?.root().to_path_buf();
            let cfg = RunConfig {
                out_dir: out.unwrap_or_else(|| repo_root.join(".concord").join("runs")),
                repo: repo_root,
                base,
                tasks,
                model,
                mode,
                budget_usd,
                test_cmd,
                allowed_tools: allow,
                max_wait: Duration::from_secs(max_wait_secs),
                peer_merge,
                contracts: !no_contracts,
                regions,
                merge_driver,
                resume_rounds,
                claude,
                agent_cli,
                opencode,
                concord_exe: std::env::current_exe()?,
            };
            let r = run::run(&cfg)?;
            print!("{}", run::render(&r));
            eprintln!("run directory: {}", r.run_dir.display());
        }
        Command::Mcp { agent_cli, model } => {
            concord_cli::mcp::serve(concord_cli::mcp::McpConfig {
                agent_cli,
                model,
                concord_exe: std::env::current_exe()?,
            })?;
        }
        Command::Watch { dir, once, interval_ms } => {
            concord_cli::watch::watch(&dir, once, Duration::from_millis(interval_ms))?;
        }
        Command::Hook { addr, agent, event } => {
            std::process::exit(live::hook_client(&addr, &agent, &event));
        }
    }
    Ok(())
}
