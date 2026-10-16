# paseo-opencode

Native Rust client for the OpenCode server (HTTP + SSE), P5 of `docs/plan.md`. Standalone workspace; it mirrors the
shape of the sibling `paseo-claude` crate: an options builder, a `Transport` trait, typed messages with a `Value`
fallback, and golden traffic replay.

```text
transport / http / sse      bytes on the wire (Transport trait, reqwest, SSE parser)
protocol + client           typed REST calls and /global/event events (raw provider layer)
server / manager / events   `opencode serve` lifecycle, leases, reconnecting event hub
agent                       AgentClient / AgentSession: provider-agnostic surface (agent-sdk-types.ts)
replay                      Transport that serves recorded traffic
```

## Use

```rust
let client = OpenCodeAgentClient::new(OpenCodeOptions::new());          // spawns `opencode serve` on demand
let session = client.create_session(config, None, Default::default()).await?;
let mut events = session.subscribe();                                   // raw Provider events + lifecycle events
let result = session.run(text_prompt("hello"), Default::default()).await?;
session.close().await?;
client.shutdown().await;
```

The surface matches `AgentClient` / `AgentSession`: create/resume session, prompts with images, event stream,
`interrupt`, permission request/response (tools and questions), modes, models, thinking options, slash commands,
`fetch_catalog`. Events are raw (`AgentEvent::Provider` carries the untouched OpenCode event); Paseo's timeline and
`ToolCallDetail` mapping is P4 and absent on purpose. Call `close()` on every session: it releases the server lease
(and deletes the native session when created with `persist_session: Some(false)`).

## Tests

```bash
export CARGO_BUILD_JOBS=4
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
cargo test --test live -- --ignored --nocapture      # real opencode + model, needs the CLI and network
```

- `tests/replay.rs`: every fixture frame round-trips, and each scenario runs the Rust client against recorded
  traffic, asserting it sent the same requests (method, path, body) as the TS adapter and made all of them.
- `tests/http_transport.rs`: the reqwest transport against an in-process server (auth, directory encoding, 404,
  204, chunked SSE split mid-frame).
- `tests/live.rs`: ignored; secured server, prompt/reply, bash permission round trip, cleanup.

Fixtures and how they were recorded: `tools/capture/README.md`. Wire details: `PROTOCOL.md`.

## Bridge plugin: reuse as-is?

`packages/server/scripts/build-opencode-bridge-plugin.mjs` bundles two ES-module plugins with esbuild
(`opencode/bridge-plugin.mjs` for OpenCode 1.x, `opencode/v2/bridge-plugin.mjs` for 2.x; zod is bundled in).
They run inside the OpenCode process, fetch `GET /_internal/opencode/tools` from a loopback HTTP server with
`Authorization: Bearer <token>`, register each as an OpenCode tool `paseo_<name>`, and call
`POST /_internal/opencode/sessions/{id}/tools/{name}` (and `GET .../sessions/{id}/context?tools` in v2) on use.

Rust can reuse the bundles unchanged. What it must supply is the other half, `bridge.ts`: that loopback server
(about 250 lines, axum/hyper), the manifest serialization from the Rust tool catalog (P4), and server env injection
(`OPENCODE_CONFIG_CONTENT` with `plugin: [[<file url>, {baseUrl, token}]]` for 1.x, `plugins: [{package, options}]`
for 2.x), plus shipping the `.bundle.mjs` files next to the binary (they are produced by Node/esbuild at build time).
Not wired in this crate yet because the tool catalog is a P4 concern; `Acquire::Dedicated(env)` and
`AgentLaunchContext::env` are the hooks for it.

## Gaps versus the TS adapter

Not ported (the TS file is ~5.5k lines):

- OpenCode 2.x (`/api/*`, `@opencode/client`, the `v2/` adapter and the `--version` major-version switch).
- Bridge/Paseo native tools (see above) and `McpServerConfig` is supported but untested against a live server.
- Timeline, `ToolCallDetail`, sub-agent presentation, child-session events (events for other sessions are ignored),
  user-message dedupe/materialization.
- Steering (`steerActiveTurn`), out-of-band prompts, `revertConversation`/`revertFiles` (only `revert_both`).
- Auto-accept feature and legacy `full-access` mode (mapped to `build` without auto-accept), `listFeatures`.
- Importable session listing, `importSession`, archive/unarchive native session.
- Server-exit recovery: a session whose server died fails with `ServerExited`; the TS adapter reconnects.
- `streamHistory` returns raw messages (no reverted-message filtering or timeline replay).
- Slash-command header-timeout tolerance, compaction suppression until idle, structured-output message handling
  beyond passing `format`.
- Provider catalogue extras: refresh deadlines/abort signals, metadata concurrency limit, configured models,
  `getCatalogCacheKey`, `resolveCreateConfig`.
- Managed-process registry records, Windows `.cmd` binary resolution, tree-kill on Windows (Unix uses process groups).
- Version drift: fixtures come from OpenCode 1.18.35; the TS adapter pins SDK 1.14.46. Everything the crate relies on
  is present in both, but pin a supported CLI range before relying on it (as the plan does for Claude).
