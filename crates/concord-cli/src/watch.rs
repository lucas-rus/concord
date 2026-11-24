//! `concord watch`: a live view of a coordinated run, read from its event log.
//!
//! The viewer is read-only. It follows `events.jsonl` in a run directory and
//! shows, per agent, what it is doing (working, waiting for whom, blocked,
//! finished) and what Concord did for it (notices, hand-offs, combined
//! insertions, merges), plus a feed of the coordination events that matter.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Feed lines kept on screen.
const FEED: usize = 14;

/// Without events for this long (and without a report or heartbeat), a run
/// looks stopped.
const STALE_SECS: u64 = 600;

/// A heartbeat older than this means Concord itself has stopped (it beats
/// every `run::HEARTBEAT`).
const HEARTBEAT_STALE: u64 = 15;

/// Agents think between actions; after this long the dashboard shows how
/// long ago the last action was (neutral, not a warning).
const QUIET_SECS: f64 = 60.0;

/// An agent whose process is alive but without an action for this long is
/// highlighted (a hint only: long reasoning is legitimate).
const LONG_QUIET_SECS: f64 = 1200.0;

/// An agent in the working state whose process has vanished for this long
/// has crashed (OpenCode agents have no process for a moment between turns).
const GONE_SECS: f64 = 30.0;

#[derive(Clone, Debug, Default, PartialEq)]
pub enum AgentState {
    #[default]
    Working,
    Waiting {
        on: Vec<String>,
        why: String,
    },
    Blocked(String),
    Finished,
    /// The agent's process ended with an error (crash, provider or quota).
    Failed(String),
    /// The process ended without passing the finish gate.
    Exited,
}

#[derive(Clone, Debug, Default)]
pub struct AgentView {
    pub state: AgentState,
    pub edits: u32,
    pub notices: u32,
    pub blocks: u32,
    pub handoffs: u32,
    pub combined: u32,
    pub merges: u32,
    pub last: f64,
}

/// Everything the dashboard shows, folded from the event log.
#[derive(Debug, Default)]
pub struct WatchState {
    pub agents: BTreeMap<String, AgentView>,
    pub feed: VecDeque<(f64, String)>,
    pub elapsed: f64,
    pub done: bool,
    /// Seconds since the event log last changed (wall clock), if known.
    pub idle: Option<u64>,
    /// Seconds since Concord's heartbeat, if the run writes one.
    pub heartbeat: Option<u64>,
    /// Problems worth a look: errors, failed agents, block-limit passes.
    pub warnings: Vec<(f64, String)>,
    /// Whether each agent's process is alive (if `concord run` recorded it).
    pub alive: BTreeMap<String, bool>,
}

fn s(v: &Value, k: &str) -> String {
    v.get(k).and_then(Value::as_str).unwrap_or("?").to_owned()
}

