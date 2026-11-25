//! `concord mcp`: Concord as an MCP server (JSON-RPC 2.0 over stdio).
//!
//! Connect it to any MCP client (OpenCode, Claude Code, Cursor, Antigravity, ...).
//! Exposes both:
//! - **Team orchestration tools** (`concord_run`, `concord_status`, `concord_result`,
//!   `concord_cancel`), and
//! - **Direct contract-level merge & inspection tools** (`concord_symbols`,
//!   `concord_diff_contracts`, `concord_analyze`, `concord_worktrees`,
//!   `concord_smart_merge`, `concord_negotiate`, `concord_ack`) so any AI agent
//!   can inspect contracts instead of whole files and merge branches safely at
//!   the symbol/contract level.

use crate::mrp::{
    SmartMergeConfig, analyze_worktrees, render_mrp_markdown, render_smart_merge, render_worktrees, smart_merge,
};
use crate::{analyze_branches, footprint_and_index, index_revision, report, simulate};
use anyhow::{Context, Result, bail};
use concord_core::ChangeKind;
use concord_git::Repo;
use concord_lang::RepoIndex;
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Defaults for runs started through MCP (from `concord mcp` arguments).
#[derive(Clone, Debug)]
pub struct McpConfig {
    pub agent_cli: String,
    pub model: String,
    pub concord_exe: PathBuf,
}

const PROTOCOL: &str = "2025-06-18";

