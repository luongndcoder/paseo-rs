// Usage: node run-scenarios.mjs [scenario numbers...]  (default: all)
// Drives the TS Codex adapter (packages/server dist) through scenarios with a stdio tee in front of
// `codex app-server`, writes ../../fixtures/NN-*.jsonl and scrubs them. CAPTURE_SCRATCH = temp parent dir.
import { mkdtempSync, mkdirSync, writeFileSync, realpathSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { scrubFile } from "./scrub.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const REPO = process.env.PASEO_REPO;
if (!REPO) { console.error("Set PASEO_REPO to the TS paseo checkout root"); process.exit(1); }
const DIST = `${REPO}/packages/server/dist/server/server/agent/providers`;
const { CodexAppServerAgentClient } = await import(`${DIST}/codex-app-server-agent.js`);

const fixturesDir = join(here, "..", "..", "fixtures");
mkdirSync(fixturesDir, { recursive: true });
const wrapper = join(here, "tee-codex.mjs");
const scratch = process.env.CAPTURE_SCRATCH || tmpdir();
const MODEL = process.env.CAPTURE_MODEL || undefined;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const noop = () => {};
const logger = new Proxy(noop, { get: (_t, p) => (p === "child" ? () => logger : noop), apply: () => undefined });

function newCwd(name) {
  return realpathSync(mkdtempSync(join(scratch, `pcx-${name}-`)));
}
function newClient() {
  return new CodexAppServerAgentClient(logger, { command: { mode: "replace", argv: [wrapper] } });
}
function cfg(cwd, extra = {}) {
  return { provider: "codex", cwd, ...(MODEL ? { model: MODEL } : {}), ...extra };
}
function waitFor(session, pred, timeoutMs = 120_000) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => { unsub(); reject(new Error("timeout waiting for event")); }, timeoutMs);
    const unsub = session.subscribe((e) => {
      if (pred(e)) { clearTimeout(timer); unsub(); resolve(e); }
    });
  });
}
const terminal = (e) => e.type === "turn_completed" || e.type === "turn_failed" || e.type === "turn_canceled";

// 1x1 red PNG
const PNG = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

const sessions = {};
const scenarios = {
  1: ["01-single-prompt", async (cwd) => {
    const s = await newClient().createSession(cfg(cwd));
    const r = await s.run("Reply with exactly the word: pong. No tools.");
    sessions[1] = s.describePersistence();
    await s.close();
    return { finalText: r.finalText };
  }],
  2: ["02-tool-allow", async (cwd) => {
    const s = await newClient().createSession(cfg(cwd, { modeId: "read-only" }));
    let asked = 0;
    s.subscribe((e) => {
      if (e.type === "permission_requested") { asked += 1; setTimeout(() => void s.respondToPermission(e.request.id, { behavior: "allow" }), 50); }
    });
    await s.run("Run the shell command `touch out.txt` to create an empty file. The sandbox is read-only, so request escalated permissions for this command (sandbox_permissions require_escalated). Then say done.");
    await s.close();
    if (!asked) throw new Error("no permission request observed");
    return { asked };
  }],
  3: ["03-tool-deny", async (cwd) => {
    const s = await newClient().createSession(cfg(cwd, { modeId: "read-only" }));
    let asked = 0;
    s.subscribe((e) => {
      if (e.type === "permission_requested") { asked += 1; setTimeout(() => void s.respondToPermission(e.request.id, { behavior: "deny", message: "Denied by capture script" }), 50); }
    });
    await s.run("Run the shell command `touch out.txt` to create an empty file. The sandbox is read-only, so request escalated permissions for this command (sandbox_permissions require_escalated). Then say done.");
    await s.close();
    if (!asked) throw new Error("no permission request observed");
    return { asked };
  }],
  4: ["04-interrupt", async (cwd) => {
    const s = await newClient().createSession(cfg(cwd));
    const done = waitFor(s, terminal);
    const { turnId } = await s.startTurn("Write a 2000 word essay about the history of the bicycle. Do not use tools.");
    await waitFor(s, (e) => e.type === "timeline");
    await sleep(500);
    await s.interrupt();
    const end = await done;
    await s.close();
    return { turnId, end: end.type };
  }],
  5: ["05-resume", async (cwd) => {
    const c = newClient();
    const s = await c.createSession(cfg(cwd));
    await s.run("Remember the secret word: banana. Reply with just: ok.");
    const handle = s.describePersistence();
    await s.close();
    const r = await newClient().resumeSession(handle, cfg(cwd));
    const out = await r.run("What was the secret word? Reply with just the word. No tools.");
    await r.close();
    return { handle, finalText: out.finalText };
  }],
  6: ["06-mode-model-switch", async (cwd) => {
    const c = newClient();
    const models = (await c.fetchCatalog({ scope: "workspace", cwd, force: true })).models;
    const s = await c.createSession(cfg(cwd, { modeId: "auto" }));
    await s.run("Reply with exactly: one. No tools.");
    await s.setMode("read-only");
    const alt = process.env.CAPTURE_MODEL_2 || models.find((m) => !m.isDefault)?.id;
    if (alt) await s.setModel(alt);
    await s.setThinkingOption("low");
    await s.run("Reply with exactly: two. No tools.");
    await s.close();
    return { models: models.map((m) => m.id), alt };
  }],
  7: ["07-catalog-commands", async (cwd) => {
    const c = newClient();
    const cat = await c.fetchCatalog({ scope: "workspace", cwd, force: true });
    const cmds = await c.listCommands?.(cfg(cwd));
    return { models: cat.models.length, modes: cat.modes.map((m) => m.id), commands: cmds?.length };
  }],
  8: ["08-image-input", async (cwd) => {
    const s = await newClient().createSession(cfg(cwd));
    const r = await s.run([
      { type: "text", text: "What color is the attached image? Reply with one word. No tools." },
      { type: "image", data: PNG, mimeType: "image/png" },
    ]);
    await s.close();
    return { finalText: r.finalText };
  }],
  9: ["09-importable-sessions", async (cwd) => {
    const c = newClient();
    const s = await c.createSession(cfg(cwd));
    await s.run("Reply with exactly the word: ok. No tools.");
    await s.close();
    const listed = await newClient().listImportableSessions({ cwd, limit: 5 });
    return { listed: listed.length };
  }],
};

const wanted = process.argv.slice(2).map(Number);
const notes = {};
for (const [n, [name, fn]] of Object.entries(scenarios)) {
  if (wanted.length && !wanted.includes(Number(n))) continue;
  const logFile = join(fixturesDir, `${name}.jsonl`);
  writeFileSync(logFile, "");
  process.env.PASEO_TEE_LOG = logFile;
  const cwd = newCwd(name);
  try {
    notes[n] = { ok: true, ...(await fn(cwd)) };
  } catch (e) {
    notes[n] = { ok: false, error: String(e?.stack ?? e) };
  }
  await sleep(500);
  scrubFile(logFile, [cwd]);
  notes[n].bytes = statSync(logFile).size;
  console.log(n, name, JSON.stringify(notes[n]).slice(0, 400));
}
process.exit(0);
