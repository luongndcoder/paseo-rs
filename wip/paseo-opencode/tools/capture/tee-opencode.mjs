#!/usr/bin/env node
// Recording proxy usable as the OpenCode launch command (provider `command: {mode:"replace", argv:[this]}`).
//
// Called as `tee-opencode serve [--hostname H] --port N` exactly like the real binary. It spawns the
// real `opencode serve` on a private port, listens on the requested port itself, forwards every HTTP
// request and SSE stream, and appends one JSONL record per message to $PASEO_TEE_LOG:
// Every record also carries `proc` (the proxy pid; request ids are unique per proc) and `t` (ms).
//   {kind:"meta", ...}                                header
//   {dir:"req", id, method, path, headers?, body}     request (path includes the query string)
//   {dir:"res", id, status, body}                     non-streaming response
//   {dir:"sse", id, path, event}                      one record per SSE `data:` frame
//   {dir:"sse-end", id}                               stream closed
//   {kind:"exit", ...}
// The Authorization header value is never recorded.
import { spawn } from "node:child_process";
import { appendFileSync } from "node:fs";
import http from "node:http";
import net from "node:net";

const logPath = process.env.PASEO_TEE_LOG;
const realBin = process.env.PASEO_TEE_REAL_OPENCODE || "opencode";
const argv = process.argv.slice(2);
const env = { ...process.env };
delete env.PASEO_TEE_LOG;
delete env.PASEO_TEE_REAL_OPENCODE;

const t0 = Date.now();
let seq = 0;
function log(obj) {
  if (!logPath) return;
  appendFileSync(logPath, JSON.stringify({ t: Date.now() - t0, proc: process.pid, ...obj }) + "\n");
}
function parse(text) {
  if (text === "" || text == null) return null;
  try {
    return JSON.parse(text);
  } catch {
    return text;
  }
}
function freePort() {
  return new Promise((resolve, reject) => {
    const s = net.createServer();
    s.listen(0, "127.0.0.1", () => {
      const { port } = s.address();
      s.close(() => resolve(port));
    });
    s.on("error", reject);
  });
}
function flag(name, fallback) {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : fallback;
}

// The runner truncates the log; one scenario can start several server processes (all append).
const requestedPort = Number(flag("--port", "0"));
const hostname = flag("--hostname", "127.0.0.1");
const upstreamPort = await freePort();
const childArgs = argv.slice();
const setFlag = (name, value) => {
  const i = childArgs.indexOf(name);
  if (i >= 0) childArgs[i + 1] = value;
  else childArgs.push(name, value);
};
setFlag("--port", String(upstreamPort));
setFlag("--hostname", "127.0.0.1");
log({ kind: "meta", argv, env_keys: Object.keys(process.env).filter((k) => /^(OPENCODE|PASEO)/.test(k)).sort() });

const child = spawn(realBin, childArgs, { env, stdio: ["ignore", "pipe", "pipe"] });
child.stderr.on("data", (c) => process.stderr.write(c));
let announced = false;
child.stdout.on("data", (c) => {
  const text = c.toString();
  if (!announced && text.includes("listening on")) {
    announced = true;
    startProxy().then((port) => process.stdout.write(`opencode server listening on http://127.0.0.1:${port}\n`));
  } else if (announced) process.stdout.write(c);
});

function startProxy() {
  return new Promise((resolve) => {
    const server = http.createServer((req, res) => {
      const id = ++seq;
      const chunks = [];
      req.on("data", (c) => chunks.push(c));
      req.on("end", () => {
        const bodyText = Buffer.concat(chunks).toString("utf8");
        const headers = { ...req.headers };
        const hadAuth = "authorization" in headers;
        delete headers.authorization;
        delete headers.host;
        delete headers.connection;
        log({ dir: "req", id, method: req.method, path: req.url, headers, auth: hadAuth || undefined, body: parse(bodyText) });
        const up = http.request(
          // identity encoding so recorded bodies are readable JSON
          { host: "127.0.0.1", port: upstreamPort, method: req.method, path: req.url, headers: { ...req.headers, "accept-encoding": "identity" } },
          (ur) => {
            res.writeHead(ur.statusCode, ur.headers);
            const ct = String(ur.headers["content-type"] || "");
            if (ct.includes("text/event-stream")) {
              let buf = "";
              ur.on("data", (c) => {
                res.write(c);
                buf += c.toString("utf8");
                let i;
                while ((i = buf.indexOf("\n\n")) >= 0) {
                  const frame = buf.slice(0, i);
                  buf = buf.slice(i + 2);
                  const data = frame
                    .split("\n")
                    .filter((l) => l.startsWith("data:"))
                    .map((l) => l.slice(5).replace(/^ /, ""))
                    .join("\n");
                  if (data) log({ dir: "sse", id, path: req.url, event: parse(data) });
                }
              });
              ur.on("end", () => {
                log({ dir: "sse-end", id });
                res.end();
              });
            } else {
              const out = [];
              ur.on("data", (c) => {
                out.push(c);
                res.write(c);
              });
              ur.on("end", () => {
                log({ dir: "res", id, status: ur.statusCode, body: parse(Buffer.concat(out).toString("utf8")) });
                res.end();
              });
            }
          },
        );
        up.on("error", (e) => {
          log({ dir: "res", id, status: 502, body: { proxyError: String(e) } });
          res.writeHead(502).end();
        });
        res.on("close", () => up.destroy());
        up.end(Buffer.concat(chunks));
      });
    });
    server.listen(requestedPort, hostname, () => resolve(server.address().port));
  });
}

child.on("exit", (code, signal) => {
  log({ kind: "exit", code, signal });
  process.exit(code ?? (signal ? 1 : 0));
});
for (const s of ["SIGINT", "SIGTERM"]) process.on(s, () => child.kill(s));
