//! `concord watch --web`: the live view in a browser.
//!
//! A small local HTTP server (127.0.0.1 only) serves one page and a JSON
//! snapshot of the run that the page polls. The snapshot holds the same state
//! as the terminal dashboard, plus each agent's recent actions and the
//! coordination graph: who waits for whom, hand-offs, combined insertions,
//! merges, negotiations and fails-together blocks between agents.

use crate::watch::{self, AgentState, Follower, WatchState};
use anyhow::Result;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PAGE: &str = include_str!("watch.html");

/// Recent actions sent per agent.
const RECENT: usize = 12;

fn tone(code: &str) -> &'static str {
    match code {
        "31;1" => "fail",
        "31" => "held",
        "33" => "wait",
        "32" => "done",
        _ => "work",
    }
}

/// The JSON the page renders.
pub fn snapshot(st: &WatchState, title: &str, run: &Path) -> Value {
    let now = st.elapsed + st.idle.unwrap_or(0) as f64;
    let alive = !st.done && st.heartbeat.is_none_or(|hb| hb <= 15);
    let status = if st.done {
        "complete".to_owned()
    } else if !alive {
        "Concord is not running".to_owned()
    } else {
        "running".to_owned()
    };
    let to_run_secs = |ms: i64| st.start_ms.map(|s| (ms - s) as f64 / 1000.0);
    let mut waits = Vec::new();
    let agents: Vec<Value> = st
        .agents
        .iter()
        .map(|(name, a)| {
            let (text, code, alarm) = watch::agent_status(st, name, a, now);
            if let AgentState::Waiting { on, why } = &a.state {
                for o in on {
                    waits.push(json!({ "from": name, "to": o, "why": why }));
                }
            }
            let act = st.activity.get(name);
            let recent: Vec<Value> = act
                .map(|x| {
                    x.actions
                        .iter()
                        .rev()
                        .take(RECENT)
                        .map(|a| json!({ "ms": a.ms, "t": to_run_secs(a.ms), "kind": a.kind, "what": a.what }))
                        .collect()
                })
                .unwrap_or_default();
            let text = if text.chars().count() > 70 {
                text.chars().take(69).collect::<String>() + "…"
            } else {
                text
            };
            json!({
                "name": name,
                "state": text,
                "tone": tone(code),
                "alarm": alarm.map(|a| format!("{a:?}")),
                "edits": a.edits.max(act.map_or(0, |x| x.edits)),
                "tools": act.map_or(0, |x| x.tools),
                "steps": act.map_or(0, |x| x.steps),
                "tokens_out": act.map_or(0, |x| x.tokens_out),
                "notices": a.notices,
                "blocks": a.blocks,
                "handoffs": a.handoffs,
                "combined": a.combined,
                "merges": a.merges,
                "recent": recent,
            })
        })
        .collect();
    let edges: Vec<Value> = st
        .edges
        .iter()
        .map(|((from, to, kind), (n, last))| json!({ "from": from, "to": to, "kind": kind, "count": n, "last": last }))
        .collect();
    let feed: Vec<Value> = st.feed.iter().map(|(t, s)| json!({ "t": t, "text": s })).collect();
    let warnings: Vec<Value> = st.warnings.iter().map(|(t, s)| json!({ "t": t, "text": s })).collect();
    let finished = st.agents.values().filter(|a| a.state == AgentState::Finished).count();
    json!({
        "title": title,
        "start_ms": st.start_ms,
        "run": run.display().to_string(),
        "clock": if alive { now } else { st.elapsed },
        "status": status,
        "done": st.done,
        "finished": finished,
        "total": st.agents.len(),
        "now_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0),
        "agents": agents,
        "edges": edges,
        "waits": waits,
        "feed": feed,
        "warnings": warnings,
    })
}

fn respond(mut s: TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes());
    let _ = s.write_all(body);
}

fn handle(s: TcpStream, latest: &Mutex<String>) {
    let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
    let mut first = String::new();
    {
        let mut r = BufReader::new(&s);
        if r.read_line(&mut first).is_err() {
            return;
        }
        // Drain the headers.
        let mut line = String::new();
        while r.read_line(&mut line).is_ok_and(|n| n > 2) {
            line.clear();
        }
    }
    let path = first.split_whitespace().nth(1).unwrap_or("/");
    match path {
        "/" | "/index.html" => respond(s, "200 OK", "text/html; charset=utf-8", PAGE.as_bytes()),
        "/state" => {
            let body = latest.lock().map(|b| b.clone()).unwrap_or_default();
            respond(s, "200 OK", "application/json", body.as_bytes())
        }
        _ => respond(s, "404 Not Found", "text/plain", b"not found"),
    }
}

/// Serve the live view of `dir` (a run, or a parent to follow) on `port`.
pub fn serve(dir: &Path, port: u16, interval: Duration) -> Result<()> {
    let follow = !dir.join("events.jsonl").exists();
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let addr = listener.local_addr()?;
    eprintln!("concord watch: live view at http://{addr}  (Ctrl+C to stop)");
    let latest = Arc::new(Mutex::new(String::from("{}")));
    {
        let latest = latest.clone();
        let dir: PathBuf = dir.to_owned();
        std::thread::spawn(move || {
            let mut f: Option<Follower> = None;
            loop {
                if let Ok(newest) = watch::resolve(&dir)
                    && (f.is_none() || (follow && f.as_ref().is_some_and(|f| f.run != newest)))
                {
                    f = Some(Follower::new(newest));
                }
                if let Some(f) = f.as_mut()
                    && f.refresh().is_ok()
                {
                    let title = if follow {
                        f.run.strip_prefix(&dir).unwrap_or(&f.run).display().to_string()
                    } else {
                        f.title.clone()
                    };
                    let snap = snapshot(&f.st, &title, &f.run).to_string();
                    if let Ok(mut l) = latest.lock() {
                        *l = snap;
                    }
                }
                std::thread::sleep(interval);
            }
        });
    }
    for s in listener.incoming().flatten() {
        let latest = latest.clone();
        std::thread::spawn(move || handle(s, &latest));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_has_graph_and_actions() {
        let mut st = WatchState::default();
        for l in [
            r#"{"t":1.0,"event":"footprint","data":{"agent":"a"}}"#,
            r#"{"t":2.0,"event":"region_union","data":{"yielder":"b","owner":"a","files":["m.py"]}}"#,
            r#"{"t":3.0,"event":"wait","data":{"agent":"c","for":["a"],"reason":"region"}}"#,
            r#"{"t":4.0,"event":"merge","data":{"agent":"c","producer":"a","clean":true}}"#,
        ] {
            st.apply(l);
        }
        let mut act = crate::activity::AgentActivity::default();
        crate::activity::apply_opencode(
            &mut act,
            "a",
            r#"{"type":"tool_use","timestamp":5000,"part":{"tool":"shell","state":{"input":{"command":"pytest"}}}}"#,
        );
        st.activity.insert("a".into(), act);
        let v = snapshot(&st, "run", Path::new("/r"));
        let edges = v["edges"].as_array().unwrap();
        assert!(
            edges
                .iter()
                .any(|e| e["from"] == "b" && e["to"] == "a" && e["kind"] == "combined")
        );
        assert!(
            edges
                .iter()
                .any(|e| e["from"] == "a" && e["to"] == "c" && e["kind"] == "merge")
        );
        assert_eq!(v["waits"][0]["from"], "c");
        let a = v["agents"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["name"] == "a")
            .unwrap();
        assert_eq!(a["recent"][0]["what"], "$ pytest");
        assert_eq!(a["tools"], 1);
    }
}