fn tools() -> Value {
    json!([
        {
            "name": "concord_symbols",
            "description": "List symbol contracts (functions, methods, classes/structs/traits/interfaces, types, module constants) with line spans and signatures—without function bodies. Supports Python, TypeScript/JS, Rust, and Go. Use this instead of reading whole files when checking APIs before editing or merging.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Files or directories relative to repository root (default: all indexed source files at HEAD)."
                    },
                    "json": { "type": "boolean", "description": "Return structured JSON instead of compact text." }
                }
            }
        },
        {
            "name": "concord_diff_contracts",
            "description": "Compare two git revisions (or base vs working tree) at symbol/contract level rather than raw line diffs. Classifies each symbol change as breaking contract, compatible contract, body-only, added, removed, or renamed, and lists repository call sites impacted by breaking changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "base": { "type": "string", "description": "Base git revision (default: HEAD~1, or HEAD when working_tree=true)." },
                    "head": { "type": "string", "description": "Target git revision (default: HEAD)." },
                    "working_tree": { "type": "boolean", "description": "Snapshot uncommitted working-tree changes as `head`." }
                }
            }
        },
        {
            "name": "concord_analyze",
            "description": "Check concurrent git branches against a common base for cross-file contract violations, duplicate symbol definitions, symbol/file overlaps, and textual conflicts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "branches": { "type": "array", "items": { "type": "string" }, "description": "Two or more branch or commit refs." },
                    "base": { "type": "string", "description": "Common base revision (default: main, falling back to merge-base)." }
                },
                "required": ["branches"]
            }
        },
        {
            "name": "concord_worktrees",
            "description": "Inspect all active `git worktree` checkouts in this repository (including uncommitted edits) in memory without touching git state. Reports cross-worktree contract breaks, duplicate symbol definitions, textual overlaps, and auto-unionable insertions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "base": { "type": "string", "description": "Common base revision (default: HEAD)." },
                    "include_clean": { "type": "boolean", "description": "Include worktrees with zero changes relative to base." }
                }
            }
        },
        {
            "name": "concord_smart_merge",
            "description": "Merge branches in contract-dependency order (producers before consumers), automatically resolving verified pure two-sided insertions, checking post-merge AST call-site conformance, and optionally running tests with binary failure bisection.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "branches": { "type": "array", "items": { "type": "string" }, "description": "Branches to integrate." },
                    "base": { "type": "string", "description": "Base revision to start integration from (default: HEAD)." },
                    "into_branch": { "type": "string", "description": "Optional branch name to create/update when merge is clean." },
                    "test_cmd": { "type": "string", "description": "Optional shell command to verify integrated code (bisects culprit branch on failure)." },
                    "merge_driver": { "type": "string", "description": "Optional external structured merge binary (e.g. 'mergiraf')." }
                },
                "required": ["branches"]
            }
        },
        {
            "name": "concord_negotiate",
            "description": "Propose a signature/contract change on a symbol currently leased or defined by another live Concord agent. Routes a structured negotiation notice to that symbol's owner.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Qualified symbol id, e.g. 'notify.email.send_email'." },
                    "proposal": { "type": "string", "description": "Proposed signature and rationale." },
                    "agent": { "type": "string", "description": "Sender agent id (defaults to $CONCORD_AGENT)." },
                    "addr": { "type": "string", "description": "Daemon TCP address (defaults to $CONCORD_DAEMON_ADDR or active run)." }
                },
                "required": ["symbol", "proposal"]
            }
        },
        {
            "name": "concord_ack",
            "description": "Explicitly acknowledge a peer's published contract change on `symbol` after verifying that your call sites conform.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Qualified symbol id to acknowledge." },
                    "agent": { "type": "string", "description": "Agent id (defaults to $CONCORD_AGENT)." },
                    "addr": { "type": "string", "description": "Daemon TCP address (defaults to $CONCORD_DAEMON_ADDR or active run)." }
                },
                "required": ["symbol"]
            }
        },
        {
            "name": "concord_plan",
            "description": "Analyse the repository symbol dependency graph to identify shared contract hubs and recommend conflict-aware parallel ticket clusters in producer-first order before calling concord_run.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "rev": { "type": "string", "description": "Git revision (default: HEAD)." },
                    "max_clusters": { "type": "integer", "description": "Maximum number of clusters (default: 6)." }
                }
            }
        },
        {
            "name": "concord_run",
            "description": "Start a team of AI coding agents working IN PARALLEL in isolated git worktrees coordinated live by Concord. Default coordination ('regions') combines contract coherence + region coherence + verified insertion union so agents never fight over git conflict markers. Returns immediately with `run_id`; monitor with `concord_status` and retrieve integrated branch + Merge-Readiness Pack with `concord_result`.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "description": "One self-contained ticket per parallel agent.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string", "description": "Short identifier (letters, digits, '-', '_')." },
                                "prompt": { "type": "string", "description": "Self-contained feature specification." },
                                "check": { "type": "string", "description": "Optional per-ticket acceptance test command." }
                            },
                            "required": ["name", "prompt"]
                        }
                    },
                    "test_cmd": { "type": "string", "description": "Shell command testing integrated output (e.g. 'pytest -q' or 'cargo test')." },
                    "base": { "type": "string", "description": "Starting git revision (default: HEAD)." },
                    "model": { "type": "string", "description": "Override agent model." },
                    "coordination": {
                        "type": "string",
                        "enum": ["regions", "full", "contracts", "peer-merge", "file-leases", "overlap-warnings", "off"],
                        "description": "regions (default, recommended): contract coherence + region coherence + verified insertion union; full: contract coherence + peer merge; contracts: contract layer only; file-leases: exclusive whole-file locks; overlap-warnings: passive overlap alerts; off: uncoordinated baseline."
                    },
                    "fails_together": { "type": "boolean", "description": "Run fails-together semantic verification with peer bisection at finish gate (requires test_cmd)." }
                },
                "required": ["tasks"]
            }
        },
        {
            "name": "concord_status",
            "description": "Progress of a Concord run (per-agent edits, notices, gate blocks, merges), or list repository runs when `run_id` is omitted.",
            "inputSchema": { "type": "object", "properties": { "run_id": { "type": "string" } } }
        },
        {
            "name": "concord_result",
            "description": "Final report and SASE Merge-Readiness Pack (MRP) of a finished Concord run, including the integrated git branch name.",
            "inputSchema": { "type": "object", "properties": { "run_id": { "type": "string" } }, "required": ["run_id"] }
        },
        {
            "name": "concord_cancel",
            "description": "Stop a running Concord team.",
            "inputSchema": { "type": "object", "properties": { "run_id": { "type": "string" } }, "required": ["run_id"] }
        }
    ])
}

