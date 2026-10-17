// Generates the *.json vectors in this directory from the REAL TS packages (packages/relay, packages/protocol).
// Run from the TS worktree: npx tsx <this file> <abs path to this directory>
// Deterministic (seeded PRNG), so re-running reproduces identical files.
import { createRequire } from "node:module";
import { createHash } from "node:crypto";
import { mkdirSync, writeFileSync } from "node:fs";

const REPO = process.env.PASEO_REPO;
if (!REPO) { console.error("Set PASEO_REPO to the TS paseo checkout root"); process.exit(1); }
const WT = `${REPO}/packages`;
const OUT = process.argv[2];
mkdirSync(OUT, { recursive: true });
const req = createRequire(`${WT}/relay/src/crypto.ts`);
const nacl = req("tweetnacl");

// Deterministic PRNG so vectors are reproducible.
let ctr = 0;
nacl.setPRNG((x: Uint8Array, n: number) => {
  let off = 0;
  while (off < n) {
    const h = createHash("sha256").update(`paseo-relay-vectors-${ctr++}`).digest();
    const take = Math.min(h.length, n - off);
    x.set(h.subarray(0, take), off);
    off += take;
  }
});

const crypto = await import(`${WT}/relay/src/crypto.ts`);
const chan = await import(`${WT}/relay/src/encrypted-channel.ts`);
const b64 = await import(`${WT}/relay/src/base64.ts`);
const endpoints = await import(`${WT}/protocol/src/daemon-endpoints.ts`);

const hex = (u: Uint8Array | ArrayBuffer) => Buffer.from(u instanceof Uint8Array ? u : new Uint8Array(u)).toString("hex");
const unhex = (h: string) => new Uint8Array(Buffer.from(h, "hex"));
const w = (name: string, v: unknown) => writeFileSync(`${OUT}/${name}.json`, JSON.stringify(v, null, 2) + "\n");

// ---------- keys + crypto ----------
const daemon = crypto.generateKeyPair();
const client = crypto.generateKeyPair();
const shared = crypto.deriveSharedKey(daemon.secretKey, client.publicKey);
const shared2 = crypto.deriveSharedKey(client.secretKey, daemon.publicKey);
if (hex(shared) !== hex(shared2)) throw new Error("shared mismatch");

const plaintexts: Array<{ name: string; hex: string }> = [
  { name: "empty", hex: "" },
  { name: "ascii", hex: hex(new TextEncoder().encode("hello world")) },
  { name: "utf8", hex: hex(new TextEncoder().encode("héllo 🌍  ")) },
  { name: "binary", hex: hex(Uint8Array.from({ length: 300 }, (_, i) => i % 256)) },
];
const fixedNonce = Uint8Array.from({ length: 24 }, (_, i) => i + 1);
const cryptoCases = plaintexts.map((p) => {
  const pt = unhex(p.hex);
  const real = crypto.encrypt(shared, pt.buffer.slice(pt.byteOffset, pt.byteOffset + pt.byteLength));
  const fixedCt = nacl.box.after(pt, fixedNonce, shared);
  const fixedBundle = new Uint8Array(24 + fixedCt.length);
  fixedBundle.set(fixedNonce, 0);
  fixedBundle.set(fixedCt, 24);
  return {
    name: p.name,
    plaintextHex: p.hex,
    realBundleHex: hex(real), // random nonce embedded; Rust must decrypt
    fixedNonceHex: hex(fixedNonce),
    fixedBundleHex: hex(fixedBundle), // Rust encrypt with fixedNonce must equal
    realBundleBase64: b64.arrayBufferToBase64(real),
  };
});
const tampered = new Uint8Array(unhex(cryptoCases[1].realBundleHex));
tampered[30] ^= 1;

const lowOrderZero = new Uint8Array(32);
const nonCanonical = crypto.exportPublicKey(client.publicKey).replace(/=$/, "A"); // len ok, bad trailing bits/padding

