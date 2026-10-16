# paseo-pi

Native Rust client for the Pi and Oh My Pi (OMP) local RPC processes (JSONL over stdio). Standalone
workspace; part of the P5 native-provider work in `docs/plan.md`.

```
src/transport.rs   Transport trait, ProcessTransport (own process group, stderr tail)
src/frame.rs       JSONL decoder, OMP protocol v2 rpc_chunk reassembly
src/rpc.rs         req_<n> correlation, timeouts, event fan-out, request_stop_work
src/protocol/      typed Command / Event / Message; unknown fields and tags survive (Other(Value))
src/session.rs     Session: the raw provider layer (Pi + OMP calls)
src/agent/         AgentClient / AgentSession: provider-agnostic surface, raw events, turn tracker
src/testing.rs     Fixture, ReplayTransport, ReplayLauncher
tools/capture/     tee-pi.mjs (stdio tee), run-scenarios.mjs (drives the TS adapter), scrub.mjs
tools/make-omp-fixtures.py   hand-written OMP fixtures (no omp binary was available)
```

Provider events are exposed raw (`AgentEvent::Provider`); the timeline / ToolCallDetail mapping is
not here (P4).

## Tests

```
cargo test                                                    # unit + replay + process scripts
PASEO_PI_LIVE=1 cargo test --test process -- --ignored        # real pi, free nvidia model
```

Replay matches client frames to the recording ignoring the request `id` and substitutes the
client's id into the response, so the Rust client does not have to reproduce the TS adapter's
`get_session_stats` polling (`Fixture::without_requests`).

## Re-capturing fixtures

Run from `packages/server` of the paseo repo (it owns `tsx` and the deps):

```
CAPTURE_SCRATCH=<tmp dir> ../../node_modules/.bin/tsx \
  <this crate>/tools/capture/run-scenarios.mjs [1..8]
```

`PI_REAL` picks the binary (`omp` for OMP), `CAPTURE_MODEL` / `CAPTURE_MODEL_B` pick models.
Scenarios: 1 prompt, 2 tool (bash), 3 permission allow, 4 permission deny, 5 interrupt, 6 resume,
7 model/thinking switch, 8 catalog. Output is scrubbed (`scrub.mjs`): temp and home paths, keys,
proxy URLs, system prompts, and the operator's skill list are removed.