/// Serve MCP on stdin/stdout until stdin closes.
pub fn serve(cfg: McpConfig) -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned() else {
            continue; // notification
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let response = match method {
            "initialize" => {
                let version = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(PROTOCOL);
                json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": version,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "concord", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Concord coordinates and merges parallel coding work at the symbol/contract level (Python, TypeScript/JS, Rust, Go). Use concord_symbols and concord_diff_contracts to inspect APIs concisely, concord_worktrees or concord_analyze to detect cross-branch contract breaks, concord_smart_merge to merge branches safely, or concord_run to launch coordinated parallel agents."
                }})
            }
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools() } }),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                let (text, is_error) = match dispatch(&cfg, &cwd, name, &args) {
                    Ok(t) => (t, false),
                    Err(e) => (format!("{e:#}"), true),
                };
                json!({ "jsonrpc": "2.0", "id": id, "result": {
                    "content": [{ "type": "text", "text": text }],
                    "isError": is_error
                }})
            }
            _ => {
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("unknown method {method}") } })
            }
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}

/// Dispatch a single MCP tool call rooted at `cwd` (public for integration tests).
pub fn dispatch(cfg: &McpConfig, cwd: &Path, name: &str, args: &Value) -> Result<String> {
    let repo = Repo::open(cwd).context("Concord needs to run inside a git repository")?;
    let root = repo.root().to_path_buf();
    match name {
        "concord_symbols" => tool_symbols(&repo, &root, args),
        "concord_diff_contracts" => tool_diff_contracts(&repo, &root, args),
        "concord_analyze" => tool_analyze(&repo, args),
        "concord_worktrees" => tool_worktrees(&repo, args),
        "concord_smart_merge" => tool_smart_merge(&root, args),
        "concord_negotiate" => tool_ipc(&root, "Negotiate", args),
        "concord_ack" => tool_ipc(&root, "Ack", args),
        "concord_plan" => tool_plan(&repo, args),
        "concord_run" => start_run(cfg, &repo, &root, args),
        "concord_status" => match args.get("run_id").and_then(Value::as_str) {
            Some(id) => status(&root, id),
            None => list_runs(&root),
        },
        "concord_result" => result(&root, arg(args, "run_id")?),
        "concord_cancel" => cancel(&root, arg(args, "run_id")?),
        _ => bail!("unknown tool {name}"),
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing argument `{key}`"))
}

fn tool_symbols(repo: &Repo, root: &Path, args: &Value) -> Result<String> {
    let want_json = args.get("json").and_then(Value::as_bool).unwrap_or(false);
    let paths: Vec<String> = args
        .get("paths")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();

    let mut targets: Vec<PathBuf> = Vec::new();
    if paths.is_empty() {
        let head = repo.rev_parse("HEAD")?;
        for rel in repo.list_files(&head)? {
            if concord_lang::is_supported_path(&rel) {
                targets.push(root.join(rel));
            }
        }
    } else {
        for p in &paths {
            collect_source_files(&root.join(p), &mut targets);
        }
    }

    let mut indices = Vec::new();
    let mut text = String::new();
    for abs in targets {
        let Ok(src) = std::fs::read_to_string(&abs) else {
            continue;
        };
        let rel = abs
            .strip_prefix(root)
            .unwrap_or(&abs)
            .to_string_lossy()
            .replace('\\', "/");
        if let Some(Ok(idx)) = concord_lang::index_file(&rel, &src) {
            if want_json {
                indices.push(idx);
            } else if !idx.symbols.is_empty() {
                text.push_str(&report::symbols(&idx.path, &idx.symbols));
            }
        }
    }
    if want_json {
        Ok(serde_json::to_string_pretty(&indices)?)
    } else if text.is_empty() {
        Ok("No supported symbols found in the requested paths.".into())
    } else {
        Ok(text)
    }
}

fn collect_source_files(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
    } else if path.is_dir()
        && let Ok(rd) = std::fs::read_dir(path)
    {
        let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|x| x.path())).collect();
        entries.sort();
        for p in entries {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.starts_with('.') || matches!(name, "node_modules" | "target" | "vendor" | "__pycache__") {
                continue;
            }
            if p.is_dir() {
                collect_source_files(&p, out);
            } else if concord_lang::is_supported_path(&p.to_string_lossy()) {
                out.push(p);
            }
        }
    }
}

