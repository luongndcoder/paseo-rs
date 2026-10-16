# paseo-rs

Rust implementation of the Paseo daemon, migrated strangler-style from the TS daemon in
`../paseo-vietnamese-voice/packages/server`. Plan: `../paseo-vietnamese-voice/docs/refactors/rust-daemon-plan.md`.

Crates:
- `paseo-protocol` — wire types generated from zod JSON Schema + golden conformance tests.
- `paseo-terminal-worker` — drop-in replacement for the Node terminal worker (`PASEO_TERMINAL_WORKER=rust`), JSONL over stdio.
- `paseo-claude` — native Claude Code CLI client (stream-json stdio + control protocol).
- `paseo-codex` — native Codex CLI app-server client (JSON-RPC over stdio).
- `paseo-acp` — generic ACP stdio client with per-agent launch profiles.
- `paseo-opencode` — OpenCode server client (HTTP + SSE).
- `paseo-pi` — Pi / Oh My Pi local RPC client (JSONL over stdio).
- `paseo-git` — checkout/git domain logic (strangler port of the TS `checkout-*` modules).
- `paseo-files` — file explorer, directory suggestions, download framing, upload.
- `paseo-file-observer` — recursive file observer (notify) and git root observers.
- `paseo-schedule` — scheduled agents: cron cadence, JSON schedule store, run bookkeeping.
- `paseo-provider-host` — Rust client for the TS provider host (JSONL-RPC over stdio).

Commands: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build --release`.
