//! `concord mcp`: Concord as an MCP server (JSON-RPC 2.0 over stdio).
//!
//! Connect it to any MCP client (OpenCode, Claude Code, ...). The client's
//! agent can then start a team of parallel agents on the current repository,
//! follow their progress, and fetch the integrated result, without the user
//! running anything in a terminal.
//!
//! A run started with `concord_run` is a detached `concord run` process.
//! Its files live under `<repo>/.concord/mcp/<id>/`, which is excluded from
//! git through `.git/info/exclude`.

use anyhow::{Context, Result, bail};
use concord_git::Repo;
use serde_json::{Value, json};
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
            "name": "concord_run",
            "description": "Start a team of AI coding agents that work IN PARALLEL on this git repository, coordinated by \
    Concord. Each task gets its own agent and git worktree. While they work, Concord tells an agent when another agent \
    changes a function signature it depends on, and when an agent finishes it merges finished teammates' overlapping work \
    into its worktree so the conflict is resolved by the agent. At the end all branches are merged into one integration \
    branch and the test command runs on it. Returns immediately with a run id; use concord_status to follow it. \
    Write each task as a self-contained ticket (what to build, where, which API), one feature per task; tasks may touch \
    the same code. The repository must be committed (uncommitted changes are not seen by the agents).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tasks": {
                        "type": "array",
                        "description": "One entry per agent.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string", "description": "Short identifier, e.g. 'auth' (letters, digits, '-', '_')." },
                                "prompt": { "type": "string", "description": "The ticket for this agent." }
                            },
                            "required": ["name", "prompt"]
                        }
                    },
                    "test_cmd": { "type": "string", "description": "Shell command that tests the integrated result (run in the repository root), e.g. 'python -m pytest -q'." },
                    "base": { "type": "string", "description": "Revision the agents start from (default HEAD)." },
                    "model": { "type": "string", "description": "Model for the agents (default from the server configuration)." },
                    "coordination": { "type": "string", "enum": ["full", "regions", "contracts", "peer-merge", "off"], "description": "full (default): contract notices + finish gate + peer merge; regions: contract notices + region coherence (agents never receive each other's conflicts; overlapping edits are moved or handed off); off: plain parallel agents (baseline)." }
                },
                "required": ["tasks"]
            }
        },
        {
            "name": "concord_status",
            "description": "Progress of a Concord run (agents, notices, finish-gate blocks, merges), or the final report if it has finished. Without run_id, lists the runs of this repository.",
            "inputSchema": { "type": "object", "properties": { "run_id": { "type": "string" } } }
        },
        {
            "name": "concord_result",
            "description": "Final report of a finished Concord run: per-agent summary, merge conflicts, test result, and the integration branch that contains all agents' merged work (merge it with `git merge <branch>`).",
            "inputSchema": { "type": "object", "properties": { "run_id": { "type": "string" } }, "required": ["run_id"] }
        },
        {
            "name": "concord_analyze",
            "description": "Check existing branches (e.g. from parallel agents or pull requests) against a base for textual merge conflicts and for contract problems a clean merge would hide: changed or removed functions whose callers on other branches no longer match. Does not run agents.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "branches": { "type": "array", "items": { "type": "string" } },
                    "base": { "type": "string", "description": "Default: main" }
                },
                "required": ["branches"]
            }
        },
        {
            "name": "concord_cancel",
            "description": "Stop a running Concord run.",
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
                    "instructions": "Concord coordinates teams of parallel coding agents on this repository. Use concord_run to split a larger piece of work into parallel tasks, concord_status to follow it, and concord_result to get the integrated branch."
                }})
            }
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools() } }),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let (text, is_error) = match call(&cfg, name, &args) {
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

fn call(cfg: &McpConfig, name: &str, args: &Value) -> Result<String> {
    let repo = Repo::open(&std::env::current_dir()?).context("Concord needs to run inside a git repository")?;
    let root = repo.root().to_path_buf();
    match name {
        "concord_run" => start_run(cfg, &repo, &root, args),
        "concord_status" => match args.get("run_id").and_then(Value::as_str) {
            Some(id) => status(&root, id),
            None => list_runs(&root),
        },
        "concord_result" => result(&root, arg(args, "run_id")?),
        "concord_analyze" => analyze(cfg, &root, args),
        "concord_cancel" => cancel(&root, arg(args, "run_id")?),
        _ => bail!("unknown tool {name}"),
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing argument `{key}`"))
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
    let coordination = args.get("coordination").and_then(Value::as_str).unwrap_or("full");
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
        "full" => cmd_args.extend(["--mode", "enforce", "--peer-merge"].map(String::from)),
        "contracts" => cmd_args.extend(["--mode", "enforce"].map(String::from)),
        "regions" => cmd_args.extend(["--mode", "enforce", "--regions"].map(String::from)),
        "peer-merge" => cmd_args.extend(["--mode", "enforce", "--peer-merge", "--no-contracts"].map(String::from)),
        "off" => cmd_args.extend(["--mode", "off"].map(String::from)),
        other => bail!("unknown coordination {other:?}"),
    }
    if let Some(t) = args.get("test_cmd").and_then(Value::as_str) {
        cmd_args.extend(["--test-cmd".into(), t.to_owned()]);
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
            "merge" | "peer_merge" => c[3] += 1,
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
    Ok(format!(
        "{report}\n\nAll agents' merged work is on branch `{branch}` (per-agent branches: `concord/{run_id}/<agent>`). \
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

fn analyze(cfg: &McpConfig, root: &Path, args: &Value) -> Result<String> {
    let branches: Vec<String> = args
        .get("branches")
        .and_then(Value::as_array)
        .context("`branches` must be a list")?
        .iter()
        .filter_map(|b| b.as_str().map(str::to_owned))
        .collect();
    let base = args.get("base").and_then(Value::as_str).unwrap_or("main");
    let out = Command::new(&cfg.concord_exe)
        .current_dir(root)
        .arg("analyze")
        .arg("--repo")
        .arg(root)
        .args(["--base", base])
        .args(&branches)
        .arg("--notices")
        .output()
        .context("running concord analyze")?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        text.push_str(&String::from_utf8_lossy(&out.stderr));
    }
    Ok(text)
}

fn cancel(root: &Path, id: &str) -> Result<String> {
    let dir = run_dir(root, id)?;
    let pid = std::fs::read_to_string(dir.join("pid"))?.trim().to_owned();
    // The run is its own process group: stop it and the agents it started.
    let _ = Command::new("kill").args(["-TERM", &format!("-{pid}")]).status();
    Ok(format!("Sent stop signal to run `{id}`."))
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}
