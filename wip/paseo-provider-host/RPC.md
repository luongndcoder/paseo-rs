# Provider-host RPC (protocol version 1)

The TS provider-host (`packages/server/src/provider-host/main.ts`) exposes the provider registry's
`AgentClient` / `AgentSession` (`agent/agent-sdk-types.ts`) over stdio. One JSON object per line,
UTF-8, `\n` terminated. stdout carries frames only; logs go to stderr (`PASEO_PROVIDER_HOST_LOG_LEVEL`,
default `warn`).

Launch: `node --import tsx src/provider-host/main.ts` with cwd `packages/server`. EOF on stdin closes
all sessions, shuts providers down, and exits.

## Framing

JSON-RPC 2.0 shaped, without the `jsonrpc` field.

| Direction     | Frame                                                       |
| ------------- | ----------------------------------------------------------- |
| client -> host | request `{"id": 1, "method": "...", "params": {...}}`       |
| host -> client | success `{"id": 1, "result": ...}` (void results are `null`) |
| host -> client | failure `{"id": 1, "error": {"code", "message", "data?"}}`   |
| both          | notification `{"method": "...", "params": {...}}` (no `id`)  |

`id` is a number or string chosen by the client. Requests run concurrently; responses can arrive in
any order. A frame that is not valid JSON gets `{"id": null, "error": {"code": -32700}}`.

Error codes: `-32700` parse, `-32600` invalid request, `-32601` method not found, `-32602` invalid
params, `-32603` internal, `-32000` provider error (the provider threw; `message` is its message),
`-32001` unknown provider, `-32002` unknown session handle, `-32003` shutting down, `-32800` canceled.

Payload field names and discriminators are exactly the TS JSON: camelCase fields, snake_case event
`type`s, `AgentSessionConfig`, `AgentPersistenceHandle`, `AgentStreamEvent`, `AgentPermissionRequest`,
`AgentPermissionResponse`, `AgentRunResult`, `FetchCatalogOptions`. `Date` values serialize as ISO strings.

## Sessions and handles

`provider.createSession`, `provider.resumeSession` and `provider.importSession` register the session in
the host and return a **handle** (`s1`, `s2`, ...). Handles are host-local and never reused. Every
`session.*` method takes `handle`. The host subscribes to the session before it writes the response, so
no event can precede the response that announces its handle.

Session info (create/resume result):

```json
{"handle": "s1", "provider": "mock", "sessionId": "..."|null, "capabilities": {...},
 "features": [...], "initialTimeline": [...], "persistence": {...}|null}
```

## Methods: host

| Method            | Params                                                                 | Result                          |
| ----------------- | ---------------------------------------------------------------------- | ------------------------------- |
| `host.initialize` | `protocolVersion?`, `isDev?`, `providers?: string[]`, `providerOverrides?`, `runtimeSettings?` | `{protocolVersion, providers: [{id,label,description,enabled,defaultModeId,modes}]}` |
| `host.shutdown`   | none                                                                   | `null`, then the host exits     |

`host.initialize` may be called once, before any provider call. It builds the registry:
`isDev` (default: env `PASEO_PROVIDER_HOST_DEV=1`) adds dev-only providers such as `mock`; `providers`
restricts the registry to those ids. A `protocolVersion` other than 1 is `-32602` with
`data.supported`. Without `host.initialize` the host uses the env defaults.

## Methods: provider (`AgentClient`)

All take `provider` (id). Clients are created lazily and cached.

| Method                              | Extra params                                | Result                              |
| ----------------------------------- | ------------------------------------------- | ----------------------------------- |
| `provider.list`                     |  (no `provider`)                            | `{providers: [...]}`                |
| `provider.isAvailable`              | `options?: FetchCatalogOptions`             | `{available}`                       |
| `provider.fetchCatalog`             | `options: FetchCatalogOptions`              | `{models, modes, defaultModeId?}`   |
| `provider.getCapabilities`          |                                             | `{capabilities}`                    |
| `provider.listCommands`             | `config: AgentSessionConfig`                | `{commands}`                        |
| `provider.listFeatures`             | `config`                                    | `{features}`                        |
| `provider.listImportableSessions`   | `options?`                                  | `{sessions}`                        |
| `provider.getDiagnostic`            |                                             | `{diagnostic}` or `null`            |
| `provider.archiveNativeSession`     | `persistence`                               | `null`                              |
| `provider.unarchiveNativeSession`   | `persistence`                               | `null`                              |
| `provider.createSession`            | `config`, `launchContext?`, `options?`      | session info                        |
| `provider.resumeSession`            | `persistence`, `overrides?`, `launchContext?`, `options?` | session info          |
| `provider.importSession`            | `input`, `context`                          | `{session, config, persistence, timeline, providerSubagentEvents}` |

