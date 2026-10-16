# paseod edge spec (P2)

Source: `docs/plan.md` P2. TS refs are relative to `packages/server/src/server/` (worktree `paseo-vietnamese-voice/.claude/worktrees/rust-edge`) unless prefixed `protocol/` or `relay/` (`packages/...`).
Line numbers are from branch `feat/rust-edge` at 92e946240; re-check before relying on them.

## 1. Goal and non-goals

The edge owns the public listening port. The TS daemon becomes an internal backend on a loopback port. A TS client sees no change, and TS sees a normal client.

P2 is a transparent front door. `OwnedTypes` is empty, so every frame is forwarded byte-for-byte. The peek-route and dispatch hook exist for P3 (`src/routing.rs`).

Not in P2: moving any message domain, agent state, `$PASEO_HOME` files (TS stays sole writer), pairing, Hub, service-proxy, or auth decisions.

## 2. TS behavior map

| Area | TS today | P2 owner |
| --- | --- | --- |
| Listen address | `PASEO_LISTEN` > `daemon.listen` in config.json > `127.0.0.1:${PORT\|6767}` (`config.ts:466`); grammar in `parseListenString` (`bootstrap.ts:43`); `httpServer.listen` `bootstrap.ts:1860`, unix socket unlink `:1862` | Edge (public). TS gets an ephemeral internal port |
| WS upgrade path | `WebSocketServer({server, path:"/ws", maxPayload: 100 MiB, handleProtocols, verifyClient})` `websocket-server.ts:818-840`; `MAX_WEBSOCKET_MESSAGE_BYTES` `protocol/src/websocket-message-limit.ts:2` | Edge accepts `/ws`, enforces the same 100 MiB max message on the client side |
| Host check (DNS rebinding) | `verifyWsUpgrade` `websocket-server.ts:878-915`: `isHostnameAllowed` (`hostnames.ts:57`; defaults localhost, `*.localhost`, any IP; plus `PASEO_HOSTNAMES`/`PASEO_ALLOWED_HOSTS`, config `hostnames`), HTTP twin in `bootstrap.ts:798-807` (TCP only) | TS (edge forwards `Host` verbatim) |
| Origin check | same fn: no Origin OR `*` OR in `allowedOrigins` OR `isWebSocketSameOrigin` (`websocket-server.ts:2927`); allowed set = `PASEO_CORS_ORIGINS` + config `cors.allowedOrigins` + fixed (`paseo://app`, `http://{host\|localhost\|127.0.0.1}:{port}`) `bootstrap.ts:809-827`. HTTP CORS `bootstrap.ts:829-843` | TS (edge forwards `Origin` verbatim). Needs the public port in TS's fixed origins (section 5) |
| Bearer on upgrade (COMPAT headerAuth) | `Authorization: Bearer` or `Sec-WebSocket-Protocol: paseo.bearer.<token>` (`auth.ts:44-80`); bcrypt compare vs stored hash; failure closes 4401 after upgrade (`websocket-server.ts:917-960`, `WS_CLOSE_DAEMON_AUTH_FAILED` `:117`); with a password set, `selectWebSocketProtocol` (`:2962`) must pick a `paseo.bearer.*` protocol | TS. Edge forwards both headers and echoes the selected subprotocol |
| hello auth | `hello.auth` = `{kind:"password"}` \| `{kind:"localCredential"}`; `resolveSessionAdmission` `session-admission-auth.ts:8`; relay may omit auth (COMPAT relayPasswordOptional, `:20`); local credential file `$PASEO_HOME/local-credential` (`local-credential.ts`) | TS. Edge replays hello verbatim and never inspects `auth` |
| Pairing | Relay pairing offer is built by TS from relay config + daemon public key (`pairing-offer.ts`, `connection-offer.ts`). Pairing invitations (`docs/permissions.md`) are documented but have no code in this branch | TS; unchanged |
| hello handshake | 15 s hello timeout, close 4001 (`HELLO_TIMEOUT_MS` `:486`, set `:1343-1355`); invalid hello 4002; incompatible protocol 4003 (`:487-489`); `handleHello` `:1560-1650`; `hello.rejected` frame `:1707-1730`; reply `server_info` (`:1641`, builder `:1781`); unknown reconnect resumes by `sessionConnectionKey(principalId, clientId)` (`:1740-1770`) | TS. Edge replays hello, relays reply |
| server_info | `permissions`, `features.*` (COMPAT-gated), `capabilities`, `serverId`, `hostname`, `version` (`:1781-1960`); re-broadcast on capability change (`:1976`) | TS; edge passes through and only recognises it (outbound `session.message.type=status` with `payload.status=="server_info"`) to start the lease |
| ping/pong lease | Inbound `{"type":"ping"}` on an active connection: `lease.claim` + reply `{"type":"pong"}` (`:2327-2333`); any inbound frame renews (`:2291`); 45 s lease, 10 s sweep, expired closes (`websocket/physical-socket.ts:8-9`, sweep `websocket-server.ts:852-863`); clients ping every 10 s | Edge (plan P2) |
| Outbound high-water | `MAX_PHYSICAL_SOCKET_BUFFERED_BYTES` = **64 MiB** (`physical-socket.ts:4`), check `bufferedAmount + frame <= max` (`:78-81`), closes physical socket (`websocket-server.ts:1256`); relay socket applies the same cap (`encrypted-relay-socket.ts:58`). `docs/plan.md` says 8 MiB: discrepancy, see open questions | Edge, default 64 MiB (configurable) |
| Binary frames | Terminal stream opcodes 0x01 Output, 0x02 Input, 0x03 Resize, 0x04 Snapshot, 0x05 Restore (`protocol/src/binary-frames/terminal.ts:10`), file transfer 0x10 FileBegin, 0x11 FileChunk, 0x12 FileEnd (`file-transfer.ts:4`); decoded `websocket-server.ts` `maybeHandleBinaryFrame` `:2211` (via `demux.ts`); binary before hello rejected `:2227` | Edge forwards unchanged; `routing::binary_family` classifies by first byte |
| Top-level text frames | Inbound `ping`, `hello`, `recording_state`, `session` (`protocol/src/messages.ts:7765`); outbound `pong`, `session`, `hello.rejected` (`:7770`) | Edge peeks `type` and `session.message.type` only |
| Relay | `startRelayTransport` (`relay-transport.ts:109`): control socket `connectControl` `:162`, data socket per client `ensureClientDataSocket` `:345` (metadata `externalSessionKey: session:<id>`, `:379`), E2EE attach `attachEncryptedSocket` `:416`, 10 s ping `CONTROL_PING_INTERVAL_MS :56`, backoff `min(30 s, 1 s*attempt)`; URL `buildRelayWebSocketUrl` (`protocol/src/daemon-endpoints.ts:227`, `/ws?serverId&role=server&v&connectionId`); runtime toggle `relay-runtime.ts:44`, started `bootstrap.ts:1832`, toggled by `relay.enabled` field `:1848`; endpoint env `PASEO_RELAY_{ENABLED,ENDPOINT,PUBLIC_ENDPOINT,USE_TLS,PUBLIC_USE_TLS}` (`config.ts:301-325`) | Edge |
| Relay crypto | tweetnacl box: `nacl.box.before` (X25519 + XSalsa20-Poly1305), bundle `[nonce24][ct]`, all-zero shared secret rejected, 24-byte nonce (`relay/src/crypto.ts`); channel handshake plaintext `e2ee_hello{key,capabilities.binaryCiphertext}` -> `e2ee_ready` (`relay/src/encrypted-channel.ts:48-110`), `HANDSHAKE_RETRY_MS 1000`, `MAX_PENDING_SENDS 200`, rehandshake rejection code 1008; relay frame cap 32 MiB (`websocket-message-limit.ts:5`) | Edge (crate `crypto_box`; cross-check vectors vs tweetnacl) |
| Daemon keypair | `$PASEO_HOME/daemon-keypair.json` `{v:2,publicKeyB64,secretKeyB64}` mode 0600, created by `loadOrCreateDaemonKeyPair` (`daemon-keypair.ts:28`) | TS creates; edge reads only |
| serverId | `$PASEO_HOME/server-id` (`server-id.ts:13`) | TS creates; edge gets it from `paseo:ready` |
| HTTP routes | see section 4 | mostly TS via reverse proxy |
| Supervisor/worker | `scripts/supervisor-entrypoint.ts` -> `supervisor.ts` forks `daemon-worker.ts` with IPC (`supervisor.ts:289-298`); worker sends `paseo:ready{listen,serverId}` (`daemon-worker.ts:331`), `paseo:shutdown`, `paseo:restart`; supervisor-liveness guard only when `process.send` exists (`daemon-worker.ts:243`); supervisor updates `paseo.pid` `listen` on ready (`supervisor-entrypoint.ts:172`; file `pid-lock.ts:103`) | Edge spawns worker; see section 5 |

