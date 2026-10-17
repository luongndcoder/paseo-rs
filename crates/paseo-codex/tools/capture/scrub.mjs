// Scrub a fixture JSONL: temp paths -> /tmp/paseo-codex-fixture, home -> /home/user, token-looking values dropped.
import { readFileSync, writeFileSync } from "node:fs";

const FIXTURE_CWD = "/tmp/paseo-codex-fixture";

export function scrubText(text, extraPaths = []) {
  let out = text;
  for (const p of extraPaths) out = out.split(p).join(FIXTURE_CWD);
  out = out.replace(/\/private\/tmp\/claude-\d+\/[^"\\\s]*?\/scratchpad\/pcx-[A-Za-z0-9_.-]+/g, FIXTURE_CWD);
  out = out.replace(/\/private\/var\/folders\/[^"\\\s]*?\/pcx-[A-Za-z0-9_.-]+/g, FIXTURE_CWD);
  out = out.replace(/\/(private\/)?var\/folders\/[A-Za-z0-9_]+\/[A-Za-z0-9_]+\/T\/paseo-attachments-[A-Za-z0-9]+/g, "/tmp/paseo-codex-fixture-attachments");
  out = out.replace(/\/Users\/[A-Za-z0-9_.-]+/g, "/home/user");
  out = out.replace(/sk-[A-Za-z0-9_-]{20,}/g, "REDACTED");
  out = out.replace(/(Bearer\s+)[A-Za-z0-9._~+\/=-]{16,}/g, "$1REDACTED");
  out = out.replace(/\b(eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,})/g, "REDACTED");
  // Codex thread/rollout ids stay (they are random uuids, not secrets); email addresses do not.
  out = out.replace(/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/g, "user@example.com");
  out = out.replace(/"serverName":"[^"]*"/g, '"serverName":"host"');
  out = out.replace(/"installationId":"[^"]*"/g, '"installationId":"00000000-0000-0000-0000-000000000000"');
  out = out.replace(/[A-Za-z0-9-]+\.local\b/g, "host.local");
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
          if (typeof o[k] === "string") {
            // strings may themselves embed JSON, where "/" is escaped as "\\/"
            o[k] = o[k].replace(/\\?\/Users\\?\/[A-Za-z0-9_.-]+/g, "/home/user");
          }
          if (/^(apiKey|api_key|token|accessToken|refreshToken|authToken|authorization|secret|password|encrypted_content)$/i.test(k) && typeof o[k] === "string") {
            o[k] = "REDACTED";
          } else walk(o[k]);
        }
      }
    })(obj);
    const line = obj.line;
    if (line && line.method === "mcpServer/startupStatus/updated" && line.params) { line.params.name = "mcp-server"; if (line.params.error) line.params.error = "MCP startup failed (redacted)"; }
    if (line && line.result && Array.isArray(line.result.data) && line.result.data.some((d) => d && Array.isArray(d.skills))) {
      // skills/list: the user's installed skills are not protocol shape
      for (const d of line.result.data) {
        if (Array.isArray(d.skills)) d.skills = [{ name: "example-skill", description: "Example skill.", path: "/home/user/.agents/skills/example-skill/SKILL.md", scope: "user", enabled: true, pluginId: null }];
      }
    }
    const cfg = line && line.result && line.result.config;
    if (cfg && typeof cfg === "object") {
      // user-specific config (project trust list, MCP servers, hooks, ...) is not protocol shape
      if ("origins" in line.result) line.result.origins = {};
      if ("layers" in line.result) line.result.layers = [];
      for (const k of ["projects", "mcp_servers", "notify", "hooks", "marketplaces", "plugins", "agents", "profiles", "model_providers", "apps", "permissions", "skills", "otel", "log_dir", "sqlite_home", "notice", "history", "js_repl_node_path", "js_repl_node_module_dirs", "cloud", "tui", "windows", "audio", "openai_base_url", "chatgpt_base_url", "instructions", "developer_instructions", "desktop", "analytics", "feedback", "memories"]) {
        if (k in cfg) cfg[k] = null;
      }
    }
    scrubIdentity(obj);
    kept.push(JSON.stringify(obj));
  }
  writeFileSync(path, kept.join("\n") + "\n");
}

if (process.argv[1] && process.argv[1].endsWith("scrub.mjs")) {
  for (const f of process.argv.slice(2)) scrubFile(f, []);
}
