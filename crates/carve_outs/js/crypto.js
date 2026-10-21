// createHash, the subset the framework uses (Simien APS 2): SHA-256 only,
// update() with strings or bytes, digest() as "hex", "base64" or bytes.
(function () {
const { core, primordials } = __bootstrap;
const { op_carve_out_sha256 } = core.ops;
const { TypeError, Uint8Array } = primordials;
const { Buffer } = core.loadExtScript("ext:simien_carve_outs/buffer.js");

const encoder = new TextEncoder();
const hex = (bytes) => Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");

class Hash {
  #parts = [];
  #done = false;

  update(data, encoding) {
    if (this.#done) throw new TypeError("Digest already called");
    const bytes = typeof data === "string"
      ? (encoding ? Buffer.from(data, encoding) : encoder.encode(data))
      : new Uint8Array(data.buffer ?? data, data.byteOffset ?? 0, data.byteLength);
    this.#parts.push(bytes);
    return this;
  }

  digest(encoding) {
    if (this.#done) throw new TypeError("Digest already called");
    this.#done = true;
    let size = 0;
    for (const part of this.#parts) size += part.byteLength;
    const all = new Uint8Array(size);
    let at = 0;
    for (const part of this.#parts) {
      all.set(part, at);
      at += part.byteLength;
    }
    const digest = op_carve_out_sha256(all);
    if (encoding === "hex") return hex(digest);
    const buffer = Buffer.from(digest);
    return encoding === undefined ? buffer : buffer.toString(encoding);
  }
}

function createHash(algorithm) {
  if (String(algorithm).toLowerCase() !== "sha256") {
    throw new TypeError(`createHash("${algorithm}") isn't available in Simien; only "sha256" is carved out`);
  }
  return new Hash();
}

return { createHash };
})();
