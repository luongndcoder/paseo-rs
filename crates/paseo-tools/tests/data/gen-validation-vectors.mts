/**
 * Generates crates/paseo-tools/tests/data/validation-vectors.json: for every tool of the TS Paseo
 * catalog, a battery of inputs run through (a) the MCP SDK validation pass (z.object(shape), strip)
 * and (b) the catalog's own parse (passthrough). Run with tsx from any directory:
 *   node <repo>/node_modules/tsx/dist/cli.mjs gen-validation-vectors.ts <repo> <paseo-rs> > out.json
 */
import { readFileSync } from "node:fs";

const [repo, rsRoot] = process.argv.slice(2);
const { createPaseoToolCatalog } = await import(`${repo}/packages/server/src/server/agent/tools/paseo-tools.ts`);
const { normalizeObjectSchema, safeParseAsync, getParseErrorMessage } = await import(
  `${repo}/node_modules/@modelcontextprotocol/sdk/dist/esm/server/zod-compat.js`
);
const { z } = await import(`${repo}/node_modules/zod/index.js`);

const pino = (await import(`${repo}/node_modules/pino/pino.js`)).default;
const logger = pino({ level: "silent" });
const defs = JSON.parse(readFileSync(`${rsRoot}/crates/paseo-tools/data/tool-defs.json`, "utf8"));
const BROWSER_ID = "00000000-0000-4000-8000-000000000000";

function catalogFor(caller: boolean) {
  return createPaseoToolCatalog({
    agentManager: {},
    agentStorage: {},
    providerSnapshotManager: {},
    logger,
    enableVoiceTools: true,
    browserToolsEnabled: true,
    browserToolsBroker: { execute: async () => ({}) },
    ...(caller
      ? { callerAgentId: "caller", resolveCallerContext: () => ({ enableVoiceTools: true }) }
      : {}),
  });
}

const { McpServer } = await import(`${repo}/node_modules/@modelcontextprotocol/sdk/dist/esm/server/mcp.js`);
const { Client } = await import(`${repo}/node_modules/@modelcontextprotocol/sdk/dist/esm/client/index.js`);
const { InMemoryTransport } = await import(`${repo}/node_modules/@modelcontextprotocol/sdk/dist/esm/inMemory.js`);

async function sdkClientFor(catalog: any) {
  const server = new McpServer({ name: "agent-mcp", version: "2.0.0" });
  for (const tool of catalog.tools.values()) {
    server.registerTool(
      tool.name,
      { title: tool.title, description: tool.description, inputSchema: tool.inputSchema },
      async (args: unknown) => ({ content: [], structuredContent: { args: args ?? null } }),
    );
  }
  const client = new Client({ name: "gen", version: "1.0.0" });
  const [ct, st] = InMemoryTransport.createLinkedPair();
  await Promise.all([server.connect(st), client.connect(ct)]);
  return client;
}

async function runSdk(client: any, name: string, input: unknown) {
  try {
    const r = await client.callTool({ name, arguments: input as never });
    if (r.isError) return { ok: false, message: r.content[0].text };
    return { ok: true, data: (r.structuredContent as any).args };
  } catch (e: any) {
    return { threw: String(e?.message ?? e) };
  }
}
async function runCatalog(tool: any, input: unknown) {
  const s = tool.inputSchema;
  const schema =
    typeof s === "object" && s !== null && typeof s.safeParseAsync === "function"
      ? s
      : z.object(s).passthrough();
  const r = await schema.safeParseAsync(input);
  return r.success ? { ok: true, data: r.data } : { ok: false, message: r.error.message };
}

function sample(s: any, name = ""): any {
  if (s.const !== undefined) return s.const;
  if (s.enum) return s.enum[0];
  if (s.oneOf) return sample(s.oneOf[0]);
  if (s.anyOf) return sample(s.anyOf.find((b: any) => b.type !== "null") ?? s.anyOf[0]);
  switch (s.type) {
    case "string":
      if (s.pattern && s.pattern.includes("@e")) return "@e1";
      if (s.pattern) return BROWSER_ID;
      if (name === "provider") return "claude/model-a";
      return "x";
    case "integer":
    case "number":
      return 1;
    case "boolean":
      return true;
    case "array":
      return [sample(s.items ?? { type: "string" })];
    case "object": {
      const o: any = {};
      for (const k of s.required ?? []) o[k] = sample(s.properties[k], k);
      return o;
    }
    default:
      return "x";
  }
}

const REP = (s: string, n: number) => ({ $rep: { s, n } });
const FULL = [
  null, 1, 1.5, -1, 0, 201, 721, 1000000000, "", "   ", "  pad  ", "x ", true, false,
  [], ["a"], {}, { a: 1 }, { a: "b" }, REP("a", 61), REP("a", 4001), "zzz", "not/a/uuid",
  "claude/", "/model", "claude/gpt", "a/b/c", " claude/m ",
];
const SMALL = [null, 1, "", true, {}];

