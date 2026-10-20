// Minimal WinterTC (Minimum Common Web API) for Workers-style handlers:
//   export default { async fetch(request) { return new Response("ok"); } }
// Host HTTP still serializes to { status, headers, body }. This file only
// gives user JS Request/Response/Headers and a fetch dispatch.

// Close over print while Deno is still an internal bootstrap global. RFD 27
// hides globalThis.Deno before user code runs, but user-visible logging must
// continue to work afterwards.
const __wintertc_print = Deno.core.print.bind(Deno.core);

// `console` (rfd#44, "Globals" — added 2026-09-16 as a cannot-be host global).
// The full WHATWG Console Standard namespace: log/info/debug/dir/dirxml/table
// -> stdout; warn/error/trace/assert -> stderr; count/countReset/time/
// timeEnd/timeLog/group/groupCollapsed/groupEnd/clear have no meaningful
// stream split (Node treats their informational output as stdout; deka
// matches). `profile`/`profileEnd`/`timeStamp`/`createTask` are Chrome-only
// extensions to the WHATWG spec, not part of it, and are simply never
// defined below — deno_core supplies no built-in `console` (only the raw
// `Deno.core.print` op used here), so nothing exposes them by accident.
//
// `__wintertc_print` -> `Deno.core.print` -> deno_core's `op_print`, which
// writes straight to the process's real fd 1/2 with an explicit flush per
// call (ops_builtin.rs). That is the same unbuffered path `io`'s `echo`
// reaches through `globalThis.__dekaPrint` (worker_execution.rs). Native
// `deka dev`/`deka run` embed the isolate pool in-process, so a direct fd
// write is already "the terminal" — there is no separate relay to drop
// output on exit.
(() => {
  // One instance of this state per isolate bootstrap (this whole file runs
  // once when an isolate is created, not once per request), so group depth
  // and count/timer labels behave like a persistent console session for the
  // isolate's lifetime — matching Node/browser semantics, where console
  // state outlives any single call.
  let groupDepth = 0;
  const counts = new Map();
  const timers = new Map();

  // The structured printer (task: rfd#44 "Printable"). Every non-string
  // console argument — struct, enum, Option/Result, tuple, array, plain
  // object, nested combinations — goes through here so native's V8 console
  // and the browser host format identically. No formatter existed anywhere
  // in the runtime before this (echo() only ever accepted `string`), so
  // this is the one implementation both hosts are meant to share; the
  // browser host (`@dekaruntime/web-ide-kit`, a separate repo) still needs
  // its own copy ported from this algorithm — see the PR body.
  // deka#1116: console must never throw and never hang a request. A
  // host-bridge object with a throwing getter, a revoked/trapping Proxy, or
  // a pathologically deep (non-cyclic — `seen` only catches true cycles)
  // structure must degrade to a placeholder string, never propagate an
  // exception out of a console call or blow the JS call stack. Two guards:
  // `MAX_INSPECT_DEPTH` bounds recursion (returns "…" past it, well short
  // of a real RangeError), and every individual field read is its own
  // try/catch so one bad property does not blank out an otherwise-fine
  // object; `safeInspect` is the outer backstop for anything that still
  // escapes (e.g. `Object.keys` itself throwing on an exotic object).
  const MAX_INSPECT_DEPTH = 24;

  // Reads `container[key]` *and* formats it inside one try/catch. Property
  // access has to be inside the guard, not just the formatting: `obj[key]`
  // is evaluated as a normal argument expression, so a throwing getter
  // fires before a callee's own try/catch ever runs — an earlier version of
  // this file called `inspect(value[k], ...)` and the getter's throw
  // escaped past `inspect`'s per-field guard entirely, taking the whole
  // object down to `safeInspect`'s outer fallback instead of isolating the
  // one bad field (deka#1116 hardening).
  function renderField(container, key, seen, depth) {
    try {
      const value = container[key];
      return `${key}: ${inspect(value, seen, depth)}`;
    } catch (_err) {
      return `${key}: [threw while formatting]`;
    }
  }

  // Guards formatting an already-resolved value (an array element `.map`
  // already read, an enum payload already unwrapped) — no property access
  // happens inside this one, so wrapping just the `inspect` call is enough.
  function guardInspect(value, seen, depth) {
    try {
      return inspect(value, seen, depth);
    } catch (_err) {
      return "[threw while formatting]";
    }
  }

  // Result recognition (rfd#44 "Formatting erased values", 2026-09-16).
  // `Result.Ok`/`Result.Err` compile to the plain object shapes
  // `{ ok: true, value }` / `{ ok: false, error }` (deka_emit's
  // RESULT_OK/RESULT_ERR prelude constants) -- unlike a user `enum`, there is
  // no `__enum` tag to dispatch on. Recognized only by the exact own
  // enumerable key set (`{ok, value}` with `ok === true`, or `{ok, error}`
  // with `ok === false`), never by duck-typing an isolated `ok` field, so an
  // unrelated plain object that merely happens to have an `ok` property is
  // never misprinted as a Result.
  function resultCaseLabel(value) {
    const keys = Object.keys(value);
    if (keys.length !== 2 || !keys.includes("ok")) return null;
    if (value.ok === true && keys.includes("value")) return "Ok";
    if (value.ok === false && keys.includes("error")) return "Err";
    return null;
  }

  function inspect(value, seen, depth) {
    if (value === null) return "null";
    // Option is fully erased at runtime (rfd#62): `Some(v)` compiles to the
    // bare value `v`, `None` compiles to `undefined`. Since no other
    // DekaScript value is ever `undefined` (rfd#62's unoccupied-undefined
    // invariant), printing it as `None` is sound at top level and wherever
    // it recurs -- array elements, tuple slots, struct fields (rfd#44
    // "Formatting erased values").
    if (value === undefined) return "None";
    const t = typeof value;
    if (t === "string") return JSON.stringify(value);
    if (t === "number" || t === "boolean") return String(value);
    if (t === "bigint") return String(value) + "n";
    if (t === "function") {
      return value.name ? `[Function: ${value.name}]` : "[Function (anonymous)]";
    }
    if (value instanceof Uint8Array) {
      let hex = "";
      for (let i = 0; i < value.length; i++) {
        hex += value[i].toString(16).padStart(2, "0");
      }
      return `Bytes(${hex})`;
    }
    if (t !== "object") return String(value);

    if (seen.has(value)) return "[Circular]";
    if (depth >= MAX_INSPECT_DEPTH) return "…";

    if (Array.isArray(value)) {
      if (value.length === 0) return "[]";
      seen.add(value);
      const items = value.map((item) => guardInspect(item, seen, depth + 1));
      seen.delete(value);
      return `[ ${items.join(", ")} ]`;
    }

    const resultLabel = resultCaseLabel(value);
    if (resultLabel !== null) {
      seen.add(value);
      const payload = resultLabel === "Ok" ? value.value : value.error;
      const formatted = `${resultLabel}(${guardInspect(payload, seen, depth + 1)})`;
      seen.delete(value);
      return formatted;
    }

    // Every user `enum` (and any Result value that still carries the
    // __enum-tagged envelope from other compiled paths, deka#582,
    // build_values.rs hydrate) shares this shape: `{ __enum, __case, name, value|error }`,
    // payload-less cases omit `value`/`error` entirely. Printed as bare
    // case labels (`Some(1)`, `None`, `Ok(1)`, `Err("e")`, `Red`,
    // `Circle(5)`) — Rust-`{:?}`-style, no enum-name prefix.
    if (typeof value.__enum === "string") {
      const label = value.__case || value.name || value.__enum;
      const hasPayload = "value" in value || "error" in value;
      if (!hasPayload) return label;
      seen.add(value);
      const payload = "error" in value ? value.error : value.value;
      const formatted = `${label}(${guardInspect(payload, seen, depth + 1)})`;
      seen.delete(value);
      return formatted;
    }

    // Structs carry a hidden, non-enumerable `__deka_struct` tag holding
    // the declared name (structs.mdx); own enumerable keys are exactly the
    // declared fields.
    if (typeof value.__deka_struct === "string") {
      seen.add(value);
      const fields = Object.keys(value).map(
        (k) => renderField(value, k, seen, depth + 1)
      );
      seen.delete(value);
      return `${value.__deka_struct} ${fields.length ? `{ ${fields.join(", ")} }` : "{}"}`;
    }

    // Newtypes (build_values.rs hydrate): hidden `__deka_newtype` name tag,
    // payload under the `deka.nt` well-known symbol.
    if (typeof value.__deka_newtype === "string") {
      const payload = value[Symbol.for("deka.nt")];
      return `${value.__deka_newtype}(${guardInspect(payload, seen, depth + 1)})`;
    }

    // Plain object (host bridge JSON, raw JS-mode values).
    seen.add(value);
    const keys = Object.keys(value);
    const fields = keys.map((k) => renderField(value, k, seen, depth + 1));
    seen.delete(value);
    return fields.length ? `{ ${fields.join(", ")} }` : "{}";
  }

  // The only entry point the rest of this file calls: never throws, no
  // matter what `value` is (deka#1116).
  function safeInspect(value) {
    try {
      return inspect(value, new Set(), 0);
    } catch (_err) {
      return "[unrepresentable value]";
    }
  }

  // Top-level string arguments print raw (no quotes); everything else,
  // including nested strings, goes through `safeInspect`.
  function formatArgs(args) {
    return args
      .map((a) => (typeof a === "string" ? a : safeInspect(a)))
      .join(" ");
  }

  // `String(label)` can itself throw (a `{ toString() { throw ... } }`
  // label); count/time labels degrade to "default" rather than take the
  // whole call down with them.
  function safeLabel(label) {
    try {
      return String(label);
    } catch (_err) {
      return "default";
    }
  }

  // Final backstop (deka#1116): no matter what bug slips past every guard
  // above, a `console.*` call must never throw into user/handler code —
  // that turns one bad log line into a failed (or, worse, hung) request.
  // Swallows the error and best-effort notes it to stderr through the raw
  // print op directly, bypassing `formatArgs`/`write` so a broken formatter
  // can't recurse into itself while reporting its own failure.
  function guarded(fn) {
    return (...args) => {
      try {
        return fn(...args);
      } catch (err) {
        try {
          const message = err && err.message ? err.message : String(err);
          __wintertc_print(`[console] internal error: ${message}\n`, true);
        } catch (_ignored) {
          // Truly nothing left to do; still must not throw.
        }
        return undefined;
      }
    };
  }

  function indent(text) {
    if (groupDepth === 0) return text;
    const prefix = "  ".repeat(groupDepth);
    return text
      .split("\n")
      .map((line) => prefix + line)
      .join("\n");
  }

  function write(text, isErr) {
    __wintertc_print(indent(text) + "\n", !!isErr);
  }

  function isPlainRecord(v) {
    return (
      v !== null &&
      typeof v === "object" &&
      !Array.isArray(v) &&
      typeof v.__enum !== "string" &&
      typeof v.__deka_struct !== "string"
    );
  }

  // Wrapped end-to-end (deka#1116): `Object.keys`/`Object.hasOwn` on an
  // exotic object (a throwing Proxy trap) can throw outside `inspectField`'s
  // per-value guards, so `console.table` gets the same never-throws
  // guarantee as every other method — falling back to the plain formatter.
  function table(data) {
    try {
      return tableInner(data);
    } catch (_err) {
      return safeInspect(data);
    }
  }

  function tableInner(data) {
    let rows;
    if (Array.isArray(data)) {
      rows = data.map((v, i) => [String(i), v]);
    } else if (isPlainRecord(data)) {
      rows = Object.keys(data).map((k) => [k, data[k]]);
    } else {
      return formatArgs([data]);
    }

    const columns = [];
    let hasValues = false;
    for (const [, v] of rows) {
      if (isPlainRecord(v)) {
        for (const k of Object.keys(v)) if (!columns.includes(k)) columns.push(k);
      } else {
        hasValues = true;
      }
    }

    const header = ["(index)", ...columns, ...(hasValues ? ["Values"] : [])];
    const body = rows.map(([key, v]) => {
      const record = isPlainRecord(v);
      const cells = [key];
      for (const c of columns) {
        cells.push(record && Object.hasOwn(v, c) ? safeInspect(v[c]) : "");
      }
      if (hasValues) cells.push(record ? "" : safeInspect(v));
      return cells;
    });

    const widths = header.map((h, i) =>
      Math.max(h.length, ...body.map((r) => r[i].length))
    );
    const rule = `+${widths.map((w) => "-".repeat(w + 2)).join("+")}+`;
    const renderRow = (cells) =>
      `| ${cells.map((c, i) => c.padEnd(widths[i])).join(" | ")} |`;
    return [rule, renderRow(header), rule, ...body.map(renderRow), rule].join("\n");
  }

  // Every method is wrapped in `guarded` (deka#1116): a console call must
  // never throw into the caller and never hang a request, regardless of
  // what value it is asked to print.
  globalThis.console = {
    log: guarded((...args) => write(formatArgs(args), false)),
    info: guarded((...args) => write(formatArgs(args), false)),
    debug: guarded((...args) => write(formatArgs(args), false)),
    // Outside a DOM there is no element/XML tree to render; every WinterTC
    // runtime (Node, Deno, Bun, Workers) falls back to plain `log`-style
    // formatting for `dir`/`dirxml`.
    dir: guarded((...args) => write(formatArgs(args), false)),
    dirxml: guarded((...args) => write(formatArgs(args), false)),
    warn: guarded((...args) => write(formatArgs(args), true)),
    error: guarded((...args) => write(formatArgs(args), true)),
    trace: guarded((...args) => {
      const stack = (new Error().stack || "").split("\n").slice(1).join("\n");
      const header = "Trace" + (args.length ? ": " + formatArgs(args) : "");
      write(stack ? `${header}\n${stack}` : header, true);
    }),
    assert: guarded((condition, ...args) => {
      if (condition) return;
      const message = args.length
        ? "Assertion failed: " + formatArgs(args)
        : "Assertion failed";
      write(message, true);
    }),
    table: guarded((data) => write(table(data), false)),
    count: guarded((label = "default") => {
      const key = safeLabel(label);
      const next = (counts.get(key) || 0) + 1;
      counts.set(key, next);
      write(`${key}: ${next}`, false);
    }),
    countReset: guarded((label = "default") => {
      counts.delete(safeLabel(label));
    }),
    time: guarded((label = "default") => {
      timers.set(safeLabel(label), Date.now());
    }),
    timeLog: guarded((label = "default", ...args) => {
      const key = safeLabel(label);
      const start = timers.get(key);
      if (start === undefined) {
        write(`Timer '${key}' does not exist`, true);
        return;
      }
      const suffix = args.length ? " " + formatArgs(args) : "";
      write(`${key}: ${Date.now() - start}ms${suffix}`, false);
    }),
    timeEnd: guarded((label = "default") => {
      const key = safeLabel(label);
      const start = timers.get(key);
      if (start === undefined) {
        write(`Timer '${key}' does not exist`, true);
        return;
      }
      timers.delete(key);
      write(`${key}: ${Date.now() - start}ms`, false);
    }),
    group: guarded((...args) => {
      if (args.length) write(formatArgs(args), false);
      groupDepth++;
    }),
    // WHATWG: groupCollapsed is group with a UI hint (start collapsed) that
    // only means something in a devtools panel; on a terminal it is group.
    groupCollapsed: guarded((...args) => {
      if (args.length) write(formatArgs(args), false);
      groupDepth++;
    }),
    groupEnd: guarded(() => {
      if (groupDepth > 0) groupDepth--;
    }),
    // No host op reports whether fd 1/2 is a TTY, so this only implements
    // the spec's non-TTY case (no-op). Clearing an interactive terminal
    // would need a new `op_php_is_tty`-shaped host op; deferred until an
    // interactive `deka dev` console session asks for it.
    clear: guarded(() => {}),
  };
})();

