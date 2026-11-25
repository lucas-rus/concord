# Contributing to Concord

Thank you for your interest in contributing to **Concord**!

## Development Setup

Requirements: **Rust ≥ 1.90** and **git ≥ 2.38**.

```bash
git clone https://github.com/lucas-rus/concord && cd concord
cargo test --all
```

## Quality Checks

Before opening a pull request, please ensure all checks pass cleanly:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all --locked
```

## Workspace Layout

- `crates/concord-core` — language-independent symbol/contract model, compatibility classifier, coherence directory, and exhaustive BFS protocol model checker (`tests/protocol_model_check.rs`).
- `crates/concord-lang` — Tree-sitter & schema front-ends (Python, TypeScript/JS, Rust, Go, C/C++, SQL/Prisma/Protobuf/OpenAPI, and cross-language HTTP routes).
- `crates/concord-git` — git plumbing, in-memory `merge-tree`, patience/diff3 verified insertion-union, worktree snapshots.
- `crates/concord-cli` — CLI subcommands, live daemon, 12-tool MCP server, SASE MRP generator, task planner, and Tier-2 type-checker wrapper.

## Adding a New Language Front-End

1. Add the `tree-sitter-<lang>` crate to workspace `Cargo.toml` and `crates/concord-lang/Cargo.toml`.
2. Implement `crates/concord-lang/src/<lang>.rs` exposing `is_<lang>_path` and `index_file`.
3. Register it in `crates/concord-lang/src/lib.rs` (`is_supported_path`, `index_file`, `is_test_path`).
4. Add unit tests covering contract vs. body changes, imports/includes, and breaking call-site detection.

## License

Contributions are dual-licensed under **MIT OR Apache-2.0**, matching the repository.
