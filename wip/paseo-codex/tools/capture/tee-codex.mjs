#!/usr/bin/env node
// Tee wrapper usable as a provider command override (`command: {mode:"replace", argv:[this]}`).
// `app-server` invocations are spawned against the real `codex` with identical argv/env and every
// stdin/stdout line is appended to $PASEO_TEE_LOG as JSONL. Every other invocation (`--version`,
// ...) is passed through untouched. Env values are never recorded, only keys.
import { spawn, spawnSync } from "node:child_process";
import { appendFileSync, writeFileSync } from "node:fs";

const logPath = process.env.PASEO_TEE_LOG;
const realCodex = process.env.PASEO_TEE_REAL_CODEX || "codex";
const argv = process.argv.slice(2);
const env = { ...process.env };
delete env.PASEO_TEE_LOG;
delete env.PASEO_TEE_REAL_CODEX;

if (argv[0] !== "app-server" || !logPath) {
  const r = spawnSync(realCodex, argv, { env, stdio: "inherit" });
  process.exit(r.status ?? 1);
}

function log(obj) {
  appendFileSync(logPath, JSON.stringify(obj) + "\n");
}
function parse(line) {
  try {
    return JSON.parse(line);
  } catch {
    return line;
  }
}
// One log file per wrapper process; the driver names it per scenario and the wrapper appends so a
// scenario that restarts the app-server (resume) keeps both processes in one file.
log({ argv, env_keys: Object.keys(process.env).filter((k) => /^(CODEX|OPENAI|PATH$|HOME$|TERM$)/.test(k)).sort() });

const child = spawn(realCodex, argv, { env, stdio: ["pipe", "pipe", "inherit"] });

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
