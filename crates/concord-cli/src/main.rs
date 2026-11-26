use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use concord_cli::live::{self, Mode};
use concord_cli::mrp::{
    MergeReadinessPack, SmartMergeConfig, analyze_worktrees, build_mrp_for_run, render_mrp_markdown,
    render_smart_merge, render_worktrees, smart_merge,
};
use concord_cli::run::{self, RunConfig, RunReport, Task};
use concord_cli::{analyze_branches, report, simulate};
use concord_core::diff_symbols;
use concord_git::Repo;
use concord_lang::python;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Concord: contract-level coherence and smart merging for parallel AI coding agents.
#[derive(Parser)]
#[command(name = "concord", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List symbol contracts and digests of source files or directories (Python, TS/JS, Rust, Go).
    Symbols {
        files: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Classify symbol changes between two versions of a source file (contract vs body).
    Diff {
        before: PathBuf,
        after: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Analyse branches developed concurrently from a common base: contract
    /// dependencies, duplicate definitions, file/symbol overlaps, textual merges,
    /// and protocol simulation.
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
    /// Inspect all active `git worktree` checkouts in memory (including uncommitted
    /// edits) for cross-worktree contract violations, duplicate definitions, and
    /// textual overlaps.
    Worktrees {
        /// Common base revision (default: HEAD).
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Include clean worktrees with no changes relative to base.
        #[arg(long)]
        include_clean: bool,
        #[arg(long)]
        json: bool,
    },
    /// Contract-verified smart merge: order branches producer-first, resolve
    /// verified pure two-sided insertions, check post-merge AST call conformance,
    /// and optionally run tests with binary failure bisection.
    Merge {
        /// Base revision to start integration from.
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Branches to merge.
        #[arg(required = true)]
        branches: Vec<String>,
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Optional branch name to create or update when merge succeeds cleanly.
        #[arg(long)]
        into: Option<String>,
        /// Optional test command to verify integrated code (bisects culprit on failure).
        #[arg(long)]
        test_cmd: Option<String>,
        /// Optional structured merge driver executable (e.g. `mergiraf`).
        #[arg(long)]
        merge_driver: Option<String>,
        /// Run Tier-2 native type checker (tsc, pyright/ty, cargo check, go vet) on the merged tree.
        #[arg(long)]
        type_check: bool,
        #[arg(long)]
        json: bool,
    },
    /// Render or export the SASE Merge-Readiness Pack (MRP) of a Concord run directory.
    Mrp {
        /// Run directory containing `mrp.json` or `report.json`.
        dir: PathBuf,
        /// Repository path (used only if `mrp.json` must be reconstructed from `report.json`).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Partition a repository into conflict-aware parallel ticket clusters and shared hubs.
    Plan {
        /// Git revision to analyse (default: HEAD).
        #[arg(long, default_value = "HEAD")]
        rev: String,
        /// Maximum parallel clusters to suggest.
        #[arg(long, default_value_t = 6)]
        max_clusters: usize,
        /// Repository path.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Auto-detect and run the repository's Tier-2 native type checker (tsc, pyright/ty, cargo check, go vet).
    Typecheck {
        /// Directory to check.
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Automatically connect Concord's MCP server to OpenCode, Claude Code, and Cursor.
    Setup {
        /// Target client: auto (default), opencode, claude, cursor, or antigravity.
        #[arg(long, default_value = "auto")]
        client: String,
        /// Optional default model passed to `concord mcp --model`.
        #[arg(long)]
        model: Option<String>,
        /// Write project-local config instead of user global config.
        #[arg(long)]
        project: bool,
    },
    /// Launch several AI coding agents in parallel worktrees, coordinate them
    /// live, integrate their work and run the tests.
    Run {
        /// JSON file with `[{"name": "...", "prompt": "..."}, ...]`.
        #[arg(long)]
        tasks: Option<PathBuf>,
        /// JSON file with sequential waves of parallel tasks: `[{"name": "wave-1", "tasks": [...]}, ...]`.
        #[arg(long)]
        milestones: Option<PathBuf>,
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
        #[arg(long)]
        no_contracts: bool,
        /// Region coherence (enforce mode): textual overlaps between agents are
        /// detected after every edit; overlapping edits are moved or handed off,
        /// and verified pure two-sided insertions are combined automatically.
        #[arg(long)]
        regions: bool,
        /// Baseline B3: exclusive whole-file leases (first writer locks the file).
        #[arg(long)]
        file_leases: bool,
        /// Baseline B4: Clash/Grove passive textual overlap warnings.
        #[arg(long)]
        overlap_warnings: bool,
        /// Ablation A1: treat every body change as a breaking contract change.
        #[arg(long)]
        all_contracts: bool,
        /// Ablation A2: suppress live PostToolUse notice delivery (gate only).
        #[arg(long)]
        no_notices: bool,
        /// Ablation A4: skip finish-gate blocking on Stop (notices only).
        #[arg(long)]
        no_gate: bool,
        /// Debounce window (milliseconds) before publishing contract changes.
        #[arg(long, default_value_t = 0)]
        publish_debounce_ms: u64,
        /// Optional interval (seconds) for continuous background speculative integration.
        #[arg(long)]
        speculative_secs: Option<u64>,
        /// Before an agent may finish, test its work merged with the finished
        /// agents' work (with --test-cmd) and block on tests that fail
        /// together but pass alone (with O(log k) peer bisection).
        #[arg(long)]
        fails_together: bool,
        /// Structured merge driver for merges and conflict checks, e.g. `mergiraf`.
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
        /// OpenCode executable (with --agent-cli opencode).
        #[arg(long, default_value = "opencode")]
        opencode: String,
    },
    /// Serve Concord as an MCP server on stdin/stdout.
    Mcp {
        /// Coding agent for the team: opencode or claude.
        #[arg(long, default_value = "opencode")]
        agent_cli: String,
        /// Default model for the team's agents.
        #[arg(long, default_value = "opencode/qwen4-max-free")]
        model: String,
    },
    /// Live view of a coordinated run: each agent's state and latest action,
    /// and the coordination between agents. Given a parent directory, follows
    /// the newest run below it.
    Watch {
        /// A run directory, or a parent of runs (e.g. `.concord/runs`).
        dir: PathBuf,
        /// Print one frame and exit.
        #[arg(long)]
        once: bool,
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        /// Serve the live view in a browser (graph, agent cards, feed).
        #[arg(long)]
        web: bool,
        /// Port for `--web` (127.0.0.1 only).
        #[arg(long, default_value_t = 4747)]
        port: u16,
    },
    /// Hook entry point (configured by `concord run`).
    Hook {
        #[arg(long)]
        addr: String,
        #[arg(long)]
        agent: String,
        #[arg(long)]
        event: String,
    },
}

/// Index a file with the front-end for its extension; unknown extensions fall
/// back to Python (preserving legacy tempfile behaviour in CLI `diff`).
fn index_any(path: &str, src: &str) -> Result<concord_lang::FileIndex, concord_lang::LangError> {
    concord_lang::index_file(path, src).unwrap_or_else(|| python::index_file(path, src))
}

fn expand_paths(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for p in inputs {
        if p.is_dir() {
            collect_dir(p, &mut out);
        } else {
            out.push(p.clone());
        }
    }
    out
}

fn collect_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|x| x.path())).collect();
    entries.sort();
    for p in entries {
        let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if name.starts_with('.') || matches!(name, "node_modules" | "target" | "vendor" | "__pycache__") {
            continue;
        }
        if p.is_dir() {
            collect_dir(&p, out);
        } else if concord_lang::is_supported_path(&p.to_string_lossy()) {
            out.push(p);
        }
    }
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Symbols { files, json } => {
            let mut all = Vec::new();
            for f in expand_paths(&files) {
                let src = std::fs::read_to_string(&f).with_context(|| format!("reading {}", f.display()))?;
                let idx = index_any(&f.to_string_lossy(), &src)?;
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
            let b = index_any(&path, &std::fs::read_to_string(&before)?)?;
            let a = index_any(&path, &std::fs::read_to_string(&after)?)?;
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
        Command::Worktrees {
            base,
            repo,
            include_clean,
            json,
        } => {
            let repo = Repo::open(&repo)?;
            let res = analyze_worktrees(&repo, &base, include_clean)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                print!("{}", render_worktrees(&res));
            }
        }
        Command::Merge {
            base,
            branches,
            repo,
            into,
            test_cmd,
            merge_driver,
            type_check,
            json,
        } => {
            let res = smart_merge(&SmartMergeConfig {
                repo,
                base,
                branches,
                into_branch: into,
                test_cmd,
                merge_driver,
                type_check,
            })?;
            let tc_ok = res.type_check.as_ref().is_none_or(|t| !t.available || t.ok);
            let clean = res.conflicts.is_empty()
                && res.post_merge_violations.is_empty()
                && res.tests_passed.unwrap_or(true)
                && tc_ok;
            if json {
                println!("{}", serde_json::to_string_pretty(&res)?);
            } else {
                print!("{}", render_smart_merge(&res));
            }
            if !clean {
                std::process::exit(1);
            }
        }
        Command::Mrp { dir, repo, json } => {
            let mrp_path = dir.join("mrp.json");
            let mrp: MergeReadinessPack = if mrp_path.exists() {
                serde_json::from_str(&std::fs::read_to_string(&mrp_path)?)?
            } else {
                let rep_val: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(dir.join("report.json"))?)?;
                let run_id = rep_val.get("run_id").and_then(|v| v.as_str()).unwrap_or("").to_owned();
                let r_repo = Repo::open(&repo)?;
                let ic = r_repo.rev_parse(&format!("concord/{run_id}/integration")).ok();
                // Reconstruct a minimal RunReport view if only report.json exists.
                let report_typed: RunReport = reconstruct_report(&rep_val, &dir);
                build_mrp_for_run(&r_repo, &report_typed, ic, None)
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&mrp)?);
            } else {
                print!("{}", render_mrp_markdown(&mrp));
            }
        }
        Command::Run {
            tasks,
            milestones,
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
            file_leases,
            overlap_warnings,
            all_contracts,
            no_notices,
            no_gate,
            publish_debounce_ms,
            speculative_secs,
            fails_together,
            merge_driver,
            resume_rounds,
            claude,
            agent_cli,
            opencode,
        } => {
            #[derive(serde::Deserialize)]
            struct MilestoneWave {
                name: String,
                tasks: Vec<Task>,
                #[serde(default)]
                test_cmd: Option<String>,
            }
            let repo_root = Repo::open(&repo)?.root().to_path_buf();
            let base_out = out.unwrap_or_else(|| repo_root.join(".concord").join("runs"));
            let concord_exe = std::env::current_exe()?;
            let waves: Vec<MilestoneWave> = match (tasks, milestones) {
                (Some(tpath), None) => vec![MilestoneWave {
                    name: String::new(),
                    tasks: serde_json::from_str(&std::fs::read_to_string(&tpath)?)?,
                    test_cmd: None,
                }],
                (None, Some(mpath)) => serde_json::from_str(&std::fs::read_to_string(&mpath)?)?,
                (Some(_), Some(_)) => bail!("pass either --tasks or --milestones, not both"),
                (None, None) => bail!("pass --tasks <file.json> or --milestones <file.json>"),
            };
            let mut cur_base = base;
            let n_waves = waves.len();
            for (idx, wave) in waves.into_iter().enumerate() {
                let wave_out = if wave.name.is_empty() {
                    base_out.clone()
                } else {
                    base_out.join(format!("{:02}-{}", idx + 1, wave.name))
                };
                if !wave.name.is_empty() {
                    eprintln!("=== Milestone wave {}: {} (base: {}) ===", idx + 1, wave.name, cur_base);
                }
                let cfg = RunConfig {
                    out_dir: wave_out,
                    repo: repo_root.clone(),
                    base: cur_base.clone(),
                    tasks: wave.tasks,
                    model: model.clone(),
                    mode,
                    budget_usd,
                    test_cmd: wave.test_cmd.unwrap_or_else(|| test_cmd.clone()),
                    allowed_tools: allow.clone(),
                    max_wait: Duration::from_secs(max_wait_secs),
                    peer_merge,
                    contracts: !no_contracts,
                    regions,
                    file_leases,
                    overlap_warnings,
                    all_contracts,
                    deliver_notices: !no_notices,
                    enforce_gate: !no_gate,
                    publish_debounce: Duration::from_millis(publish_debounce_ms),
                    speculative_secs,
                    fails_together,
                    merge_driver: merge_driver.clone(),
                    resume_rounds,
                    claude: claude.clone(),
                    agent_cli,
                    opencode: opencode.clone(),
                    concord_exe: concord_exe.clone(),
                };
                let r = run::run(&cfg)?;
                print!("{}", run::render(&r));
                eprintln!("run directory: {}", r.run_dir.display());
                // Conflicts end a multi-wave run (the next wave would build on
                // a broken integration), but the last (or only) wave's
                // conflicts are a result, reported, not a failure of the run.
                if !r.integration.conflicts.is_empty() && idx + 1 < n_waves {
                    bail!(
                        "wave {} ended with {} merge conflict(s); stopping before next wave",
                        idx + 1,
                        r.integration.conflicts.len()
                    );
                }
                cur_base = format!("concord/{}/integration", r.run_id);
            }
        }
        Command::Mcp { agent_cli, model } => {
            concord_cli::mcp::serve(concord_cli::mcp::McpConfig {
                agent_cli,
                model,
                concord_exe: std::env::current_exe()?,
            })?;
        }
        Command::Plan {
            rev,
            max_clusters,
            repo,
            json,
        } => {
            let repo = Repo::open(&repo)?;
            let plan = concord_cli::plan::build_plan(&repo, &rev, max_clusters)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                print!("{}", concord_cli::plan::render_plan(&plan));
            }
        }
        Command::Typecheck { repo, json } => match concord_cli::typecheck::run_type_check(&repo) {
            Some(res) => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&res)?);
                } else if !res.available {
                    println!("{} not found on PATH; skipping Tier-2 type check.", res.tool);
                } else if res.ok {
                    println!("{} passed (0 errors).", res.tool);
                } else {
                    for d in &res.diagnostics {
                        println!("{d}");
                    }
                    std::process::exit(1);
                }
            }
            None => {
                if json {
                    println!("null");
                } else {
                    println!("No native type checker detected.");
                }
            }
        },
        Command::Setup { client, model, project } => {
            let target = client.parse()?;
            let actions = concord_cli::setup::run_setup(&concord_cli::setup::SetupOptions {
                client: target,
                model,
                project_local: project,
                concord_exe: std::env::current_exe()?,
            })?;
            for a in actions {
                println!("{a}");
            }
        }
        Command::Watch {
            dir,
            once,
            interval_ms,
            web,
            port,
        } => {
            if web {
                concord_cli::web::serve(&dir, port, Duration::from_millis(interval_ms))?;
            } else {
                concord_cli::watch::watch(&dir, once, Duration::from_millis(interval_ms))?;
            }
        }
        Command::Hook { addr, agent, event } => {
            std::process::exit(live::hook_client(&addr, &agent, &event));
        }
    }
    Ok(())
}

