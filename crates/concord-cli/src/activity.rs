//! What each agent is actually doing, read from its own output stream.
//!
//! Concord's event log records coordination; the agents' streams record
//! everything else: each tool call (shell commands, files read and edited,
//! searches), what the agent says, and its steps and tokens. OpenCode agents
//! stream JSON events to `agents/<name>.<n>.turn<k>.jsonl`; Claude Code agents
//! write a transcript under `~/.claude/projects/`. Both are followed
//! incrementally, from where the last refresh stopped.

use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Actions kept per agent.
const KEEP: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Shell,
    Read,
    Edit,
    Search,
    Web,
    Plan,
    Say,
    Other,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Action {
    /// Wall clock, milliseconds since the Unix epoch.
    pub ms: i64,
    pub kind: Kind,
    pub what: String,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct AgentActivity {
    pub actions: VecDeque<Action>,
    pub tools: u32,
    pub edits: u32,
    pub steps: u32,
    pub tokens_out: u64,
}

impl AgentActivity {
    pub fn last(&self) -> Option<&Action> {
        self.actions.back()
    }

    fn push(&mut self, a: Action) {
        if a.kind != Kind::Say {
            self.tools += 1;
        }
        if a.kind == Kind::Edit {
            self.edits += 1;
        }
        self.actions.push_back(a);
        while self.actions.len() > KEEP {
            self.actions.pop_front();
        }
    }
}

/// Follows every agent stream of one run.
#[derive(Default)]
pub struct ActivityReader {
    offsets: BTreeMap<PathBuf, u64>,
    pending: BTreeMap<PathBuf, String>,
    pub agents: BTreeMap<String, AgentActivity>,
}

fn first_line(s: &str, max: usize) -> String {
    let line = s.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    if line.chars().count() > max {
        line.chars().take(max - 1).collect::<String>() + "…"
    } else {
        line.to_owned()
    }
}

/// A path relative to the agent's worktree when it lies inside it.
fn short_path(p: &str, agent: &str) -> String {
    let marker = format!("/wt/{agent}/");
    match p.find(&marker) {
        Some(i) => p[i + marker.len()..].to_owned(),
        None => p.to_owned(),
    }
}

/// A shell command with the agent's worktree path written as `.`, and a
/// leading `cd <worktree> &&` dropped.
fn short_command(cmd: &str, agent: &str) -> String {
    let marker = format!("/wt/{agent}");
    let mut out = cmd.to_owned();
    while let Some(i) = out.find(&marker) {
        let start = out[..i]
            .rfind(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '=' | '('))
            .map_or(0, |j| j + 1);
        let end = i + marker.len();
        if out[end..].starts_with('/') {
            out.replace_range(start..=end, "");
        } else {
            out.replace_range(start..end, ".");
        }
    }
    for prefix in ["cd . && ", "cd .; ", "cd . ; "] {
        if let Some(rest) = out.strip_prefix(prefix) {
            out = rest.to_owned();
            break;
        }
    }
    // `/long/path/to/python -m pytest` reads as `python -m pytest`: skip
    // leading `VAR=value` words, then shorten the program to its name.
    let mut pos = 0;
    for word in out.split(' ') {
        if word.contains('=') && !word.starts_with('/') {
            pos += word.len() + 1;
            continue;
        }
        if word.starts_with('/')
            && let Some(slash) = word.rfind('/')
        {
            out.replace_range(pos..=pos + slash, "");
        }
        break;
    }
    out
}

/// One line describing a tool call, for any agent's tool names.
pub fn describe(tool: &str, input: &Value, agent: &str) -> (Kind, String) {
    let get = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| input.get(*k).and_then(Value::as_str))
            .unwrap_or("")
            .to_owned()
    };
    let path = || short_path(&get(&["filePath", "file_path", "path", "notebook_path"]), agent);
    match tool.to_ascii_lowercase().as_str() {
        "bash" | "shell" => (
            Kind::Shell,
            format!("$ {}", first_line(&short_command(&get(&["command"]), agent), 100)),
        ),
        "read" => (Kind::Read, format!("read {}", path())),
        "edit" | "multiedit" | "notebookedit" | "patch" | "apply_patch" => (Kind::Edit, format!("edit {}", path())),
        "write" => (Kind::Edit, format!("write {}", path())),
        "grep" => (Kind::Search, format!("grep {}", first_line(&get(&["pattern"]), 80))),
        "glob" | "list" | "ls" => (Kind::Search, format!("list {}", get(&["pattern", "path"]))),
        "webfetch" => (Kind::Web, format!("fetch {}", get(&["url"]))),
        "websearch" => (Kind::Web, format!("search {}", get(&["query"]))),
        "todowrite" | "todoread" => (Kind::Plan, "update its plan".to_owned()),
        "task" | "agent" => (
            Kind::Other,
            format!("subagent: {}", first_line(&get(&["description"]), 80)),
        ),
        other => (Kind::Other, other.to_owned()),
    }
}

