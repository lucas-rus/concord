// Concord adapter for OpenCode (plugin format of OpenCode 2.x).
//
// Forwards every completed tool call to the Concord daemon, as Claude Code's
// PostToolUse hook does. Any coordination notice the daemon returns is
// appended to the tool result, so the model sees it with the tool output.
//
// The plugin is the same for every agent. It reads its settings from
// `concord.json` in the parent directory (the agent's OPENCODE_CONFIG_DIR),
// because environment variables do not reach OpenCode's server process.
// The finish gate is not handled here: `concord run` drives the session and
// asks the gate whenever the agent stops.

import { spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { isAbsolute, join } from "node:path";

let settings = null;
try {
  settings = JSON.parse(readFileSync(new URL("../concord.json", import.meta.url), "utf8"));
} catch {
  settings = null;
}

// OpenCode tool name -> Claude Code tool name understood by the daemon.
const TOOLS = {
  read: "Read",
  grep: "Grep",
  edit: "Edit",
  write: "Write",
  patch: "Edit",
  multiedit: "MultiEdit",
  bash: "Bash",
};

function toolInput(tool, input) {
  const raw = input?.filePath ?? input?.file_path ?? input?.path;
  const out = {};
  if (typeof raw === "string") out.file_path = isAbsolute(raw) ? raw : join(settings.worktree, raw);
  if (typeof input?.pattern === "string") out.pattern = input.pattern;
  else if (typeof input?.query === "string") out.pattern = input.query;
  if (Number.isInteger(input?.offset)) out.offset = input.offset;
  if (Number.isInteger(input?.limit)) out.limit = input.limit;
  return out;
}

function notify(event) {
  const tool = TOOLS[event.tool];
  if (!tool) return null;
  const payload = { session_id: event.sessionID, tool_name: tool, tool_input: toolInput(tool, event.input) };
  const r = spawnSync(
    settings.exe,
    ["hook", "--addr", settings.addr, "--agent", settings.agent, "--event", "PostToolUse"],
    { input: JSON.stringify(payload), encoding: "utf8", timeout: 120000 },
  );
  if (r.status !== 0 || !r.stdout?.trim()) return null;
  try {
    return JSON.parse(r.stdout)?.hookSpecificOutput?.additionalContext ?? null;
  } catch {
    return null;
  }
}

// Output trimming (settings.trim): a large tool output stays in the agent's
// context and is re-read by every later model call. Outputs above the limit
// keep their head and tail (errors and summaries are usually at the end); the
// full output is saved to a file the agent can search. Applies only to
// transient output: commands and searches. Never to file reads or edits, and
// not to fetched web pages: those are reference material the agent works
// from (in a pilot, every trimmed page was re-read about 3 times, while
// trimmed command output was almost never re-read).
const TRIMMED = new Set(["bash", "shell", "execute", "grep", "glob", "list"]);
let spilled = 0;

function trimText(tool, text) {
  const limit = settings.trim?.output ?? 8000;
  if (typeof text !== "string" || text.length <= limit) return text;
  const head = Math.floor(limit * 0.3);
  const tail = limit - head;
  let where = "";
  try {
    mkdirSync(settings.trim.dir, { recursive: true });
    const file = join(settings.trim.dir, `${settings.agent}-${tool}-${Date.now()}-${spilled++}.txt`);
    writeFileSync(file, text);
    where = ` The full output is in ${file}; search it with grep or read parts with sed if you need more.`;
  } catch {
    // Without a file, the trimmed text still says what was left out.
  }
  const note = `\n[concord: ${text.length - head - tail} of ${text.length} characters omitted here to keep your context small.${where}]\n`;
  return text.slice(0, head) + note + (tail > 0 ? text.slice(text.length - tail) : "");
}

function trimResult(event) {
  if (!settings?.trim || !TRIMMED.has(event.tool) || event.status !== "completed") return;
  const r = event.result;
  if (!r) return;
  if (typeof r.content === "string") {
    event.result = { ...r, content: trimText(event.tool, r.content) };
  } else if (Array.isArray(r.content)) {
    event.result = {
      ...r,
      content: r.content.map((c) => (c?.type === "text" ? { ...c, text: trimText(event.tool, c.text) } : c)),
    };
  }
}

export default {
  id: "concord",
  setup(ctx) {
    if (!settings?.enabled && !settings?.trim) return;
    ctx.tool.hook("execute.after", async (event) => {
      try {
        trimResult(event);
      } catch {
        // Never break the agent's tool call because of Concord.
      }
      if (!settings?.enabled) return;
      try {
        const notice = notify(event);
        if (notice && event.status === "completed" && Array.isArray(event.result?.content)) {
          event.result = { ...event.result, content: [...event.result.content, { type: "text", text: notice }] };
        }
      } catch {
        // Never break the agent's tool call because of Concord.
      }
    });
  },
};
