// CommonJS modules in a served app (esm_loader/cjs.rs). Each file's
// definition module registers its code as a function with what its
// fixed-name require() calls load; the function runs on its first require,
// once, as Node runs it, so a cycle sees the partial exports Node's would
// and a require in a branch not taken loads nothing.

const defined = new Map();
const loaded = new Map();

// The served app's own process, if it has one, except that NODE_ENV, which
// packages pick their build by, is "production" unless the app's policy lets
// it read the variable and it's set (the vendored React builtins are the
// production build too). Every other variable reads as the policy says.
const host = globalThis.process;
const NODE_ENV = host?.env?.NODE_ENV ?? "production";
const process = host === undefined
  ? Object.freeze({ env: Object.freeze({ NODE_ENV }) })
  : Object.create(host, {
    env: { value: Object.create(host.env ?? null, { NODE_ENV: { value: NODE_ENV, enumerable: true } }) },
  });

const CJS = 0;
const BUILTIN = 1;
const VALUE = 2;

function notFound(message) {
  const error = new Error(message);
  error.code = "MODULE_NOT_FOUND";
  return error;
}

export function define(filename, requires, body) {
  defined.set(filename, { requires, body });
}

export function load(filename) {
  const cached = loaded.get(filename);
  if (cached !== undefined) {
    return cached.exports;
  }
  const definition = defined.get(filename);
  if (definition === undefined) {
    throw notFound(`Cannot find module '${filename}'`);
  }
  const module = { id: filename, filename, exports: {}, loaded: false };
  const require = (specifier) => {
    const target = definition.requires[specifier];
    if (target === undefined) {
      throw notFound(
        `Cannot find module '${specifier}' from ${filename}: Simien links the require() calls that name a fixed string`,
      );
    }
    switch (target[0]) {
      case CJS:
        return load(target[1]);
      case BUILTIN: {
        const namespace = target[1]();
        return namespace.default ?? namespace;
      }
      case VALUE:
        return target[1]();
      default:
        throw notFound(target[1]);
    }
  };
  loaded.set(filename, module);
  const dirname = filename.slice(0, filename.lastIndexOf("/")) || "/";
  try {
    definition.body.call(module.exports, module.exports, require, module, filename, dirname, process, globalThis);
  } catch (error) {
    loaded.delete(filename);
    throw error;
  }
  module.loaded = true;
  return module.exports;
}
