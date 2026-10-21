// Copyright 2018-2026 the Deno authors. MIT license.
// Copyright Joyent, Inc. and Node.js contributors. MIT license.
// Vendored from Deno's ext/node (deno_node 0.198.0, polyfills/path/common.ts),
// types stripped. Changes for Simien: internal imports point at
// ext:simien_carve_outs; there is no current directory (resolve() uses "/");
// matchesGlob is left out (it needs a file-system glob).

(function() {
  const { core, primordials } = __bootstrap;
  const {
    StringPrototypeSubstring,
    StringPrototypeLastIndexOf,
    StringPrototypeSplit,
    ArrayPrototypeSlice,
    StringPrototypeEndsWith,
    ArrayPrototypeJoin,
    SafeArrayIterator
  } = primordials;
  const { SEP } = core.loadExtScript("ext:simien_carve_outs/path/separator.js");
  function common(paths, sep = SEP) {
    const [first = "", ...remaining] = new SafeArrayIterator(paths);
    if (first === "" || remaining.length === 0) {
      return StringPrototypeSubstring(
        first,
        0,
        StringPrototypeLastIndexOf(first, sep) + 1
      );
    }
    const parts = StringPrototypeSplit(first, sep);
    let endOfPrefix = parts.length;
    for (const path of new SafeArrayIterator(remaining)) {
      const compare = StringPrototypeSplit(path, sep);
      for (let i = 0; i < endOfPrefix; i++) {
        if (compare[i] !== parts[i]) {
          endOfPrefix = i;
        }
      }
      if (endOfPrefix === 0) {
        return "";
      }
    }
    const prefix = ArrayPrototypeJoin(
      ArrayPrototypeSlice(parts, 0, endOfPrefix),
      sep
    );
    return StringPrototypeEndsWith(prefix, sep) ? prefix : `${prefix}${sep}`;
  }
  return {
    common
  };
})();
