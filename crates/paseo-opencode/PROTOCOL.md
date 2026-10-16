# OpenCode wire protocol (1.x REST + SSE)

Observed from OpenCode 1.18.35 (the TS adapter is written against `@opencode-ai/sdk` 1.14.46). The server also
serves a `/api/*` surface for OpenCode 2.x clients (`@opencode/client` 2.0.10, used by `providers/opencode/v2/`);
this crate does not speak it.

## Process

`opencode serve --hostname 127.0.0.1 --port <free port>` in a neutral cwd (`$PASEO_HOME/opencode-home`).
Ready when stdout prints `... listening on http://127.0.0.1:<port>`. Basic auth (user `opencode`) when
`OPENCODE_SERVER_PASSWORD` is set; this crate sets it (and `OPENCODE_PASSWORD`) to a random value per server
unless `OpenCodeOptions::unsecured()`. The TS 1.x adapter runs unsecured.

## Requests

Every project-scoped request carries the directory twice, URI-encoded: query `?directory=` and header
`x-opencode-directory`. `GET /global/event` carries neither.

| Call | Notes |
| ---- | ----- |
| `POST /session` | body omitted, or `{permission: [{permission, pattern, action: ask|allow|deny}]}` |
| `GET /session/{id}`, `PATCH /session/{id}`, `DELETE /session/{id}` | delete returns `true` |
| `GET /session/{id}/message` | `[{info, parts}]` |
| `GET /session/status` | busy/retry sessions only; idle sessions are absent |
| `POST /session/{id}/prompt_async` | 204. `{messageID, parts, model:{providerID,modelID}, agent, variant, system, format}` |
| `POST /session/{id}/command` / `summarize` / `revert` / `abort` | `abort` returns `true` |
| `POST /permission/{id}/reply` | `{reply: once|always|reject, message?}` |
| `POST /question/{id}/reply` / `reject` | `{answers: string[][]}` |
| `GET /provider` | `{all: [{id, name, source, models: {id: {name, family, limit, variants}}}], default, connected}` |
| `GET /agent`, `GET /command`, `POST /mcp` | modes, slash commands, MCP registration |

Image prompt parts are `{type:"file", mime, filename:"attachment-N.ext", url:"data:<mime>;base64,..."}`.
Message ids are client generated and ascending: `msg_` + 12 hex of `ms * 0x1000 + counter` + 14 random base62.

## Events: `GET /global/event`

One SSE stream per server, all directories. Frame: `{directory?, project?, payload: {id, type, properties}}`.
First frame `server.connected` (ready signal), `server.heartbeat` about every 10 s. Session events used here:
`session.created|updated|deleted|status|idle|error|diff|compacted`, `message.updated`, `message.part.updated`,
`message.part.delta {field:"text", delta}`, `permission.asked|replied`, `question.asked|replied|rejected`,
`todo.updated`. Other types (`sync`, `plugin.added`, `catalog.updated`, ...) arrive as `Event::Unknown` and are
preserved verbatim.

Turn lifecycle: `prompt_async` -> `session.status busy` -> message/part events -> `session.status idle` +
`session.idle`. Idle events also fire on abort and on session delete, so a turn only ends on idle after the
server showed activity for it. `permission.asked` blocks the tool until `POST /permission/{id}/reply`.

Interrupt: `POST /session/{id}/abort` returns `true` quickly (1.14.42+ may block until tools stop, hence the 2 s
cap); the TS adapter acknowledges the cancel when the abort succeeds, not when idle arrives.

## Types

Wire types keep unknown fields in `extra` and unions fall back to `Unknown(Value)`: `parse -> serialize` is
lossless up to explicit `null` vs absent keys (OpenCode sends `null` for unset optionals; the typed structs
drop them). Pure-`Value` access remains available through `RawClient::transport`.
