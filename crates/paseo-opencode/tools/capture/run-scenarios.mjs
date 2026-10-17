// Usage (from any cwd): tsx run-scenarios.mjs [scenario numbers...]   (default: all)
// Drives the TS OpenCodeAgentClient (legacy 1.x path) through scenarios with the recording proxy
// (tee-opencode.mjs) as its launch command, writing ../../fixtures/NN-*.jsonl, then scrubs them.
import { mkdtempSync, mkdirSync, realpathSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { scrubFile } from "./scrub.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const repo = process.env.PASEO_REPO;
if (!repo) { console.error("Set PASEO_REPO to the TS paseo checkout root"); process.exit(1); }
const scratch = realpathSync(process.env.CAPTURE_SCRATCH || tmpdir());
process.env.PASEO_HOME = join(scratch, "paseo-home");
mkdirSync(process.env.PASEO_HOME, { recursive: true });

const providers = `${repo}/packages/server/src/server/agent/providers`;
const { OpenCodeAgentClient } = await import(`${providers}/opencode-agent.ts`);
const { OpenCodeServerManager } = await import(`${providers}/opencode/server-manager.ts`);
const pino = (await import(`${repo}/node_modules/pino/pino.js`)).default;

const MODEL = process.env.CAPTURE_MODEL || "opencode/glm-5.3-flash-free";
const MODEL_B = process.env.CAPTURE_MODEL_B || "opencode/deepseek-v4.1-flash-free";
const fixturesDir = join(here, "..", "..", "fixtures");
mkdirSync(fixturesDir, { recursive: true });
const wrapper = join(here, "tee-opencode.mjs");
const logger = pino({ level: "silent" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const created = [];

function newCwd(name) {
  const d = realpathSync(mkdtempSync(join(scratch, `pc-${name}-`)));
  created.push(d);
  return d;
}
function makeClient(name) {
  const logFile = join(fixturesDir, `${name}.jsonl`);
  writeFileSync(logFile, "");
  const settings = { command: { mode: "replace", argv: [wrapper] }, env: { PASEO_TEE_LOG: logFile } };
  const serverManager = new OpenCodeServerManager({ logger, runtimeSettings: settings });
  const client = new OpenCodeAgentClient(logger, settings, { serverManager });
  return { client, serverManager, logFile };
}
const TERMINAL = new Set(["turn_completed", "turn_failed", "turn_canceled"]);
function collect(session) {
  const events = [];
  const waiters = [];
  const off = session.subscribe((e) => {
    events.push(e);
    for (const w of [...waiters]) w.check();
  });
  return {
    events,
    off,
    until(pred, ms = 120_000) {
      return new Promise((resolve, reject) => {
        const w = {
          check: () => {
            if (events.some(pred)) {
              waiters.splice(waiters.indexOf(w), 1);
              clearTimeout(timer);
              resolve();
            }
          },
        };
        const timer = setTimeout(() => {
          waiters.splice(waiters.indexOf(w), 1);
          reject(new Error("timeout waiting for event; saw " + events.map((e) => e.type).join(",")));
        }, ms);
        waiters.push(w);
        w.check();
      });
    },
    done: () => events.filter((e) => TERMINAL.has(e.type)).length,
  };
}
async function turn(session, prompt, onEvent) {
  const c = collect(session);
  const before = 0;
  if (onEvent) session.subscribe((e) => void onEvent(e));
  await session.startTurn(prompt);
  await c.until((e) => TERMINAL.has(e.type));
  c.off();
  return c.events;
}
const baseConfig = (cwd, extra = {}) => ({ provider: "opencode", cwd, model: MODEL, modeId: "build", ...extra });
const notes = {};
const state = {};

const scenarios = {
  1: ["01-prompt-reply", async (name) => {
    const cwd = newCwd(name);
    const { client, serverManager } = makeClient(name);
    const session = await client.createSession(baseConfig(cwd), undefined, { persistSession: false });
    const events = await turn(session, "Reply with exactly the word: pong. Do not use any tools.");
    notes[1] = { types: events.map((e) => e.type) };
    await session.close();
    await serverManager.shutdown();
  }],
  2: ["02-permission-allow", async (name) => {
    const cwd = newCwd(name);
    const { client, serverManager } = makeClient(name);
    const session = await client.createSession(
      baseConfig(cwd, { providerOptions: { permission: { bash: "ask" } } }),
      undefined,
      { persistSession: false },
    );
    const events = await turn(
      session,
      "Use the bash tool to run exactly: echo captured > out.txt   Then reply done.",
      async (e) => {
        if (e.type === "permission_requested") {
          await session.respondToPermission(e.request.id, { behavior: "allow" });
        }
      },
    );
    notes[2] = { types: events.map((e) => e.type) };
    await session.close();
    await serverManager.shutdown();
  }],
  3: ["03-permission-deny", async (name) => {
    const cwd = newCwd(name);
    const { client, serverManager } = makeClient(name);
    const session = await client.createSession(
      baseConfig(cwd, { providerOptions: { permission: { bash: "ask" } } }),
      undefined,
      { persistSession: false },
    );
    const events = await turn(
      session,
      "Use the bash tool to run exactly: echo denied > out.txt   Then reply done.",
      async (e) => {
        if (e.type === "permission_requested") {
          await session.respondToPermission(e.request.id, { behavior: "deny", message: "Denied by test" });
        }
      },
    );
    notes[3] = { types: events.map((e) => e.type) };
    await session.close();
    await serverManager.shutdown();
  }],
  4: ["04-interrupt", async (name) => {
    const cwd = newCwd(name);
    const { client, serverManager } = makeClient(name);
    const session = await client.createSession(baseConfig(cwd), undefined, { persistSession: false });
    let interrupted = false;
    const events = await turn(
      session,
      "Write a very long story of at least 2000 words about a lighthouse keeper. No tools.",
      async (e) => {
        if (!interrupted && (e.type === "timeline" || e.type === "turn_started")) {
          if (e.type === "timeline") {
            interrupted = true;
            await sleep(300);
            await session.interrupt();
          }
        }
      },
    );
    notes[4] = { types: events.map((e) => e.type), interrupted };
    await session.close();
    await serverManager.shutdown();
  }],
  5: ["05-resume", async (name) => {
    const cwd = newCwd(name);
    const { client, serverManager } = makeClient(name);
    const session = await client.createSession(baseConfig(cwd), undefined, { persistSession: true });
    await turn(session, "Remember the secret word: tangerine. Reply with exactly: ok.");
    const handle = session.describePersistence();
    await session.close();
    const resumed = await client.resumeSession(handle, { cwd });
    const history = [];
    for await (const ev of resumed.streamHistory()) history.push(ev.type);
    const events = await turn(resumed, "What was the secret word? Reply with just the word.");
    notes[5] = { handle, history: history.length, types: events.map((e) => e.type) };
    await resumed.close();
    await serverManager.shutdown();
  }],
  6: ["06-model-mode-switch", async (name) => {
    const cwd = newCwd(name);
    const { client, serverManager } = makeClient(name);
    const session = await client.createSession(baseConfig(cwd), undefined, { persistSession: false });
    const modes = await session.getAvailableModes();
    await session.setMode("plan");
    await turn(session, "Reply with exactly: one. No tools.");
    await session.setModel(MODEL_B);
    await session.setMode("build");
    const events = await turn(session, "Reply with exactly: two. No tools.");
    notes[6] = { modes: modes.map((m) => m.id), types: events.map((e) => e.type) };
    await session.close();
    await serverManager.shutdown();
  }],
  7: ["07-catalog", async (name) => {
    const cwd = newCwd(name);
    const { client, serverManager } = makeClient(name);
    const ctl = new AbortController();
    const ctx = { signal: ctl.signal, runActivity: (_n, op) => op() };
    const catalog = await client.fetchCatalog({ scope: "workspace", cwd, force: false }, ctx);
    const commands = await client.listCommands(baseConfig(cwd));
    notes[7] = { models: catalog.models.length, modes: catalog.modes.map((m) => m.id), commands: commands.length };
    await serverManager.shutdown();
  }],
  8: ["08-image-prompt", async (name) => {
    const cwd = newCwd(name);
    const { client, serverManager } = makeClient(name);
    const png =
      "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    const session = await client.createSession(baseConfig(cwd), undefined, { persistSession: false });
    const events = await turn(session, [
      { type: "text", text: "Describe this image in five words. No tools." },
      { type: "image", mimeType: "image/png", data: png },
    ]);
    notes[8] = { types: events.map((e) => e.type) };
    await session.close();
    await serverManager.shutdown();
  }],
};

const only = process.argv.slice(2).map(Number);
const run = Object.keys(scenarios).map(Number).filter((n) => only.length === 0 || only.includes(n));
for (const n of run) {
  const [name, fn] = scenarios[n];
  try {
    await fn(name);
    console.log(`scenario ${n} ${name}: ok`, JSON.stringify(notes[n]));
  } catch (e) {
    console.log(`scenario ${n} ${name}: FAILED`, e?.stack || e);
  }
  try {
    scrubFile(join(fixturesDir, `${name}.jsonl`), created);
    console.log(`  ${statSync(join(fixturesDir, `${name}.jsonl`)).size} bytes`);
  } catch (e) {
    console.log("  no fixture", String(e));
  }
}
process.exit(0);