fn list(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

impl WatchState {
    /// Register agents known before they produce events (from `agents/`).
    pub fn add_agent(&mut self, name: &str) {
        self.agents.entry(name.to_owned()).or_default();
    }

    fn agent(&mut self, name: &str, t: f64) -> &mut AgentView {
        let a = self.agents.entry(name.to_owned()).or_default();
        a.last = a.last.max(t);
        a
    }

    fn warn(&mut self, t: f64, text: String) {
        self.warnings.push((t, text.clone()));
        self.say(t, text);
    }

    fn say(&mut self, t: f64, text: String) {
        // Collapse an exact repeat of the previous line (waits, re-sent notices).
        if self.feed.back().is_some_and(|(_, last)| *last == text) {
            return;
        }
        self.feed.push_back((t, text));
        while self.feed.len() > FEED {
            self.feed.pop_front();
        }
    }

    /// Apply one line of `events.jsonl`.
    pub fn apply(&mut self, line: &str) {
        let Ok(e) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let t = e.get("t").and_then(Value::as_f64).unwrap_or(0.0);
        self.elapsed = self.elapsed.max(t);
        let d = e.get("data").cloned().unwrap_or(Value::Null);
        let ev = e.get("event").and_then(Value::as_str).unwrap_or("");
        match ev {
            "footprint" => {
                let a = self.agent(&s(&d, "agent"), t);
                a.edits += 1;
                if matches!(a.state, AgentState::Blocked(_) | AgentState::Waiting { .. }) {
                    a.state = AgentState::Working;
                }
            }
            "notice" => {
                let to = s(&d, "to");
                self.agent(&to, t).notices += 1;
                let kind = s(&d, "kind");
                if kind == "contract_breaking" || kind == "removed" {
                    let what = if kind == "removed" { "removed" } else { "changed" };
                    self.say(t, format!("contract  {} {what} → {to}", s(&d, "symbol")));
                }
            }
            "wait" => {
                let who = s(&d, "agent");
                let on = list(&d, "for");
                let why = d.get("reason").and_then(Value::as_str).unwrap_or("contract").to_owned();
                self.agent(&who, t).state = AgentState::Waiting { on, why };
            }
            "stop" => {
                let who = s(&d, "agent");
                let decision = s(&d, "decision");
                let reason = d.get("reason").and_then(Value::as_str).unwrap_or("").to_owned();
                if decision == "block" {
                    let a = self.agent(&who, t);
                    if reason != "region_handoff" {
                        a.blocks += 1;
                    }
                    a.state = AgentState::Blocked(reason.clone());
                    let detail = match (d.get("owner"), d.get("owners")) {
                        (Some(o), _) => format!(" (owner {})", o.as_str().unwrap_or("?")),
                        (_, Some(_)) => format!(" (owners {})", list(&d, "owners").join(", ")),
                        _ => String::new(),
                    };
                    if reason != "region_handoff" {
                        self.say(t, format!("gate      {who} held: {}{detail}", reason.replace('_', " ")));
                    }
                } else if reason == "block_limit" {
                    self.warn(
                        t,
                        format!("gate      {who} let through at the block limit (coordination unresolved)"),
                    );
                }
            }
            "finished" => {
                let who = s(&d, "agent");
                self.agent(&who, t).state = AgentState::Finished;
                self.say(t, format!("done      {who} finished"));
            }
            "resume" => {
                let who = s(&d, "agent");
                self.agent(&who, t).state = AgentState::Working;
                self.say(
                    t,
                    format!("resume    {who}: a contract it uses changed after it finished"),
                );
            }
            "region_conflict" => {
                let (y, o) = (s(&d, "yielder"), s(&d, "owner"));
                self.agent(&y, t);
                self.say(
                    t,
                    format!("region    {y} yields {} to {o}", list(&d, "files").join(", ")),
                );
            }
            "region_union" => {
                let (y, o) = (s(&d, "yielder"), s(&d, "owner"));
                self.agent(&y, t).combined += 1;
                self.say(
                    t,
                    format!(
                        "combine   {y} + {o} add at the same place in {}",
                        list(&d, "files").join(", ")
                    ),
                );
            }
            "region_handoff" => {
                let (who, o) = (s(&d, "agent"), s(&d, "owner"));
                self.agent(&who, t).handoffs += 1;
                self.say(t, format!("hand-off  {o} → {who}"));
            }
            "merge" => {
                let (who, p) = (s(&d, "agent"), s(&d, "producer"));
                self.agent(&who, t).merges += 1;
                let clean = d.get("clean").and_then(Value::as_bool).unwrap_or(false);
                let how = if clean { "clean" } else { "with conflicts to resolve" };
                self.say(t, format!("merge     {p} → {who} ({how})"));
            }
            "error" => {
                let who = s(&d, "agent");
                let msg = s(&d, "error");
                self.warn(t, format!("error     {who}: {}", msg.lines().next().unwrap_or("")));
            }
            "agent_exit" => {
                let who = s(&d, "agent");
                let error = d.get("error").and_then(Value::as_bool).unwrap_or(false);
                let result = s(&d, "result");
                let first = result.lines().next().unwrap_or("").to_owned();
                let a = self.agent(&who, t);
                if error {
                    a.state = AgentState::Failed(first.clone());
                    self.warn(t, format!("failed    {who}: {first}"));
                } else if a.state != AgentState::Finished {
                    a.state = AgentState::Exited;
                    self.warn(t, format!("exited    {who} ended without passing the finish gate"));
                }
            }
            _ => {}
        }
    }
}

fn mmss(t: f64) -> String {
    let t = t.max(0.0) as u64;
    format!("{:02}:{:02}", t / 60, t % 60)
}

/// Render the dashboard. `color` adds ANSI colours.
pub fn render(st: &WatchState, title: &str, color: bool) -> String {
    let paint = |code: &str, text: &str| {
        if color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    };
    let finished = st.agents.values().filter(|a| a.state == AgentState::Finished).count();
    let status = match (st.done, st.heartbeat, st.idle) {
        (true, _, _) => "complete".to_owned(),
        // Concord writes a heartbeat every few seconds while it runs.
        (false, Some(hb), _) if hb > HEARTBEAT_STALE => {
            format!("Concord is not running (no heartbeat for {hb} s)")
        }
        (false, Some(_), _) => "running".to_owned(),
        // Older runs without a heartbeat: fall back to the event log.
        (false, None, Some(idle)) if idle > STALE_SECS => format!("no events for {} min: stopped?", idle / 60),
        _ => "running".to_owned(),
    };
    // The run's clock now, to tell how long each agent has been quiet.
    let now = st.elapsed + st.idle.unwrap_or(0) as f64;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}  {}  {}/{} finished  {}",
        paint("1", "concord"),
        title,
        finished,
        st.agents.len(),
        paint("2", &format!("{} · {status}", mmss(st.elapsed)))
    );
    let width = st.agents.keys().map(String::len).max().unwrap_or(5).max(5);
    let mut gone: Vec<String> = Vec::new();
    let _ = writeln!(
        out,
        "\n  {:width$}  {:<34} {:>5} {:>7} {:>6} {:>8} {:>8} {:>6}",
        "agent", "state", "edits", "notices", "blocks", "hand-off", "combined", "merges"
    );
    for (name, a) in &st.agents {
        let (state, code) = match &a.state {
            AgentState::Working => ("working".to_owned(), "36"),
            AgentState::Waiting { on, why } => (format!("waiting for {} ({why})", on.join(", ")), "33"),
            AgentState::Blocked(r) => (format!("held: {}", r.replace('_', " ")), "31"),
            AgentState::Finished => ("finished".to_owned(), "32"),
            AgentState::Failed(m) => (format!("FAILED: {m}"), "31;1"),
            AgentState::Exited => ("exited without finishing".to_owned(), "31"),
        };
        let quiet = now - a.last;
        let alive = st.alive.get(name).copied();
        let (state, code) = if a.state == AgentState::Working && st.idle.is_some() && !st.done {
            match alive {
                Some(false) if quiet > GONE_SECS => {
                    gone.push(name.clone());
                    ("process gone: crashed?".to_owned(), "31;1")
                }
                _ if quiet > LONG_QUIET_SECS => (format!("no action for {}", mmss(quiet)), "33"),
                _ if quiet > QUIET_SECS => (format!("working, last action {} ago", mmss(quiet)), "36"),
                _ => (state, code),
            }
        } else {
            (state, code)
        };
        let mut state = state;
        if state.chars().count() > 34 {
            state = state.chars().take(33).collect::<String>() + "…";
        }
        let _ = writeln!(
            out,
            "  {:width$}  {} {:>5} {:>7} {:>6} {:>8} {:>8} {:>6}",
            name,
            paint(code, &format!("{state:<34}")),
            a.edits,
            a.notices,
            a.blocks,
            a.handoffs,
            a.combined,
            a.merges
        );
    }
    let mut warnings: Vec<(f64, String)> = st.warnings.clone();
    for g in &gone {
        warnings.push((now, format!("gone      {g}: its process ended without an exit record")));
    }
    if !warnings.is_empty() {
        let _ = writeln!(out, "\n  {}", paint("31;1", &format!("{} warning(s)", warnings.len())));
        for (t, line) in warnings.iter().rev().take(5).rev() {
            let _ = writeln!(out, "  {}  {}", paint("2", &mmss(*t)), paint("31", line));
        }
    }
    if !st.feed.is_empty() {
        let _ = writeln!(out, "\n  {}", paint("1", "coordination"));
        for (t, line) in &st.feed {
            let _ = writeln!(out, "  {}  {line}", paint("2", &mmss(*t)));
        }
    }
    out
}

