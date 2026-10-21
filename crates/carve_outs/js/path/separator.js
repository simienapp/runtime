// Copyright 2018-2026 the Deno authors. MIT license.
// Copyright Joyent, Inc. and Node.js contributors. MIT license.
// Vendored from Deno's ext/node (deno_node 0.198.0, polyfills/path/separator.ts),
// types stripped. Changes for Simien: internal imports point at
// ext:simien_carve_outs; there is no current directory (resolve() uses "/");
// matchesGlob is left out (it needs a file-system glob).

(function() {
  const { core, primordials } = __bootstrap;
  const { isWindows } = core.loadExtScript("ext:simien_carve_outs/os.js");
  const { SafeRegExp } = primordials;
  const SEP = isWindows ? "\\" : "/";
  const SEP_PATTERN = isWindows ? new SafeRegExp("[\\\\/]+") : new SafeRegExp("/+");
  return {
    SEP,
    SEP_PATTERN
  };
})();
