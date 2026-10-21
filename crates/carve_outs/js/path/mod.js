// Copyright 2018-2026 the Deno authors. MIT license.
// Copyright Joyent, Inc. and Node.js contributors. MIT license.
// Vendored from Deno's ext/node (deno_node 0.198.0, polyfills/path/mod.ts),
// types stripped. Changes for Simien: internal imports point at
// ext:simien_carve_outs; there is no current directory (resolve() uses "/");
// matchesGlob is left out (it needs a file-system glob).

(function() {
  const { core } = __bootstrap;
  const { isWindows } = core.loadExtScript("ext:simien_carve_outs/os.js");
  const _win32 = core.loadExtScript("ext:simien_carve_outs/path/_win32.js").default;
  const _posix = core.loadExtScript("ext:simien_carve_outs/path/_posix.js").default;
  const win32 = {
    ..._win32,
    win32: null,
    posix: null
  };
  const posix = {
    ..._posix,
    win32: null,
    posix: null
  };
  posix.win32 = win32.win32 = win32;
  posix.posix = win32.posix = posix;
  const path = isWindows ? win32 : posix;
  const {
    basename,
    delimiter,
    dirname,
    extname,
    format,
    isAbsolute,
    join,
    normalize,
    parse,
    relative,
    resolve,
    sep,
    toNamespacedPath,
    _makeLong
  } = path;
  const { common } = core.loadExtScript("ext:simien_carve_outs/path/common.js");
  return {
    win32,
    posix,
    basename,
    delimiter,
    dirname,
    extname,
    format,
    isAbsolute,
    join,
    normalize,
    parse,
    relative,
    resolve,
    sep,
    toNamespacedPath,
    _makeLong,
    common,
    default: path
  };
})();