/// Resolve a run directory: the directory holding `events.jsonl`, or a parent
/// of run directories (the most recent one is used).
pub fn resolve(dir: &Path) -> Result<PathBuf> {
    if dir.join("events.jsonl").exists() {
        return Ok(dir.to_owned());
    }
    let mut runs: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.join("events.jsonl").exists())
        .filter_map(|p| p.metadata().and_then(|m| m.modified()).ok().map(|m| (m, p)))
        .collect();
    runs.sort();
    runs.pop()
        .map(|(_, p)| p)
        .with_context(|| format!("no run directory (with events.jsonl) in {}", dir.display()))
}

fn known_agents(run: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(run.join("agents"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().to_str().map(str::to_owned))
                .filter_map(|f| f.split('.').next().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}

/// The newest recorded process id per agent (`agents/<name>[.<n>].pid`).
fn agent_pids(run: &Path) -> BTreeMap<String, u32> {
    let mut best: BTreeMap<String, (std::time::SystemTime, u32)> = BTreeMap::new();
    let Ok(rd) = std::fs::read_dir(run.join("agents")) else {
        return BTreeMap::new();
    };
    for e in rd.filter_map(|e| e.ok()) {
        let file = e.file_name().to_string_lossy().into_owned();
        let Some(stem) = file.strip_suffix(".pid") else {
            continue;
        };
        let name = stem.split('.').next().unwrap_or(stem).to_owned();
        let Some(pid) = std::fs::read_to_string(e.path())
            .ok()
            .and_then(|t| t.trim().parse().ok())
        else {
            continue;
        };
        let Ok(m) = e.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if best.get(&name).is_none_or(|(t, _)| m > *t) {
            best.insert(name, (m, pid));
        }
    }
    best.into_iter().map(|(n, (_, p))| (n, p)).collect()
}

/// Which agents' processes are alive (one `ps` call).
fn alive_agents(run: &Path) -> BTreeMap<String, bool> {
    let pids = agent_pids(run);
    if pids.is_empty() {
        return BTreeMap::new();
    }
    let list: Vec<String> = pids.values().map(u32::to_string).collect();
    let Ok(out) = std::process::Command::new("ps")
        .args(["-o", "pid=", "-p", &list.join(",")])
        .output()
    else {
        return BTreeMap::new();
    };
    let live: std::collections::BTreeSet<u32> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect();
    pids.into_iter().map(|(n, p)| (n, live.contains(&p))).collect()
}

/// Follow a run until it completes (or once, with `once`).
pub fn watch(dir: &Path, once: bool, interval: Duration) -> Result<()> {
    let run = resolve(dir)?;
    let title = run
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let color = !once && std::env::var_os("NO_COLOR").is_none();
    let mut st = WatchState::default();
    let mut file = std::fs::File::open(run.join("events.jsonl"))?;
    let mut pending = String::new();
    loop {
        for a in known_agents(&run) {
            st.add_agent(&a);
        }
        let mut chunk = String::new();
        file.read_to_string(&mut chunk)?;
        pending.push_str(&chunk);
        while let Some(i) = pending.find('\n') {
            let line: String = pending.drain(..=i).collect();
            st.apply(line.trim_end());
        }
        st.done = run.join("report.json").exists();
        st.alive = alive_agents(&run);
        st.heartbeat = std::fs::metadata(run.join("heartbeat"))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok())
            .map(|d| d.as_secs());
        st.idle = std::fs::metadata(run.join("events.jsonl"))
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| m.elapsed().ok())
            .map(|d| d.as_secs());
        let screen = render(&st, &title, color);
        let mut out = std::io::stdout().lock();
        if once {
            write!(out, "{screen}")?;
            return Ok(());
        }
        write!(out, "\x1b[H\x1b[2J{screen}")?;
        out.flush()?;
        if st.done {
            return Ok(());
        }
        std::thread::sleep(interval);
        // Keep reading from where we stopped (the log only grows).
        let pos = file.stream_position()?;
        file.seek(SeekFrom::Start(pos))?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(lines: &[&str]) -> WatchState {
        let mut st = WatchState::default();
        for l in lines {
            st.apply(l);
        }
        st
    }

    #[test]
    fn follows_waits_hand_offs_and_finishes() {
        let st = fold(&[
            r#"{"t":1.0,"event":"footprint","data":{"agent":"a"}}"#,
            r#"{"t":2.0,"event":"footprint","data":{"agent":"b"}}"#,
            r#"{"t":3.0,"event":"region_union","data":{"yielder":"b","owner":"a","files":["m.py"]}}"#,
            r#"{"t":4.0,"event":"wait","data":{"agent":"b","for":["a"],"reason":"region"}}"#,
            r#"{"t":5.0,"event":"wait","data":{"agent":"b","for":["a"],"reason":"region"}}"#,
            r#"{"t":6.0,"event":"finished","data":{"agent":"a"}}"#,
            r#"{"t":7.0,"event":"region_handoff","data":{"agent":"b","owner":"a"}}"#,
            r#"{"t":7.0,"event":"stop","data":{"agent":"b","decision":"block","reason":"region_handoff"}}"#,
        ]);
        assert_eq!(st.agents["a"].state, AgentState::Finished);
        assert_eq!(st.agents["b"].combined, 1);
        assert_eq!(st.agents["b"].handoffs, 1);
        // A hand-off is progress, not a gate block.
        assert_eq!(st.agents["b"].blocks, 0);
        let screen = render(&st, "run", false);
        assert!(screen.contains("1/2 finished"), "{screen}");
        assert!(screen.contains("hand-off  a → b"), "{screen}");
        assert!(
            screen.contains("combine   b + a add at the same place in m.py"),
            "{screen}"
        );
    }

    #[test]
    fn waiting_state_names_whom_and_why() {
        let st = fold(&[r#"{"t":1.0,"event":"wait","data":{"agent":"c","for":["a","b"]}}"#]);
        let screen = render(&st, "run", false);
        assert!(screen.contains("waiting for a, b (contract)"), "{screen}");
    }

    #[test]
    fn a_run_without_events_for_long_looks_stopped() {
        let mut st = fold(&[r#"{"t":1.0,"event":"footprint","data":{"agent":"a"}}"#]);
        st.idle = Some(1200);
        assert!(render(&st, "run", false).contains("no events for 20 min: stopped?"));
        st.idle = Some(30);
        assert!(render(&st, "run", false).contains("running"));
    }

    #[test]
    fn a_stale_heartbeat_means_concord_stopped() {
        let mut st = fold(&[r#"{"t":1.0,"event":"footprint","data":{"agent":"a"}}"#]);
        st.heartbeat = Some(3);
        assert!(render(&st, "run", false).contains("running"));
        st.heartbeat = Some(40);
        assert!(render(&st, "run", false).contains("Concord is not running (no heartbeat for 40 s)"));
    }

    #[test]
    fn failures_and_block_limit_passes_are_warnings() {
        let st = fold(&[
            r#"{"t":5.0,"event":"agent_exit","data":{"agent":"a","error":true,"result":"Free usage limit reached"}}"#,
            r#"{"t":6.0,"event":"stop","data":{"agent":"b","decision":"allow","reason":"block_limit"}}"#,
            r#"{"t":7.0,"event":"agent_exit","data":{"agent":"c","error":false,"result":"done"}}"#,
        ]);
        assert_eq!(
            st.agents["a"].state,
            AgentState::Failed("Free usage limit reached".into())
        );
        assert_eq!(st.agents["c"].state, AgentState::Exited);
        let screen = render(&st, "run", false);
        assert!(screen.contains("3 warning(s)"), "{screen}");
        assert!(screen.contains("FAILED: Free usage limit reached"), "{screen}");
    }

    #[test]
    fn thinking_is_not_a_warning_but_a_vanished_process_is() {
        let mut st = fold(&[
            r#"{"t":10.0,"event":"footprint","data":{"agent":"a"}}"#,
            r#"{"t":400.0,"event":"footprint","data":{"agent":"b"}}"#,
        ]);
        st.idle = Some(5);
        // `a` has been thinking for 6.5 minutes with its process alive.
        st.alive = [("a".to_owned(), true), ("b".to_owned(), true)].into();
        let screen = render(&st, "run", false);
        assert!(screen.contains("working, last action 06:35 ago"), "{screen}");
        assert!(!screen.contains("warning"), "{screen}");
        // Its process vanished: that is a crash.
        st.alive.insert("a".into(), false);
        let screen = render(&st, "run", false);
        assert!(screen.contains("process gone: crashed?"), "{screen}");
        assert!(screen.contains("1 warning(s)"), "{screen}");
        // Very long silence with a live process is only a hint.
        st.alive.insert("a".into(), true);
        st.idle = Some(1300);
        let screen = render(&st, "run", false);
        assert!(screen.contains("no action for"), "{screen}");
        assert!(!screen.contains("warning"), "{screen}");
    }

    #[test]
    fn ignores_malformed_lines() {
        let st = fold(&["not json", r#"{"t":1.0,"event":"finished","data":{"agent":"x"}}"#]);
        assert_eq!(st.agents.len(), 1);
    }
}
