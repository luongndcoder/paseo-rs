# P4 WP0 investigations

Paths are in the TS repo (`/path/to/paseo`) unless noted.

## Q13. Plugin lifecycle consumers

Sources: `packages/server/src/server/plugins/{index,runtime}.ts`, `plugins/lifecycle/index.ts`,
`agent/agent-manager.ts`, `session.ts`, `bootstrap.ts`, `plugin-examples/*`, `packages/plugin/src/server/lifecycle.ts`.
No built-in plugin under `plugins/` registers a lifecycle hook; the consumers are third-party plugins
and the three examples in `plugin-examples/` (`lifecycle-actions`, `lifecycle-logger`, `agent-configuration`).

### Hook catalog

`before` hooks are awaited and rewrite the request. Every plugin that registered the hook runs in
plugin-id order (`localeCompare`), one at a time, each with a 30 s RPC timeout
(`REQUEST_TIMEOUT_MS`, `runtime.ts:42`). Any throw or timeout aborts the operation with
`Plugin <id> before <name> failed: <message>`. A hook that returns `undefined` leaves the request
unchanged.

| Hook | Call site | When | What a hook may change |
|---|---|---|---|
| `agent.create` | `agent-manager.ts:1298` | start of `createAgentInternal`, skipped when `config.internal` | whole `config` and `env`, except `config.cwd` (validated); examples swap provider/model/mode and add env |
| `agent.session_open` | `agent-manager.ts:5373` (`buildLaunchContext`) | every session open: reasons `create`, `resume`, `refresh`, `import`, purposes `interactive` and `history`; NOT skipped for internal agents | `env` only (`validateBeforeResult` throws on any other field) |
| `workspace.create` | `session.ts:6751` (`createRequestedWorkspace`) | before the local/worktree handler, once per operation (retries fingerprint the original input and do not rerun hooks) | whole request, including `source.kind` (the example turns a directory into a worktree) |

Events are fire-and-forget (`runtime.emit` returns `void`). Per plugin one `hook` RPC is sent per
event; a failure is appended to that plugin's stderr log and never reaches the daemon. Inside the
plugin process handlers run sequentially and their errors are logged and swallowed
(`PluginHookHandlers.invoke`). Pending sends are tracked so `bootstrap.ts:1904` can
`drainEvents()` at shutdown (bounded by `AGENT_CLOSE_TIMEOUT_MS`).

| Event | Call site | State at emit time |
|---|---|---|
| `agent.created` | `agent-manager.ts:1336` | after `registerSession` (agent in the manager; persistence is a subscriber, not awaited); skipped for internal agents |
| `agent.closed` | `agent-manager.ts:3873` (`emitClosedAgent`) | after `emitState`; skipped for internal |
| `agent.archived` | `agent-manager.ts:1972` (`persistArchivedRecord`) | after the registry upsert, so persisted state is already written; only on the first archive, skipped for internal |
| `agent.turn_started`, `agent.turn_ended`, `agent.permission_requested`, `agent.permission_resolved` | `publishAgentStream`, `agent-manager.ts:5178` | after `dispatch(agent_stream)`; only for non-`timeline` stream events of non-internal agents; `turn_ended` carries `timeline: timelineStore.getItems(agentId)`, a snapshot of all projected items taken at emit time |
| `workspace.created` | `workspace-provisioning-service.ts:310` | after the workspace record exists |
| `workspace.archived` | `bootstrap.ts:980` via `workspaceRegistry.subscribeToMutations` (`kind === "archive"`) | after the registry mutation committed |

### What consumers assume

- `lifecycle-actions` reads `event.timeline` on `agent.turn_ended` (`latestOutputText`) and reacts by
  `context.paseo.agents.ref(id).send(...)` or `respondToPermission(...)`. So (1) the timeline snapshot
  must include the turn's final rows (rows are recorded before the terminal event dispatches), and
  (2) plugin code calls back into the daemon session (`bindPaseoSessionHost(wsServer)`,
  `bootstrap.ts:1828`) while the event is in flight.
- Nothing found assumes events are synchronous with persisted state. The only ordering that exists
  today is incidental: `agent.archived` and `workspace.archived` fire after the write, the rest fire
  after in-memory registration.
- Events for one plugin are delivered in emit order over one IPC channel; across plugins there is no
  ordering. Nothing depends on cross-plugin order.
