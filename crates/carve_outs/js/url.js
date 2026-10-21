// Copyright 2018-2026 the Deno authors. MIT license.
// Copyright Joyent, Inc. and Node.js contributors. MIT license.
// pathToFileURL, vendored from Deno's ext/node (deno_node 0.198.0,
// polyfills/url.ts): the POSIX branch and encodePathChars. Simien serves from
// POSIX systems, so the Windows (UNC, drive letter) branches are left out.
(function () {
const { core, primordials } = __bootstrap;
const { StringPrototypeCharCodeAt, StringPrototypeIncludes, StringPrototypeReplace, SafeRegExp } = primordials;
const { validateString } = core.loadExtScript("ext:simien_carve_outs/errors.js");
const { CHAR_FORWARD_SLASH } = core.loadExtScript("ext:simien_carve_outs/path/_constants.js");
const lazyPath = () => core.loadExtScript("ext:simien_carve_outs/path/mod.js").default;

const replacements = [
  [new SafeRegExp(/%/g), "%", "%25"],
  [new SafeRegExp(/\\/g), "\\", "%5C"],
  [new SafeRegExp(/\n/g), "\n", "%0A"],
  [new SafeRegExp(/\r/g), "\r", "%0D"],
  [new SafeRegExp(/\t/g), "\t", "%09"],
  [new SafeRegExp(/\^/g), "^", "%5E"],
  [new SafeRegExp(/\[/g), "[", "%5B"],
  [new SafeRegExp(/\]/g), "]", "%5D"],
  [new SafeRegExp(/\|/g), "|", "%7C"],
  [new SafeRegExp(/~/g), "~", "%7E"],
];

function encodePathChars(filepath) {
  for (let i = 0; i < replacements.length; i++) {
    const [regex, char, encoded] = replacements[i];
    if (StringPrototypeIncludes(filepath, char)) {
      filepath = StringPrototypeReplace(filepath, regex, encoded);
    }
  }
  return filepath;
}

function pathToFileURL(filepath) {
  validateString(filepath, "path");
  const outURL = new URL("file://");
  const path = lazyPath();
  let resolved = path.posix.resolve(filepath);
  if (
    StringPrototypeCharCodeAt(filepath, filepath.length - 1) === CHAR_FORWARD_SLASH &&
    resolved[resolved.length - 1] !== path.posix.sep
  ) {
    resolved += "/";
  }
  outURL.pathname = encodePathChars(resolved);
  return outURL;
}

return { pathToFileURL };
})();
