# paseo-rs

Rust implementation of the Paseo daemon, migrated strangler-style from the TypeScript daemon in
[getpaseo/paseo](https://github.com/getpaseo/paseo) (`packages/server`).

The TypeScript-side changes this port depends on (shims, Rust edge wiring, provider host, preview release)
live in the fork [luongndcoder/paseo](https://github.com/luongndcoder/paseo), branches `feat/rust-shims`,
`feat/rust-edge`, `feat/rust-provider-host` and `release/rust-preview`.

## Status

- [docs/STATUS-2026-10-17.md](docs/STATUS-2026-10-17.md) — current state and open items
- [docs/benchmarks.md](docs/benchmarks.md) — performance numbers
- [docs/p4-core-spec.md](docs/p4-core-spec.md) — core spec
- Deploy: see `DEPLOY.md` on the `release/rust-preview` branch of the fork.

## Running tests that need the TS checkout

Some tests and capture scripts need a checkout of the TS repo. Set `PASEO_REPO` to its root
(optionally `PASEO_PROVIDER_HOST_SERVER_DIR` to point at `packages/server`). Without it those tests print
`SKIP: ...` and pass.

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
- `paseod` — Rust edge: owns the public port and fronts the TS daemon (P2, lib + bin).
- `paseo-speech` — speech for the daemon: dictation streams, STT/TTS providers, VAD (`sherpa` feature off by default).

Commands: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build --release`.
