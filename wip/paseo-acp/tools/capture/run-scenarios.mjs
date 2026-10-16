// Usage: tsx run-scenarios.mjs [scenario numbers...]   (default: all)
// Drives the TS Copilot ACP adapter (packages/server .../acp-agent.ts) through a tee wrapper.
// Env: PASEO_REPO (default /path/to/paseo), CAPTURE_SCRATCH, ACP_MODEL (default gpt-4.1)
import { mkdtempSync, mkdirSync, realpathSync, writeFileSync, existsSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { scrubFile } from "./scrub.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const repo = process.env.PASEO_REPO || "/path/to/paseo";
const providers = `${repo}/packages/server/src/server/agent/providers`;
const { CopilotACPAgentClient } = await import(`${providers}/copilot-acp-agent.ts`);
const pino = (await import(`${repo}/node_modules/pino/pino.js`)).default;

const logger = pino({ level: "silent" });
const fixturesDir = join(here, "..", "..", "fixtures");
mkdirSync(fixturesDir, { recursive: true });
const wrapper = join(here, "tee-acp.mjs");
const scratch = process.env.CAPTURE_SCRATCH || tmpdir();
const MODEL = process.env.ACP_MODEL || "gpt-4.1";
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const notes = {};
const handles = {};
const allCwds = [];

function newCwd(name) {
  const d = realpathSync(mkdtempSync(join(scratch, `pa-${name}-`)));
  allCwds.push(d);
  return d;
}
function newClient(name) {
  const logFile = join(fixturesDir, `${name}.jsonl`);
  const client = new CopilotACPAgentClient({
    logger,
    runtimeSettings: {
      command: { mode: "replace", argv: [wrapper] },
      env: { PASEO_TEE_LOG: logFile, PASEO_TEE_REAL: "copilot", NO_COLOR: "1" },
    },
  });
  return { client, logFile };
}
function sessionConfig(cwd, extra = {}) {
  return { provider: "copilot", cwd, model: MODEL, ...extra };
}

// Resolves when the foreground turn ends. onEvent may react to events (permissions, interrupt).
async function runTurn(session, prompt, onEvent, timeoutMs = 120_000) {
  const events = [];
  let done;
  const finished = new Promise((r) => (done = r));
  const unsub = session.subscribe((ev) => {
    events.push(ev);
    try {
      onEvent?.(ev);
    } catch (e) {
      console.error("onEvent error", e);
    }
    if (ev.type === "turn_completed" || ev.type === "turn_failed" || ev.type === "turn_canceled") done(ev);
  });
  await session.startTurn(prompt);
  const timer = setTimeout(() => done({ type: "timeout" }), timeoutMs);
  const end = await finished;
  clearTimeout(timer);
  unsub();
  return { events, end };
}

const scenarios = {
  1: ["01-prompt-reply", async (name) => {
    const cwd = newCwd(name);
    const { client } = newClient(name);
    const session = await client.createSession(sessionConfig(cwd));
    const r = await runTurn(session, "Reply with exactly the word: pong. Do not use any tools.");
    handles.reply = { handle: session.describePersistence(), cwd };
    notes[name] = r.end.type;
    await session.close();
    return cwd;
  }],
  2: ["02-permission-allow", async (name) => {
    const cwd = newCwd(name);
    const { client } = newClient(name);
    const session = await client.createSession(sessionConfig(cwd));
    let asked = 0;
    const r = await runTurn(
      session,
      "Use your shell tool to run exactly: echo hello > hello.txt . Then reply done.",
      (ev) => {
        if (ev.type === "permission_requested") {
          asked += 1;
          void session.respondToPermission(ev.request.id, { behavior: "allow" });
        }
      },
    );
    notes[name] = `${r.end.type}; permissions=${asked}; hello.txt=${existsSync(join(cwd, "hello.txt"))}`;
    await session.close();
    return cwd;
  }],
  3: ["03-permission-deny", async (name) => {
    const cwd = newCwd(name);
    const { client } = newClient(name);
    const session = await client.createSession(sessionConfig(cwd));
    let asked = 0;
    const r = await runTurn(
      session,
      "Use your shell tool to run exactly: echo hello > hello.txt . Then reply done.",
      (ev) => {
        if (ev.type === "permission_requested") {
          asked += 1;
          void session.respondToPermission(ev.request.id, { behavior: "deny" });
        }
      },
    );
    notes[name] = `${r.end.type}; permissions=${asked}; hello.txt=${existsSync(join(cwd, "hello.txt"))}`;
    await session.close();
    return cwd;
  }],
  4: ["04-interrupt", async (name) => {
    const cwd = newCwd(name);
    const { client } = newClient(name);
    const session = await client.createSession(sessionConfig(cwd));
    let interrupted = false;
    const r = await runTurn(
      session,
      "Write the numbers from 1 to 400, one per line, with a short sentence after each. Do not use tools.",
      (ev) => {
        if (!interrupted && ev.type === "timeline" && ev.item?.type === "assistant_message") {
          interrupted = true;
          setTimeout(() => void session.interrupt(), 300);
        }
      },
    );
    notes[name] = `${r.end.type}; interrupted=${interrupted}`;
    await session.close();
    return cwd;
  }],
  5: ["05-resume", async (name) => {
    const prior = handles.reply;
    if (!prior) throw new Error("scenario 5 needs scenario 1 in the same run");
    const { client } = newClient(name);
    const session = await client.resumeSession(prior.handle, { model: MODEL });
    const r = await runTurn(session, "What single word did you reply with earlier? Answer with just that word.");
    notes[name] = r.end.type;
    await session.close();
    return prior.cwd;
  }],
  6: ["06-model-mode-switch", async (name) => {
    const cwd = newCwd(name);
    const { client } = newClient(name);
    const session = await client.createSession(sessionConfig(cwd));
    const modes = await session.getAvailableModes();
    const plan = modes.find((m) => /plan/i.test(m.id))?.id;
    if (plan) await session.setMode(plan);
    const agent = modes.find((m) => /#agent$/.test(m.id))?.id;
    if (agent) await session.setMode(agent);
    await session.setModel("gpt-5-mini");
    const r = await runTurn(session, "Reply with exactly the word: pong. Do not use any tools.");
    notes[name] = `${r.end.type}; modes=${modes.map((m) => m.id.split("#").pop()).join(",")}`;
    await session.close();
    return cwd;
  }],
  7: ["07-allow-all-unsupported", async (name) => {
    const cwd = newCwd(name);
    const { client } = newClient(name);
    const session = await client.createSession(sessionConfig(cwd));
    // Copilot 1.0.11 rejects the allow_all config option (-32602); the error response is the fixture.
    let err = null;
    try {
      await session.setMode("allow-all");
    } catch (e) {
      err = e?.message ?? String(e);
    }
    notes[name] = `setMode error: ${err}`;
    await session.close();
    return cwd;
  }],
  8: ["08-catalog", async (name) => {
    const cwd = newCwd(name);
    const { client } = newClient(name);
    const catalog = await client.fetchCatalog({ scope: "workspace", cwd, force: true });
    notes[name] = `models=${catalog.models.length}; modes=${catalog.modes.length}`;
    return cwd;
  }],
  9: ["09-commands", async (name) => {
    const cwd = newCwd(name);
    const { client } = newClient(name);
    const session = await client.createSession(sessionConfig(cwd));
    const cmds = await session.listCommands();
    notes[name] = `commands=${cmds.length}`;
    await session.close();
    return cwd;
  }],
};

const wanted = process.argv.slice(2).map(Number);
const run = wanted.length ? wanted : Object.keys(scenarios).map(Number);
for (const n of run) {
  const [name, fn] = scenarios[n];
  console.log(`== ${n} ${name}`);
  try {
    const cwd = await Promise.race([fn(name), sleep(240_000).then(() => { throw new Error("scenario timeout"); })]);
    await sleep(500);
    const logFile = join(fixturesDir, `${name}.jsonl`);
    if (existsSync(logFile)) scrubFile(logFile, allCwds);
  } catch (e) {
    notes[name] = `ERROR ${e?.message ?? e}`;
    console.error(e);
    const logFile = join(fixturesDir, `${name}.jsonl`);
    if (existsSync(logFile)) scrubFile(logFile, allCwds);
  }
  console.log(`   -> ${notes[name]}`);
}
process.exit(0);
