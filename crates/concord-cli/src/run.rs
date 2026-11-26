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
    /// Acceptance check of the ticket (a shell command run in the agent's
    /// worktree), e.g. the tests of its own feature. The finish gate sends the
    /// agent back while it fails and the agent is still making progress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

/// Files handed to the structured merge driver (languages Mergiraf supports
/// that occur in the benchmarks).
const STRUCTURED_MERGE_PATTERNS: &[&str] = &[
    "*.py", "*.js", "*.ts", "*.tsx", "*.rs", "*.go", "*.java", "*.json", "*.toml", "*.yaml", "*.yml",
];

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
    /// Contract layer on (default). Off with peer_merge = peer-merge-only ablation.
    pub contracts: bool,
    /// Region coherence (see `live::State::regions`).
    pub regions: bool,
    /// Baseline B3: exclusive whole-file leases.
    pub file_leases: bool,
    /// Baseline B4: Clash/Grove passive textual overlap warnings.
    pub overlap_warnings: bool,
    /// Ablation A1: treat every body modification as a contract change.
    pub all_contracts: bool,
    /// Ablation A2 (`false` = `--no-notices`): suppress live PostToolUse notices.
    pub deliver_notices: bool,
    /// Ablation A4 (`false` = `--no-gate`): skip finish-gate blocking on Stop.
    pub enforce_gate: bool,
    /// Debounce window before publishing contract changes.
    pub publish_debounce: Duration,
    /// Optional interval (seconds) for continuous background speculative integration.
    pub speculative_secs: Option<u64>,
    /// Fails-together check at the finish gate (see `together`), with `test_cmd`.
    pub fails_together: bool,
    /// Structured merge driver executable (Mergiraf-compatible CLI).
    pub merge_driver: Option<String>,
    pub resume_rounds: u32,
    pub claude: String,
    /// Which coding agent runs the tasks.
    pub agent_cli: AgentCli,
    /// OpenCode executable (for `AgentCli::Opencode`).
    pub opencode: String,
    pub concord_exe: PathBuf,
}

/// The coding agent that `concord run` drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum AgentCli {
    /// Claude Code (`claude -p`), coordinated through its hooks.
    Claude,
    /// OpenCode (`opencode run`), coordinated through a plugin plus a
    /// controller loop that implements the finish gate.
    Opencode,
}

const OPENCODE_PLUGIN: &str = include_str!("opencode_plugin.js");
/// A turn producing no output for this long is considered hung.
/// Time allowed for a ticket's acceptance check (seconds).
pub const CHECK_TIMEOUT_SECS: u64 = 300;

/// Interval of the run's heartbeat file (read by `concord watch`).
pub const HEARTBEAT: Duration = Duration::from_secs(5);

const OPENCODE_IDLE: Duration = Duration::from_secs(300);
/// How often a turn is retried when OpenCode's server fails to start.
const OPENCODE_START_RETRIES: u64 = 4;
const OPENCODE_START_FAILURE: &str = "exited before reporting readiness";
/// Upper bound on turns (first prompt plus gate continuations) per agent.
const OPENCODE_MAX_TURNS: u32 = 10;

/// Resumes of a session after its turn ended on a network error.
const OPENCODE_TRANSPORT_RETRIES: u32 = 5;
/// Repair rounds after integration, and resumes per ticket across them.
const POST_MERGE_ROUNDS: u32 = 3;
const POST_MERGE_ATTEMPTS: u32 = 2;

const TRANSPORT_CONTINUE: &str = "The connection to the model provider was lost in the middle of your last step. \
Nothing you did was lost. Continue the task exactly where you left off.";

/// Markers of transport failures in an OpenCode error event.
const TRANSPORT_MARKERS: &[&str] = &[
    "provider.transport",
    "enotfound",
    "econnreset",
    "etimedout",
    "econnrefused",
    "socket",
    "fetch failed",
    "network",
];