if (!globalThis.TextEncoder) {
  globalThis.TextEncoder = class TextEncoder {
    encode(input) {
      const str = String(input);
      const utf8 = [];
      for (let i = 0; i < str.length; i++) {
        let charCode = str.charCodeAt(i);
        if (charCode < 0x80) {
          utf8.push(charCode);
        } else if (charCode < 0x800) {
          utf8.push(0xc0 | (charCode >> 6), 0x80 | (charCode & 0x3f));
        } else if (charCode < 0xd800 || charCode >= 0xe000) {
          utf8.push(
            0xe0 | (charCode >> 12),
            0x80 | ((charCode >> 6) & 0x3f),
            0x80 | (charCode & 0x3f)
          );
        } else {
          i++;
          charCode =
            0x10000 +
            (((charCode & 0x3ff) << 10) | (str.charCodeAt(i) & 0x3ff));
          utf8.push(
            0xf0 | (charCode >> 18),
            0x80 | ((charCode >> 12) & 0x3f),
            0x80 | ((charCode >> 6) & 0x3f),
            0x80 | (charCode & 0x3f)
          );
        }
      }
      return new Uint8Array(utf8);
    }
  };
}

if (!globalThis.TextDecoder) {
  globalThis.TextDecoder = class TextDecoder {
    decode(bytes) {
      if (!bytes) return "";
      const arr = new Uint8Array(bytes);
      let str = "";
      let i = 0;
      while (i < arr.length) {
        const byte = arr[i++];
        if (byte < 0x80) {
          str += String.fromCharCode(byte);
        } else if (byte < 0xe0) {
          str += String.fromCharCode(
            ((byte & 0x1f) << 6) | (arr[i++] & 0x3f)
          );
        } else if (byte < 0xf0) {
          str += String.fromCharCode(
            ((byte & 0x0f) << 12) |
              ((arr[i++] & 0x3f) << 6) |
              (arr[i++] & 0x3f)
          );
        } else {
          const code =
            ((byte & 0x07) << 18) |
            ((arr[i++] & 0x3f) << 12) |
            ((arr[i++] & 0x3f) << 6) |
            (arr[i++] & 0x3f);
          const high = ((code - 0x10000) >> 10) | 0xd800;
          const low = ((code - 0x10000) & 0x3ff) | 0xdc00;
          str += String.fromCharCode(high, low);
        }
      }
      return str;
    }
  };
}

