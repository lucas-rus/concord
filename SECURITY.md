# Security Policy

## Local-First Execution Model

Concord runs locally against your git repository and communicates with local AI agent CLIs (OpenCode, Claude Code) over loopback TCP (`127.0.0.1`).

- **Remote / container daemon authentication:** When exposing Concord's live daemon across container or network boundaries, configure `serve_with_auth` and set `CONCORD_AUTH_TOKEN` so unauthenticated requests are rejected.
- **No telemetry:** Concord does not send source code, symbol graphs, or run logs to any external service.

## Reporting a Vulnerability

If you discover a security vulnerability (e.g., command injection in git/shell wrappers, path traversal in MCP tool arguments, or daemon auth bypass), please open a private security advisory on GitHub at:

https://github.com/lucas-rus/concord/security/advisories/new
