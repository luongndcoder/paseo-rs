// Usage (from packages/server of the paseo repo, which owns tsx + deps):
//   CAPTURE_SCRATCH=<dir> ../../node_modules/.bin/tsx <this file> [scenario numbers...]
// Drives the TS Pi adapter through PiCliRuntime with tee-pi.mjs as the pi command and writes
// ../../fixtures/NN-*.jsonl, then scrubs them.
import { mkdtempSync, mkdirSync, realpathSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { scrubFile } from "./scrub.mjs";

const here = dirname(fileURLToPath(import.meta.url));
// cwd is packages/server of the paseo repo (see usage above).
const SRC = `${process.cwd()}/src/server/agent/providers/pi`;
const { PiRpcAgentClient } = await import(`${SRC}/agent.ts`);
const { PiCliRuntime } = await import(`${SRC}/cli-runtime.ts`);
const pino = (await import(`${process.cwd()}/../../node_modules/pino/pino.js`)).default;

const fixturesDir = join(here, "..", "..", "fixtures");
mkdirSync(fixturesDir, { recursive: true });
const tee = join(here, "tee-pi.mjs");
const scratch = process.env.CAPTURE_SCRATCH || tmpdir();
const MODEL = process.env.CAPTURE_MODEL || "nvidia/openai/gpt-oss-20b";
const MODEL_B = process.env.CAPTURE_MODEL_B || "nvidia/z-ai/glm-5.3-flash";
const logger = pino({ level: "silent" });
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const withTimeout = (p, ms, what) =>
  Promise.race([p, new Promise((_, rej) => setTimeout(() => rej(new Error(`timeout: ${what}`)), ms))]);

const selected = process.argv.slice(2).map(Number);
const cwds = [];
const results = {};
const sessionFiles = [];

function newCwd(name) {
  const d = realpathSync(mkdtempSync(join(scratch, `pp-${name}-`)));
  cwds.push(d);
  return d;
}
function client(name) {
  const logFile = join(fixturesDir, `${name}.jsonl`);
  process.env.PASEO_TEE_LOG = logFile;
  process.env.PASEO_TEE_REAL = process.env.PI_REAL || "pi";
  const runtime = new PiCliRuntime({ logger, command: [tee], requestTimeoutMs: 60_000 });
  return { logFile, client: new PiRpcAgentClient({ logger, runtime }) };
}
function cfg(cwd, extra = {}) {
  return { provider: "pi", cwd, model: MODEL, ...extra };
}
function collect(session) {
  const events = [];
  const unsub = session.subscribe((e) => events.push(e));
  return { events, unsub };
}
async function turn(session, prompt, ms = 120_000) {
  const { events, unsub } = collect(session);
  try {
    const res = await withTimeout(session.run(prompt), ms, "run");
    return { res, events };
  } finally {
    unsub();
  }
}
function remember(session) {
  const h = session.describePersistence();
  if (h?.nativeHandle) sessionFiles.push(h.nativeHandle);
  return h;
}

const scenarios = {
  1: ["01-prompt-reply", async () => {
    const { client: c } = client("01-prompt-reply");
    const s = await c.createSession(cfg(newCwd("s1"), { internal: true }));
    const { res } = await turn(s, "Reply with exactly: pong");
    results[1] = res?.finalText;
    await s.close();
  }],
  2: ["02-tool-bash", async () => {
    const { client: c } = client("02-tool-bash");
    const s = await c.createSession(cfg(newCwd("s2"), { internal: true }));
    const { res } = await turn(s, "Use the bash tool to run `echo hello-from-pi` and tell me the output.");
    results[2] = res?.finalText;
    await s.close();
  }],
  3: ["03-permission-allow", async () => {
    const { client: c } = client("03-permission-allow");
    const s = await c.createSession(cfg(newCwd("s3"), { internal: true }));
    let answered = false;
    s.subscribe((e) => {
      if (e.type === "permission_requested" && !answered) {
        answered = true;
        if (process.env.CAPTURE_DEBUG) console.log(JSON.stringify(e.request));
        const q = e.request.input?.questions?.[0];
        const label = q?.options?.[0]?.label ?? "Yes";
        void s.respondToPermission(e.request.id, { behavior: "allow", updatedInput: { answers: { [q?.header ?? q?.question ?? "q"]: label } } });
      }
    });
    const { res } = await turn(s, "Call the ask_user_question tool exactly once with one question 'Which snack?' and options Apple and Cookie. Then tell me which one I picked.");
    results[3] = { answered, text: res?.finalText };
    await s.close();
  }],
  4: ["04-permission-deny", async () => {
    const { client: c } = client("04-permission-deny");
    const s = await c.createSession(cfg(newCwd("s4"), { internal: true }));
    let answered = false;
    s.subscribe((e) => {
      if (e.type === "permission_requested" && !answered) {
        answered = true;
        void s.respondToPermission(e.request.id, { behavior: "deny" });
      }
    });
    const { res } = await turn(s, "Call the ask_user_question tool exactly once with one question 'Which snack?' and options Apple and Cookie. Then tell me what happened.");
    results[4] = { answered, text: res?.finalText };
    await s.close();
  }],
  5: ["05-interrupt", async () => {
    const { client: c } = client("05-interrupt");
    const s = await c.createSession(cfg(newCwd("s5"), { internal: true }));
    const { events, unsub } = collect(s);
    const started = s.startTurn("Count from 1 to 400, one number per line, with no other text.");
    await started;
    const t0 = Date.now();
    while (!events.some((e) => e.type === "timeline") && Date.now() - t0 < 60_000) await sleep(100);
    await sleep(400);
    await s.interrupt();
    await sleep(500);
    unsub();
    results[5] = events.map((e) => e.type).filter((t, i, a) => a.indexOf(t) === i);
    await s.close();
  }],
  6: ["06-resume", async () => {
    const cwd = newCwd("s6");
    const first = client("06-resume-first");
    const s1 = await first.client.createSession(cfg(cwd));
    await turn(s1, "Remember the secret word: tangerine. Reply with exactly: ok");
    const handle = remember(s1);
    await s1.close();
    const second = client("06-resume");
    const s2 = await second.client.resumeSession(handle);
    const { res } = await turn(s2, "What was the secret word? Reply with just the word.");
    results[6] = res?.finalText;
    await s2.close();
  }],
  7: ["07-model-thinking-switch", async () => {
    const { client: c } = client("07-model-thinking-switch");
    const s = await c.createSession(cfg(newCwd("s7"), { internal: true }));
    results[7] = {};
    results[7].info0 = await s.getRuntimeInfo();
    await s.setModel(MODEL_B);
    await s.setThinkingOption("high");
    results[7].info1 = await s.getRuntimeInfo();
    results[7].commands = (await s.listCommands?.())?.length;
    const { res } = await turn(s, "Reply with exactly: pong");
    results[7].text = res?.finalText;
    await s.close();
  }],
  8: ["08-catalog", async () => {
    const { client: c } = client("08-catalog");
    const cat = await c.fetchCatalog({ scope: "workspace", cwd: newCwd("s8"), force: true });
    results[8] = cat.models.length;
  }],
};

for (const [n, [name, fn]] of Object.entries(scenarios)) {
  if (selected.length && !selected.includes(Number(n))) continue;
  try {
    await fn();
    console.log(`scenario ${n} ${name}: OK`, JSON.stringify(results[n])?.slice(0, 300));
  } catch (e) {
    console.log(`scenario ${n} ${name}: FAIL`, e?.message ?? e);
  }
}
for (const f of [...new Set(sessionFiles)]) {
  try { rmSync(f, { force: true }); } catch {}
}
import { readdirSync } from "node:fs";
for (const f of readdirSync(fixturesDir)) {
  if (!f.endsWith(".jsonl")) continue;
  const p = join(fixturesDir, f);
  scrubFile(p, cwds);
  console.log(f, statSync(p).size);
}
process.exit(0);