## 3. Decision table: edge owns vs passes through

Owns in P2:

1. Listen socket (TCP or Unix) and HTTP server.
2. WS physical-socket lifecycle: accept, 100 MiB max message, per-socket internal WS to TS.
3. Top-level application ping/pong and the 45 s lease (after `server_info` seen).
4. Outbound 64 MiB high-water per physical socket (direct and relay).
5. Relay transport + E2EE (control socket, data sockets, crypto, reconnect).
6. TS worker spawn, readiness, crash restart, graceful shutdown.
7. Routing hook (`OwnedTypes`, empty).

Passes through (TS stays authority): Host/Origin/DNS-rebinding checks, bearer/password/localCredential auth, hello validation and 15 s hello timeout, protocol-version check, `server_info`, all `session` messages, binary frames, all HTTP routes including web UI and `/mcp/agents`, service-proxy, Hub, push, config files, pairing.

Why TS keeps Host/Origin/auth: those decisions read mutable TS config (`daemonConfigStore` fields `hostnames`, `cors.allowedOrigins`, `relay.enabled`). Duplicating them in Rust splits one policy across two processes and the two will drift. Edge cost is one extra internal dial per socket.

## 4. HTTP routes (`bootstrap.ts`)

Middleware order (must remain, it is why the whole chain stays in TS): service-proxy host classifier `:793` -> Host allowlist `:798` -> CORS `:829` -> `POST /api/terminal-activity` `:845` -> web UI `:859` -> bearer middleware `:862` -> `express.json` `:865` -> routes.

