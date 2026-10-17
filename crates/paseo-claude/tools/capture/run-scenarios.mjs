// Usage: node run-scenarios.mjs [scenario numbers...]  (default: all)
import { mkdtempSync, mkdirSync, writeFileSync, existsSync, readFileSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { realpathSync } from "node:fs";
import { scrubFile } from "./scrub.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const REPO = process.env.PASEO_REPO;
if (!REPO) { console.error("Set PASEO_REPO to the TS paseo checkout root"); process.exit(1); }
const root = `${REPO}/node_modules`;
const sdk = await import(`${root}/@anthropic-ai/claude-agent-sdk/sdk.mjs`);
const { z } = await import(`${root}/zod/index.js`);
const { query, tool, createSdkMcpServer } = sdk;

const fixturesDir = join(here, "..", "..", "fixtures");
mkdirSync(fixturesDir, { recursive: true });
const wrapper = join(here, "tee-claude.mjs");
const scratch = process.env.CAPTURE_SCRATCH || tmpdir();
const notes = {};
const sessions = {};

function newCwd(name) {
  const d = realpathSync(mkdtempSync(join(scratch, `pc-${name}-`)));
  return d;
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function baseOptions(name, cwd, extra = {}) {
  const logFile = join(fixturesDir, `${name}.jsonl`);
  return {
    logFile,
    options: {
      cwd,
      model: "haiku",
      maxTurns: 3,
      pathToClaudeCodeExecutable: wrapper,
      env: { ...process.env, PASEO_TEE_LOG: logFile },
      settingSources: [],
      stderr: () => {},
      ...extra,
    },
  };
}

async function drain(q, onMsg) {
  const msgs = [];
  for await (const m of q) {
    msgs.push(m);
    if (onMsg) {
      const r = await onMsg(m);
      if (r === "stop") break;
    }
  }
  return msgs;
}
function resultOf(msgs) {
  return msgs.find((m) => m.type === "result");
}
function sessionOf(msgs) {
  return msgs.find((m) => m.session_id)?.session_id;
}

const scenarios = {
  1: ["01-single-prompt", async (name, cwd) => {
    const { options } = baseOptions(name, cwd, { maxTurns: 1 });
    const q = query({ prompt: "Reply with exactly the word: pong. No tools.", options });
    const msgs = await drain(q);
    sessions[1] = sessionOf(msgs);
    notes[1] = { session_id: sessions[1], cwd };
  }],
  2: ["02-tool-allow", async (name, cwd) => {
    writeFileSync(join(cwd, "hello.txt"), "hello from fixture\n");
    const calls = [];
    const { options } = baseOptions(name, cwd, {
      canUseTool: async (toolName, input, ctx) => {
        calls.push({ toolName, input, ctx: { toolUseID: ctx?.toolUseID, suggestions: ctx?.suggestions } });
        return { behavior: "allow", updatedInput: input };
      },
    });
    const q = query({ prompt: "Use the Bash tool to run `echo captured > out.txt` and then say done.", options });
    await drain(q);
    notes[2] = { canUseToolCalls: calls.length };
    if (!calls.length) throw new Error("canUseTool never invoked");
  }],
  3: ["03-tool-deny", async (name, cwd) => {
    writeFileSync(join(cwd, "hello.txt"), "hello from fixture\n");
    const calls = [];
    const { options } = baseOptions(name, cwd, {
      canUseTool: async (toolName, input, ctx) => {
        calls.push(toolName);
        return { behavior: "deny", message: "Denied by capture script", interrupt: false };
      },
    });
    const q = query({ prompt: "Use the Bash tool to run `echo captured > out.txt` and then say done.", options });
    await drain(q);
    notes[3] = { canUseToolCalls: calls.length };
    if (!calls.length) throw new Error("canUseTool never invoked");
  }],
  4: ["04-pretooluse-hook", async (name, cwd) => {
    writeFileSync(join(cwd, "hello.txt"), "hello from fixture\n");
    const calls = [];
    const { options } = baseOptions(name, cwd, {
      permissionMode: "bypassPermissions",
      allowDangerouslySkipPermissions: true,
      hooks: {
        PreToolUse: [{
          hooks: [async (input, toolUseID) => {
            calls.push({ tool: input.tool_name, toolUseID });
            return { continue: true };
          }],
        }],
        PostToolUse: [{ hooks: [async () => ({ continue: true })] }],
      },
    });
    const q = query({ prompt: "Use the Read tool to read hello.txt and tell me its content.", options });
    await drain(q);
    notes[4] = { hookCalls: calls.length };
    if (!calls.length) throw new Error("PreToolUse hook never invoked");
  }],
  5: ["05-sdk-mcp-tool", async (name, cwd) => {
    let invoked = 0;
    const server = createSdkMcpServer({
      name: "fixture",
      version: "1.0.0",
      tools: [tool("add_numbers", "Add two integers and return the sum", { a: z.number(), b: z.number() }, async (args) => {
        invoked++;
        return { content: [{ type: "text", text: String(args.a + args.b) }] };
      })],
    });
    const { options } = baseOptions(name, cwd, {
      mcpServers: { fixture: server },
      permissionMode: "bypassPermissions",
      allowDangerouslySkipPermissions: true,
    });
    const q = query({ prompt: "Call the add_numbers tool from the fixture MCP server with a=17 and b=25, then state the result.", options });
    await drain(q);
    notes[5] = { toolInvocations: invoked };
    if (!invoked) throw new Error("sdk mcp tool never invoked");
  }],
  6: ["06-interrupt", async (name, cwd) => {
    const { options } = baseOptions(name, cwd, { maxTurns: 5, includePartialMessages: true });
    const q = query({ prompt: "Write a very long essay (at least 2000 words) about the history of the bicycle.", options });
    let interrupted = false;
    let sdkError;
    try {
      await drain(q, async (m) => {
        if (!interrupted && m.type === "stream_event") {
          interrupted = true;
          await sleep(500);
          await q.interrupt();
        }
      });
    } catch (e) {
      sdkError = String(e?.message || e); // SDK throws on the error_during_execution result after interrupt
    }
    notes[6] = { interrupted, sdkError };
    if (!interrupted) throw new Error("never saw stream_event to interrupt on");
  }],
  7: ["07-controls", async (name, cwd) => {
    const { options } = baseOptions(name, cwd, { maxTurns: 1 });
    async function* input() {
      yield { type: "user", message: { role: "user", content: "Reply with exactly: ok" }, parent_tool_use_id: null, session_id: "" };
      await sleep(120000);
    }
    const q = query({ prompt: input(), options });
    const info = {};
    const it = q[Symbol.asyncIterator]();
    // pump messages in background
    const pump = (async () => {
      for (;;) {
        const r = await it.next();
        if (r.done) break;
        if (r.value.type === "result") break;
      }
    })();
    const safe = async (k, fn) => { try { info[k] = await fn(); } catch (e) { info[k] = { error: String(e?.message || e) }; } };
    await safe("supportedCommands", async () => (await q.supportedCommands()).length);
    await safe("supportedModels", async () => (await q.supportedModels()).map((m) => m.value));
    await safe("setPermissionMode", async () => { await q.setPermissionMode("acceptEdits"); return "ok"; });
    await safe("setModel", async () => { await q.setModel("sonnet"); return "ok"; });
    await safe("setPermissionMode2", async () => { await q.setPermissionMode("default"); return "ok"; });
    await safe("setModelDefault", async () => { await q.setModel(undefined); return "ok"; });
    if (typeof q.mcpServerStatus === "function") await safe("mcpServerStatus", async () => (await q.mcpServerStatus()).length);
    await Promise.race([pump, sleep(60000)]);
    try { q.close(); } catch {}
    notes[7] = info;
  }],
  8: ["08-resume", async (name, cwd) => {
    const sid = sessions[1] || notes[1]?.session_id;
    if (!sid) throw new Error("scenario 1 session id unavailable (run 1 first)");
    // resume must use the same cwd as scenario 1
    const { options } = baseOptions(name, notes[1].cwd, { resume: sid, maxTurns: 1 });
    const q = query({ prompt: "What single word did I ask you to reply with earlier? Answer in one word.", options });
    const msgs = await drain(q);
    notes[8] = { resumed: sid, result: resultOf(msgs)?.subtype };
  }],
  9: ["09-partial-messages", async (name, cwd) => {
    const { options } = baseOptions(name, cwd, { maxTurns: 1, includePartialMessages: true });
    const q = query({ prompt: "Count from 1 to 10 separated by spaces. No tools.", options });
    const msgs = await drain(q);
    notes[9] = { streamEvents: msgs.filter((m) => m.type === "stream_event").length };
  }],
  10: ["10-rewind-files", async (name, cwd) => {
    const target = join(cwd, "notes.txt");
    writeFileSync(target, "original\n");
    const { options } = baseOptions(name, cwd, {
      enableFileCheckpointing: true,
      permissionMode: "bypassPermissions",
      allowDangerouslySkipPermissions: true,
      maxTurns: 4,
      extraArgs: {},
      env: { ...process.env, PASEO_TEE_LOG: join(fixturesDir, `${name}.jsonl`), CLAUDE_CODE_ENABLE_SDK_FILE_CHECKPOINTING: "1" },
    });
    const PROMPT_UUID = crypto.randomUUID();
    async function* input() {
      yield { type: "user", uuid: PROMPT_UUID, message: { role: "user", content: "Use the Write tool to overwrite notes.txt with the single line: modified" }, parent_tool_use_id: null, session_id: "" };
      await sleep(120000);
    }
    const q = query({ prompt: input(), options });
    let userUuid = PROMPT_UUID;
    let dry, real;
    for await (const m of q) {
      if (m.type === "result") {
        // must rewind inside the loop: breaking out of for-await closes the query
        if (!userUuid) throw new Error("no user message uuid observed");
        dry = await q.rewindFiles(userUuid, { dryRun: true });
        real = await q.rewindFiles(userUuid);
        break;
      }
    }
    notes[10] = { dry, real, content: readFileSync(target, "utf8") };
    try { q.close(); } catch {}
  }],
};

const want = process.argv.slice(2).map(Number);
const ids = want.length ? want : Object.keys(scenarios).map(Number);
const summary = {};
for (const id of ids) {
  const [name, fn] = scenarios[id];
  const cwd = newCwd(id);
  process.stderr.write(`== scenario ${id} ${name} (${cwd})\n`);
  try {
    await Promise.race([fn(name, cwd), sleep(240000).then(() => { throw new Error("timeout 240s"); })]);
    summary[id] = { ok: true, ...notes[id] };
  } catch (e) {
    summary[id] = { ok: false, error: String(e?.stack || e).split("\n").slice(0, 3).join(" | "), ...notes[id] };
  }
  const f = join(fixturesDir, `${name}.jsonl`);
  if (existsSync(f)) scrubFile(f, [cwd, scratch]);
  process.stderr.write(JSON.stringify(summary[id]) + "\n");
}
writeFileSync(join(here, "last-run-summary.json"), JSON.stringify(summary, null, 2));
process.exit(0);