fn tool_diff_contracts(repo: &Repo, root: &Path, args: &Value) -> Result<String> {
    let working_tree = args.get("working_tree").and_then(Value::as_bool).unwrap_or(false);
    let base_arg = args
        .get("base")
        .and_then(Value::as_str)
        .unwrap_or(if working_tree { "HEAD" } else { "HEAD~1" });
    let base_commit = repo
        .rev_parse(base_arg)
        .or_else(|_| repo.rev_parse("HEAD"))
        .context("cannot resolve base revision")?;
    let head_commit = if working_tree {
        Repo::snapshot_worktree(root)?
    } else {
        let h = args.get("head").and_then(Value::as_str).unwrap_or("HEAD");
        repo.rev_parse(h)?
    };

    let mut warnings = Vec::new();
    let base_files = index_revision(repo, &base_commit, &mut warnings)?;
    let base_index = RepoIndex::new(base_files.values().cloned());
    let (fp, head_index) = footprint_and_index(
        repo,
        &base_commit,
        &head_commit,
        &base_files,
        &base_index,
        &mut warnings,
    )?;

    if fp.changes.is_empty() {
        return Ok(format!(
            "No symbol changes between {} and {}.",
            &base_commit[..base_commit.len().min(10)],
            &head_commit[..head_commit.len().min(10)]
        ));
    }

    let mut out = format!(
        "Contract diff {} -> {} ({} files, {} symbol changes):\n",
        &base_commit[..base_commit.len().min(10)],
        &head_commit[..head_commit.len().min(10)],
        fp.changed_files.len(),
        fp.changes.len()
    );
    let all_syms: std::collections::BTreeSet<_> = head_index.symbols().map(|s| s.id.clone()).collect();
    let all_files: std::collections::BTreeSet<_> = head_index.files().map(|f| f.path.clone()).collect();
    let repo_uses = head_index.uses_from(&all_syms, &all_files);

    for c in &fp.changes {
        let tag = match &c.kind {
            ChangeKind::ContractChanged if c.is_breaking() => "BREAKING_CONTRACT",
            ChangeKind::ContractChanged => "COMPAT_CONTRACT  ",
            ChangeKind::Added => "ADDED            ",
            ChangeKind::Removed => "REMOVED          ",
            ChangeKind::Renamed { .. } => "RENAMED          ",
            ChangeKind::BodyChanged => "BODY_ONLY        ",
        };
        let _ = writeln!(out, "  {tag} {} ({})", c.symbol, c.path);
        if c.is_contract_change() {
            if let Some(b) = &c.before {
                let _ = writeln!(out, "    before: {}", b.display);
            }
            if let Some(a) = &c.after {
                let _ = writeln!(out, "    after : {}", a.display);
            }
            if c.is_breaking()
                && let Some(sites) = repo_uses.get(&c.symbol)
            {
                for u in sites.iter().take(10) {
                    let _ = writeln!(out, "    caller: {}:{}  {}", u.path, u.line, u.text);
                }
            }
        }
    }
    Ok(out)
}

fn tool_analyze(repo: &Repo, args: &Value) -> Result<String> {
    let branches: Vec<String> = args
        .get("branches")
        .and_then(Value::as_array)
        .context("`branches` must be a list")?
        .iter()
        .filter_map(|b| b.as_str().map(str::to_owned))
        .collect();
    if branches.len() < 2 {
        bail!("provide at least two branches to compare");
    }
    let base = match args.get("base").and_then(Value::as_str) {
        Some(b) => b.to_owned(),
        None => repo
            .rev_parse("main")
            .or_else(|_| repo.merge_base(&branches[0], &branches[1]))
            .unwrap_or_else(|_| "HEAD".into()),
    };
    let analysis = analyze_branches(repo, &base, &branches, true)?;
    let sim = simulate(&analysis.branches);
    Ok(report::analysis(&analysis, Some(&sim)))
}

fn tool_worktrees(repo: &Repo, args: &Value) -> Result<String> {
    let base = args.get("base").and_then(Value::as_str).unwrap_or("HEAD");
    let include_clean = args.get("include_clean").and_then(Value::as_bool).unwrap_or(false);
    let wt = analyze_worktrees(repo, base, include_clean)?;
    Ok(render_worktrees(&wt))
}

fn tool_plan(repo: &Repo, args: &Value) -> Result<String> {
    let rev = args.get("rev").and_then(Value::as_str).unwrap_or("HEAD");
    let max_clusters = args.get("max_clusters").and_then(Value::as_u64).unwrap_or(6) as usize;
    let plan = crate::plan::build_plan(repo, rev, max_clusters)?;
    Ok(crate::plan::render_plan(&plan))
}

