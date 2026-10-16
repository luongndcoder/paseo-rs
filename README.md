# paseo-rs

Rust implementation of the Paseo daemon, migrated strangler-style from the TS daemon in
`../paseo-vietnamese-voice/packages/server`. Plan: `../paseo-vietnamese-voice/docs/refactors/rust-daemon-plan.md`.

Crates:
- `paseo-protocol` — wire types generated from zod JSON Schema + golden conformance tests.
- `paseo-terminal-worker` — drop-in replacement for the Node terminal worker (`PASEO_TERMINAL_WORKER=rust`), JSONL over stdio.

Commands: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, `cargo build --release`.
