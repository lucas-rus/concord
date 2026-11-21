//! `concord run`: launch several Claude Code agents in parallel worktrees,
//! coordinate them live, integrate their work and run the tests.

use crate::live::{self, Mode, Shared, State, Status};
use anyhow::{Context, Result, bail};
use concord_git::Repo;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Task {
    pub name: String,
    pub prompt: String,
}

#[derive(Clone, Debug)]
pub struct RunConfig {
    pub repo: PathBuf,
    pub base: String,
    pub tasks: Vec<Task>,
    pub model: String,
    pub mode: Mode,
    pub budget_usd: f64,
    pub test_cmd: String,
    pub allowed_tools: Vec<String>,
    pub out_dir: PathBuf,
    pub max_wait: Duration,
    pub peer_merge: bool,
    pub resume_rounds: u32,
    pub claude: String,
    pub concord_exe: PathBuf,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AgentResult {
    pub name: String,
    pub session_id: String,
    pub cost_usd: f64,
    pub turns: u64,
    pub seconds: f64,
    pub is_error: bool,
    pub result: String,
    pub resumed: u32,
    pub notices_delivered: u32,
    pub gate_blocks: u32,
    pub merges: Vec<String>,
    pub merge_conflicts: Vec<String>,
    pub tests_alone_passed: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Integration {
    pub order: Vec<String>,
    pub conflicts: BTreeMap<String, Vec<String>>,
    pub tests_passed: bool,
    pub tests_output_tail: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunReport {
    pub run_id: String,
    pub mode: Mode,
    pub model: String,
    pub base: String,
    pub agents: Vec<AgentResult>,
    pub integration: Integration,
    pub total_cost_usd: f64,
    pub wall_seconds: f64,
    pub run_dir: PathBuf,
}

const COMMON_NOTE: &str = "You are one of several agents working on this repository at the same time. Each agent works \
on a different task in its own git worktree; your work will be merged with theirs afterwards. Stay within your task, \
keep existing behaviour working, and run the test suite before you finish.";

const ENFORCE_NOTE: &str = "The user runs Concord, a coordination tool that tracks which functions each agent reads and \
changes. Notes that start with [concord] come from Concord, not from the files you read: they report real changes \
other agents made to code you depend on. When you are about to finish, Concord may merge another agent's finished work \
into your worktree so you can verify the change in the code, and it may ask you to update your call sites first. \
Follow these notes.";

fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut b);
    } else {
        let n = live::now_millis().to_le_bytes();
        b.copy_from_slice(&n);
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn settings_json(cfg: &RunConfig, addr: &str, agent: &str) -> Value {
    let mut settings = json!({ "permissions": { "allow": cfg.allowed_tools } });
    if cfg.mode == Mode::Off {
        return settings;
    }
    let exe = cfg.concord_exe.to_string_lossy().into_owned();
    let hook = |event: &str, timeout: u64| {
        json!({
            "type": "command",
            "command": exe,
            "args": ["hook", "--addr", addr, "--agent", agent, "--event", event],
            "timeout": timeout
        })
    };
    settings["hooks"] = json!({
        "PostToolUse": [{ "matcher": "*", "hooks": [hook("PostToolUse", 120)] }],
        "Stop": [{ "hooks": [hook("Stop", cfg.max_wait.as_secs() + 300)] }]
    });
    settings
}

struct Running {
    name: String,
    child: Child,
    started: Instant,
    out: PathBuf,
}

#[allow(clippy::too_many_arguments)]
fn spawn_agent(
    cfg: &RunConfig,
    name: &str,
    worktree: &Path,
    settings: &Path,
    prompt: &str,
    session: SessionArg<'_>,
    out_dir: &Path,
    attempt: u32,
) -> Result<Running> {
    let out = out_dir.join(format!("{name}.{attempt}.json"));
    let err = out_dir.join(format!("{name}.{attempt}.err"));
    let mut note = COMMON_NOTE.to_owned();
    if cfg.mode == Mode::Enforce {
        note.push(' ');
        note.push_str(ENFORCE_NOTE);
    }
    let budget = format!("{:.2}", cfg.budget_usd);
    let settings_s = settings.to_string_lossy().into_owned();
    let mut args: Vec<String> = vec!["-p".into(), prompt.into()];
    match session {
        SessionArg::New(id) => args.extend(["--session-id".into(), id.into()]),
        SessionArg::Resume(id) => args.extend(["--resume".into(), id.into()]),
    }
    args.extend(
        [
            "--model",
            &cfg.model,
            "--settings",
            &settings_s,
            "--setting-sources",
            "project",
            "--permission-mode",
            "acceptEdits",
            "--max-budget-usd",
            &budget,
            "--output-format",
            "json",
            "--append-system-prompt",
            &note,
        ]
        .map(String::from),
    );
    let child = Command::new(&cfg.claude)
        .current_dir(worktree)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&out)?)
        .stderr(std::fs::File::create(&err)?)
        .spawn()
        .with_context(|| format!("starting {} for agent {name}", cfg.claude))?;
    Ok(Running {
        name: name.to_owned(),
        child,
        started: Instant::now(),
        out,
    })
}