w("crypto", {
  daemon: { publicKeyB64: crypto.exportPublicKey(daemon.publicKey), secretKeyB64: crypto.exportSecretKey(daemon.secretKey), publicKeyHex: hex(daemon.publicKey), secretKeyHex: hex(daemon.secretKey) },
  client: { publicKeyB64: crypto.exportPublicKey(client.publicKey), secretKeyB64: crypto.exportSecretKey(client.secretKey), publicKeyHex: hex(client.publicKey), secretKeyHex: hex(client.secretKey) },
  sharedKeyHex: hex(shared),
  cases: cryptoCases,
  negative: {
    tamperedBundleHex: hex(tampered),
    shortBundleHex: hex(new Uint8Array(23)),
    zeroPublicKeyB64: Buffer.from(lowOrderZero).toString("base64"),
    nonCanonicalPublicKeyB64: nonCanonical,
    shortPublicKeyB64: Buffer.from(new Uint8Array(31)).toString("base64"),
  },
  base64: {
    // base64ToArrayBuffer normalisation (url-safe, unpadded, whitespace)
    lenient: [
      { input: "  AQID  ", hex: "010203" },
      { input: "-_8", hex: "fbff" },
      { input: "+/8=", hex: "fbff" },
      { input: "AQI", hex: "0102" },
    ].map((c) => ({ ...c, check: hex(new Uint8Array(b64.base64ToArrayBuffer(c.input))) })),
  },
});
for (const c of (JSON.parse(JSON.stringify([]))) as unknown[]) void c;

// ---------- keypair file ----------
w("keypair-file", {
  fileContents: JSON.stringify({ v: 2, publicKeyB64: crypto.exportPublicKey(daemon.publicKey), secretKeyB64: crypto.exportSecretKey(daemon.secretKey) }, null, 2) + "\n",
  publicKeyHex: hex(daemon.publicKey),
  secretKeyHex: hex(daemon.secretKey),
});

// ---------- urls ----------
const urlCases = [
  { endpoint: "relay.paseo.sh:443", useTls: true, serverId: "srv_abc", connectionId: undefined },
  { endpoint: "relay.paseo.sh:443", useTls: true, serverId: "srv_abc", connectionId: "conn-1" },
  { endpoint: "localhost:8080", useTls: false, serverId: "a b&c=d", connectionId: "x/y z" },
  { endpoint: "[::1]:9000", useTls: false, serverId: "srv", connectionId: undefined },
  { endpoint: "relay.example.com:8443", useTls: true, serverId: "srv-ü", connectionId: "c" },
];
w("urls", urlCases.map((c) => ({ ...c, expected: endpoints.buildRelayWebSocketUrl({ endpoint: c.endpoint, useTls: c.useTls, serverId: c.serverId, role: "server", connectionId: c.connectionId }) })));

// ---------- handshake transcripts ----------
interface Frame { kind: "text" | "binary"; text?: string; hex?: string }
const toFrame = (d: string | ArrayBuffer): Frame => (typeof d === "string" ? { kind: "text", text: d } : { kind: "binary", hex: hex(d) });
const toFrameEvent = (data: string | ArrayBuffer, isBinary: boolean): Frame => (isBinary ? { kind: "binary", hex: hex(typeof data === "string" ? new TextEncoder().encode(data) : data) } : { kind: "text", text: typeof data === "string" ? data : new TextDecoder().decode(data) });

class Pipe {
  daemonSide: any;
  events: any[] = [];
  inputs: Frame[] = [];
  outbound: Array<{ app: Frame; wire: Frame }> = [];
  appSending: Frame | null = null;
  closed = false;
  constructor() {
    const self = this;
    this.daemonSide = {
      onmessage: null, onclose: null, onerror: null,
      async send(data: string | ArrayBuffer) {
        const wire = toFrame(data);
        if (self.appSending) { self.outbound.push({ app: self.appSending, wire }); }
        else self.events.push({ t: "wire", frame: wire });
      },
      close(code?: number, reason?: string) { self.events.push({ t: "close", code, reason }); self.closed = true; },
    };
  }
  async deliver(data: string | ArrayBuffer, isBinary: boolean) {
    this.inputs.push(toFrameEvent(data, isBinary));
    await this.daemonSide.onmessage?.({ data, isBinary });
    await new Promise((r) => setTimeout(r, 5));
  }
}

