#!/usr/bin/env node
// Tee wrapper usable as pathToClaudeCodeExecutable. Spawns the real `claude`
// with identical argv/env and logs every stdin/stdout line to $PASEO_TEE_LOG.
// Env values are never recorded, only keys.
import { spawn } from "node:child_process";
import { appendFileSync, writeFileSync } from "node:fs";

const logPath = process.env.PASEO_TEE_LOG;
const realClaude = process.env.PASEO_TEE_REAL_CLAUDE || "claude";
const argv = process.argv.slice(2);
const env = { ...process.env };
delete env.PASEO_TEE_LOG;
delete env.PASEO_TEE_REAL_CLAUDE;

function log(obj) {
  if (!logPath) return;
  appendFileSync(logPath, JSON.stringify(obj) + "\n");
}
function parse(line) {
  try {
    return JSON.parse(line);
  } catch {
    return line;
  }
}
if (logPath) writeFileSync(logPath, "");
log({ argv, env_keys: Object.keys(process.env).filter((k) => /^(CLAUDE|ANTHROPIC|MCP|DISABLE|ENABLE|NODE|PATH$|HOME$|CI$|TERM$)/.test(k)).sort() });

const child = spawn(realClaude, argv, { env, stdio: ["pipe", "pipe", "inherit"] });

function lineSplitter(dir) {
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
const inSplit = lineSplitter("in");
const outSplit = lineSplitter("out");

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