| Route | Auth | Edge in P2 |
| --- | --- | --- |
| `GET /ws` (WS upgrade) | Section 2 | **Edge-owned** (accept + proxy) |
| `POST /api/terminal-activity` `:845` | token-gated, skips bearer; called by agent shell hooks on the internal URL (`createTerminalActivityUrl` `:275`) | Reverse proxy. Hooks keep calling the internal port directly |
| web UI static (`createWebUiMiddleware`, `web-ui.ts`; skips `/api/ /mcp/ /public/`; gz/br, hashed-asset caching) `:859` | none | Reverse proxy (TS serves). Rust static serving deferred: needs `PASEO_WEB_UI_ENABLED`/`DIST_DIR`, SPA fallback, encoding negotiation parity |
| `GET /public/*` `:868` | bearer | Reverse proxy |
| `GET /api/health` `:871` | exempt (`auth.ts:118`) | Reverse proxy; edge also uses the internal one for readiness |
| `GET /api/status` `:875` | bearer or local credential | Reverse proxy. `listen` must show the public address (section 5) |
| `GET /api/files/download?token=` `:940` | self-authenticating one-shot token | Reverse proxy, streaming, `Content-Length`/`Content-Disposition` preserved |
| `POST/GET/DELETE /mcp/agents` `:1659-1661` | capability token or password (`auth.ts` `isAgentMcpRequestAuthorized`) | Reverse proxy, streaming (MCP streamable HTTP/SSE). Agents spawned by TS call the internal port directly (injected base URL `createAgentMcpBaseUrl :264`) |
| Service-proxy host routes + non-`/ws` Upgrade (`serviceProxy.middleware()` `:793`, `upgradeHandler` `:950`) | by registered route | Reverse proxy / raw upgrade tunnel with `Host` preserved |
| OPTIONS preflight | `:837` | Reverse proxy |