const daemonKP = { publicKey: daemon.publicKey, secretKey: daemon.secretKey };
const dPub = crypto.exportPublicKey(daemon.publicKey);
const enc = (s: string) => new TextEncoder().encode(s);
const ab = (u: Uint8Array) => u.buffer.slice(u.byteOffset, u.byteOffset + u.byteLength) as ArrayBuffer;

async function scenarioModern() {
  const p = new Pipe();
  const received: Frame[] = [];
  const dch = chan.createDaemonChannel(p.daemonSide, daemonKP, { onmessage: (d: string | ArrayBuffer) => received.push(toFrame(d)) });
  const k = crypto.generateKeyPair();
  const sk = crypto.deriveSharedKey(k.secretKey, daemon.publicKey);
  const hello = JSON.stringify({ type: "e2ee_hello", key: crypto.exportPublicKey(k.publicKey), capabilities: { binaryCiphertext: true } });
  await p.deliver(hello, false);
  const channel = await dch;
  // client -> daemon
  await p.deliver(b64.arrayBufferToBase64(crypto.encrypt(sk, "hello text")), false); // text ciphertext
  await p.deliver(ab(new Uint8Array(crypto.encrypt(sk, ab(Uint8Array.from([1, 2, 3, 0, 255]))))), true); // binary ciphertext
  await p.deliver(b64.arrayBufferToBase64(crypto.encrypt(sk, "héllo 🌍")), false);
  await p.deliver(b64.arrayBufferToBase64(crypto.encrypt(sk, "")), false);
  // rehello same key -> ready again
  await p.deliver(hello, false);
  // stray plaintext ready is ignored
  await p.deliver(JSON.stringify({ type: "e2ee_ready" }), false);
  // daemon -> client
  for (const m of ["pong text", "ünïcode ✓", ab(Uint8Array.from([9, 8, 7])), ""] as Array<string | ArrayBuffer>) {
    p.appSending = toFrame(m);
    await channel.send(m);
    p.appSending = null;
  }
  // decrypt check on client side
  const outboundChecked = p.outbound.map((o) => {
    const data = o.wire.kind === "text" ? b64.base64ToArrayBuffer(o.wire.text!) : ab(unhex(o.wire.hex!));
    const pt = new Uint8Array(crypto.decrypt(sk, data));
    return { ...o, nonceHex: hex(new Uint8Array(data).slice(0, 24)), plaintextHex: hex(pt) };
  });
  // rehello different key -> close 1008
  const k2 = crypto.generateKeyPair();
  await p.deliver(JSON.stringify({ type: "e2ee_hello", key: crypto.exportPublicKey(k2.publicKey), capabilities: { binaryCiphertext: true } }), false);
  return { name: "modern-binary-ciphertext", binaryCiphertext: true, inputs: p.inputs, events: p.events, received, outbound: outboundChecked, clientPublicKeyHex: hex(k.publicKey), clientSecretHex: hex(k.secretKey) };
}