function batteryFor(ps: any, heavy: boolean): unknown[] {
  const t = ps.type ?? (ps.anyOf ? "union" : ps.oneOf ? "union" : "string");
  if (!heavy) return [null, 1, ""];
  if (ps.enum) return [null, 1, "zzz", ""];
  switch (t) {
    case "string":
      return [null, 1, true, [], {}, "", "   ", "  pad  ", REP("a", 61), REP("a", 4001), "claude/", "/model", "a/b/c", " claude/m "];
    case "integer":
    case "number":
      return [null, "x", true, 0, -1, 1.5, -1.5, 201, 721, 1000000000, 100000000000000000000, -100000000000000000000, 9007199254740992];
    case "boolean":
      return [null, "x", 1, false];
    case "array":
      return [null, "x", [], [1], [""], ["a"]];
    case "object":
      return [null, "x", [], 1, { a: 1 }, { a: "b" }];
    default:
      return [null, 1, "", true, [], {}, "x"];
  }
}

function* propCases(base: any, props: any, required: string[], heavy: boolean) {
  for (const [k, ps] of Object.entries<any>(props)) {
    for (const v of batteryFor(ps, heavy)) yield { ...base, [k]: v };
    yield { ...base, [k]: sample(ps, k) };
    if (ps.type === "object" && ps.properties) {
      const sub = sample(ps);
      for (const [sk, sps] of Object.entries<any>(ps.properties)) {
        for (const v of SMALL) yield { ...base, [k]: { ...sub, [sk]: v } };
        yield { ...base, [k]: { ...sub, [sk]: sample(sps, sk) } };
      }
      yield { ...base, [k]: { ...sub, extra: 1 } };
    }
    if (ps.oneOf) {
      for (const b of ps.oneOf) {
        const sub = sample(b);
        yield { ...base, [k]: sub };
        yield { ...base, [k]: { ...sub, extra: 1 } };
        for (const [sk, sps] of Object.entries<any>(b.properties)) {
          for (const v of SMALL) yield { ...base, [k]: { ...sub, [sk]: v } };
          yield { ...base, [k]: { ...sub, [sk]: sample(sps, sk) } };
        }
      }
      yield { ...base, [k]: { behavior: "nope" } };
      yield { ...base, [k]: { } };
    }
  }
}

function casesFor(name: string, schema: any) {
  const props = schema.properties ?? {};
  const required: string[] = schema.required ?? [];
  const base: any = {};
  for (const k of required) base[k] = sample(props[k], k);
  const isBrowser = name.startsWith("browser_");
  const heavy = !isBrowser || ["browser_snapshot", "browser_click", "browser_wait", "browser_new_tab", "browser_upload", "browser_evaluate", "browser_screenshot", "browser_scroll"].includes(name);
  const out: unknown[] = [{ $undefined: true }, null, "str", [], {}, base, { ...base, unknownKey: 1 }, { ...base, unknownKey: 1, other: [1] }];
  for (const k of required) {
    const { [k]: _omit, ...rest } = base;
    out.push(rest);
  }
  for (const c of propCases(base, props, required, heavy)) out.push(c);
  // each optional prop alone and all optional props at once
  const all: any = { ...base };
  for (const [k, ps] of Object.entries<any>(props)) all[k] = sample(ps, k);
  out.push(all);
  // browser_wait refine cases
  if (name === "browser_wait") {
    out.push({ browserId: BROWSER_ID, text: "a", url: "b" }, { browserId: BROWSER_ID }, { browserId: BROWSER_ID, text: "" });
  }
  if (name === "browser_new_tab" || name === "browser_navigate") {
    for (const u of ["example.com", "http://a.b/c", "HTTPS://A.B", "ftp://x", "javascript:alert(1)", "a b", "  x.y  ", "localhost:3000", "http://", "http:///x", "http://:80", "http://a:99999", "http://a:80/p?q#f", "http://[::1]:3000", "://x", "file:///etc/passwd", "mailto:a@b.c"]) {
      out.push({ ...base, url: u });
    }
  }
  const seen = new Set<string>();
  return out.filter((c) => { const k = JSON.stringify(c); if (seen.has(k)) return false; seen.add(k); return true; });
}

function materialize(v: any): any {
  if (v && typeof v === "object") {
    if (v.$undefined) return undefined;
    if (v.$rep) return v.$rep.s.repeat(v.$rep.n);
    if (Array.isArray(v)) return v.map(materialize);
    return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, materialize(x)]));
  }
  return v;
}

const result: any[] = [];
for (const variant of ["default", "caller"]) {
  const catalog = await catalogFor(variant === "caller");
  const client = await sdkClientFor(catalog);
  for (const d of defs) {
    if (variant === "caller" && !d.inputCaller) continue;
    const tool = catalog.getTool(d.name);
    if (!tool) throw new Error(`missing ${d.name}`);
    const schema = variant === "caller" ? d.inputCaller : d.input;
    const cases = [];
    for (const input of casesFor(d.name, schema)) {
      const real = materialize(input);
      const sdk = await runSdk(client, d.name, real);
      const cat = await runCatalog(tool, real);
      cases.push({ input, sdk, catalog: JSON.stringify(sdk) === JSON.stringify(cat) ? "=sdk" : cat });
    }
    result.push({ tool: d.name, variant, cases });
  }
}
process.stdout.write(JSON.stringify(result));