enum SessionArg<'a> {
    New(&'a str),
    Resume(&'a str),
}

fn wait_all(running: Vec<Running>, results: &mut BTreeMap<String, AgentResult>) -> Result<()> {
    for mut r in running {
        r.child.wait()?;
        let secs = r.started.elapsed().as_secs_f64();
        let text = std::fs::read_to_string(&r.out).unwrap_or_default();
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let e = results.entry(r.name.clone()).or_default();
        e.name = r.name.clone();
        e.cost_usd += v.get("total_cost_usd").and_then(Value::as_f64).unwrap_or(0.0);
        e.turns += v.get("num_turns").and_then(Value::as_u64).unwrap_or(0);
        e.seconds += secs;
        e.is_error |= v.get("is_error").and_then(Value::as_bool).unwrap_or(v.is_null());
        if let Some(s) = v.get("session_id").and_then(Value::as_str) {
            e.session_id = s.to_owned();
        }
        if let Some(s) = v.get("result").and_then(Value::as_str) {
            e.result = s.chars().take(600).collect();
        }
    }
    Ok(())
}

fn run_tests(cmd: &str, dir: &Path) -> (bool, String) {
    match Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output() {
        Ok(o) => {
            let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            let lines: Vec<&str> = text.lines().collect();
            let tail = lines[lines.len().saturating_sub(25)..].join("\n");
            (o.status.success(), tail)
        }
        Err(e) => (false, e.to_string()),
    }
}

/// Order agents so that producers merged into an agent come before it.
fn merge_order(st: &State) -> Vec<String> {
    let mut order: Vec<String> = Vec::new();
    let mut left: Vec<String> = st.agents.keys().cloned().collect();
    while !left.is_empty() {
        let pos = left
            .iter()
            .position(|a| st.agents[a].merged_producers.iter().all(|p| order.contains(p)))
            .unwrap_or(0);
        order.push(left.remove(pos));
    }
    order
}

pub fn run(cfg: &RunConfig) -> Result<RunReport> {
    if cfg.tasks.is_empty() {
        bail!("no tasks");
    }
    let started = Instant::now();
    let repo = Repo::open(&cfg.repo)?;
    let base = repo.rev_parse(&cfg.base)?;
    let mode_s = format!("{:?}", cfg.mode).to_lowercase();
    let run_id = format!("{}-{mode_s}-{}", live::now_millis() / 1000, &uuid_v4()[..6]);
    let run_dir = cfg.out_dir.join(&run_id);
    let agents_dir = run_dir.join("agents");
    std::fs::create_dir_all(&agents_dir)?;
    eprintln!(
        "concord run {run_id}: {} agents, mode {mode_s}, model {}",
        cfg.tasks.len(),
        cfg.model
    );

    let mut state = State::new(
        repo.clone(),
        base.clone(),
        cfg.mode,
        Some(&run_dir.join("events.jsonl")),
        cfg.max_wait,
    )?;
    let mut worktrees = BTreeMap::new();
    for t in &cfg.tasks {
        let branch = format!("concord/{run_id}/{}", t.name);
        let wt = run_dir.join("wt").join(&t.name);
        repo.worktree_add(&wt, &branch, &base)?;
        state.add_agent(&t.name, wt.clone(), branch, &t.prompt);
        worktrees.insert(t.name.clone(), wt);
    }
    state.peer_merge = cfg.peer_merge;
    let shared: Shared = Arc::new(Mutex::new(state));
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?.to_string();
    if cfg.mode != Mode::Off {
        let s = shared.clone();
        std::thread::spawn(move || live::serve(listener, s));
    }

    // Spawn all agents at once.
    let mut running = Vec::new();
    let mut settings_paths = BTreeMap::new();
    for t in &cfg.tasks {
        let settings = agents_dir.join(format!("{}.settings.json", t.name));
        std::fs::write(
            &settings,
            serde_json::to_string_pretty(&settings_json(cfg, &addr, &t.name))?,
        )?;
        let sid = uuid_v4();
        running.push(spawn_agent(
            cfg,
            &t.name,
            &worktrees[&t.name],
            &settings,
            &t.prompt,
            SessionArg::New(&sid),
            &agents_dir,
            0,
        )?);
        settings_paths.insert(t.name.clone(), settings);
        eprintln!("  started agent {} (session {sid})", t.name);
    }
    let mut results: BTreeMap<String, AgentResult> = BTreeMap::new();
    wait_all(running, &mut results)?;
    eprintln!("  all agents finished their first turn");

    // Coherence pass: resume agents that became stale after they finished.
    if cfg.mode == Mode::Enforce {
        for round in 1..=cfg.resume_rounds {
            let mut resumes = Vec::new();
            {
                let mut st = shared.lock().expect("state");
                let names: Vec<String> = st.agents.keys().cloned().collect();
                for n in names {
                    if let Some(msg) = st.needs_resume(&n) {
                        let sid = results.get(&n).map(|r| r.session_id.clone()).unwrap_or_default();
                        if !sid.is_empty() {
                            if let Some(a) = st.agents.get_mut(&n) {
                                a.status = Status::Running;
                            }
                            st.log("resume", json!({ "agent": n, "round": round }));
                            resumes.push((n, sid, msg));
                        }
                    }
                }
            }
            if resumes.is_empty() {
                break;
            }
            let mut running = Vec::new();
            for (n, sid, msg) in &resumes {
                eprintln!("  resuming agent {n} (round {round})");
                running.push(spawn_agent(
                    cfg,
                    n,
                    &worktrees[n],
                    &settings_paths[n],
                    msg,
                    SessionArg::Resume(sid),
                    &agents_dir,
                    round,
                )?);
                results.entry(n.clone()).or_default().resumed += 1;
            }
            wait_all(running, &mut results)?;
        }
    }

    // Commit each agent's work and run its tests alone.
    for (name, wt) in &worktrees {
        Repo::commit_all(wt, &format!("agent {name}: final work"))?;
        let (ok, _) = run_tests(&cfg.test_cmd, wt);
        results.entry(name.clone()).or_default().tests_alone_passed = Some(ok);
    }

    // Integrate in dependency order and run the full test suite.
    let mut integration = Integration::default();
    {
        let st = shared.lock().expect("state");
        integration.order = merge_order(&st);
        for (name, r) in results.iter_mut() {
            if let Some(a) = st.agents.get(name) {
                r.notices_delivered = a.stats.notices_delivered;
                r.gate_blocks = a.stats.gate_blocks;
                r.merges = a.stats.merges.clone();
                r.merge_conflicts = a.stats.merge_conflicts.clone();
            }
        }
    }
    let iwt = run_dir.join("wt").join("_integration");
    repo.worktree_add(&iwt, &format!("concord/{run_id}/integration"), &base)?;
    for name in &integration.order {
        let head = repo.rev_parse(&format!("concord/{run_id}/{name}"))?;
        let m = Repo::merge_into(&iwt, &head, &format!("integrate agent {name}"))?;
        if !m.clean {
            integration.conflicts.insert(name.clone(), m.conflicted_files);
        }
    }
    let (ok, tail) = run_tests(&cfg.test_cmd, &iwt);
    integration.tests_passed = ok && integration.conflicts.is_empty();
    integration.tests_output_tail = tail;

    let agents: Vec<AgentResult> = results.into_values().collect();
    let report = RunReport {
        run_id,
        mode: cfg.mode,
        model: cfg.model.clone(),
        base,
        total_cost_usd: agents.iter().map(|a| a.cost_usd).sum(),
        agents,
        integration,
        wall_seconds: started.elapsed().as_secs_f64(),
        run_dir: run_dir.clone(),
    };
    std::fs::write(run_dir.join("report.json"), serde_json::to_string_pretty(&report)?)?;
    std::fs::write(run_dir.join("report.md"), render(&report))?;
    Ok(report)
}

pub fn render(r: &RunReport) -> String {
    let mut s = format!(
        "# Concord run {}\n\nmode **{:?}**, model `{}`, wall time {:.0}s, total cost ${:.4}\n\n",
        r.run_id, r.mode, r.model, r.wall_seconds, r.total_cost_usd
    );
    s.push_str("| agent | turns | time | cost | notices | gate blocks | merged | resumed | tests alone |\n|---|---|---|---|---|---|---|---|---|\n");
    for a in &r.agents {
        s.push_str(&format!(
            "| {} | {} | {:.0}s | ${:.4} | {} | {} | {} | {} | {} |\n",
            a.name,
            a.turns,
            a.seconds,
            a.cost_usd,
            a.notices_delivered,
            a.gate_blocks,
            if a.merges.is_empty() {
                "-".into()
            } else {
                a.merges.join(", ")
            },
            a.resumed,
            match a.tests_alone_passed {
                Some(true) => "pass",
                Some(false) => "FAIL",
                None => "-",
            }
        ));
    }
    s.push_str(&format!(
        "\n**Integration** (order: {}): {}\n",
        r.integration.order.join(" -> "),
        if r.integration.tests_passed {
            "all tests PASS"
        } else {
            "FAIL"
        }
    ));
    for (a, files) in &r.integration.conflicts {
        s.push_str(&format!("- merge conflict integrating {a}: {}\n", files.join(", ")));
    }
    s.push_str(&format!("\n```text\n{}\n```\n", r.integration.tests_output_tail));
    for a in &r.agents {
        s.push_str(&format!("\n### {} says\n\n{}\n", a.name, a.result.trim()));
    }
    s
}