No HTTP route exists for uploads (binary WS frames 0x10-0x12), push (outbound only), or Hub (outbound relationship via `attachExternalSocket` with `transport:"hub"`); those stay inside TS.

## 5. Backend contract (how TS runs behind the edge)

Constants live in `src/internal.rs`. The TS shim is a later stage (`packages/server/src/server/edge-backend.ts` + small hooks), listed in section 7.

**Process model.** `PASEO_EDGE=rust`: the TS supervisor spawns `paseod` instead of forking the worker. `paseod` spawns the worker. The supervisor keeps owning `paseo.pid` (single writer unchanged); it learns readiness from `paseod`'s own lifecycle line (same `@@paseo ` JSONL format on stdout, `listen` = public address). `paseod` forwards worker `paseo:shutdown`/`paseo:restart` the same way. Rationale: Node IPC (`NODE_CHANNEL_FD`) is not worth reimplementing in Rust; JSONL on stdout is portable (Windows too).

**Worker launch** (edge sets env, never argv, for secrets):

| Var | Value |
| --- | --- |
| `PASEO_LISTEN` | `127.0.0.1:<free port chosen by edge>` (TCP always, even when the edge listens on a Unix socket, because service-proxy and MCP base URLs assume TCP) |
| `PASEO_EDGE_TOKEN` | 32 random bytes base64url, per run. Presence enables edge-backend mode |
| `PASEO_EDGE_PUBLIC_LISTEN` | the edge's `PASEO_LISTEN` |
| `PASEO_RELAY_ENABLED` | inherited. The worker keeps reporting the relay config (pairing, status); in edge mode it never opens its own relay transport |
| `PASEO_EDGE_PARENT_STDIN` | removed from the worker env (it only applies to the edge) |
| `PASEO_CORS_ORIGINS` | user value plus the public origins (`http://localhost:<pub>`, `http://127.0.0.1:<pub>`, `http://<host>:<pub>`) until TS derives them from `PASEO_EDGE_PUBLIC_LISTEN` |
| `PASEO_HOME`, `PASEO_PASSWORD`, rest | inherited |

**Readiness.** Worker prints `@@paseo {"type":"paseo:ready","listen":"127.0.0.1:<port>","serverId":"..."}` on stdout. The edge then polls internal `GET /api/health` until 200 (bounded by `ready_timeout`). Before that, `/ws` returns 503 `Server not ready` (the same status TS gives while `connectionLifecycle != "accepting"`, `websocket-server.ts:889`); TS clients already retry on it. If the worker exits or crashes, the state goes `Restarting`; open client sockets are closed 1011 so clients reconnect (TS keeps no WS state across a worker restart anyway).

**Parent liveness.** Worker stdin is a pipe held by the edge; worker exits on EOF (replaces the IPC heartbeat guard at `daemon-worker.ts:243`). Graceful stop: SIGTERM on Unix, `{"type":"paseo:graceful-shutdown"}` line on stdin on Windows.

The edge in turn watches its own stdin when `PASEO_EDGE_PARENT_STDIN=1` (set by the TS supervisor, which holds that pipe): EOF stops the edge and with it the worker, and the shutdown line is accepted on every platform. Linux also sets `PR_SET_PDEATHSIG=SIGTERM`. The flag is opt-in because a daemonized edge with `/dev/null` as stdin would otherwise stop at once.

**Worker secrets.** The worker reads `PASEO_EDGE_TOKEN` once and deletes it (and `PASEO_EDGE_PUBLIC_LISTEN`, `PASEO_EDGE_BACKEND_CMD`, `PASEO_EDGE_PARENT_STDIN`) from its own `process.env`. They are also in `RUNTIME_CONTROL_ENV_KEYS`, so agents and terminals never inherit them. A flag-off supervisor scrubs them from the worker env.