- `before` hooks are the only blocking contract. A plugin can veto or rewrite creation, and a slow
  plugin delays creation by up to 30 s per hook.

### Consequences for the Rust core

1. Every `emit` event can be an async notification `internal.plugins.lifecycle.emit` as long as it
   is sent from the actor in the same order as the TS call sites, over one ordered channel to TS, and
   after the state change it follows (persist-then-emit for `agent.archived`).
2. Compute the payload on the actor at emit time. `turn_ended.timeline` must be captured
   synchronously from the `TimelineState`; reading it later races with the next turn.
3. Core needs a shutdown flush equivalent to `drainEvents` (wait until TS acks the queued emits,
   bounded) before TS stops plugins.
4. The three `before` hooks need a request/response (`internal.plugins.lifecycle.before.request`),
   keep the 30 s timeout, the error text above, and the validation rules (`agent.create` cwd lock,
   `agent.session_open` env-only). `agent.session_open` also fires for `history` purpose and for
   internal agents, so Rust must call it from the same place (launch-context build), not from the
   public create path only. `workspace.create` stays in the TS session while workspaces are created
   there.
5. `workspace.archived` is not emitted by the agent manager; it comes from the registry mutation
   stream, so the registry owner (P4a) must emit it, not P4b.
6. Plugin-originated calls (`paseo.agents.*`) need a route into the Rust core once agents move; the
   TS session host they bind to will no longer own agents.

Answer to Q13: the `emit` hooks tolerate async delivery; the `before` hooks do not (they gate
create/resume/refresh/import and workspace creation) and need an awaited core -> TS round trip. This
matches risk 12 and open question Q16 in the spec.

## Q14. App dependence on cross-domain frame ordering

Domains today share one socket and one process, so the app has never seen reordering between them.
Sources: `packages/app/src/contexts/session-context.tsx`, `runtime/directory-sync/*`,
`timeline/turn-liveness.ts`, `utils/agent-directory-update-policy.ts`, `packages/client/src/daemon-client.ts`,
`docs/timeline-sync.md`.

### Dependencies found

1. **Turn liveness crosses domains.** `agent_stream` turn events become `stream_open`/`stream_close`
   transitions (`session-context.tsx:390`) applied to `session.agents[agentId].turn`
   (`AgentStoreProjection.applyTurn`). `applyTurn` returns unchanged when the agent is not yet in
   `session.agents` (`agent-store.ts`, `if (!agent) return current`), so a `turn_started` that
   arrives before the agent's first `agent_update`/`fetch_agents` entry is dropped, not buffered. It
   is only recovered because the later agent snapshot carries the active turn (`mergeSnapshotTurn`).
   Rust must therefore keep `agent_update`/`agent_state` for an agent ahead of its stream events, or
   make sure the snapshot reports the running turn.
2. **A snapshot overwrites liveness.** `mergeSnapshotTurn` reduces `{type: "snapshot"}`: an idle
   snapshot closes an open turn. The only guard is `acceptAgentDirectoryUpdate`: a snapshot with
   `updatedAt` older than the stored one is ignored. So per-agent `updatedAt` monotonicity is
   load-bearing. TS bumps it (`touchUpdatedAt`) before it dispatches `turn_started`
   (`agent-manager.ts:2658-2668`); the spec's "+1 ms if the clock did not advance" rule must hold
   across the agent_state and agent_stream paths.
3. **Accepted prompt: liveness before the canonical row.** `docs/timeline-sync.md:218-220` and
   `agent-manager.ts:2663-2680` publish `turn_started` first and then record/dispatch the canonical
   `user_message`, so the client applies authoritative activity before it retires optimistic
   activity. Spec section 9 says the opposite ("canonical user row before turn liveness"); that
   wording is wrong for accepted turns and is right only for daemon-handled commands that allocate no
   turn (`tryRunOutOfBand`, `agent-manager.ts:2480`: row recorded before handler output).
   `docs/timeline-sync.md:228` states the second case.
4. **Timeline cursors are single-domain.** Gap detection and `after` catch-up use per-agent
   `{epoch, seq}` from `agent_stream` and `fetch_agent_timeline_response`
   (`readCursor`, `session-context.tsx:437`). `agent.timeline.replacement` triggers a refetch.
   Stream, fetch, replacement and `set_subscription` must come from one process (already in spec 9).