// Minimal URL polyfill matching WHATWG acceptance/rejection (deka#932).
// Lives here rather than in the isolate bootstrap source so the Rust
// file-size gate baseline holds (deka#391).
if (typeof globalThis.URL === 'undefined') {
  globalThis.URL = class URL {
    constructor(url, base) {
      url = String(url);
      // WHATWG: without a scheme a URL only parses against a base;
      // scheme-less input is invalid.
      if (!/^[a-z][a-z0-9+.-]*:/i.test(url)) {
        if (base === undefined) throw new TypeError('Invalid URL');
        const baseUrl = new URL(String(base));
        const root = baseUrl.protocol + '//' + baseUrl.host;
        if (url.startsWith('//')) url = baseUrl.protocol + url;
        else if (url.startsWith('/')) url = root + url;
        else if (url.startsWith('?')) url = root + (baseUrl.pathname || '/') + url;
        else if (url.startsWith('#')) url = root + (baseUrl.pathname || '/') + baseUrl.search + url;
        else url = root + (baseUrl.pathname || '/').replace(/\/[^/]*$/, '/') + url;
      }
      this.href = url;

      // Parse protocol
      const schemeMatch = url.match(/^([a-z][a-z0-9+.-]*):/i);
      const scheme = schemeMatch[1].toLowerCase();
      this.protocol = scheme + ':';
      // WHATWG special schemes; file is special too but uniquely permits an
      // empty host, so it is excluded.
      const special = /^(https?|wss?|ftp)$/.test(scheme);

      let rest = url.slice(schemeMatch[0].length);
      if (rest.startsWith('//')) {
        rest = rest.slice(2);
        // WHATWG collapses redundant slashes for special schemes
        // (http:///path names host "path").
        if (special) rest = rest.replace(/^\/+/, '');
      } else if (special) {
        // http:/path and http:path still name a host for special schemes.
        rest = rest.replace(/^\/+/, '');
      } else {
        // Non-special schemes (mailto:, a:, ...) carry an opaque path with
        // no host.
        this.host = '';
        rest = null;
      }

      if (rest !== null) {
        // Remove hostname/port (everything before first /, ?, or #)
        const hostMatch = rest.match(/^([^/?#]*)/);
        this.host = hostMatch ? hostMatch[1] : '';
        rest = rest.slice(this.host.length);

        // WHATWG: a special scheme with an empty or malformed host is
        // invalid.
        if (special) {
          if (!this.host) throw new TypeError('Invalid URL');
          const hostPort = this.host.slice(this.host.lastIndexOf('@') + 1);
          const portIdx = hostPort[0] === '[' ? -1 : hostPort.lastIndexOf(':');
          if (portIdx !== -1) {
            const port = hostPort.slice(portIdx + 1);
            if (port && !/^\d+$/.test(port)) throw new TypeError('Invalid URL');
          }
          if (/\s/.test(this.host)) throw new TypeError('Invalid URL');
        }
      }

      // If nothing left after host, pathname is '/'
      if (!rest) {
        this.pathname = '/';
        this.search = '';
        this.hash = '';
        return;
      }

      // Extract pathname, search, and hash
      const pathMatch = rest.match(/^([^?#]*)(\?[^#]*)?(#.*)?$/);
      if (pathMatch) {
        this.pathname = pathMatch[1] || '/';
        this.search = pathMatch[2] || '';
        this.hash = pathMatch[3] || '';
      } else {
        this.pathname = '/';
        this.search = '';
        this.hash = '';
      }
    }
  };
}

if (typeof globalThis.URLSearchParams === "undefined") {
globalThis.URLSearchParams = class URLSearchParams {
  constructor(init) {
    this.params = [];
    if (typeof init === "string") {
      const pairs = init.replace(/^\?/, "").split("&");
      for (const pair of pairs) {
        if (!pair) continue;
        const idx = pair.indexOf("=");
        if (idx === -1) {
          this.params.push([decodeURIComponent(pair), ""]);
        } else {
          this.params.push([
            decodeURIComponent(pair.slice(0, idx)),
            decodeURIComponent(pair.slice(idx + 1)),
          ]);
        }
      }
    }
  }

  append(name, value) {
    this.params.push([String(name), String(value)]);
  }

  get(name) {
    const entry = this.params.find(([key]) => key === name);
    return entry ? entry[1] : null;
  }

  *entries() {
    for (const param of this.params) {
      yield param;
    }
  }

  toString() {
    return this.params
      .map(([key, value]) => `${encodeURIComponent(key)}=${encodeURIComponent(value)}`)
      .join("&");
  }
};
}

if (typeof globalThis.Headers !== "function") {
  class Headers {
    constructor(init) {
      this._map = Object.create(null);
      if (!init) return;
      if (typeof init.forEach === "function") {
        init.forEach((value, key) => this.append(key, value));
      } else if (Array.isArray(init)) {
        for (const pair of init) {
          if (pair && pair.length >= 2) this.append(pair[0], pair[1]);
        }
      } else {
        for (const key in init) this.append(key, init[key]);
      }
    }
    _key(name) {
      return String(name).toLowerCase();
    }
    append(name, value) {
      const key = this._key(name);
      const next = String(value);
      this._map[key] = this._map[key] ? this._map[key] + ", " + next : next;
      this[key] = this._map[key];
    }
    set(name, value) {
      const key = this._key(name);
      this._map[key] = String(value);
      this[key] = this._map[key];
    }
    get(name) {
      const value = this._map[this._key(name)];
      return value === undefined ? null : value;
    }
    has(name) {
      return Object.prototype.hasOwnProperty.call(this._map, this._key(name));
    }
    delete(name) {
      delete this._map[this._key(name)];
    }
    forEach(callback, thisArg) {
      for (const key in this._map) {
        callback.call(thisArg, this._map[key], key, this);
      }
    }
  }
  globalThis.Headers = Headers;
}

if (typeof globalThis.Request !== "function") {
  class Request {
    constructor(input, init) {
      init = init || {};
      if (typeof input === "string") {
        this.url = input;
      } else if (input && typeof input.url === "string") {
        this.url = input.url;
        if (init.method == null) init.method = input.method;
        if (init.headers == null) init.headers = input.headers;
        if (init.body == null && input._body != null) init.body = input._body;
      } else {
        this.url = "http://localhost/";
      }
      this.method = String(init.method || "GET").toUpperCase();
      this.headers =
        init.headers instanceof globalThis.Headers
          ? init.headers
          : new globalThis.Headers(init.headers || {});
      this._body = init.body == null ? "" : String(init.body);
      // Deka generated handlers (and CF-style fetch) read these as fields.
      this.body = this._body;
      try {
        const parsed = new URL(this.url, "http://localhost");
        this.pathname = parsed.pathname || "/";
        this.path = parsed.pathname + parsed.search || "/";
      } catch (_err) {
        this.pathname = "/";
        this.path = "/";
      }
      if (init.pathname) this.pathname = String(init.pathname);
      if (init.path) this.path = String(init.path);
    }
    async text() {
      return this._body;
    }
    async json() {
      if (!this._body) return null;
      return JSON.parse(this._body);
    }
    clone() {
      return new Request(this.url, {
        method: this.method,
        headers: this.headers,
        body: this._body,
      });
    }
  }
  globalThis.Request = Request;
}

if (typeof globalThis.Response !== "function") {
  class Response {
    constructor(body, init) {
      init = init || {};
      this.status = typeof init.status === "number" ? init.status : 200;
      this.statusText = init.statusText != null ? String(init.statusText) : "";
      this.ok = this.status >= 200 && this.status < 300;
      this.headers =
        init.headers instanceof globalThis.Headers
          ? init.headers
          : new globalThis.Headers(init.headers || {});
      if (body == null) {
        this._body = "";
        this.body = "";
      } else if (typeof body === "string") {
        this._body = body;
        this.body = body;
      } else if (body instanceof Uint8Array) {
        this._bytes = body;
        this.body = body;
        this._body = null;
      } else if (body instanceof ArrayBuffer) {
        this._bytes = new Uint8Array(body);
        this.body = this._bytes;
        this._body = null;
      } else {
        this._body = JSON.stringify(body);
        this.body = this._body;
      }
    }
    async text() {
      if (this._body != null) return this._body;
      if (!this._bytes) return "";
      let out = "";
      for (let i = 0; i < this._bytes.length; i++) {
        out += String.fromCharCode(this._bytes[i]);
      }
      return out;
    }
    async json() {
      return JSON.parse(await this.text());
    }
    async arrayBuffer() {
      if (this._bytes) return this._bytes.buffer;
      const text = this._body || "";
      const bytes = new Uint8Array(text.length);
      for (let i = 0; i < text.length; i++) bytes[i] = text.charCodeAt(i) & 0xff;
      return bytes.buffer;
    }
  }
  globalThis.Response = Response;
}

if (typeof globalThis.__dekaExecuteRequest !== "function") {
  globalThis.__dekaExecuteRequest = async function () {
    function base64Encode(bytes) {
      if (typeof btoa === "function") {
        let binary = "";
        for (let i = 0; i < bytes.length; i += 1) {
          binary += String.fromCharCode(bytes[i]);
        }
        return btoa(binary);
      }
      const alphabet =
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
      let output = "";
      for (let i = 0; i < bytes.length; i += 3) {
        const a = bytes[i];
        const b = i + 1 < bytes.length ? bytes[i + 1] : 0;
        const c = i + 2 < bytes.length ? bytes[i + 2] : 0;
        const triple = (a << 16) | (b << 8) | c;
        output += alphabet[(triple >> 18) & 63];
        output += alphabet[(triple >> 12) & 63];
        output += i + 1 < bytes.length ? alphabet[(triple >> 6) & 63] : "=";
        output += i + 2 < bytes.length ? alphabet[triple & 63] : "=";
      }
      return output;
    }

    const requestData = globalThis.__requestData || {};
    requestData.__body = requestData.body ?? "";
    requestData.params = requestData.params || {};
    if (typeof requestData.json !== "function") {
      requestData.json = async function () {
        const body = this.__body || "";
        if (!body) return null;
        return JSON.parse(body);
      };
    }
    if (typeof requestData.text !== "function") {
      requestData.text = async function () {
        return this.__body || "";
      };
    }

    const method = String(requestData.method || "GET").toUpperCase();
    const url = requestData.url || "http://localhost/";
    const init = { method, headers: requestData.headers || {} };
    if (requestData.__body && method !== "GET" && method !== "HEAD") {
      init.body = requestData.__body;
    }
    if (requestData.pathname) init.pathname = requestData.pathname;
    if (requestData.path) init.path = requestData.path;
    const request = new Request(url, init);

    const context = globalThis.__requestContext || requestData.context || null;
    const handler = globalThis.app;
    if (!handler) {
      throw new Error(
        'Handler did not export default { fetch } (or define globalThis.app)'
      );
    }

    const wsEvent = requestData.__dekaWsEvent;
    if (wsEvent) {
      const wsHandler = handler.websocket || globalThis.__dekaWebsocket;
      if (wsHandler) {
        const ws = globalThis.__dekaWsCreate
          ? globalThis.__dekaWsCreate(
              requestData.__dekaWsId,
              requestData.__dekaWsData
            )
          : null;
        if (
          wsEvent === "message" &&
          requestData.__dekaWsBinary &&
          Array.isArray(requestData.__dekaWsMessage)
        ) {
          requestData.__dekaWsMessage = new Uint8Array(
            requestData.__dekaWsMessage
          );
        }
        if (wsEvent === "open" && typeof wsHandler.open === "function") {
          wsHandler.open(ws);
        } else if (
          wsEvent === "message" &&
          typeof wsHandler.message === "function"
        ) {
          wsHandler.message(ws, requestData.__dekaWsMessage);
        } else if (wsEvent === "close" && typeof wsHandler.close === "function") {
          wsHandler.close(
            ws,
            requestData.__dekaWsCode,
            requestData.__dekaWsReason
          );
        } else if (wsEvent === "drain" && typeof wsHandler.drain === "function") {
          wsHandler.drain(ws);
        }
      }
      return { status: 204, headers: {}, body: "" };
    }

    let response;
    if (typeof handler.fetch === "function") {
      response = await handler.fetch(request, context);
    } else if (typeof handler === "function") {
      response = await handler(request, context);
    } else {
      throw new Error("Handler is not callable and has no fetch()");
    }

    const normalized = globalThis.__dekaResponse ||
      (globalThis.__dekaResponse = {
        status: 200,
        headers: {},
        body: "",
        body_base64: undefined,
        upgrade: undefined,
      });
    normalized.status = 200;
    normalized.body = "";
    normalized.body_base64 = undefined;
    normalized.upgrade = undefined;
    const headerTarget = normalized.headers;
    for (const key in headerTarget) delete headerTarget[key];

    const applyHeaders = (headers) => {
      if (!headers) return;
      if (typeof headers.forEach === "function") {
        headers.forEach((value, key) => {
          headerTarget[key] = String(value);
        });
        return;
      }
      for (const key in headers) headerTarget[key] = String(headers[key]);
    };

    if (response && typeof response.text === "function") {
      if (typeof response.status === "number") {
        normalized.status = response.status;
      }
      applyHeaders(response.headers);
      if (response.upgrade) normalized.upgrade = response.upgrade;
      const bodyValue = response.body;
      if (bodyValue instanceof Uint8Array) {
        normalized.body_base64 = base64Encode(bodyValue);
      } else if (bodyValue instanceof ArrayBuffer) {
        normalized.body_base64 = base64Encode(new Uint8Array(bodyValue));
      } else {
        const contentType = String(
          headerTarget["content-type"] || headerTarget["Content-Type"] || ""
        ).toLowerCase();
        const isTextLike =
          contentType.startsWith("text/") ||
          contentType.includes("json") ||
          contentType.includes("javascript") ||
          contentType.includes("xml") ||
          contentType.includes("svg") ||
          contentType.includes("x-www-form-urlencoded") ||
          !contentType;
        if (!isTextLike && typeof response.arrayBuffer === "function") {
          const bytes = new Uint8Array(await response.arrayBuffer());
          normalized.body_base64 = base64Encode(bytes);
        } else {
          normalized.body = await response.text();
        }
      }
    } else if (response && typeof response === "object") {
      if (typeof response.status === "number") {
        normalized.status = response.status;
      }
      applyHeaders(response.headers);
      if (typeof response.body_base64 === "string") {
        normalized.body_base64 = response.body_base64;
      }
      if (response.body != null) {
        if (response.body instanceof Uint8Array) {
          normalized.body_base64 = base64Encode(response.body);
        } else if (response.body instanceof ArrayBuffer) {
          normalized.body_base64 = base64Encode(new Uint8Array(response.body));
        } else if (typeof response.body === "string") {
          normalized.body = response.body;
        } else {
          normalized.body = JSON.stringify(response.body);
        }
      }
      if (response.upgrade) normalized.upgrade = response.upgrade;
    } else if (response != null) {
      normalized.body = String(response);
    }

    return normalized;
  };
}