/// Days since 1970-01-01 of a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-11-26T01:02:03.456Z` to milliseconds since the epoch (UTC only).
pub fn iso_ms(s: &str) -> Option<i64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, mo, da) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let time = time.trim_end_matches('Z');
    let mut t = time.split(':');
    let (h, mi) = (t.next()?.parse::<i64>().ok()?, t.next()?.parse::<i64>().ok()?);
    let sec: f64 = t.next()?.parse().ok()?;
    Some((days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60) * 1000 + (sec * 1000.0) as i64)
}

/// Apply one line of an OpenCode `run --format json` stream.
pub fn apply_opencode(act: &mut AgentActivity, agent: &str, line: &str) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let ms = v.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
    let part = v.get("part").cloned().unwrap_or(Value::Null);
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "tool_use" => {
            let tool = part.get("tool").and_then(Value::as_str).unwrap_or("tool");
            let input = part.pointer("/state/input").cloned().unwrap_or(Value::Null);
            let (kind, what) = describe(tool, &input, agent);
            act.push(Action { ms, kind, what });
        }
        "text" => {
            let text = part.get("text").and_then(Value::as_str).unwrap_or("");
            if !text.trim().is_empty() {
                act.push(Action {
                    ms,
                    kind: Kind::Say,
                    what: first_line(text, 140),
                });
            }
        }
        "step_finish" => {
            act.steps += 1;
            act.tokens_out += part.pointer("/tokens/output").and_then(Value::as_u64).unwrap_or(0);
        }
        _ => {}
    }
}

/// Apply one line of a Claude Code transcript.
pub fn apply_claude(act: &mut AgentActivity, agent: &str, line: &str) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    if v.get("type").and_then(Value::as_str) != Some("assistant") {
        return;
    }
    let ms = v.get("timestamp").and_then(Value::as_str).and_then(iso_ms).unwrap_or(0);
    act.steps += 1;
    act.tokens_out += v
        .pointer("/message/usage/output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    for c in v
        .pointer("/message/content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match c.get("type").and_then(Value::as_str) {
            Some("tool_use") => {
                let tool = c.get("name").and_then(Value::as_str).unwrap_or("tool");
                let (kind, what) = describe(tool, c.get("input").unwrap_or(&Value::Null), agent);
                act.push(Action { ms, kind, what });
            }
            Some("text") => {
                let text = c.get("text").and_then(Value::as_str).unwrap_or("");
                if !text.trim().is_empty() {
                    act.push(Action {
                        ms,
                        kind: Kind::Say,
                        what: first_line(text, 140),
                    });
                }
            }
            _ => {}
        }
    }
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

/// `(agent, turn)` of an OpenCode stream file name (`name.0.turn3.jsonl`).
fn stream_key(file: &str) -> Option<(String, String)> {
    let stem = file.strip_suffix(".jsonl")?;
    let (head, turn) = stem.rsplit_once(".turn")?;
    let name = head.split('.').next()?.to_owned();
    Some((name, format!("{head}.{turn}")))
}

