//! One-command MCP client setup (`concord setup`).
//!
//! Automatically connects Concord's MCP server to OpenCode, Claude Code,
//! and/or Cursor with zero manual JSON editing.

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientTarget {
    All,
    Opencode,
    Claude,
    Cursor,
}

impl std::str::FromStr for ClientTarget {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "all" | "auto" => Ok(Self::All),
            "opencode" => Ok(Self::Opencode),
            "claude" | "claude-code" => Ok(Self::Claude),
            "cursor" => Ok(Self::Cursor),
            other => anyhow::bail!("unknown client {other:?} (expected: auto, opencode, claude, cursor)"),
        }
    }
}

pub struct SetupOptions {
    pub client: ClientTarget,
    pub model: Option<String>,
    pub project_local: bool,
    pub concord_exe: PathBuf,
}

fn has_bin(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME environment variable is not set")
}

pub fn run_setup(opts: &SetupOptions) -> Result<Vec<String>> {
    let mut actions = Vec::new();
    let exe = opts.concord_exe.to_string_lossy().into_owned();

    let want_opencode = matches!(opts.client, ClientTarget::All | ClientTarget::Opencode);
    let want_claude = matches!(opts.client, ClientTarget::All | ClientTarget::Claude);
    let want_cursor = matches!(opts.client, ClientTarget::All | ClientTarget::Cursor);

    if want_opencode && (opts.client == ClientTarget::Opencode || has_bin("opencode")) {
        actions.push(setup_opencode(&exe, opts.model.as_deref(), opts.project_local)?);
    }

    if want_claude && (opts.client == ClientTarget::Claude || has_bin("claude")) {
        actions.push(setup_claude(&exe, opts.model.as_deref(), opts.project_local)?);
    }

    let cursor_dir = home_dir()?.join(".cursor");
    if want_cursor && (opts.client == ClientTarget::Cursor || cursor_dir.is_dir()) {
        actions.push(setup_cursor(&cursor_dir, &exe, opts.model.as_deref())?);
    }

    if actions.is_empty() {
        // Default fallback: write global OpenCode config so it's ready as soon as OpenCode is installed.
        actions.push(write_opencode_json(
            &home_dir()?.join(".config").join("opencode").join("opencode.json"),
            &exe,
            opts.model.as_deref(),
        )?);
    }
    Ok(actions)
}

fn build_mcp_args(agent_cli: &str, model: Option<&str>) -> Vec<String> {
    let mut args = vec!["mcp".into(), "--agent-cli".into(), agent_cli.into()];
    if let Some(m) = model
        && !m.trim().is_empty()
    {
        args.push("--model".into());
        args.push(m.trim().into());
    }
    args
}

fn setup_opencode(exe: &str, model: Option<&str>, project_local: bool) -> Result<String> {
    let mcp_args = build_mcp_args("opencode", model);
    if has_bin("opencode") {
        let mut cmd = Command::new("opencode");
        cmd.args(["mcp", "add"]);
        if !project_local {
            cmd.arg("--global");
        }
        cmd.args(["concord", "--", exe]);
        cmd.args(&mcp_args);
        if let Ok(out) = cmd.output()
            && out.status.success()
        {
            let scope = if project_local { "project" } else { "global" };
            return Ok(format!(
                "OpenCode ({scope}): registered `concord` via `opencode mcp add`"
            ));
        }
    }
    let path = if project_local {
        PathBuf::from("opencode.json")
    } else {
        home_dir()?.join(".config").join("opencode").join("opencode.json")
    };
    write_opencode_json(&path, exe, model)
}

pub fn write_opencode_json(path: &Path, exe: &str, model: Option<&str>) -> Result<String> {
    let mut root = read_json_object(path)?;
    if !root.contains_key("$schema") {
        root.insert("$schema".into(), json!("https://opencode.ai/config.json"));
    }
    let mcp = root
        .entry("mcp")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("`mcp` field in opencode.json is not an object")?;

    let mut command = vec![exe.to_owned()];
    command.extend(build_mcp_args("opencode", model));
    mcp.insert(
        "concord".into(),
        json!({
            "type": "local",
            "command": command,
            "enabled": true
        }),
    );

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&Value::Object(root))? + "\n")?;
    Ok(format!("OpenCode: wrote `concord` MCP entry to {}", path.display()))
}

fn setup_claude(exe: &str, model: Option<&str>, project_local: bool) -> Result<String> {
    let mcp_args = build_mcp_args("claude", model);
    if has_bin("claude") {
        let mut cmd = Command::new("claude");
        cmd.args(["mcp", "add", "--scope"]);
        cmd.arg(if project_local { "project" } else { "user" });
        cmd.args(["concord", "--", exe]);
        cmd.args(&mcp_args);
        if let Ok(out) = cmd.output()
            && out.status.success()
        {
            let scope = if project_local { "project" } else { "user" };
            return Ok(format!(
                "Claude Code ({scope}): registered `concord` via `claude mcp add`"
            ));
        }
    }
    Ok(
        "Claude Code: run `/plugin marketplace add lucas-rus/concord && /plugin install concord@lucas-rus-concord`"
            .into(),
    )
}

fn setup_cursor(cursor_dir: &Path, exe: &str, model: Option<&str>) -> Result<String> {
    let path = cursor_dir.join("mcp.json");
    let mut root = read_json_object(&path)?;
    let servers = root
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("`mcpServers` field in Cursor mcp.json is not an object")?;
    servers.insert(
        "concord".into(),
        json!({
            "command": exe,
            "args": build_mcp_args("opencode", model)
        }),
    );
    std::fs::create_dir_all(cursor_dir)?;
    std::fs::write(&path, serde_json::to_string_pretty(&Value::Object(root))? + "\n")?;
    Ok(format!("Cursor: wrote `concord` MCP entry to {}", path.display()))
}

fn read_json_object(path: &Path) -> Result<Map<String, Value>> {
    if !path.is_file() {
        return Ok(Map::new());
    }
    let raw = std::fs::read_to_string(path)?;
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    let val: Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing existing JSON config {}", path.display()))?;
    val.as_object()
        .cloned()
        .with_context(|| format!("{} is not a JSON object", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_opencode_json_merges_without_clobbering_existing_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("opencode.json");
        std::fs::write(
            &cfg,
            r#"{"theme":"dark","mcp":{"other":{"type":"local","command":["x"]}}}"#,
        )
        .unwrap();
        let msg = write_opencode_json(&cfg, "/usr/local/bin/concord", Some("opencode-go/hy4-flash-free")).unwrap();
        assert!(msg.contains("OpenCode"));
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(parsed["theme"], "dark");
        assert!(parsed["mcp"]["other"].is_object());
        assert_eq!(parsed["mcp"]["concord"]["enabled"], true);
        assert_eq!(parsed["mcp"]["concord"]["command"][0], "/usr/local/bin/concord");
    }
}
