// The few Node error helpers the vendored path module uses, kept to what it
// needs: Node's ERR_INVALID_ARG_TYPE (a TypeError with `code`), its
// validateString, and an assert.
(function () {
const { primordials } = __bootstrap;
const { TypeError, Error } = primordials;

function describe(value) {
  if (value === null) return "Received null";
  if (value === undefined) return "Received undefined";
  if (typeof value === "function") return `Received function ${value.name}`;
  if (typeof value === "object") return "Received an instance of " + (value.constructor?.name ?? "Object");
  return `Received type ${typeof value} (${String(value).slice(0, 25)})`;
}

class ERR_INVALID_ARG_TYPE extends TypeError {
  constructor(name, expected, actual) {
    const kinds = Array.isArray(expected) ? expected.join(" or ") : expected;
    super(`The "${name}" argument must be of type ${kinds}. ${describe(actual)}`);
    this.code = "ERR_INVALID_ARG_TYPE";
  }
}

function validateString(value, name) {
  if (typeof value !== "string") {
    throw new ERR_INVALID_ARG_TYPE(name, "string", value);
  }
}

function assert(value, message) {
  if (!value) {
    throw new Error(message ?? "assertion failed");
  }
}

return { ERR_INVALID_ARG_TYPE, validateString, default: assert };
})();