impl ActivityReader {
    /// Read whatever the agents wrote since the last refresh.
    pub fn refresh(&mut self, run: &Path) {
        let mut files: Vec<(String, PathBuf, bool)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(run.join("agents")) {
            let mut streams: Vec<(String, String, PathBuf)> = rd
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let f = e.file_name().to_string_lossy().into_owned();
                    stream_key(&f).map(|(n, k)| (n, k, e.path()))
                })
                .collect();
            // Read turns in order, so actions stay in time order.
            streams.sort_by(|a, b| (&a.0, natural(&a.1)).cmp(&(&b.0, natural(&b.1))));
            files.extend(streams.into_iter().map(|(n, _, p)| (n, p, true)));
        }
        if let Ok(rd) = std::fs::read_dir(run.join("wt")) {
            for e in rd.filter_map(|e| e.ok()) {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with('_') {
                    continue;
                }
                let Some(dir) = transcript_dir(&e.path()) else {
                    continue;
                };
                if let Ok(rd) = std::fs::read_dir(&dir) {
                    let mut ts: Vec<PathBuf> = rd
                        .filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
                        .collect();
                    ts.sort();
                    files.extend(ts.into_iter().map(|p| (name.clone(), p, false)));
                }
            }
        }
        for (name, path, opencode) in files {
            let lines = self.read_new(&path);
            if lines.is_empty() {
                continue;
            }
            let act = self.agents.entry(name.clone()).or_default();
            for l in lines {
                if opencode {
                    apply_opencode(act, &name, &l);
                } else {
                    apply_claude(act, &name, &l);
                }
            }
        }
    }

    /// Complete lines appended to `path` since the last read.
    fn read_new(&mut self, path: &Path) -> Vec<String> {
        let off = self.offsets.get(path).copied().unwrap_or(0);
        let Ok(mut f) = std::fs::File::open(path) else {
            return Vec::new();
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len <= off || f.seek(SeekFrom::Start(off)).is_err() {
            return Vec::new();
        }
        let mut buf = Vec::new();
        if f.take(len - off).read_to_end(&mut buf).is_err() {
            return Vec::new();
        }
        self.offsets.insert(path.to_owned(), len);
        let pending = self.pending.entry(path.to_owned()).or_default();
        pending.push_str(&String::from_utf8_lossy(&buf));
        let mut out = Vec::new();
        while let Some(i) = pending.find('\n') {
            let line: String = pending.drain(..=i).collect();
            out.push(line.trim_end().to_owned());
        }
        out
    }
}

/// Sort key that orders `name.0.10` after `name.0.9`.
fn natural(s: &str) -> Vec<(u64, String)> {
    s.split('.')
        .map(|p| match p.parse::<u64>() {
            Ok(n) => (n, String::new()),
            Err(_) => (0, p.to_owned()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opencode_stream_becomes_actions() {
        let mut a = AgentActivity::default();
        for l in [
            r#"{"type":"text","timestamp":1000,"part":{"type":"text","text":"I'll start by exploring.\nmore"}}"#,
            r#"{"type":"tool_use","timestamp":2000,"part":{"type":"tool","tool":"shell","state":{"input":{"command":"pytest -q tests/test_spec.py -k Links"}}}}"#,
            r#"{"type":"tool_use","timestamp":3000,"part":{"type":"tool","tool":"edit","state":{"input":{"filePath":"/x/runs/r/wt/links/src/cmark/inlines.py"}}}}"#,
            r#"{"type":"step_finish","timestamp":3100,"part":{"type":"step-finish","tokens":{"output":42}}}"#,
        ] {
            apply_opencode(&mut a, "links", l);
        }
        let what: Vec<&str> = a.actions.iter().map(|x| x.what.as_str()).collect();
        assert_eq!(
            what,
            [
                "I'll start by exploring.",
                "$ pytest -q tests/test_spec.py -k Links",
                "edit src/cmark/inlines.py"
            ]
        );
        assert_eq!((a.tools, a.edits, a.steps, a.tokens_out), (2, 1, 1, 42));
        assert_eq!(a.last().unwrap().ms, 3000);
    }

    #[test]
    fn shell_commands_drop_the_worktree_path() {
        assert_eq!(
            short_command("cd /x/runs/r/wt/links && pytest -q tests", "links"),
            "pytest -q tests"
        );
        assert_eq!(
            short_command("python3 /x/runs/r/wt/links/src/a.py", "links"),
            "python3 src/a.py"
        );
        assert_eq!(short_command("git status", "links"), "git status");
        assert_eq!(
            short_command("PYTHONPATH=src /opt/venv/bin/python -m pytest -q", "links"),
            "PYTHONPATH=src python -m pytest -q"
        );
        assert_eq!(short_command("cat ../a/./b", "links"), "cat ../a/./b");
    }

    #[test]
    fn claude_transcript_becomes_actions() {
        let mut a = AgentActivity::default();
        apply_claude(
            &mut a,
            "x",
            r#"{"type":"assistant","timestamp":"1970-01-01T00:00:02.500Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}],"usage":{"output_tokens":7}}}"#,
        );
        assert_eq!(a.actions[0].what, "$ cargo test");
        assert_eq!(a.actions[0].ms, 2500);
        assert_eq!(iso_ms("2026-11-26T00:00:00Z"), Some(1_795_651_200_000));
    }

    #[test]
    fn stream_files_are_keyed_by_agent_and_turn() {
        assert_eq!(
            stream_key("html-blocks.0.turn12.jsonl"),
            Some(("html-blocks".into(), "html-blocks.0.12".into()))
        );
        assert_eq!(stream_key("a.settings.json"), None);
        assert!(natural("a.0.10") > natural("a.0.9"));
    }
}