fn tool_smart_merge(root: &Path, args: &Value) -> Result<String> {
    let branches: Vec<String> = args
        .get("branches")
        .and_then(Value::as_array)
        .context("`branches` must be a list")?
        .iter()
        .filter_map(|b| b.as_str().map(str::to_owned))
        .collect();
    let base = args.get("base").and_then(Value::as_str).unwrap_or("HEAD").to_owned();
    let into_branch = args.get("into_branch").and_then(Value::as_str).map(str::to_owned);
    let test_cmd = args.get("test_cmd").and_then(Value::as_str).map(str::to_owned);
    let merge_driver = args.get("merge_driver").and_then(Value::as_str).map(str::to_owned);
    let rep = smart_merge(&SmartMergeConfig {
        repo: root.to_path_buf(),
        base,
        branches,
        into_branch,
        test_cmd,
        merge_driver,
    })?;
    Ok(render_smart_merge(&rep))
}

fn discover_daemon_addr(root: &Path) -> Option<String> {
    if let Ok(a) = std::env::var("CONCORD_DAEMON_ADDR")
        && !a.trim().is_empty()
    {
        return Some(a.trim().to_owned());
    }
    for sub in ["runs", "mcp"] {
        let d = root.join(".concord").join(sub);
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.filter_map(|e| e.ok()) {
                let p = e.path();
                if let Ok(addr) = std::fs::read_to_string(p.join("daemon.addr")) {
                    return Some(addr.trim().to_owned());
                }
                if let Some(inner) = inner_run_dir(&p)
                    && let Ok(addr) = std::fs::read_to_string(inner.join("daemon.addr"))
                {
                    return Some(addr.trim().to_owned());
                }
            }
        }
    }
    None
}

fn tool_ipc(root: &Path, event: &str, args: &Value) -> Result<String> {
    let agent = args
        .get("agent")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| std::env::var("CONCORD_AGENT").ok())
        .filter(|s| !s.is_empty())
        .context("provide `agent` or set $CONCORD_AGENT")?;
    let addr = args
        .get("addr")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| discover_daemon_addr(root))
        .context("no active Concord daemon found (provide `addr` or run inside `concord run`)")?;
    let resp = crate::live::daemon_request(&addr, &agent, event, args)?;
    Ok(serde_json::to_string_pretty(&resp)?)
}

fn mcp_dir(root: &Path) -> PathBuf {
    root.join(".concord").join("mcp")
}

fn run_dir(root: &Path, id: &str) -> Result<PathBuf> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("invalid run id {id:?}");
    }
    let d = mcp_dir(root).join(id);
    if !d.is_dir() {
        bail!("no Concord run {id:?} in this repository (see concord_status without run_id)");
    }
    Ok(d)
}

/// Keep Concord's files out of `git status`.
fn exclude_concord_dir(root: &Path) {
    let Ok(out) = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "--git-common-dir"])
        .output()
    else {
        return;
    };
    let common = root.join(String::from_utf8_lossy(&out.stdout).trim());
    let exclude = common.join("info").join("exclude");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !current.lines().any(|l| l.trim() == "/.concord/") {
        let _ = std::fs::create_dir_all(exclude.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::write(
            &exclude,
            format!(
                "{current}{}/.concord/\n",
                if current.ends_with('\n') || current.is_empty() {
                    ""
                } else {
                    "\n"
                }
            ),
        );
    }
}

