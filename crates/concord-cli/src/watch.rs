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

/// Agents think between actions; after this long without an event the
/// dashboard says what the agent is doing instead.
const QUIET_SECS: f64 = 60.0;

/// CPU use or output this recent counts as the agent being active.
const ACTIVE_SECS: u64 = 60;

/// An agent alive but with no CPU use, no output and no command for this
/// long has stalled.
const STALL_SECS: u64 = 600;

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
    /// What each agent's process is doing (if `concord run` recorded it).
    pub probes: BTreeMap<String, crate::probe::Probe>,
    /// What each agent did, from its own output stream.
    pub activity: BTreeMap<String, crate::activity::AgentActivity>,
    /// Coordination between two agents, `(from, to, kind)`: how often, and
    /// when last (run clock).
    pub edges: BTreeMap<(String, String, String), (u32, f64)>,
    /// Wall clock of the run's start (ms since the epoch), from its id.
    pub start_ms: Option<i64>,
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

    fn link(&mut self, from: &str, to: &str, kind: &str) {
        if from != to && from != "?" && to != "?" {
            let t = self.elapsed;
            let e = self
                .edges
                .entry((from.to_owned(), to.to_owned(), kind.to_owned()))
                .or_default();
            e.0 += 1;
            e.1 = e.1.max(t);
        }
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
                    if reason == "fails_together" {
                        for c in list(&d, "culprits") {
                            self.link(&c, &who, "fails together");
                        }
                    }
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
                self.link(&y, &o, "yields");
                self.say(
                    t,
                    format!("region    {y} yields {} to {o}", list(&d, "files").join(", ")),
                );
            }
            "region_union" => {
                let (y, o) = (s(&d, "yielder"), s(&d, "owner"));
                self.agent(&y, t).combined += 1;
                self.link(&y, &o, "combined");
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
                self.link(&o, &who, "hand-off");
                self.say(t, format!("hand-off  {o} → {who}"));
            }
            "merge" => {
                let (who, p) = (s(&d, "agent"), s(&d, "producer"));
                self.agent(&who, t).merges += 1;
                self.link(&p, &who, "merge");
                let clean = d.get("clean").and_then(Value::as_bool).unwrap_or(false);
                let how = if clean { "clean" } else { "with conflicts to resolve" };
                self.say(t, format!("merge     {p} → {who} ({how})"));
            }
            "negotiate" => {
                let (from, to) = (s(&d, "from"), s(&d, "to"));
                self.link(&from, &to, "negotiate");
                self.say(t, format!("negotiate {from} → {to}: {}", s(&d, "symbol")));
            }
            "post_merge_resume" => {
                let who = s(&d, "agent");
                self.agent(&who, t).state = AgentState::Working;
                let n = d.get("failures").and_then(Value::as_u64).unwrap_or(0);
                self.say(t, format!("repair    {who}: {n} check failure(s) on the merged code"));
            }
            "error" => {
                let who = s(&d, "agent");
                let msg = s(&d, "error");
                self.warn(t, format!("error     {who}: {}", msg.lines().next().unwrap_or("")));
            }
            "transport_retry" => {
                let who = s(&d, "agent");
                let n = d.get("attempt").and_then(Value::as_u64).unwrap_or(0);
                self.agent(&who, t);
                self.say(
                    t,
                    format!("network   {who}: connection to the model provider lost, resuming (attempt {n})"),
                );
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

/// Why an agent needs attention.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Alarm {
    Gone,
    Stalled(u64),
}

/// One agent's state line, its ANSI colour, and an alarm. Silent agents are
/// explained (a command, thinking, waiting for the model) before anything is
/// called a failure.
pub fn agent_status(st: &WatchState, name: &str, a: &AgentView, now: f64) -> (String, &'static str, Option<Alarm>) {
    let (state, code) = match &a.state {
        AgentState::Working => ("working".to_owned(), "36"),
        AgentState::Waiting { on, why } => (format!("waiting for {} ({why})", on.join(", ")), "33"),
        AgentState::Blocked(r) => (format!("held: {}", r.replace('_', " ")), "31"),
        AgentState::Finished => ("finished".to_owned(), "32"),
        AgentState::Failed(m) => (format!("FAILED: {m}"), "31;1"),
        AgentState::Exited => ("exited without finishing".to_owned(), "31"),
    };
    let quiet = now - a.last;
    if !(a.state == AgentState::Working && st.idle.is_some() && !st.done && quiet > QUIET_SECS) {
        return (state, code, None);
    }
    match st.probes.get(name) {
        None => (format!("working, last action {} ago", mmss(quiet)), "36", None),
        Some(p) if !p.alive => {
            if quiet > GONE_SECS {
                ("process gone: crashed?".to_owned(), "31;1", Some(Alarm::Gone))
            } else {
                (state, code, None)
            }
        }
        Some(p) => match &p.running {
            Some((cmd, age)) => (format!("running {cmd} ({})", mmss(*age as f64)), "36", None),
            None if p.active_ago <= ACTIVE_SECS => ("thinking".to_owned(), "36", None),
            None if p.active_ago > STALL_SECS => (
                format!("stalled {}", mmss(p.active_ago as f64)),
                "31;1",
                Some(Alarm::Stalled(p.active_ago)),
            ),
            None => (
                format!("idle {} (waiting for the model?)", mmss(p.active_ago as f64)),
                "33",
                None,
            ),
        },
    }
}

