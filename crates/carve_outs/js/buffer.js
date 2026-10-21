// The Buffer subset the framework uses (Simien APS 2): Buffer.from(bytes or
// string, encoding) and toString(encoding), for "utf8" and "base64". A Buffer
// is a Uint8Array, so it works wherever bytes do (a Response body included).
(function () {
const { core, primordials } = __bootstrap;
const { op_base64_decode, op_base64_encode_from_buffer } = core.ops;
const { ArrayBufferIsView, ObjectPrototypeIsPrototypeOf, TypeError, Uint8Array, ArrayBuffer } = primordials;

const encoder = new TextEncoder();
const decoder = new TextDecoder();

function encodingOf(encoding) {
  const name = (encoding ?? "utf8").toLowerCase();
  if (name === "utf8" || name === "utf-8") return "utf8";
  if (name === "base64") return "base64";
  throw new TypeError(`Buffer encoding "${encoding}" isn't available in Simien; use "utf8" or "base64"`);
}

class Buffer extends Uint8Array {
  static from(value, encodingOrOffset, length) {
    if (typeof value === "string") {
      const bytes = encodingOf(encodingOrOffset) === "base64" ? op_base64_decode(value) : encoder.encode(value);
      return new Buffer(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    }
    if (ObjectPrototypeIsPrototypeOf(ArrayBuffer.prototype, value)) {
      return new Buffer(value, encodingOrOffset ?? 0, length ?? value.byteLength - (encodingOrOffset ?? 0));
    }
    if (ArrayBufferIsView(value)) {
      const copy = new Buffer(value.byteLength);
      copy.set(new Uint8Array(value.buffer, value.byteOffset, value.byteLength));
      return copy;
    }
    throw new TypeError("Buffer.from takes a string, an ArrayBuffer or a typed array");
  }

  toString(encoding) {
    const view = new Uint8Array(this.buffer, this.byteOffset, this.byteLength);
    return encodingOf(encoding) === "base64" ? op_base64_encode_from_buffer(view, 0, view.byteLength) : decoder.decode(view);
  }
}

return { Buffer };
})();