fn start_run(cfg: &McpConfig, repo: &Repo, root: &Path, args: &Value) -> Result<String> {
    let tasks = args
        .get("tasks")
        .and_then(Value::as_array)
        .context("`tasks` must be a list")?;
    if tasks.is_empty() {
        bail!("`tasks` is empty");
    }
    let mut names = std::collections::BTreeSet::new();
    for t in tasks {
        let n = t
            .get("name")
            .and_then(Value::as_str)
            .context("every task needs a `name`")?;
        if n.is_empty() || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            bail!("task name {n:?} must use letters, digits, '-' or '_'");
        }
        if !names.insert(n.to_owned()) {
            bail!("duplicate task name {n:?}");
        }
        t.get("prompt")
            .and_then(Value::as_str)
            .context("every task needs a `prompt`")?;
    }
    let base = args.get("base").and_then(Value::as_str).unwrap_or("HEAD");
    let base_rev = repo
        .rev_parse(base)
        .with_context(|| format!("cannot resolve base {base:?}"))?;
    exclude_concord_dir(root);

    let id = format!("run-{}", crate::live::now_millis() / 1000);
    let dir = mcp_dir(root).join(&id);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("tasks.json"), serde_json::to_string_pretty(tasks)?)?;
    let model = args
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&cfg.model)
        .to_owned();
    let coordination = args.get("coordination").and_then(Value::as_str).unwrap_or("regions");
    let mut cmd_args: Vec<String> = vec![
        "run".into(),
        "--repo".into(),
        root.to_string_lossy().into_owned(),
        "--base".into(),
        base_rev.clone(),
        "--tasks".into(),
        dir.join("tasks.json").to_string_lossy().into_owned(),
        "--out".into(),
        dir.to_string_lossy().into_owned(),
        "--agent-cli".into(),
        cfg.agent_cli.clone(),
        "--model".into(),
        model.clone(),
    ];
    match coordination {
        "regions" => cmd_args.extend(["--mode", "enforce", "--regions"].map(String::from)),
        "full" => cmd_args.extend(["--mode", "enforce", "--peer-merge"].map(String::from)),
        "contracts" => cmd_args.extend(["--mode", "enforce"].map(String::from)),
        "peer-merge" => cmd_args.extend(["--mode", "enforce", "--peer-merge", "--no-contracts"].map(String::from)),
        "file-leases" => cmd_args.extend(["--mode", "enforce", "--file-leases", "--no-contracts"].map(String::from)),
        "overlap-warnings" => cmd_args
            .extend(["--mode", "enforce", "--overlap-warnings", "--no-contracts", "--no-gate"].map(String::from)),
        "off" => cmd_args.extend(["--mode", "off"].map(String::from)),
        other => bail!("unknown coordination {other:?}"),
    }
    if let Some(t) = args.get("test_cmd").and_then(Value::as_str) {
        cmd_args.extend(["--test-cmd".into(), t.to_owned()]);
        if args.get("fails_together").and_then(Value::as_bool).unwrap_or(false) {
            cmd_args.push("--fails-together".into());
        }
    } else if args.get("fails_together").and_then(Value::as_bool).unwrap_or(false) {
        bail!("fails_together needs test_cmd");
    }
    let log = std::fs::File::create(dir.join("concord.log"))?;
    let mut cmd = Command::new(&cfg.concord_exe);
    cmd.current_dir(root)
        .args(&cmd_args)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // survive the MCP client closing
    }
    let child = cmd.spawn().context("starting concord run")?;
    std::fs::write(dir.join("pid"), child.id().to_string())?;
    std::fs::write(
        dir.join("meta.json"),
        serde_json::to_string_pretty(&json!({
            "id": id, "base": base_rev, "model": model, "coordination": coordination,
            "agents": names, "started_ms": crate::live::now_millis()
        }))?,
    )?;
    Ok(format!(
        "Started Concord run `{id}`: {} agents in parallel ({}), model {model}, coordination {coordination}, from {}.\n\
         Agents usually need a few minutes. Call concord_status with run_id `{id}` to follow progress, and \
         concord_result when it has finished.",
        names.len(),
        names.iter().cloned().collect::<Vec<_>>().join(", "),
        &base_rev[..base_rev.len().min(10)]
    ))
}

fn inner_run_dir(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.is_dir() && p.join("events.jsonl").exists())
}

