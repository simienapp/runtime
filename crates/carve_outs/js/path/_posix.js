// Copyright 2018-2026 the Deno authors. MIT license.
// Copyright Joyent, Inc. and Node.js contributors. MIT license.
// Vendored from Deno's ext/node (deno_node 0.198.0, polyfills/path/_posix.ts),
// types stripped. Changes for Simien: internal imports point at
// ext:simien_carve_outs; there is no current directory (resolve() uses "/");
// matchesGlob is left out (it needs a file-system glob).

(function() {
  const { core, primordials } = __bootstrap;
  const { CHAR_DOT, CHAR_FORWARD_SLASH } = core.loadExtScript(
    "ext:simien_carve_outs/path/_constants.js"
  );
  const { ERR_INVALID_ARG_TYPE } = core.loadExtScript(
    "ext:simien_carve_outs/errors.js"
  );
  const {
    _format,
    assertPath,
    isPosixPathSeparator,
    normalizeString
  } = core.loadExtScript("ext:simien_carve_outs/path/_util.js");
  const { validateString } = core.loadExtScript(
    "ext:simien_carve_outs/errors.js"
  );
  const { isWindows } = core.loadExtScript("ext:simien_carve_outs/os.js");
  const {
    StringPrototypeReplace,
    StringPrototypeCharCodeAt,
    StringPrototypeIndexOf,
    StringPrototypeSlice,
    SafeRegExp
  } = primordials;
  const sep = "/";
  const delimiter = ":";
  const posixCwd = (() => {
    if (isWindows) {
      const regexp = new SafeRegExp(/\\/g);
      return () => {
        const cwd = StringPrototypeReplace("/", regexp, "/");
        return StringPrototypeSlice(cwd, StringPrototypeIndexOf(cwd, "/"));
      };
    }
    return () => "/";
  })();
  function resolve(...pathSegments) {
    if (pathSegments.length === 0 || pathSegments.length === 1 && (pathSegments[0] === "" || pathSegments[0] === ".")) {
      const cwd = posixCwd();
      if (StringPrototypeCharCodeAt(cwd, 0) === CHAR_FORWARD_SLASH) {
        return cwd;
      }
    }
    let resolvedPath = "";
    let resolvedAbsolute = false;
    for (let i = pathSegments.length - 1; i >= 0 && !resolvedAbsolute; i--) {
      const path = pathSegments[i];
      validateString(path, `paths[${i}]`);
      if (path.length === 0) {
        continue;
      }
      resolvedPath = `${path}/${resolvedPath}`;
      resolvedAbsolute = StringPrototypeCharCodeAt(path, 0) === CHAR_FORWARD_SLASH;
    }
    if (!resolvedAbsolute) {
      const cwd = posixCwd();
      resolvedPath = `${cwd}/${resolvedPath}`;
      resolvedAbsolute = StringPrototypeCharCodeAt(cwd, 0) === CHAR_FORWARD_SLASH;
    }
    resolvedPath = normalizeString(
      resolvedPath,
      !resolvedAbsolute,
      "/",
      isPosixPathSeparator
    );
    if (resolvedAbsolute) {
      return `/${resolvedPath}`;
    }
    return resolvedPath.length > 0 ? resolvedPath : ".";
  }
  function normalize(path) {
    assertPath(path);
    if (path.length === 0) return ".";
    const isAbsolute2 = StringPrototypeCharCodeAt(path, 0) === CHAR_FORWARD_SLASH;
    const trailingSeparator = StringPrototypeCharCodeAt(path, path.length - 1) === CHAR_FORWARD_SLASH;
    path = normalizeString(path, !isAbsolute2, "/", isPosixPathSeparator);
    if (path.length === 0 && !isAbsolute2) path = ".";
    if (path.length > 0 && trailingSeparator) path += "/";
    if (isAbsolute2) return `/${path}`;
    return path;
  }
  function isAbsolute(path) {
    assertPath(path);
    return path.length > 0 && StringPrototypeCharCodeAt(path, 0) === CHAR_FORWARD_SLASH;
  }
  function join(...paths) {
    if (paths.length === 0) return ".";
    let joined;
    for (let i = 0, len = paths.length; i < len; ++i) {
      const path = paths[i];
      assertPath(path);
      if (path.length > 0) {
        if (!joined) joined = path;
        else joined += `/${path}`;
      }
    }
    if (!joined) return ".";
    return normalize(joined);
  }
  function relative(from, to) {
    assertPath(from);
    assertPath(to);
    if (from === to) return "";
    from = resolve(from);
    to = resolve(to);
    if (from === to) return "";
    const fromStart = 1;
    const fromEnd = from.length;
    const fromLen = fromEnd - fromStart;
    const toStart = 1;
    const toLen = to.length - toStart;
    const length = fromLen < toLen ? fromLen : toLen;
    let lastCommonSep = -1;
    let i = 0;
    for (; i < length; i++) {
      const fromCode = StringPrototypeCharCodeAt(from, fromStart + i);
      if (fromCode !== StringPrototypeCharCodeAt(to, toStart + i)) {
        break;
      } else if (fromCode === CHAR_FORWARD_SLASH) {
        lastCommonSep = i;
      }
    }
    if (i === length) {
      if (toLen > length) {
        if (StringPrototypeCharCodeAt(to, toStart + i) === CHAR_FORWARD_SLASH) {
          return StringPrototypeSlice(to, toStart + i + 1);
        }
        if (i === 0) {
          return StringPrototypeSlice(to, toStart + i);
        }
      } else if (fromLen > length) {
        if (StringPrototypeCharCodeAt(from, fromStart + i) === CHAR_FORWARD_SLASH) {
          lastCommonSep = i;
        } else if (i === 0) {
          lastCommonSep = 0;
        }
      }
    }
    let out = "";
    for (i = fromStart + lastCommonSep + 1; i <= fromEnd; ++i) {
      if (i === fromEnd || StringPrototypeCharCodeAt(from, i) === CHAR_FORWARD_SLASH) {
        out += out.length === 0 ? ".." : "/..";
      }
    }
    return out + StringPrototypeSlice(to, toStart + lastCommonSep);
  }
  function toNamespacedPath(path) {
    return path;
  }
  function dirname(path) {
    assertPath(path);
    if (path.length === 0) return ".";
    const hasRoot = StringPrototypeCharCodeAt(path, 0) === CHAR_FORWARD_SLASH;
    let end = -1;
    let matchedSlash = true;
    for (let i = path.length - 1; i >= 1; --i) {
      if (StringPrototypeCharCodeAt(path, i) === CHAR_FORWARD_SLASH) {
        if (!matchedSlash) {
          end = i;
          break;
        }
      } else {
        matchedSlash = false;
      }
    }
    if (end === -1) return hasRoot ? "/" : ".";
    if (hasRoot && end === 1) return "//";
    return StringPrototypeSlice(path, 0, end);
  }
  function basename(path, ext = "") {
    if (ext !== void 0 && typeof ext !== "string") {
      throw new ERR_INVALID_ARG_TYPE("ext", ["string"], ext);
    }
    assertPath(path);
    let start = 0;
    let end = -1;
    let matchedSlash = true;
    let i;
    if (ext !== void 0 && ext.length > 0 && ext.length <= path.length) {
      if (ext.length === path.length && ext === path) return "";
      let extIdx = ext.length - 1;
      let firstNonSlashEnd = -1;
      for (i = path.length - 1; i >= 0; --i) {
        const code = StringPrototypeCharCodeAt(path, i);
        if (code === CHAR_FORWARD_SLASH) {
          if (!matchedSlash) {
            start = i + 1;
            break;
          }
        } else {
          if (firstNonSlashEnd === -1) {
            matchedSlash = false;
            firstNonSlashEnd = i + 1;
          }
          if (extIdx >= 0) {
            if (code === StringPrototypeCharCodeAt(ext, extIdx)) {
              if (--extIdx === -1) {
                end = i;
              }
            } else {
              extIdx = -1;
              end = firstNonSlashEnd;
            }
          }
        }
      }
      if (start === end) end = firstNonSlashEnd;
      else if (end === -1) end = path.length;
      return StringPrototypeSlice(path, start, end);
    } else {
      for (i = path.length - 1; i >= 0; --i) {
        if (StringPrototypeCharCodeAt(path, i) === CHAR_FORWARD_SLASH) {
          if (!matchedSlash) {
            start = i + 1;
            break;
          }
        } else if (end === -1) {
          matchedSlash = false;
          end = i + 1;
        }
      }
      if (end === -1) return "";
      return StringPrototypeSlice(path, start, end);
    }
  }
  function extname(path) {
    assertPath(path);
    let startDot = -1;
    let startPart = 0;
    let end = -1;
    let matchedSlash = true;
    let preDotState = 0;
    for (let i = path.length - 1; i >= 0; --i) {
      const code = StringPrototypeCharCodeAt(path, i);
      if (code === CHAR_FORWARD_SLASH) {
        if (!matchedSlash) {
          startPart = i + 1;
          break;
        }
        continue;
      }
      if (end === -1) {
        matchedSlash = false;
        end = i + 1;
      }
      if (code === CHAR_DOT) {
        if (startDot === -1) startDot = i;
        else if (preDotState !== 1) preDotState = 1;
      } else if (startDot !== -1) {
        preDotState = -1;
      }
    }
    if (startDot === -1 || end === -1 || // We saw a non-dot character immediately before the dot
    preDotState === 0 || // The (right-most) trimmed path component is exactly '..'
    preDotState === 1 && startDot === end - 1 && startDot === startPart + 1) {
      return "";
    }
    return StringPrototypeSlice(path, startDot, end);
  }
  function format(pathObject) {
    if (pathObject === null || typeof pathObject !== "object") {
      throw new ERR_INVALID_ARG_TYPE("pathObject", ["Object"], pathObject);
    }
    return _format("/", pathObject);
  }
  function parse(path) {
    assertPath(path);
    const ret = { root: "", dir: "", base: "", ext: "", name: "" };
    if (path.length === 0) return ret;
    const isAbsolute2 = StringPrototypeCharCodeAt(path, 0) === CHAR_FORWARD_SLASH;
    let start;
    if (isAbsolute2) {
      ret.root = "/";
      start = 1;
    } else {
      start = 0;
    }
    let startDot = -1;
    let startPart = 0;
    let end = -1;
    let matchedSlash = true;
    let i = path.length - 1;
    let preDotState = 0;
    for (; i >= start; --i) {
      const code = StringPrototypeCharCodeAt(path, i);
      if (code === CHAR_FORWARD_SLASH) {
        if (!matchedSlash) {
          startPart = i + 1;
          break;
        }
        continue;
      }
      if (end === -1) {
        matchedSlash = false;
        end = i + 1;
      }
      if (code === CHAR_DOT) {
        if (startDot === -1) startDot = i;
        else if (preDotState !== 1) preDotState = 1;
      } else if (startDot !== -1) {
        preDotState = -1;
      }
    }
    if (startDot === -1 || end === -1 || // We saw a non-dot character immediately before the dot
    preDotState === 0 || // The (right-most) trimmed path component is exactly '..'
    preDotState === 1 && startDot === end - 1 && startDot === startPart + 1) {
      if (end !== -1) {
        if (startPart === 0 && isAbsolute2) {
          ret.base = ret.name = StringPrototypeSlice(path, 1, end);
        } else {
          ret.base = ret.name = StringPrototypeSlice(path, startPart, end);
        }
      }
    } else {
      if (startPart === 0 && isAbsolute2) {
        ret.name = StringPrototypeSlice(path, 1, startDot);
        ret.base = StringPrototypeSlice(path, 1, end);
      } else {
        ret.name = StringPrototypeSlice(path, startPart, startDot);
        ret.base = StringPrototypeSlice(path, startPart, end);
      }
      ret.ext = StringPrototypeSlice(path, startDot, end);
    }
    if (startPart > 0) ret.dir = StringPrototypeSlice(path, 0, startPart - 1);
    else if (isAbsolute2) ret.dir = "/";
    return ret;
  }
  const _makeLong = toNamespacedPath;
  const _default = {
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
  };
  return {
    sep,
    delimiter,
    resolve,
    normalize,
    isAbsolute,
    join,
    relative,
    toNamespacedPath,
    dirname,
    basename,
    extname,
    format,
    parse,
    _makeLong,
    default: _default
  };
})();