**Forwarding chain.** The edge reads the daemon's `trustedProxies` (`PASEO_TRUSTED_PROXIES`, then `config.json`, default `["loopback"]`). A trusted peer keeps its `X-Forwarded-*`/`Forwarded` headers and the edge appends the peer to `X-Forwarded-For`; any other peer's headers are dropped and `X-Forwarded-For` is the peer. The worker adds loopback (the edge hop) to its Express trust list in edge mode. `trustedProxies` changes need an edge restart.

**Service hosts.** `/ws` is terminated by the edge only when the Host is not a registered script service. The edge asks the worker (`HOST_CLASS_PATH`, token required; IP literals and `localhost` skip the call, failure means daemon) and tunnels service hosts raw, like any other upgrade. The daemon `/ws` dial forwards every client header except handshake, hop-by-hop, forwarding-chain and edge-identity headers.

**Identity forwarded to TS.** Headers on every internal request/upgrade (`internal.rs`):

- `x-paseo-edge-token`: shared secret. TS honours the other `x-paseo-edge-*` headers only when it matches (constant-time).
- `x-paseo-edge-transport`: `direct` | `relay`.
- `x-paseo-edge-remote-addr`: real client `ip:port`. Without it every client looks like loopback (`resolveConnectionPeer` `websocket-server.ts:2819` uses the socket address). Today `peer` feeds logs/identity only, and authz comes from `hello`/permissions, so a missing header is not a security bug but would corrupt logs and any future peer-based policy.
- `x-paseo-edge-connection-id`: relay `connectionId`.

The edge strips these four from every client request before adding its own, so clients cannot forge them. `Host`, `Origin`, `Authorization`, `Sec-WebSocket-Protocol`, `User-Agent` pass unchanged. Express `trust proxy` defaults to `["loopback"]` (`bootstrap.ts` `resolveExpressTrustProxySetting`), so `X-Forwarded-For` from the edge would also be trusted; the edge sets `X-Forwarded-For` to the real client address and drops any inbound value (no chaining).

**Relay clients on the TS side.** With a valid token and `transport=relay`, TS must route the internal `/ws` socket to `attachExternalSocket(ws, {transport:"relay", externalSessionKey:"session:<connectionId>", relayConnectionId})` instead of `attachAuthenticatedSocket`, so relay admission rules (no `hello.auth` required, `session-admission-auth.ts:20`) and session keys are unchanged.

**What breaks without a TS patch** (all listed for stage 2):

1. `/api/status.listen`, service-proxy public URLs/port (`daemonPort` `bootstrap.ts:775`, `getDaemonTcpPort :1254`), and the `paseo.pid` listen value would show the internal port. Fix: `PASEO_EDGE_PUBLIC_LISTEN` overrides what is *reported*, not what is bound. MCP base URL and terminal-activity URL deliberately stay internal.
2. Fixed CORS origins use the bound (internal) port, so a browser at the public origin loses CORS headers. Interim fix is the `PASEO_CORS_ORIGINS` env above.
3. `relay.enabled` toggled at runtime through config RPC: TS must tell the edge. Contract: `POST http://127.0.0.1:<edge-control-port>/internal/edge/relay {"enabled":bool}` with `x-paseo-edge-token`. The edge control port is announced to the worker via env `PASEO_EDGE_CONTROL` (added in stage 2).

**Ping/pong consequences.** The worker no longer sees top-level pings, so it never claims its own lease on the internal socket (no expiry there; the edge lease is the authority). TS counters/metrics for `ping` drop to 0. `websocket-server.liveness.e2e.test.ts` must be run against the edge, not expected to pass unmodified inside TS.

## 6. Edge internals

### WS proxy ("connect-then-accept", `src/ws/mod.rs`)

