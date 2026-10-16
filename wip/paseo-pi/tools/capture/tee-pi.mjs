#!/usr/bin/env node
// Stdio tee for Pi / OMP RPC. Used as the `command` of the TS runtime. Spawns the real binary
// (PASEO_TEE_REAL, default `pi`) with identical argv/env and logs every stdin/stdout line to
// $PASEO_TEE_LOG as JSONL: header {argv, env_keys}, {dir:"in"|"out", line}, {exit}.
// Env values are never recorded.
import { spawn } from "node:child_process";
import { appendFileSync, writeFileSync } from "node:fs";

const logPath = process.env.PASEO_TEE_LOG;
const real = process.env.PASEO_TEE_REAL || "pi";
const argv = process.argv.slice(2);
const env = { ...process.env };
delete env.PASEO_TEE_LOG;
delete env.PASEO_TEE_REAL;

const log = (obj) => logPath && appendFileSync(logPath, JSON.stringify(obj) + "\n");
const parse = (line) => {
  try {
    return JSON.parse(line);
  } catch {
    return line;
  }
};
if (logPath) writeFileSync(logPath, "");
log({
  argv,
  env_keys: Object.keys(process.env)
    .filter((k) => /^(PI_|OMP_|PASEO|NODE|PATH$|HOME$|TERM$)/.test(k))
    .sort(),
});

const child = spawn(real, argv, { env, stdio: ["pipe", "pipe", "inherit"] });

function splitter(dir) {
  let buf = "";
  return {
    push(chunk) {
      buf += chunk.toString("utf8");
      let i;
      while ((i = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, i);
        buf = buf.slice(i + 1);
        if (line.length) log({ dir, line: parse(line) });
      }
    },
    flush() {
      if (buf.length) log({ dir, line: parse(buf) });
      buf = "";
    },
  };
}
const inSplit = splitter("in");
const outSplit = splitter("out");

process.stdin.on("data", (c) => {
  inSplit.push(c);
  child.stdin.write(c);
});
process.stdin.on("end", () => {
  inSplit.flush();
  child.stdin.end();
});
child.stdin.on("error", () => {});
child.stdout.on("data", (c) => {
  outSplit.push(c);
  process.stdout.write(c);
});
child.stdout.on("end", () => outSplit.flush());
child.on("exit", (code, signal) => {
  outSplit.flush();
  log({ exit: { code, signal } });
  process.exit(code ?? (signal ? 1 : 0));
});
for (const s of ["SIGINT", "SIGTERM"]) process.on(s, () => child.kill(s));
