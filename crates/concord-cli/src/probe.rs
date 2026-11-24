//! What an agent's process is doing while Concord sees no events from it.
//!
//! Silence is normal: an agent may be reasoning, streaming a long answer,
//! running a test suite or a build, or waiting for the model API. The probe
//! looks at the agent's process tree (one `ps` call for all agents) and at
//! the files it writes, and only an agent with none of these signs of life
//! for a long time is considered stalled.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

/// What one agent is doing right now.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Probe {
    /// The agent's process exists.
    pub alive: bool,
    /// A command the agent is running (a child process), with its age in s.
    pub running: Option<(String, u64)>,
    /// Seconds since the agent last used CPU or wrote output.
    pub active_ago: u64,
}

/// One row of `ps -A -o pid=,ppid=,time=,etime=,command=`.
#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// Cumulative CPU time in seconds.
    pub cpu: f64,
    /// Elapsed time since start in seconds.
    pub age: u64,
    pub command: String,
}

/// `[[dd-]hh:]mm:ss[.cc]` to seconds.
pub fn clock_secs(s: &str) -> f64 {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().unwrap_or(0.0), r),
        None => (0.0, s),
    };
    let secs = rest
        .split(':')
        .fold(0.0, |acc, part| acc * 60.0 + part.parse::<f64>().unwrap_or(0.0));
    days * 86_400.0 + secs
}

pub fn parse_ps(out: &str) -> Vec<Proc> {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let cpu = clock_secs(it.next()?);
            let age = clock_secs(it.next()?) as u64;
            let command = it.collect::<Vec<_>>().join(" ");
            Some(Proc {
                pid,
                ppid,
                cpu,
                age,
                command,
            })
        })
        .collect()
}

/// Helper processes an agent keeps for its whole life: not "running a command".
fn is_helper(cmd: &str) -> bool {
    ["opencode serve", "concord hook", " mcp", "mcp-server", "caffeinate"]
        .iter()
        .any(|h| cmd.contains(h))
}

/// Descendants of `pid` (breadth first).
pub fn descendants(table: &[Proc], pid: u32) -> Vec<&Proc> {
    let mut children: BTreeMap<u32, Vec<&Proc>> = BTreeMap::new();
    for p in table {
        children.entry(p.ppid).or_default().push(p);
    }
    let mut out = Vec::new();
    let mut queue = vec![pid];
    let mut seen = BTreeSet::new();
    while let Some(p) = queue.pop() {
        for c in children.get(&p).into_iter().flatten() {
            if seen.insert(c.pid) {
                out.push(*c);
                queue.push(c.pid);
            }
        }
    }
    out
}

/// The command an agent is running: its oldest non-helper descendant.
pub fn running_command(table: &[Proc], pid: u32) -> Option<(String, u64)> {
    descendants(table, pid)
        .into_iter()
        .filter(|p| !is_helper(&p.command))
        .max_by_key(|p| p.age)
        .map(|p| {
            let cmd: String = p.command.chars().take(60).collect();
            (cmd, p.age)
        })
}

/// Claude Code's transcript directory for a worktree.
fn transcript_dir(worktree: &Path) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let enc: String = worktree
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Some(PathBuf::from(home).join(".claude/projects").join(enc))
}

/// Newest modification time among the agent's output files (OpenCode turn
/// streams, Claude Code results) and its Claude Code transcript.
fn last_output(run: &Path, name: &str) -> Option<SystemTime> {
    let mut newest: Option<SystemTime> = None;
    let mut consider = |dir: &Path, prefix: Option<&str>| {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.filter_map(|e| e.ok()) {
                let f = e.file_name().to_string_lossy().into_owned();
                if prefix.is_some_and(|p| !f.starts_with(p)) || f.ends_with(".pid") {
                    continue;
                }
                if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                    newest = Some(newest.map_or(m, |n| n.max(m)));
                }
            }
        }
    };
    consider(&run.join("agents"), Some(&format!("{name}.")));
    if let Some(d) = transcript_dir(&run.join("wt").join(name)) {
        consider(&d, None);
    }
    newest
}

/// The newest recorded process id per agent (`agents/<name>[.<n>].pid`).
pub fn agent_pids(run: &Path) -> BTreeMap<String, u32> {
    let mut best: BTreeMap<String, (SystemTime, u32)> = BTreeMap::new();
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

/// Keeps CPU totals between refreshes, to tell when an agent last did anything.
#[derive(Default)]
pub struct Prober {
    cpu: BTreeMap<String, f64>,
    active: BTreeMap<String, Instant>,
}

impl Prober {
    /// Probe every agent of the run that recorded a process id.
    pub fn probe(&mut self, run: &Path) -> BTreeMap<String, Probe> {
        let pids = agent_pids(run);
        if pids.is_empty() {
            return BTreeMap::new();
        }
        let table = std::process::Command::new("ps")
            .args(["-A", "-o", "pid=,ppid=,time=,etime=,command="])
            .output()
            .map(|o| parse_ps(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default();
        let now = Instant::now();
        let mut out = BTreeMap::new();
        for (name, pid) in pids {
            let alive = table.iter().any(|p| p.pid == pid);
            let cpu: f64 = table
                .iter()
                .filter(|p| p.pid == pid)
                .chain(descendants(&table, pid))
                .map(|p| p.cpu)
                .sum();
            // CPU used since the last refresh, or output just written, is activity.
            let used_cpu = self.cpu.get(&name).is_some_and(|prev| cpu > prev + 0.05);
            self.cpu.insert(name.clone(), cpu);
            let wrote = last_output(run, &name)
                .and_then(|m| m.elapsed().ok())
                .map(|d| now.checked_sub(d).unwrap_or(now));
            let last = self.active.entry(name.clone()).or_insert(now);
            if used_cpu {
                *last = now;
            }
            if let Some(w) = wrote
                && w > *last
            {
                *last = w;
            }
            out.insert(
                name,
                Probe {
                    alive,
                    running: if alive { running_command(&table, pid) } else { None },
                    active_ago: now.duration_since(*last).as_secs(),
                },
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ps_clock_formats() {
        assert_eq!(clock_secs("0:01.50"), 1.5);
        assert_eq!(clock_secs("12:03"), 723.0);
        assert_eq!(clock_secs("1:02:03"), 3723.0);
        assert_eq!(clock_secs("2-01:00:00"), 2.0 * 86_400.0 + 3600.0);
    }

    #[test]
    fn finds_the_command_an_agent_runs() {
        let table = parse_ps(
            "  100     1   0:20.00  10:00 claude -p implement\n\
               101   100   0:00.10  09:59 /opt/opencode serve --stdio\n\
               102   100   0:00.01  00:40 /bin/zsh -c pytest -q tests\n\
               103   102   0:05.00  00:39 python -m pytest -q tests\n\
               104   100   0:00.00  00:01 concord hook --event PostToolUse\n",
        );
        assert_eq!(descendants(&table, 100).len(), 4);
        let (cmd, age) = running_command(&table, 100).unwrap();
        assert!(cmd.contains("pytest"), "{cmd}");
        assert_eq!(age, 40);
        // An agent with only helpers runs no command.
        let idle = parse_ps("  200 1 0:01.00 01:00 opencode run x\n  201 200 0:00.10 00:59 opencode serve --stdio\n");
        assert_eq!(running_command(&idle, 200), None);
    }
}
