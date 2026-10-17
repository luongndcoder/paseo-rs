# Emulator golden files

`*.json` here are produced by `@xterm/headless` 6.0.0 plus the snapshot/capture logic of
`packages/server/src/terminal/terminal.ts` and `terminal-capture.ts`. Each file holds a byte
script (`feed` = base64, `resize`) and the expected `snapshot` / `capture` result after the
steps that precede it. `src/emulator/tests.rs` replays them against `Emulator` (cells omit
`false` boolean attributes on both sides). The `fuzz_*` and `diff_*` files come from seeded
random scripts.

Regenerate from a checkout of the TS repo that has `node_modules` installed:

    PASEO_TS_CHECKOUT=$PASEO_REPO node gen/gen-golden.mjs

The `diff_*` files only keep scripts the Rust emulator already matches, so build the
differential runner first (`cd gen/emudiff && cargo build --release`); without it every
candidate is kept. `gen/fuzz.mjs <sets> <count> [resize]` runs the same differential fuzz
interactively and prints minimized divergences.
