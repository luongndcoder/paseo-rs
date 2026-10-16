#!/usr/bin/env node
// Stdio tee for ACP agents. Used as the provider launch command: spawns the real
// agent ($PASEO_TEE_REAL, default "copilot") with identical argv/env and logs every
// newline-delimited JSON-RPC frame to $PASEO_TEE_LOG as JSONL.
//   header: {argv, env_keys}   (env values are never recorded)
//   frames: {dir:"in"|"out", line}   in = client -> agent (agent stdin), out = agent -> client
//   footer: {exit:{code,signal}}
import { spawn } from "node:child_process";
import { appendFileSync, writeFileSync } from "node:fs";

const logPath = process.env.PASEO_TEE_LOG;
const real = process.env.PASEO_TEE_REAL || "copilot";
const argv = process.argv.slice(2);
const env = { ...process.env };
delete env.PASEO_TEE_LOG;
delete env.PASEO_TEE_REAL;

function log(obj) {
  if (logPath) appendFileSync(logPath, JSON.stringify(obj) + "\n");
}
function parse(line) {
  try {
    return JSON.parse(line);
  } catch {
    return line;
  }
}
if (logPath) writeFileSync(logPath, "");
log({ argv: [real, ...argv], env_keys: Object.keys(process.env).filter((k) => /^(PASEO_|NO_BROWSER)/.test(k)).sort() });

const child = spawn(real, argv, { env, stdio: ["pipe", "pipe", "inherit"] });

function splitter(dir) {
  let buf = "";
  return {
    push(chunk) {
      buf += chunk.toString("utf8");
      let i;
      while ((i = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, i).trim();
        buf = buf.slice(i + 1);
        if (line.length) log({ dir, line: parse(line) });
      }
    },
    flush() {
      if (buf.trim().length) log({ dir, line: parse(buf.trim()) });
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