/// Whether a turn's event stream ended on a transport error.
fn ended_on_transport_error(log: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(log) else {
        return false;
    };
    let Some(last) = text.lines().rev().find_map(|l| serde_json::from_str::<Value>(l).ok()) else {
        return false;
    };
    last.get("type").and_then(Value::as_str) == Some("error")
        && TRANSPORT_MARKERS
            .iter()
            .any(|m| last.to_string().to_lowercase().contains(m))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
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
    /// Times the acceptance check sent the agent back.
    #[serde(default)]
    pub check_blocks: u32,
    pub merges: Vec<String>,
    pub merge_conflicts: Vec<String>,
    pub tests_alone_passed: Option<bool>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Integration {
    pub order: Vec<String>,
    pub conflicts: BTreeMap<String, Vec<String>>,
    /// Branches integrated with a union merge of insert/insert collisions
    /// (region coherence), with the files.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unions: BTreeMap<String, Vec<String>>,
    pub tests_passed: bool,
    pub tests_output_tail: String,
    /// When `tests_passed` is false despite clean git merges, binary bisection
    /// identifies the first branch whose integration broke `test_cmd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bisected_culprit: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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
Follow these notes. Concord's finish gate does all waiting for other agents itself: never sleep, poll, or schedule \
wake-ups to wait for another agent; just finish when your work is done.";

/// Tools that let an agent wait or loop outside the finish gate (they turned
/// a gate block into 20-minute self-scheduled polling loops).
const DENIED_TOOLS: &[&str] = &["ScheduleWakeup", "CronCreate", "CronDelete", "Monitor", "RemoteTrigger"];

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

fn settings_json(cfg: &RunConfig, addr: &str, agent: &str, check: bool) -> Value {
    let mut settings = json!({ "permissions": { "allow": cfg.allowed_tools, "deny": DENIED_TOOLS } });
    if cfg.mode == Mode::Off && !check {
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
    // Without coordination only the finish gate is installed, for the
    // ticket's acceptance check.
    settings["hooks"] = if cfg.mode == Mode::Off {
        json!({ "Stop": [{ "hooks": [hook("Stop", CHECK_TIMEOUT_SECS)] }] })
    } else {
        json!({
            "PostToolUse": [{ "matcher": "*", "hooks": [hook("PostToolUse", 120)] }],
            "Stop": [{ "hooks": [hook("Stop", cfg.max_wait.as_secs() + CHECK_TIMEOUT_SECS)] }]
        })
    };
    settings
}

struct Running {
    name: String,
    proc: Proc,
    started: Instant,
    out: PathBuf,
}

/// An agent is either a child process (Claude Code) or a controller thread
/// driving OpenCode turn by turn.
enum Proc {
    Child(Child),
    Thread(std::thread::JoinHandle<()>),
}

impl Proc {
    fn finished(&mut self) -> Result<bool> {
        Ok(match self {
            Proc::Child(c) => c.try_wait()?.is_some(),
            Proc::Thread(h) => h.is_finished(),
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_agent(
    cfg: &RunConfig,
    agent: AgentCtx<'_>,
    prompt: &str,
    session: SessionArg<'_>,
    out_dir: &Path,
    attempt: u32,
) -> Result<Running> {
    let AgentCtx {
        name,
        worktree,
        settings,
        shared,
        addr,
    } = agent;
    if cfg.agent_cli == AgentCli::Opencode {
        return spawn_opencode(cfg, agent, prompt, session, out_dir, attempt);
    }
    let _ = shared;
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
        .env("PWD", worktree)
        .env("CONCORD_DAEMON_ADDR", addr)
        .env("CONCORD_AGENT", name)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&out)?)
        .stderr(std::fs::File::create(&err)?)
        .spawn()
        .with_context(|| format!("starting {} for agent {name}", cfg.claude))?;
    // For `concord watch`: an agent thinking is alive; a vanished one crashed.
    let _ = std::fs::write(out.with_file_name(format!("{name}.pid")), child.id().to_string());
    Ok(Running {
        name: name.to_owned(),
        proc: Proc::Child(child),
        started: Instant::now(),
        out,
    })
}

/// Everything about one agent that spawning it needs.
#[derive(Clone, Copy)]
struct AgentCtx<'a> {
    name: &'a str,
    worktree: &'a Path,
    settings: &'a Path,
    shared: &'a Shared,
    addr: &'a str,
}

/// Start an OpenCode agent: a controller thread runs `opencode run` and, each
/// time the agent stops, asks Concord's finish gate (the equivalent of Claude
/// Code's Stop hook). A block continues the session with the gate's reason.
fn spawn_opencode(
    cfg: &RunConfig,
    agent: AgentCtx<'_>,
    prompt: &str,
    session: SessionArg<'_>,
    out_dir: &Path,
    attempt: u32,
) -> Result<Running> {
    let name = agent.name.to_owned();
    let out = out_dir.join(format!("{name}.{attempt}.json"));
    // Per-agent OpenCode config directory with the plugin and its settings.
    let cfg_dir = out_dir.join(format!("{name}.opencode"));
    std::fs::create_dir_all(cfg_dir.join("plugin"))?;
    let plugin = cfg_dir.join("plugin").join("concord.js");
    if std::fs::read_to_string(&plugin).ok().as_deref() != Some(OPENCODE_PLUGIN) {
        std::fs::write(&plugin, OPENCODE_PLUGIN)?;
    }
    std::fs::write(
        cfg_dir.join("concord.json"),
        serde_json::to_string_pretty(&json!({
            "enabled": cfg.mode != Mode::Off,
            "exe": cfg.concord_exe,
            "addr": agent.addr,
            "agent": name,
            "worktree": agent.worktree,
        }))?,
    )?;
    let first = match session {
        SessionArg::New(_) => {
            let mut note = COMMON_NOTE.to_owned();
            if cfg.mode == Mode::Enforce {
                note.push(' ');
                note.push_str(ENFORCE_NOTE);
            }
            format!("{note}\n\n{prompt}")
        }
        SessionArg::Resume(_) => prompt.to_owned(),
    };
    let resume = match session {
        SessionArg::Resume(id) => Some(id.to_owned()),
        SessionArg::New(_) => None,
    };
    let job = OpencodeJob {
        exe: cfg.opencode.clone(),
        model: cfg.model.clone(),
        // OpenCode has no hooks: the controller asks the gate after each turn
        // (in off mode it only runs the ticket's acceptance check, if any).
        gate: true,
        cfg_dir,
        worktree: agent.worktree.to_path_buf(),
        shared: agent.shared.clone(),
        addr: agent.addr.to_owned(),
        name: name.clone(),
        out: out.clone(),
        log_prefix: out_dir.join(format!("{name}.{attempt}")),
    };
    let handle = std::thread::spawn(move || job.run(first, resume));
    Ok(Running {
        name,
        proc: Proc::Thread(handle),
        started: Instant::now(),
        out,
    })
}

struct OpencodeJob {
    exe: String,
    model: String,
    gate: bool,
    cfg_dir: PathBuf,
    worktree: PathBuf,
    shared: Shared,
    addr: String,
    name: String,
    out: PathBuf,
    log_prefix: PathBuf,
}

impl OpencodeJob {
    fn run(self, first: String, mut session: Option<String>) {
        let mut message = first;
        let mut turns = 0u32;
        let mut cost = 0.0f64;
        let mut last_text = String::new();
        let mut error = false;
        let mut transport_retries = 0u32;
        while turns < OPENCODE_MAX_TURNS + transport_retries {
            turns += 1;
            let (sid, text, c, ok) = self.turn(turns, &message, session.as_deref());
            cost += c;
            if sid.is_some() {
                session = sid;
            }
            if !text.is_empty() {
                last_text = text;
            }
            // A dropped connection to the model provider (DNS, reset,
            // timeout) is not the agent's doing: wait and resume the same
            // session instead of ending the agent.
            if !ok
                && session.is_some()
                && transport_retries < OPENCODE_TRANSPORT_RETRIES
                && ended_on_transport_error(&PathBuf::from(format!(
                    "{}.turn{turns}.jsonl",
                    self.log_prefix.display()
                )))
            {
                transport_retries += 1;
                if let Ok(mut st) = self.shared.lock() {
                    st.log(
                        "transport_retry",
                        json!({ "agent": self.name, "attempt": transport_retries }),
                    );
                }
                std::thread::sleep(Duration::from_secs(20 * u64::from(transport_retries)));
                message = TRANSPORT_CONTINUE.to_owned();
                continue;
            }
            // The last turn decides: an agent that recovered from a failed
            // turn (and went on to finish) did not fail.
            error = !ok;
            if !self.gate {
                break;
            }
            // The agent stopped: ask the finish gate, as Claude Code's Stop hook would.
            let input = json!({ "session_id": session.clone().unwrap_or_default(), "hook_event_name": "Stop" });
            let (stdout, _) = live::handle(&self.shared, &self.name, "Stop", &input);
            let decision: Value = serde_json::from_str(&stdout).unwrap_or(Value::Null);
            match decision.get("decision").and_then(Value::as_str) {
                Some("block") if session.is_some() => {
                    message = decision.get("reason").and_then(Value::as_str).unwrap_or("").to_owned();
                }
                _ => break,
            }
        }
        let summary = json!({
            "session_id": session.unwrap_or_default(),
            "total_cost_usd": cost,
            "num_turns": turns,
            "result": last_text,
            "is_error": error,
        });
        let _ = std::fs::write(&self.out, summary.to_string());
    }

    /// One turn. Retries when OpenCode's server fails to start, which
    /// happens now and then when many agents start at the same moment.
    fn turn(&self, n: u32, message: &str, session: Option<&str>) -> (Option<String>, String, f64, bool) {
        let mut attempt = 0u64;
        loop {
            let (r, startup_failed) = self.turn_once(n, message, session);
            if !startup_failed || attempt >= OPENCODE_START_RETRIES {
                return r;
            }
            attempt += 1;
            // Back off, spread over agents so they do not collide again.
            let spread = self.name.bytes().map(u64::from).sum::<u64>() % 1000;
            std::thread::sleep(Duration::from_millis(2000 * attempt + spread));
        }
    }

    /// One `opencode run` invocation. Returns ((session id, final text, cost,
    /// ok), whether OpenCode's server failed to start).
    fn turn_once(&self, n: u32, message: &str, session: Option<&str>) -> ((Option<String>, String, f64, bool), bool) {
        let log = PathBuf::from(format!("{}.turn{n}.jsonl", self.log_prefix.display()));
        let err = PathBuf::from(format!("{}.turn{n}.err", self.log_prefix.display()));
        let (Ok(out_f), Ok(err_f)) = (std::fs::File::create(&log), std::fs::File::create(&err)) else {
            return ((None, String::new(), 0.0, false), false);
        };
        let mut cmd = Command::new(&self.exe);
        cmd.current_dir(&self.worktree)
            // OpenCode takes its project directory from $PWD, which
            // current_dir() does not update: without this the agent works in
            // the directory `concord run` was started from.
            .env("PWD", &self.worktree)
            .env("OPENCODE_CONFIG_DIR", &self.cfg_dir)
            .env("CONCORD_DAEMON_ADDR", &self.addr)
            .env("CONCORD_AGENT", &self.name)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .args(["run", "--standalone", "--auto", "--format", "json", "-m", &self.model]);
        if let Some(s) = session {
            cmd.args(["--session", s]);
        }
        cmd.arg(message).stdin(Stdio::null()).stdout(out_f).stderr(err_f);
        let Ok(mut child) = cmd.spawn() else {
            return ((None, String::new(), 0.0, false), false);
        };
        let _ = std::fs::write(
            PathBuf::from(format!("{}.pid", self.log_prefix.display())),
            child.id().to_string(),
        );
        // Watchdog: kill a turn that produces no output for OPENCODE_IDLE.
        let mut last_len = 0u64;
        let mut last_change = Instant::now();
        let ok = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status.success(),
                Ok(None) => {}
                Err(_) => break false,
            }
            let len = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
            if len != last_len {
                last_len = len;
                last_change = Instant::now();
            } else if last_change.elapsed() > OPENCODE_IDLE {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
            std::thread::sleep(Duration::from_millis(500));
        };
        let events = std::fs::read_to_string(&log).unwrap_or_default();
        let startup_failed = !ok && events.contains(OPENCODE_START_FAILURE);
        let (sid, text, cost) = parse_opencode_events(&events);
        (
            (sid.or_else(|| session.map(str::to_owned)), text, cost, ok),
            startup_failed,
        )
    }
}

/// Extract the session id, the agent's last text and the total cost from
/// `opencode run --format json` output (one JSON event per line).
fn parse_opencode_events(jsonl: &str) -> (Option<String>, String, f64) {
    let mut sid = None;
    let mut text = String::new();
    let mut cost = 0.0;
    for line in jsonl.lines() {
        let Ok(e) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let part = e.get("part").unwrap_or(&Value::Null);
        if let Some(s) = e
            .get("sessionID")
            .or_else(|| part.get("sessionID"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            sid = Some(s.to_owned());
        }
        if e.get("type").and_then(Value::as_str) == Some("text")
            && let Some(t) = part.get("text").and_then(Value::as_str)
        {
            text = t.to_owned();
        }
        if let Some(c) = part.get("cost").and_then(Value::as_f64) {
            cost += c;
        }
    }
    (sid, text, cost)
}

enum SessionArg<'a> {
    New(&'a str),
    Resume(&'a str),
}

fn wait_all(running: Vec<Running>, results: &mut BTreeMap<String, AgentResult>, shared: &Shared) -> Result<()> {
    // Poll all agents so each one's time is its own, not the time until the
    // agents before it in the list have finished.
    let mut pending = running;
    let mut done: Vec<(Running, f64)> = Vec::new();
    while !pending.is_empty() {
        let mut still = Vec::new();
        for mut r in pending {
            if r.proc.finished()? {
                let secs = r.started.elapsed().as_secs_f64();
                // Report the exit at once, so a crashed or failed agent is
                // visible while the others are still working.
                let v: Value = std::fs::read_to_string(&r.out)
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or(Value::Null);
                let error = v.get("is_error").and_then(Value::as_bool).unwrap_or(v.is_null());
                let result: String = v
                    .get("result")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .chars()
                    .take(200)
                    .collect();
                if let Ok(mut st) = shared.lock() {
                    st.log(
                        "agent_exit",
                        json!({ "agent": r.name, "error": error, "seconds": secs.round(), "result": result }),
                    );
                }
                done.push((r, secs));
            } else {
                still.push(r);
            }
        }
        pending = still;
        if !pending.is_empty() {
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }
    for (r, secs) in done {
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

struct FailingTaskCheck {
    name: String,
    cmd: String,
    ids: Vec<String>,
    output: String,
}

/// Evaluate every task's `check` command on `dir`, returning `(unique_failure_count,
/// failing_tasks)`.
fn evaluate_task_checks(tasks: &[Task], dir: &Path) -> (usize, Vec<FailingTaskCheck>) {
    let mut all_unique: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut failing_tasks = Vec::new();
    for t in tasks {
        let Some(cmd) = &t.check else { continue };
        let Ok(o) = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).output() else {
            continue;
        };
        if o.status.success() {
            continue;
        }
        let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&o.stderr));
        let ids: Vec<String> = crate::together::failing_tests(&text).into_iter().collect();
        if ids.is_empty() {
            all_unique.insert(format!("__task_failed__:{}", t.name));
        } else {
            all_unique.extend(ids.iter().cloned());
        }
        failing_tasks.push(FailingTaskCheck {
            name: t.name.clone(),
            cmd: cmd.clone(),
            ids,
            output: text,
        });
    }
    (all_unique.len(), failing_tasks)
}

/// Order agents so that producers merged into an agent (and contract producers
/// whose symbols an agent depends on) come before it, breaking ties by task rank.
pub(crate) fn merge_order(st: &State) -> Vec<String> {
    let mut deps: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
    for (name, a) in &st.agents {
        let mut d = a.merged_producers.clone();
        if let Some(ix) = &a.index {
            for s in ix.symbols() {
                if let Some(p) = st.dir.published_by(&s.id)
                    && p.0 != *name
                    && !st.dir.uses_of(&concord_core::AgentId::new(name), &s.id).is_empty()
                {
                    d.insert(p.0.clone());
                }
            }
        }
        deps.insert(name.clone(), d);
    }
    let mut order: Vec<String> = Vec::new();
    let mut left: Vec<String> = st.agents.keys().cloned().collect();
    left.sort_by_key(|n| st.agents.get(n).map(|a| a.rank).unwrap_or(usize::MAX));
    while !left.is_empty() {
        let pos = left
            .iter()
            .position(|a| deps[a].iter().all(|p| order.contains(p) || !st.agents.contains_key(p)))
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
    let has_real_test_cmd = !cfg.test_cmd.trim().is_empty() && cfg.test_cmd.trim() != "true";
    let fallback_check =
        (cfg.mode == Mode::Enforce && cfg.contracts && has_real_test_cmd).then(|| cfg.test_cmd.clone());
    let mut worktrees = BTreeMap::new();
    for t in &cfg.tasks {
        let branch = format!("concord/{run_id}/{}", t.name);
        let wt = run_dir.join("wt").join(&t.name);
        repo.worktree_add(&wt, &branch, &base)?;
        state.add_agent(&t.name, wt.clone(), branch, &t.prompt);
        if let Some(a) = state.agents.get_mut(&t.name) {
            a.check = t.check.clone().or_else(|| fallback_check.clone());
        }
        worktrees.insert(t.name.clone(), wt);
    }
    state.peer_merge = cfg.peer_merge;
    state.contracts = cfg.contracts;
    state.regions = cfg.regions;
    state.file_leases = cfg.file_leases;
    state.overlap_warnings = cfg.overlap_warnings;
    state.all_contracts = cfg.all_contracts;
    state.deliver_notices = cfg.deliver_notices;
    state.enforce_gate = cfg.enforce_gate;
    state.publish_debounce = cfg.publish_debounce;
    state.together = cfg.fails_together.then(|| cfg.test_cmd.clone());
    if let Some(cmd) = &cfg.merge_driver {
        let driver = format!("{cmd} merge --git %O %A %B -s %S -x %X -y %Y -p %P -l %L");
        repo.set_merge_driver("concord-structured", &driver, STRUCTURED_MERGE_PATTERNS)?;
    }
    let shared: Shared = Arc::new(Mutex::new(state));
    // Heartbeat: `concord watch` tells a live run from a dead one within
    // seconds, however long the agents think between events.
    let beating = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let heartbeat = run_dir.join("heartbeat");
    {
        let (beating, heartbeat) = (beating.clone(), heartbeat.clone());
        std::thread::spawn(move || {
            while beating.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = std::fs::write(&heartbeat, std::process::id().to_string());
                std::thread::sleep(HEARTBEAT);
            }
        });
    }
    // Optional continuous background speculative integration loop (`Alg. 2`).
    if let Some(secs) = cfg.speculative_secs.filter(|&s| s > 0) {
        let (beating_s, shared_s, repo_s, base_s) = (beating.clone(), shared.clone(), repo.clone(), base.clone());
        std::thread::spawn(move || {
            while beating_s.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(secs));
                if !beating_s.load(std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                let snaps: Vec<(String, String)> = {
                    let Ok(st) = shared_s.lock() else { continue };
                    merge_order(&st)
                        .into_iter()
                        .filter_map(|n| st.agents.get(&n).and_then(|a| a.snapshot.clone().map(|s| (n, s))))
                        .collect()
                };
                if snaps.len() < 2 {
                    continue;
                }
                let mut cur = base_s.clone();
                let mut unmergeable = Vec::new();
                for (name, snap) in &snaps {
                    match repo_s.virtual_merge_union(&cur, snap, &crate::regions::verify_union) {
                        Ok(Some(m)) => cur = m,
                        _ => unmergeable.push(name.clone()),
                    }
                }
                let (parse_errs, contract_viols) = crate::mrp::verify_merged_contracts(&repo_s, &base_s, &cur);
                if let Ok(mut st) = shared_s.lock() {
                    st.log(
                        "speculative_check",
                        json!({
                            "agents": snaps.len(),
                            "unmergeable": unmergeable,
                            "parse_errors": parse_errs.len(),
                            "contract_violations": contract_viols.len(),
                        }),
                    );
                }
            }
        });
    }
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?.to_string();
    let _ = std::fs::write(run_dir.join("daemon.addr"), &addr);
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
            serde_json::to_string_pretty(&settings_json(
                cfg,
                &addr,
                &t.name,
                t.check.is_some() || fallback_check.is_some(),
            ))?,
        )?;
        let sid = uuid_v4();
        running.push(spawn_agent(
            cfg,
            AgentCtx {
                name: &t.name,
                worktree: &worktrees[&t.name],
                settings: &settings,
                shared: &shared,
                addr: &addr,
            },
            &t.prompt,
            SessionArg::New(&sid),
            &agents_dir,
            0,
        )?);
        settings_paths.insert(t.name.clone(), settings);
        eprintln!("  started agent {} (session {sid})", t.name);
    }
    let mut results: BTreeMap<String, AgentResult> = BTreeMap::new();
    wait_all(running, &mut results, &shared)?;
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
                    AgentCtx {
                        name: n,
                        worktree: &worktrees[n],
                        settings: &settings_paths[n],
                        shared: &shared,
                        addr: &addr,
                    },
                    msg,
                    SessionArg::Resume(sid),
                    &agents_dir,
                    round,
                )?);
                results.entry(n.clone()).or_default().resumed += 1;
            }
            wait_all(running, &mut results, &shared)?;
        }
    }

    // Commit each agent's work (only if dirty).
    for (name, wt) in &worktrees {
        Repo::commit_if_dirty(wt, &format!("agent {name}: final work"))?;
    }

    // Integrate in dependency order with verified union merge and batch-and-bisect.
    let order = {
        let st = shared.lock().expect("state");
        merge_order(&st)
    };
    let mut integration = Integration {
        order,
        ..Default::default()
    };
    let iwt = run_dir.join("wt").join("_integration");
    repo.worktree_add(&iwt, &format!("concord/{run_id}/integration"), &base)?;
    let allow_union = cfg.regions || (cfg.mode == Mode::Enforce && cfg.contracts);
    let mut prefix_commits: Vec<(String, String)> = Vec::new();
    for name in &integration.order {
        let head = repo.rev_parse(&format!("concord/{run_id}/{name}"))?;
        let msg = format!("integrate agent {name}");
        let mut m = Repo::merge_into(&iwt, &head, &msg)?;
        if !m.clean && allow_union {
            // Validated two-sided insertion union: both insertions kept whole,
            // earlier agents first, AST-verified for syntax and zero duplicate defs.
            let files = m.conflicted_files.clone();
            let u = Repo::merge_into_union(&iwt, &head, &msg, &files, false, &crate::regions::verify_union)?;
            if u.clean {
                integration.unions.insert(name.clone(), files);
                m = u;
            }
        }
        if m.clean {
            if let Ok(iwt_repo) = Repo::open(&iwt)
                && let Ok(sha) = iwt_repo.rev_parse("HEAD")
            {
                prefix_commits.push((name.clone(), sha));
            }
        } else {
            integration.conflicts.insert(name.clone(), m.conflicted_files);
        }
    }

    // Post-integration acceptance verification pass (enforce mode): once all
    // parallel features are merged together, cross-ticket dependencies are
    // satisfied. Works both with explicit per-ticket `t.check` commands AND
    // zero-config with a single project-wide `--test-cmd` (using O(log k)
    // prefix bisection to target the culprit agent), guarded by a strict
    // monotonic test-improvement ratchet.
    let explicit_checks = cfg.tasks.iter().any(|t| t.check.is_some());
    if cfg.mode == Mode::Enforce
        && cfg.contracts
        && cfg.enforce_gate
        && cfg.resume_rounds > 0
        && integration.conflicts.is_empty()
        && (explicit_checks || has_real_test_cmd)
    {
        let check_targets: Vec<Task> = if explicit_checks {
            cfg.tasks.clone()
        } else {
            let probe = vec![Task {
                name: prefix_commits
                    .last()
                    .map(|(n, _)| n.clone())
                    .unwrap_or_else(|| cfg.tasks[0].name.clone()),
                prompt: String::new(),
                check: Some(cfg.test_cmd.clone()),
            }];
            let (probe_fails, _) = evaluate_task_checks(&probe, &iwt);
            if probe_fails == 0 {
                Vec::new()
            } else {
                let culprit = crate::mrp::bisect_prefix_failure(&repo, &prefix_commits, &cfg.test_cmd)
                    .or_else(|| prefix_commits.last().map(|(n, _)| n.clone()))
                    .unwrap_or_else(|| cfg.tasks[0].name.clone());
                vec![Task {
                    name: culprit,
                    prompt: String::new(),
                    check: Some(cfg.test_cmd.clone()),
                }]
            }
        };
        // Repair rounds: every ticket whose check still fails on the integrated
        // tree is resumed at once, in its own worktree brought up to the
        // integrated head. The repairs are merged back one by one and each is
        // kept only if the total number of failures shrinks. The checks are
        // re-evaluated every round, so a ticket left failing by an earlier
        // repair is never skipped.
        let (mut cur_failures, mut failing) = evaluate_task_checks(&check_targets, &iwt);
        let mut attempts: BTreeMap<String, u32> = BTreeMap::new();
        for round in 1..=POST_MERGE_ROUNDS {
            if cur_failures == 0 {
                break;
            }
            let Ok(iwt_repo) = Repo::open(&iwt) else { break };
            let Ok(prev_iwt_head) = iwt_repo.rev_parse("HEAD") else {
                break;
            };
            let mut repairs: Vec<(String, String)> = Vec::new();
            let mut running = Vec::new();
            for ft in failing.drain(..) {
                let task_name = ft.name;
                if attempts.get(&task_name).copied().unwrap_or(0) >= POST_MERGE_ATTEMPTS {
                    continue;
                }
                let sid = results
                    .get(&task_name)
                    .map(|r| r.session_id.clone())
                    .unwrap_or_default();
                let Some(wt) = worktrees.get(&task_name) else {
                    continue;
                };
                if sid.is_empty() {
                    continue;
                }
                let Ok(prev_wt_head) = repo.rev_parse(&format!("concord/{run_id}/{task_name}")) else {
                    continue;
                };
                // The agent's branch is already in the integrated head, so this
                // brings in every teammate's code without conflicts.
                let Ok(m_in) = Repo::merge_into(
                    wt,
                    &prev_iwt_head,
                    &format!("concord: bring integrated codebase into {task_name} for final verification"),
                ) else {
                    continue;
                };
                if !m_in.clean {
                    continue;
                }
                let failing_ids = ft.ids;
                {
                    let mut st = shared.lock().expect("state");
                    let peer_revs: BTreeMap<String, String> = st
                        .agents
                        .iter()
                        .filter(|(n, _)| *n != &task_name)
                        .filter_map(|(n, a)| st.repo.rev_parse(&a.branch).ok().map(|h| (n.clone(), h)))
                        .collect();
                    if let Some(a) = st.agents.get_mut(&task_name) {
                        a.base = prev_iwt_head.clone();
                        a.status = Status::Running;
                        a.check_failures = None;
                        a.stats.check_blocks = 0;
                        a.peer_check_blocks = 0;
                        for (pn, ph) in peer_revs {
                            a.merged_producers.insert(pn.clone());
                            a.merged_revs.insert(pn, ph);
                        }
                    }
                    st.log(
                        "post_merge_resume",
                        json!({ "agent": task_name, "round": round, "failures": failing_ids.len().max(1) }),
                    );
                }
                let check_cmd = ft.cmd;
                let mut msg = format!(
                    "[concord] All parallel teammates' branches have now been integrated into your worktree, \
                     so every teammate's feature is present and no failures are due to missing teammate code.\n\
                     Your ticket's acceptance check (`{check_cmd}`) still has {} failing test(s):\n",
                    failing_ids.len().max(1)
                );
                for id in failing_ids.iter().take(20) {
                    msg.push_str(&format!("  {id}\n"));
                }
                msg.push_str(&format!(
                    "Assertion details:\n{}\n\
                     Fix these remaining failure(s) carefully without breaking any teammate's tests, \
                     run `{check_cmd}` to verify it passes, then finish.",
                    live::assertion_excerpt(&ft.output, 35)
                ));
                eprintln!("  reconciling integrated acceptance check for {task_name} (round {round})");
                running.push(spawn_agent(
                    cfg,
                    AgentCtx {
                        name: &task_name,
                        worktree: wt,
                        settings: &settings_paths[&task_name],
                        shared: &shared,
                        addr: &addr,
                    },
                    &msg,
                    SessionArg::Resume(&sid),
                    &agents_dir,
                    cfg.resume_rounds + round,
                )?);
                *attempts.entry(task_name.clone()).or_default() += 1;
                results.entry(task_name.clone()).or_default().resumed += 1;
                repairs.push((task_name, prev_wt_head));
            }
            if running.is_empty() {
                break;
            }
            wait_all(running, &mut results, &shared)?;

            for (task_name, prev_wt_head) in repairs {
                let wt = &worktrees[&task_name];
                let _ = Repo::commit_if_dirty(wt, &format!("agent {task_name}: post-integration reconciliation"));
                let Ok(new_wt_head) = repo.rev_parse(&format!("concord/{run_id}/{task_name}")) else {
                    continue;
                };
                let Ok(before) = iwt_repo.rev_parse("HEAD") else { break };
                let msg = format!("concord: integrate post-merge fix from {task_name}");
                let mut merged = Repo::merge_into(&iwt, &new_wt_head, &msg);
                if allow_union
                    && let Ok(o) = &merged
                    && !o.clean
                {
                    let files = o.conflicted_files.clone();
                    merged =
                        Repo::merge_into_union(&iwt, &new_wt_head, &msg, &files, false, &crate::regions::verify_union);
                }
                let kept = match merged {
                    Ok(m) if m.clean => {
                        let (after, _) = evaluate_task_checks(&check_targets, &iwt);
                        let better = after < cur_failures;
                        if better {
                            cur_failures = after;
                        }
                        better
                    }
                    _ => false,
                };
                shared.lock().expect("state").log(
                    "post_merge_repair",
                    json!({ "agent": task_name, "round": round, "kept": kept, "failures": cur_failures }),
                );
                if kept {
                    if let Ok(sha) = iwt_repo.rev_parse("HEAD") {
                        prefix_commits.push((task_name, sha));
                    }
                } else {
                    let _ = iwt_repo.git(&["reset", "--hard", &before]);
                    if let Ok(wt_repo) = Repo::open(wt) {
                        let _ = wt_repo.git(&["reset", "--hard", &prev_wt_head]);
                    }
                }
            }
            failing = evaluate_task_checks(&check_targets, &iwt).1;
        }
    }

    for (name, wt) in &worktrees {
        let (ok, _) = run_tests(&cfg.test_cmd, wt);
        results.entry(name.clone()).or_default().tests_alone_passed = Some(ok);
    }
    {
        let st = shared.lock().expect("state");
        for (name, r) in results.iter_mut() {
            if let Some(a) = st.agents.get(name) {
                r.notices_delivered = a.stats.notices_delivered;
                r.gate_blocks = a.stats.gate_blocks;
                r.check_blocks = a.stats.check_blocks;
                r.merges = a.stats.merges.clone();
                r.merge_conflicts = a.stats.merge_conflicts.clone();
            }
        }
    }
    let (ok, tail) = run_tests(&cfg.test_cmd, &iwt);
    integration.tests_passed = ok && integration.conflicts.is_empty();
    integration.tests_output_tail = tail;
    if !ok && integration.conflicts.is_empty() && prefix_commits.len() > 1 {
        integration.bisected_culprit = crate::mrp::bisect_prefix_failure(&repo, &prefix_commits, &cfg.test_cmd);
    }
    let integrated_commit = Repo::open(&iwt).and_then(|r| r.rev_parse("HEAD")).ok();

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
    beating.store(false, std::sync::atomic::Ordering::Relaxed);
    let _ = std::fs::remove_file(&heartbeat);
    let _ = std::fs::remove_file(run_dir.join("daemon.addr"));
    std::fs::write(run_dir.join("report.json"), serde_json::to_string_pretty(&report)?)?;
    std::fs::write(run_dir.join("report.md"), render(&report))?;
    let mrp = crate::mrp::build_mrp_for_run(
        &repo,
        &report,
        integrated_commit,
        report.integration.bisected_culprit.clone(),
    );
    std::fs::write(run_dir.join("mrp.json"), serde_json::to_string_pretty(&mrp)?)?;
    std::fs::write(run_dir.join("mrp.md"), crate::mrp::render_mrp_markdown(&mrp))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_events_skip_empty_session_ids() {
        let startup_failure = r#"{"type":"error","sessionID":"","error":{"message":"Standalone server exited before reporting readiness"}}"#;
        let (sid, text, cost) = parse_opencode_events(startup_failure);
        assert_eq!(sid, None);
        assert!(text.is_empty());
        assert_eq!(cost, 0.0);
        assert!(startup_failure.contains(OPENCODE_START_FAILURE));

        let ok = concat!(
            r#"{"type":"text","sessionID":"ses_1","part":{"sessionID":"ses_1","text":"done"}}"#,
            "\n",
            r#"{"type":"step_finish","part":{"sessionID":"ses_1","cost":0.25}}"#,
        );
        assert_eq!(
            parse_opencode_events(ok),
            (Some("ses_1".to_owned()), "done".to_owned(), 0.25)
        );
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;

    #[test]
    fn detects_a_turn_that_ended_on_a_network_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(
            &p,
            "{\"type\":\"step_finish\"}\n{\"type\":\"error\",\"error\":{\"type\":\"provider.transport\",\"message\":\"getaddrinfo ENOTFOUND opencode.ai\"}}\n",
        )
        .unwrap();
        assert!(ended_on_transport_error(&p));
        // Recovered: progress after the error.
        std::fs::write(
            &p,
            "{\"type\":\"error\",\"error\":{\"message\":\"ECONNRESET\"}}\n{\"type\":\"step_finish\"}\n",
        )
        .unwrap();
        assert!(!ended_on_transport_error(&p));
        // Other errors are the agent's outcome.
        std::fs::write(
            &p,
            "{\"type\":\"error\",\"error\":{\"message\":\"context length exceeded\"}}\n",
        )
        .unwrap();
        assert!(!ended_on_transport_error(&p));
    }
}