5. **Directory collections are independent.** projects, workspaces and agents each carry their own
   `generation`/`seq` and are applied per source (`isCurrent(client, source)` in
   `directory-sync/index.ts`). No code found that requires `workspace_update` before the agent that
   lives in it, or the reverse. Modern daemons give each agent a `workspaceId` and a `project`
   placement in its own payload (`agent-store.ts` `applyDelta`), so an early agent is shown without
   its workspace and appears in the right group once the workspace arrives.
6. **Legacy-only coupling.** For daemons without `features.workspaceMultiplicity`, the client
   matches agents to workspaces by cwd (`agent-store.ts:94`) and synthesizes `workspace_update` from
   `agent_update` (`client/src/connection/legacy-workspaces.ts`). Rust is a modern daemon, so this
   does not apply.
7. **Create RPC vs updates.** `createAgent` resolves on the correlated `agent.create.response`
   (or legacy `status`); no code found that needs `agent_update` to precede it.

### Recommendation

Spec section 9 keeps the right rule (one process per message family per phase). Add two explicit
constraints: (a) never emit `agent_stream` turn events for an agent before that agent's directory
entry has been sent on the same socket, and (b) keep the turn-open sequence `touchUpdatedAt` ->
`turn_started` -> canonical row -> `agent_state`, as in `agent-manager.ts:2658-2690`. Correct the
section 9 sentence about the canonical row.

## Q2. Socket high-water (verified)

`MAX_PHYSICAL_SOCKET_BUFFERED_BYTES = 64 * 1024 * 1024` at
`packages/server/src/server/websocket/physical-socket.ts:4`; `docs/plan.md:75` and spec Q2 already say
64 MiB. Nothing to change.

## Corrections to the spec found while capturing

- Tool count: the default catalog registers 39 tools, plus `speak` (voice) = 40, plus 22
  `browser_*` tools when browser tools are enabled with a broker = 62. Section 7's "41 tools" and
  `docs/p4-core-spec.md` risk 16 are off; `tool-catalog/tools-list.json` is the authoritative list.
  `speak` is exempt from `paseoToolPolicy` (`isPaseoToolEnabled`), `policy.enabled === false` removes
  everything else.
- Tool schemas depend on the caller: `create_schedule.provider` is required without a caller agent
  and optional with one, and `create_agent` swaps its input schema, so `ToolCatalogFactory::build`
  needs `caller_agent_id` to select the schema (goldens: `default` vs `caller_agent`).
- MCP `tools/list` carries only `title`, `description`, `inputSchema` (`mcp-server.ts` registers no
  `outputSchema` and no annotations); the output schemas matter only to native host tools
  (`serializePaseoToolInputParameters` and friends).
- An unmapped message type does not deny in TS: `SessionAuthorization.allows*` throws
  `TypeError: Cannot read properties of undefined (reading 'some')` (`authz/behavior.json`). The
  spec's "a missing entry denies" is Rust-side hardening, not parity.
- Directory quirk Q12 at service level: when a non-background reader's snapshot omits a
  background-only workspace, the shared `VersionedCollection` writes a tombstone for it and the
  background reader re-adds it under a new seq (`directory/service-scenarios.json`,
  `background_reader_alternation`). So the "vanishes without a tombstone" wording describes the
  session layer (what each reader is shown), not `DirectorySyncService`.
- Directory ordering of same-seq values uses ICU `localeCompare` (en-US, ICU 78.3), not byte order:
  `a < A < b < B`, `_` before `-`, digits after punctuation. Rust needs an ICU-compatible collator
  (`locale-compare.json` has 1369 expected comparisons).
- `fingerprint` is `JSON.stringify`, so integer-like keys reorder, `undefined` equals missing,
  `-0`/`NaN`/`Infinity` collapse (`fingerprint-cases.json`).
- `docs/timeline-sync.md:218-223` contradicts spec section 9 on user-row vs liveness order (see Q14.3).
- The provider fixtures in `crates/paseo-*/fixtures` carry only the provider side; the ACP and
  OpenCode replays therefore cover the update-translation seam (`translateSessionUpdate`,
  `translateOpenCodeEvent`) and not turn start/finish, which come from the prompt controllers.