/// When (m:ss ago while the run lives, else the run's clock) and what the
/// agent last did, from its own stream.
pub fn doing(st: &WatchState, name: &str) -> Option<(String, String)> {
    let last = st.activity.get(name)?.last()?;
    if st.done
        && let Some(start) = st.start_ms
    {
        return Some((
            format!("at {}", mmss((last.ms - start) as f64 / 1000.0)),
            last.what.clone(),
        ));
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some((
        format!("{} ago", mmss(((now_ms - last.ms).max(0) / 1000) as f64)),
        last.what.clone(),
    ))
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
    // The run's clock now, to tell how long each agent has been quiet. It
    // keeps ticking between events (and before the first one) while the run
    // is alive.
    let now = st.elapsed + st.idle.unwrap_or(0) as f64;
    let clock = if st.done || st.heartbeat.is_some_and(|hb| hb > HEARTBEAT_STALE) {
        st.elapsed
    } else {
        now
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{}  {}  {}/{} finished  {}",
        paint("1", "concord"),
        title,
        finished,
        st.agents.len(),
        paint("2", &format!("{} · {status}", mmss(clock)))
    );
    let width = st.agents.keys().map(String::len).max().unwrap_or(5).max(5);
    let mut gone: Vec<String> = Vec::new();
    let mut stalled: Vec<(String, u64)> = Vec::new();
    let _ = writeln!(
        out,
        "\n  {:width$}  {:<34} {:>5} {:>7} {:>6} {:>8} {:>8} {:>6}",
        "agent", "state", "edits", "notices", "blocks", "hand-off", "combined", "merges"
    );
    for (name, a) in &st.agents {
        let (state, code, alarm) = agent_status(st, name, a, now);
        match alarm {
            Some(Alarm::Gone) => gone.push(name.clone()),
            Some(Alarm::Stalled(secs)) => stalled.push((name.clone(), secs)),
            None => {}
        }
        let mut state = state;
        if state.chars().count() > 34 {
            state = state.chars().take(33).collect::<String>() + "…";
        }
        let _ = writeln!(
            out,
            "  {:width$}  {} {:>5} {:>7} {:>6} {:>8} {:>8} {:>6}",
            name,
            paint(code, &format!("{state:<34}")),
            a.edits.max(st.activity.get(name).map_or(0, |x| x.edits)),
            a.notices,
            a.blocks,
            a.handoffs,
            a.combined,
            a.merges
        );
        if let Some((ago, what)) = doing(st, name) {
            let mut what = what;
            if what.chars().count() > 90 {
                what = what.chars().take(89).collect::<String>() + "…";
            }
            let _ = writeln!(out, "  {:width$}  {}", "", paint("2", &format!("↳ {ago:>9}  {what}")));
        }
    }
    let mut warnings: Vec<(f64, String)> = st.warnings.clone();
    for g in &gone {
        warnings.push((now, format!("gone      {g}: its process ended without an exit record")));
    }
    for (g, secs) in &stalled {
        warnings.push((
            now,
            format!(
                "stalled   {g}: no CPU use, output or command for {}",
                mmss(*secs as f64)
            ),
        ));
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

/// Resolve a run directory: the directory holding `events.jsonl`, or the most
/// recently active run anywhere below a parent directory (a few levels deep, so
/// `.concord/runs` and benchmark trees both work).
pub fn resolve(dir: &Path) -> Result<PathBuf> {
    if dir.join("events.jsonl").exists() {
        return Ok(dir.to_owned());
    }
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let mut stack = vec![(dir.to_owned(), 0)];
    while let Some((d, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.filter_map(|e| e.ok()) {
            let p = e.path();
            if !p.is_dir() || e.file_name() == "wt" || e.file_name() == "agents" || e.file_name() == ".git" {
                continue;
            }
            let ev = p.join("events.jsonl");
            if let Ok(m) = ev.metadata().and_then(|m| m.modified()) {
                if best.as_ref().is_none_or(|(t, _)| m > *t) {
                    best = Some((m, p));
                }
            } else if depth < 5 {
                stack.push((p, depth + 1));
            }
        }
    }
    best.map(|(_, p)| p)
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

/// Follows one run: its event log, its agents' processes and their streams.
pub struct Follower {
    pub run: PathBuf,
    pub title: String,
    pub st: WatchState,
    file: Option<std::fs::File>,
    pending: String,
    prober: crate::probe::Prober,
    reader: crate::activity::ActivityReader,
}

impl Follower {
    pub fn new(run: PathBuf) -> Self {
        let title = run
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let st = WatchState {
            start_ms: title
                .split('-')
                .next()
                .and_then(|s| s.parse::<i64>().ok())
                .map(|s| s * 1000),
            ..Default::default()
        };
        Follower {
            run,
            title,
            st,
            file: None,
            pending: String::new(),
            prober: Default::default(),
            reader: Default::default(),
        }
    }

    /// Read everything new.
    pub fn refresh(&mut self) -> Result<()> {
        let run = self.run.clone();
        for a in known_agents(&run) {
            self.st.add_agent(&a);
        }
        if self.file.is_none() {
            self.file = Some(std::fs::File::open(run.join("events.jsonl"))?);
        }
        if let Some(f) = self.file.as_mut() {
            let mut chunk = String::new();
            f.read_to_string(&mut chunk)?;
            self.pending.push_str(&chunk);
            // Keep reading from where we stopped (the log only grows).
            let pos = f.stream_position()?;
            f.seek(SeekFrom::Start(pos))?;
        }
        while let Some(i) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=i).collect();
            self.st.apply(line.trim_end());
        }
        self.st.done = run.join("report.json").exists();
        self.st.probes = self.prober.probe(&run);
        self.reader.refresh(&run);
        self.st.activity = self.reader.agents.clone();
        for a in self.reader.agents.keys() {
            self.st.add_agent(a);
        }
        let age = |f: &str| {
            std::fs::metadata(run.join(f))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|d| d.as_secs())
        };
        self.st.heartbeat = age("heartbeat");
        self.st.idle = age("events.jsonl");
        Ok(())
    }
}

/// Follow a run until it completes (or once, with `once`). Given a parent
/// directory, keep following the newest run below it.
pub fn watch(dir: &Path, once: bool, interval: Duration) -> Result<()> {
    let follow = !dir.join("events.jsonl").exists();
    let mut f = Follower::new(resolve(dir)?);
    let color = !once && std::env::var_os("NO_COLOR").is_none();
    loop {
        if follow
            && let Ok(newest) = resolve(dir)
            && newest != f.run
        {
            f = Follower::new(newest);
        }
        f.refresh()?;
        let title = if follow {
            f.run.strip_prefix(dir).unwrap_or(&f.run).display().to_string()
        } else {
            f.title.clone()
        };
        let screen = render(&f.st, &title, color);
        let mut out = std::io::stdout().lock();
        if once {
            write!(out, "{screen}")?;
            return Ok(());
        }
        write!(out, "\x1b[H\x1b[2J{screen}")?;
        out.flush()?;
        if f.st.done && !follow {
            return Ok(());
        }
        std::thread::sleep(interval);
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
    fn the_clock_ticks_between_events_while_the_run_lives() {
        let mut st = fold(&[r#"{"t":60.0,"event":"footprint","data":{"agent":"a"}}"#]);
        st.heartbeat = Some(2);
        st.idle = Some(200);
        assert!(render(&st, "run", false).contains("04:20 · running"));
        // A dead run's clock stops at its last event.
        st.heartbeat = Some(600);
        assert!(render(&st, "run", false).contains("01:00 · Concord is not running"));
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

    fn probe(alive: bool, running: Option<&str>, active_ago: u64) -> crate::probe::Probe {
        crate::probe::Probe {
            alive,
            running: running.map(|c| (c.to_owned(), 90)),
            active_ago,
        }
    }

    #[test]
    fn silence_is_explained_before_it_is_called_a_failure() {
        let mut st = fold(&[
            r#"{"t":10.0,"event":"footprint","data":{"agent":"a"}}"#,
            r#"{"t":400.0,"event":"footprint","data":{"agent":"b"}}"#,
        ]);
        st.idle = Some(5);
        let screen_with = |st: &mut WatchState, p| {
            st.probes.insert("a".into(), p);
            render(st, "run", false)
        };
        // Running a command (a test suite): fine.
        let s1 = screen_with(&mut st, probe(true, Some("python -m pytest -q"), 0));
        assert!(s1.contains("running python -m pytest"), "{s1}");
        assert!(!s1.contains("warning"), "{s1}");
        // Thinking (CPU or output just now): fine.
        let s2 = screen_with(&mut st, probe(true, None, 5));
        assert!(s2.contains("thinking"), "{s2}");
        assert!(!s2.contains("warning"), "{s2}");
        // Alive and idle for a while (model API?): a hint, not a warning.
        let s3 = screen_with(&mut st, probe(true, None, 200));
        assert!(s3.contains("idle 03:20"), "{s3}");
        assert!(!s3.contains("warning"), "{s3}");
        // No CPU, output or command for over 10 minutes: stalled.
        let s4 = screen_with(&mut st, probe(true, None, 700));
        assert!(s4.contains("stalled 11:40") && s4.contains("1 warning(s)"), "{s4}");
        // Process gone: crashed.
        let s5 = screen_with(&mut st, probe(false, None, 0));
        assert!(
            s5.contains("process gone: crashed?") && s5.contains("1 warning(s)"),
            "{s5}"
        );
    }

    #[test]
    fn ignores_malformed_lines() {
        let st = fold(&["not json", r#"{"t":1.0,"event":"finished","data":{"agent":"x"}}"#]);
        assert_eq!(st.agents.len(), 1);
    }
}