`fetchCatalog` goes through the registry definition, so configured model overrides are merged as in the
daemon. `isAvailable` and `fetchCatalog` receive an `AbortSignal` wired to `$/cancel`.

## Methods: session (`AgentSession`)

| Method                          | Extra params                          | Result                                  |
| ------------------------------- | ------------------------------------- | --------------------------------------- |
| `session.run`                   | `prompt`, `options?`                  | `AgentRunResult`                        |
| `session.startTurn`             | `prompt`, `options?`                  | `{turnId}`                              |
| `session.steerActiveTurn`       | `prompt`, `options` (`expectedTurnId`) | `{status: "accepted"\|"unavailable"}`  |
| `session.tryHandleOutOfBand`    | `prompt`                              | `{handled}`; events stream as notifications |
| `session.streamHistory`         |                                       | `{events: AgentStreamEvent[]}` (collected) |
| `session.getRuntimeInfo`        |                                       | `AgentRuntimeInfo`                      |
| `session.getAvailableModes`     |                                       | `{modes}`                               |
| `session.getCurrentMode`        |                                       | `{modeId}`                              |
| `session.setMode`               | `modeId`                              | `{notice}`                              |
| `session.setModel`              | `modelId` (string or null)            | `null`                                  |
| `session.setThinkingOption`     | `thinkingOptionId` (string or null)   | `{notice}`                              |
| `session.setFeature`            | `featureId`, `value`                  | `null`                                  |
| `session.getPendingPermissions` |                                       | `{requests}`                            |
| `session.respondToPermission`   | `requestId`, `response`               | `{result}` (`AgentPermissionResult` or null) |
| `session.describePersistence`   |                                       | `{persistence}`                         |
| `session.listCommands`          |                                       | `{commands}`                            |
| `session.revertConversation` / `revertFiles` / `revertBoth` | `messageId` | `null`                    |
| `session.interrupt`             |                                       | `null`                                  |
| `session.close`                 |                                       | `null`, then notification `session.closed` |

`prompt` is a string or an `AgentPromptContentBlock[]`. Optional provider methods the session lacks return
`null`/empty results (`steerActiveTurn` returns `unavailable`); a missing revert method is `-32600`.

## Notifications (host -> client)

- `session.event`: `{handle, seq, event}`. `event` is an `AgentStreamEvent`. `seq` starts at 1 per handle
  and increases by 1. Delivery order equals provider emit order. Events from `run`, `startTurn`, out-of-band
  handlers and permission responses all arrive this way.
- `session.closed`: `{handle}`, after `session.close` completes.

## Permission round-trip

Permissions are events, as in the TS interface. There is no host-initiated request.

1. A turn raises `session.event` with `event.type == "permission_requested"` and `event.request`.
   The request stays in `session.getPendingPermissions` until answered.
2. The client answers with `session.respondToPermission {handle, requestId, response}`.
   `response` is `{"behavior": "allow", "selectedActionId?", "updatedInput?", "updatedPermissions?"}` or
   `{"behavior": "deny", "selectedActionId?", "message?", "interrupt?"}`.
3. The provider emits `permission_resolved` and the blocked turn continues. A `run` waiting on the
   permission stays pending until then.

The Rust client's `PermissionHandler` trait automates steps 1-2; the event is still delivered to the stream.

## Cancellation

Client notification `{"method": "$/cancel", "params": {"id": <request id>}}`:

- aborts the request's `AbortSignal` (`provider.isAvailable`, `provider.fetchCatalog`);
- for `session.run`, calls `session.interrupt()`. The provider then ends the turn: `turn_canceled` is
  emitted and `run` resolves with `canceled: true`.
- anything else, or an already finished request: the notification is ignored. A canceled request that
  throws returns `-32800`.

`session.startTurn` turns are canceled with `session.interrupt`.

## Shutdown

`host.shutdown` aborts in-flight requests, closes every session, runs `AgentClient.shutdown()` for each
instantiated client, replies, then the process exits. stdin EOF does the same. Requests after shutdown
starts get `-32003`.