fn is_alive(dir: &Path) -> bool {
    let Some(pid) = std::fs::read_to_string(dir.join("pid"))
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
    else {
        return false;
    };
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn status(root: &Path, id: &str) -> Result<String> {
    let dir = run_dir(root, id)?;
    let Some(inner) = inner_run_dir(&dir) else {
        let log = std::fs::read_to_string(dir.join("concord.log")).unwrap_or_default();
        return Ok(format!("Run `{id}` is starting.\n{}", tail(&log, 10)));
    };
    if inner.join("report.md").exists() {
        return result(root, id);
    }
    let alive = is_alive(&dir);
    let events = std::fs::read_to_string(inner.join("events.jsonl")).unwrap_or_default();
    let mut per: std::collections::BTreeMap<String, [u32; 5]> = Default::default();
    let mut started_s = 0.0f64;
    let mut last_t = 0.0f64;
    for line in events.lines() {
        let Ok(e) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let ev = e.get("event").and_then(Value::as_str).unwrap_or("");
        let d = e.get("data").cloned().unwrap_or(Value::Null);
        last_t = e.get("t").and_then(Value::as_f64).unwrap_or(last_t);
        if started_s == 0.0 {
            started_s = last_t;
        }
        let agent = d.get("agent").and_then(Value::as_str).unwrap_or("").to_owned();
        if agent.is_empty() {
            continue;
        }
        let c = per.entry(agent).or_default();
        match ev {
            "footprint" => c[0] += 1,
            "deliver" => c[1] += 1,
            "stop" if d.get("decision").and_then(Value::as_str) == Some("block") => c[2] += 1,
            "merge" | "peer_merge" | "region_handoff" => c[3] += 1,
            "finished" => c[4] = 1,
            _ => {}
        }
    }
    let mut s = format!(
        "Run `{id}`: {} ({:.0}s of activity).\n\n| agent | edits seen | notices | gate blocks | merges | finished |\n|---|---|---|---|---|---|\n",
        if alive {
            "running"
        } else {
            "not running (no report: it may have failed; see concord.log)"
        },
        last_t
    );
    for (a, c) in &per {
        s.push_str(&format!(
            "| {a} | {} | {} | {} | {} | {} |\n",
            c[0],
            c[1],
            c[2],
            c[3],
            if c[4] == 1 { "yes" } else { "no" }
        ));
    }
    if !alive {
        let log = std::fs::read_to_string(dir.join("concord.log")).unwrap_or_default();
        s.push_str(&format!("\nLast log lines:\n```\n{}\n```\n", tail(&log, 15)));
    }
    Ok(s)
}

fn result(root: &Path, id: &str) -> Result<String> {
    let dir = run_dir(root, id)?;
    let inner = inner_run_dir(&dir).context("the run has not started yet")?;
    let Ok(report) = std::fs::read_to_string(inner.join("report.md")) else {
        bail!("run `{id}` has not finished yet; use concord_status");
    };
    let rep: Value = serde_json::from_str(&std::fs::read_to_string(inner.join("report.json"))?)?;
    let run_id = rep.get("run_id").and_then(Value::as_str).unwrap_or("");
    let branch = format!("concord/{run_id}/integration");
    let mrp_section = std::fs::read_to_string(inner.join("mrp.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<crate::mrp::MergeReadinessPack>(&s).ok())
        .map(|mrp| format!("\n\n---\n\n{}", render_mrp_markdown(&mrp)))
        .unwrap_or_default();
    Ok(format!(
        "{report}{mrp_section}\n\nAll agents' merged work is on branch `{branch}` (per-agent branches: `concord/{run_id}/<agent>`). \
         Review it, then merge it into your branch with `git merge {branch}`."
    ))
}

fn list_runs(root: &Path) -> Result<String> {
    let mut rows = Vec::new();
    if let Ok(rd) = std::fs::read_dir(mcp_dir(root)) {
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            let id = e.file_name().to_string_lossy().into_owned();
            let meta: Value = std::fs::read_to_string(p.join("meta.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or(Value::Null);
            let state = match inner_run_dir(&p) {
                Some(i) if i.join("report.md").exists() => "finished",
                _ if is_alive(&p) => "running",
                _ => "stopped",
            };
            rows.push(format!(
                "| {id} | {state} | {} | {} |",
                meta.get("agents")
                    .map(|a| a.as_array().map(|v| v.len()).unwrap_or(0))
                    .unwrap_or(0),
                meta.get("coordination").and_then(Value::as_str).unwrap_or("?")
            ));
        }
    }
    rows.sort();
    if rows.is_empty() {
        return Ok("No Concord runs in this repository yet.".into());
    }
    Ok(format!(
        "| run | state | agents | coordination |\n|---|---|---|---|\n{}",
        rows.join("\n")
    ))
}

fn cancel(root: &Path, id: &str) -> Result<String> {
    let dir = run_dir(root, id)?;
    let pid = std::fs::read_to_string(dir.join("pid"))?.trim().to_owned();
    let _ = Command::new("kill").args(["-TERM", &format!("-{pid}")]).status();
    Ok(format!("Sent stop signal to run `{id}`."))
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
