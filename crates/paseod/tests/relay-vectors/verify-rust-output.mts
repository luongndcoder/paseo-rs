// Verifies frames produced by the Rust relay channel using the REAL TS crypto.
//   RELAY_RUST_OUT=<file> cargo test --test relay_vectors dump_rust_frames   (writes <file>)
//   npx tsx verify-rust-output.mts <abs vectors dir> <file>                  (run from the TS worktree)
import { readFileSync } from "node:fs";
const REPO = process.env.PASEO_REPO;
if (!REPO) { console.error("Set PASEO_REPO to the TS paseo checkout root"); process.exit(1); }
const WT = `${REPO}/packages`;
const crypto = await import(`${WT}/relay/src/crypto.ts`);
const b64 = await import(`${WT}/relay/src/base64.ts`);
const [dir, file] = process.argv.slice(2);
const tr = JSON.parse(readFileSync(`${dir}/transcripts.json`, "utf8"));
const sc = tr.scenarios[0];
const unhex = (h: string) => new Uint8Array(Buffer.from(h, "hex"));
const daemonPublic = crypto.importPublicKey(tr.daemon.publicKeyB64);
const shared = crypto.deriveSharedKey(unhex(sc.clientSecretHex), daemonPublic);
const frames = JSON.parse(readFileSync(file, "utf8"));
let ok = 0;
for (const f of frames) {
  const data = f.wire.kind === "text" ? b64.base64ToArrayBuffer(f.wire.text) : (() => { const u = unhex(f.wire.hex); return u.buffer.slice(u.byteOffset, u.byteOffset + u.byteLength); })();
  const plain = Buffer.from(new Uint8Array(crypto.decrypt(shared, data))).toString("hex");
  if (plain !== f.plainHex) throw new Error(`mismatch: ${plain} != ${f.plainHex}`);
  if ((f.plainKind === "binary") !== (f.wire.kind === "binary")) throw new Error("wire kind mismatch for binaryCiphertext channel");
  ok++;
}
console.log(`TS decrypted ${ok}/${frames.length} Rust-produced frames`);
