# paseo-protocol

Rust view of the Paseo WebSocket wire protocol. The TS zod schemas in `packages/protocol/src/messages.ts` stay the source of truth.

## Layout

- `schema/*.json` — JSON Schema (draft-07) exported from zod. Checked in. Never edit by hand.
- `src/generated.rs` — typify output, one struct per message `type`, plus `validate(type, &Value)` per module. Checked in, ~2 MB. Never edit by hand.
- `src/bin/regen.rs` — the generator (behind the `codegen` feature so normal builds skip typify).
- `src/frame.rs` — `Frame`: any frame, lossless (keeps the original `Value`); `typed()` says whether a generated struct accepts it.
- `src/route.rs` — `peek_route(bytes)`: `type` and `session.message.type` without building a `Value`.
- `tests/golden.rs` — `fixtures/*.jsonl` parse -> serialize equality, typed/fallback counts.

## Regenerate

Run from the TS checkout, then from this repo:

```bash
# 1. zod -> schema/*.json   (PASEO_RS_DIR defaults to ~/Documents/AI/paseo-rs)
cd <paseo checkout> && npm run protocol:schema -w @getpaseo/protocol

# 2. schema/*.json -> src/generated.rs
cargo run -p paseo-protocol --features codegen --bin regen
```

`regen` must run against a compiling crate. If `generated.rs` is broken, replace it with stub modules that each define `pub fn validate(_: &str, _: &serde_json::Value) -> Option<bool> { None }`, then run it again.

## Generator workarounds (typify 0.6.2)

- typify panics on the large nested `oneOf` unions, so each top-level variant is generated on its own, named from its `type` literal. A variant that panics is skipped and stays `Value`; none are skipped today.
- `anyOf: [X, null]` definitions generate conflicting types. `regen` strips the null from the definition and re-adds it at each `$ref`.
- `pattern`, `format`, `minLength`, `maxLength` are stripped. Typed means structural; no `regress`/`uuid`/`chrono` deps.
- The `ws_*` modules skip the `session` envelope (it inlines the whole session union). `Frame` classifies it through `session_*`.
- zod `z.date()` (one field, `timestamp`) becomes an unconstrained value in the schema.

## Compatibility rules

`Frame` never errors on unknown variants or fields; it re-emits exactly what it parsed. Generated structs are for code that needs fields, not for forwarding. Follow `docs/protocol-compatibility.md` in the TS repo before changing schemas.