fn reconstruct_report(v: &serde_json::Value, dir: &Path) -> RunReport {
    if let Ok(mut r) = serde_json::from_value::<RunReport>(v.clone()) {
        r.run_dir = dir.to_path_buf();
        return r;
    }
    let mode = v
        .get("mode")
        .and_then(|m| m.as_str())
        .and_then(|m| m.parse().ok())
        .unwrap_or(Mode::Enforce);
    RunReport {
        run_id: v.get("run_id").and_then(|x| x.as_str()).unwrap_or("").to_owned(),
        mode,
        model: v.get("model").and_then(|x| x.as_str()).unwrap_or("").to_owned(),
        base: v.get("base").and_then(|x| x.as_str()).unwrap_or("").to_owned(),
        agents: Vec::new(),
        integration: run::Integration {
            order: v
                .pointer("/integration/order")
                .and_then(|x| x.as_array())
                .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_owned)).collect())
                .unwrap_or_default(),
            conflicts: Default::default(),
            unions: Default::default(),
            tests_passed: v
                .pointer("/integration/tests_passed")
                .and_then(|x| x.as_bool())
                .unwrap_or(false),
            tests_output_tail: v
                .pointer("/integration/tests_output_tail")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_owned(),
            bisected_culprit: None,
        },
        total_cost_usd: v.get("total_cost_usd").and_then(|x| x.as_f64()).unwrap_or(0.0),
        wall_seconds: v.get("wall_seconds").and_then(|x| x.as_f64()).unwrap_or(0.0),
        run_dir: dir.to_path_buf(),
    }
}
