// The Node modules carved out for the framework (Simien APS 2), as a
// registry the module loader reads. Each module is a lazily loaded script:
// an app pays only for the carve-outs it imports. The registry is a
// non-enumerable, frozen slot, not globals.
import { core, primordials } from "ext:core/mod.js";
const { ObjectDefineProperty, ObjectFreeze, SymbolFor } = primordials;

const load = (name) => () => core.loadExtScript(`ext:simien_carve_outs/${name}`);
const modules = {
  __proto__: null,
  "node:async_hooks": load("async_hooks.js"),
  "node:buffer": load("buffer.js"),
  "node:crypto": load("crypto.js"),
  "node:path": () => {
    const path = core.loadExtScript("ext:simien_carve_outs/path/mod.js").default;
    return { posix: path.posix, win32: path.win32, default: path };
  },
  "node:url": load("url.js"),
  // Imported for side effects only by the framework; they expose nothing.
  "node:fs": () => ({}),
  "node:module": () => ({}),
};

// `process`, carved out with only `platform` and `env` (vinext reads both
// while loading); defined only where nothing defines `process`. `env` is
// read-only and checked against the project's policy per read: a variable
// the policy doesn't allow reads as undefined, and none can be listed.
if (globalThis.process === undefined) {
  const env = new Proxy(ObjectFreeze({ __proto__: null }), {
    get: (_target, name) => typeof name === "string" ? (core.ops.op_carve_out_env(name) ?? undefined) : undefined,
    has: (_target, name) => typeof name === "string" && core.ops.op_carve_out_env(name) !== null,
    set: () => false,
    deleteProperty: () => false,
    defineProperty: () => false,
    ownKeys: () => [],
  });
  ObjectDefineProperty(globalThis, "process", {
    value: ObjectFreeze({ __proto__: null, platform: core.ops.op_carve_out_platform(), env }),
    enumerable: false,
    writable: false,
    configurable: true,
  });
}

ObjectDefineProperty(globalThis, SymbolFor("simien.carve-outs"), {
  value: ObjectFreeze({ __proto__: null, load: (specifier) => modules[specifier]() }),
  enumerable: false,
  writable: false,
  configurable: false,
});
