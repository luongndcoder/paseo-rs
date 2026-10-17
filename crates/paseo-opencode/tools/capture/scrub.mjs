// Scrub a fixture JSONL: temp paths -> /tmp/paseo-opencode-fixture, home -> /home/user, secrets dropped.
import { readFileSync, writeFileSync } from "node:fs";

export function scrubText(text, extraPaths = []) {
  let out = text;
  for (const p of extraPaths) {
    out = out.split(p).join("/tmp/paseo-opencode-fixture");
    out = out.split(encodeURIComponent(p)).join(encodeURIComponent("/tmp/paseo-opencode-fixture"));
  }
  out = out.replace(/%2Fprivate%2Ftmp%2Fclaude-\d+%2F[^&"\s]*?%2Fpc-[A-Za-z0-9_.-]+/g, "%2Ftmp%2Fpaseo-opencode-fixture");
  out = out.replace(/(?<![\/A-Za-z0-9])private\/tmp\/claude-\d+\/[^"\\\s]*?\/pc-[A-Za-z0-9_.-]+/g, "tmp/paseo-opencode-fixture");
  out = out.replace(/\/private\/tmp\/claude-\d+\/[^"\\\s]*?\/scratchpad\/[A-Za-z0-9_.-]+/g, "/tmp/paseo-opencode-fixture");
  out = out.replace(/\/private\/tmp\/claude-\d+\/[^"\\\s]*?\/pc-[A-Za-z0-9_.-]+/g, "/tmp/paseo-opencode-fixture");
  out = out.replace(/\/Users\/[A-Za-z0-9_.-]+/g, "/home/user");
  out = out.replace(/sk-[A-Za-z0-9_-]{16,}/g, "REDACTED");
  out = out.replace(/\bwrk_[A-Za-z0-9]+/g, "wrk_REDACTED");
  out = out.replace(/(Bearer\s+)[A-Za-z0-9._~+\/=-]{16,}/g, "$1REDACTED");
  out = out.replace(/\b(eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,})/g, "REDACTED");
  // Account / machine identity and user-level skill / MCP names (private to whoever captured).
  out = out.replace(/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/g, "user@example.com");
  out = out.replace(/("(?:organization|organizationName|orgName|org)"\s*:\s*)"[^"]*"/g, '$1"Example Org"');
  out = out.replace(/("subscriptionType"\s*:\s*)"[^"]*"/g, '$1"Claude Pro"');
  out = out.replace(/("(?:hostname|hostName|host_name|machineName|computerName)"\s*:\s*)"[^"]*"/g, '$1"dev-machine"');
  out = out.replace(/[^"\\\n]*[\u2019']s (?:MacBook|iMac|Mac mini|Mac Studio)[^"\\\n]*/g, "dev-machine");
  out = out.replace(/\bsrv_[A-Za-z0-9]{8,}/g, "srv_example000001");
  out = out.replace(/\bmcp__(?!paseo__|example__)[A-Za-z0-9-]+__/g, "mcp__example__");
  out = out.replace(/(\/\.(?:claude|agents)\/skills\/)[A-Za-z0-9_.-]+/g, "$1example-skill");
  return out;
}
// Size control (fixtures stay well under the 3 MB budget):
//  - `GET /provider` bodies are ~600 KB of model catalogue; keep only connected providers and a handful of models.
//  - repeated `plugin.added` boot noise is capped at 2 frames per file.
const KEEP_MODELS = new Set(["glm-5.3-flash-free", "deepseek-v4.1-flash-free", "big-pickle", "beam-free", "gpt-5.4-nano"]);
function trimProviderBody(body) {
  if (!body || !Array.isArray(body.all)) return body;
  const connected = new Set(body.connected ?? []);
  const all = body.all
    .filter((p) => connected.has(p.id))
    .map((p) => ({ ...p, models: Object.fromEntries(Object.entries(p.models ?? {}).filter(([id]) => KEEP_MODELS.has(id))) }));
  return { ...body, all };
}
//  - `GET /command` keeps 8 commands with truncated templates; `GET /agent` truncates agent prompts and permission rule lists (6 rules).
function clip(v, n = 160) {
  return typeof v === "string" && v.length > n ? v.slice(0, n) + "...[truncated]" : v;
}
function trimCommandBody(body) {
  return Array.isArray(body) ? body.slice(0, 8).map((c) => ({ ...c, template: clip(c.template) })) : body;
}
function trimAgentBody(body) {
  return Array.isArray(body) ? body.map((a) => ({ ...a, prompt: clip(a.prompt), permission: Array.isArray(a.permission) ? a.permission.slice(0, 6) : a.permission })) : body;
}
// Generic user-level metadata: account blocks, skill lists, MCP server names.
export function scrubIdentity(o) {
  if (Array.isArray(o)) return o.forEach(scrubIdentity);
  if (!o || typeof o !== "object") return;
  for (const k of Object.keys(o)) {
    if (/^(account|accountInfo)$/.test(k) && o[k] && typeof o[k] === "object") {
      o[k] = { email: "user@example.com", organization: "Example Org", subscriptionType: "Claude Pro" };
    } else if (k === "skills" && Array.isArray(o[k]) && o[k].every((s) => typeof s === "string")) {
      o[k] = o[k].length ? ["example-skill"] : [];
    } else if (k === "mcp_servers" && Array.isArray(o[k])) {
      o[k] = o[k].map((s, i) => (s && typeof s === "object" ? { ...s, name: `example-mcp-${i}` } : s));
    } else scrubIdentity(o[k]);
  }
}

export function scrubFile(path, extraPaths) {
  const lines = readFileSync(path, "utf8").split("\n").filter(Boolean);
  const kept = [];
  const reqPath = new Map();
  let pluginAdded = 0;
  for (const raw of lines) {
    let obj;
    try {
      obj = JSON.parse(scrubText(raw, extraPaths));
    } catch {
      continue;
    }
    (function walk(o) {
      if (o && typeof o === "object") {
        for (const k of Object.keys(o)) {
          if (k === "signature" && typeof o[k] === "string" && o[k].length > 40) o[k] = "REDACTED_SIGNATURE";
          else if (/^(apiKey|api_key|key|token|authToken|oauth.*|authorization|secret|password)$/i.test(k) && typeof o[k] === "string") o[k] = "REDACTED";
          else walk(o[k]);
        }
      }
    })(obj);
    if (obj.dir === "req") reqPath.set(`${obj.proc}:${obj.id}`, String(obj.path).split("?")[0]);
    if (obj.dir === "res" && reqPath.get(`${obj.proc}:${obj.id}`) === "/provider") obj.body = trimProviderBody(obj.body);
    if (obj.dir === "res" && reqPath.get(`${obj.proc}:${obj.id}`) === "/command") obj.body = trimCommandBody(obj.body);
    if (obj.dir === "res" && reqPath.get(`${obj.proc}:${obj.id}`) === "/agent") obj.body = trimAgentBody(obj.body);
    const evType = obj.event?.payload?.type ?? obj.event?.type;
    if (obj.dir === "sse" && evType === "plugin.added" && ++pluginAdded > 2) continue;
    scrubIdentity(obj);
    kept.push(JSON.stringify(obj));
  }
  writeFileSync(path, kept.join("\n") + "\n");
}