1. `GET /ws` -> if backend not `Ready`: 503.
2. Dial internal `ws://127.0.0.1:<port>/ws` with copied headers + edge headers.
3. Internal handshake fails with an HTTP status (403 Host/Origin, 503): return that status/body to the client. This keeps `verifyWsUpgrade` behavior exactly observable.
4. Accept the client upgrade echoing TS's selected subprotocol; start the pump.

Pump: client actor, backend reader and backend writer tasks per socket; the controller never awaits socket I/O. Client-bound queue is byte-budgeted (outbound high-water). Backend-bound bytes are capped by `inbound_high_water_bytes` (16 MiB): past it the actor stops reading the client. A direct client socket has its own writer task, so a client that stops reading cannot keep its pings from renewing the lease. Close codes are mirrored (4001/4002/4003/4401/1001), 1005/1006/1015 map to 1011. Client gone -> close internal 1000; TS retains the reconnectable session as for a direct disconnect (`externalSessionsByKey`, `websocket-server.ts:1740`). Lease: starts at the first inbound ping after `server_info` was seen; ping is then answered locally and not forwarded.

Latency budget (plan acceptance): p99 < 1 ms added on `agent_stream` frames. Peek-route runs only when `OwnedTypes` is non-empty; with the empty table forwarding never touches the payload.

### Relay (`src/relay/mod.rs`)

Same state machine as `relay-transport.ts`. Each decrypted data socket implements `PhysicalSocket` and runs through the same `WsProxy::run`, so lease, high-water and routing apply to relay clients too. High-water for relay uses the transport's queued bytes plus the outbound ciphertext length, as `encrypted-relay-socket.ts:50-60`.

### Config (`src/config.rs`)

Same precedence as TS for `PASEO_LISTEN` and relay vars. `$PASEO_HOME/config.json` is read-only in the edge (single-writer rule). Edge-only vars: `PASEO_EDGE_BACKEND_CMD` (JSON argv for the worker), `PASEO_EDGE_OUTBOUND_HIGH_WATER_BYTES`.

### Startup order

Config -> spawn backend -> bind public listener right away (503 until ready) -> on `Ready` start relay (needs `serverId` + keypair file) -> relay lifecycle events to supervisor.

## 7. Stage plan (parallel builders)

| Builder | Files | Depends on |
| --- | --- | --- |
| config + http | `config.rs`, `http/mod.rs`, `main.rs` | contracts only |
| backend | `backend/mod.rs`, `internal.rs` | none |
| ws proxy | `ws/mod.rs` (+ `ws/direct.rs`) | `backend` types, `routing` |
| relay | `relay/mod.rs` | `ws::PhysicalSocket`, `WsProxy::run` |
| TS shim | `edge-backend.ts`, `daemon-worker.ts`, `bootstrap.ts`, `websocket-server.ts`, `supervisor-entrypoint.ts` (all in the worktree) | section 5 |
| acceptance | run `daemon-client.e2e.test.ts`, `client-activity.e2e.test.ts` via edge (single files, `--bail=1`); latency bench; RSS check | all |

## 8. Open questions

1. High-water: TS uses 64 MiB, plan says 8 MiB. Edge defaults to 64 MiB for parity. Switching to 8 MiB would close sockets on a single large `Snapshot`/`FileChunk` frame TS delivers today.
2. Should the edge also answer `hello.rejected`/4401 itself? Spec says no (TS owns auth). Revisit only if P3 needs auth in Rust.
3. Windows named pipes (`PASEO_LISTEN=\\.\pipe\...`): P2 returns a config error. Is Windows edge support required before P6?
4. Reverse-proxied web UI costs one extra hop for static assets. Acceptable for P2, or move static serving to Rust in the same phase?
5. Worker crash policy: respawn indefinitely with backoff, or exit the edge so the supervisor decides? Spec assumes respawn with backoff capped at 30 s and `Failed` after 5 consecutive crashes inside 60 s.
6. `x-paseo-edge-remote-addr` plumbing needs a small TS patch to `extractSocketRequestMetadata`; confirm that touching `websocket-server.ts` is in the TS-shim stage's ownership.