async function scenarioLegacy() {
  const p = new Pipe();
  const received: Frame[] = [];
  const dch = chan.createDaemonChannel(p.daemonSide, daemonKP, { onmessage: (d: string | ArrayBuffer) => received.push(toFrame(d)) });
  const k = crypto.generateKeyPair();
  const sk = crypto.deriveSharedKey(k.secretKey, daemon.publicKey);
  await p.deliver(JSON.stringify({ type: "e2ee_hello", key: crypto.exportPublicKey(k.publicKey) }), false);
  const channel = await dch;
  await p.deliver(b64.arrayBufferToBase64(crypto.encrypt(sk, "legacy text")), false);
  await p.deliver(b64.arrayBufferToBase64(crypto.encrypt(sk, ab(Uint8Array.from([0xff, 0xfe, 0x00])))), false); // non-utf8 -> binary
  for (const m of ["legacy out", ab(Uint8Array.from([1, 2, 3]))] as Array<string | ArrayBuffer>) {
    p.appSending = toFrame(m);
    await channel.send(m);
    p.appSending = null;
  }
  const outboundChecked = p.outbound.map((o) => {
    const data = b64.base64ToArrayBuffer(o.wire.text!);
    return { ...o, nonceHex: hex(new Uint8Array(data).slice(0, 24)), plaintextHex: hex(new Uint8Array(crypto.decrypt(sk, data))) };
  });
  return { name: "legacy-base64", binaryCiphertext: false, inputs: p.inputs, events: p.events, received, outbound: outboundChecked, clientPublicKeyHex: hex(k.publicKey), clientSecretHex: hex(k.secretKey) };
}

async function scenarioErrors() {
  const p = new Pipe();
  const dch = chan.createDaemonChannel(p.daemonSide, daemonKP, {});
  const k = crypto.generateKeyPair();
  const sk = crypto.deriveSharedKey(k.secretKey, daemon.publicKey);
  await p.deliver(JSON.stringify({ type: "e2ee_hello", key: crypto.exportPublicKey(k.publicKey), capabilities: { binaryCiphertext: true } }), false);
  await dch;
  await p.deliver(JSON.stringify({ type: "session", x: 1 }), false); // plaintext frame
  await p.deliver("not base64 !!", false);
  const bad = new Uint8Array(crypto.encrypt(sk, "x")); bad[30] ^= 1;
  await p.deliver(ab(bad), true); // decrypt failure
  await p.deliver(ab(new Uint8Array(10)), true); // too short
  return { name: "errors", binaryCiphertext: true, inputs: p.inputs, events: p.events, received: [], outbound: [], clientPublicKeyHex: hex(k.publicKey), clientSecretHex: hex(k.secretKey) };
}

async function invalidHello(label: string, data: string | ArrayBuffer, isBinary: boolean) {
  const p = new Pipe();
  const dch = chan.createDaemonChannel(p.daemonSide, daemonKP, {});
  const rejected = dch.then(() => null, (e: Error) => e.message);
  await p.deliver(data, isBinary);
  return { label, input: toFrameEvent(data, isBinary), error: await Promise.race([rejected, new Promise((r) => setTimeout(() => r("PENDING"), 50))]) };
}
const longText = JSON.stringify({ type: "e2ee_hello", note: "x".repeat(300) });
const invalid = [
  await invalidHello("binary", ab(Uint8Array.from([1, 2])), true),
  await invalidHello("garbage", "  not\n json  ", false),
  await invalidHello("missing-key", JSON.stringify({ type: "e2ee_hello" }), false),
  await invalidHello("wrong-type", JSON.stringify({ type: "session", key: "k" }), false),
  await invalidHello("blank-key", JSON.stringify({ type: "e2ee_hello", key: "   " }), false),
  await invalidHello("bad-capabilities", JSON.stringify({ type: "e2ee_hello", key: "AAAA", capabilities: null }), false),
  await invalidHello("long-preview", longText, false),
  await invalidHello("bad-key-encoding", JSON.stringify({ type: "e2ee_hello", key: "not-a-key" }), false),
  await invalidHello("zero-key", JSON.stringify({ type: "e2ee_hello", key: Buffer.from(new Uint8Array(32)).toString("base64") }), false),
];

w("transcripts", {
  daemon: { publicKeyB64: dPub, secretKeyB64: crypto.exportSecretKey(daemon.secretKey) },
  scenarios: [await scenarioModern(), await scenarioLegacy(), await scenarioErrors()],
  invalidHello: invalid,
});
console.log("vectors written to", OUT);
