// Copyright 2018-2026 the Deno authors. MIT license.
// Copyright Joyent, Inc. and Node.js contributors. MIT license.
// Vendored from Deno's ext/node (deno_node 0.198.0, polyfills/path/_win32.ts),
// types stripped. Changes for Simien: internal imports point at
// ext:simien_carve_outs; there is no current directory (resolve() uses "/");
// matchesGlob is left out (it needs a file-system glob).

(function() {
  const { core, primordials } = __bootstrap;
  const {
    CHAR_BACKWARD_SLASH,
    CHAR_COLON,
    CHAR_DOT,
    CHAR_QUESTION_MARK
  } = core.loadExtScript("ext:simien_carve_outs/path/_constants.js");
  const { ERR_INVALID_ARG_TYPE } = core.loadExtScript(
    "ext:simien_carve_outs/errors.js"
  );
  const {
    _format,
    assertPath,
    isPathSeparator,
    isPosixPathSeparator,
    isWindowsDeviceRoot,
    normalizeString
  } = core.loadExtScript("ext:simien_carve_outs/path/_util.js");
  const { default: assert } = core.loadExtScript("ext:simien_carve_outs/errors.js");
  const {
    ArrayPrototypeIncludes,
    ArrayPrototypeJoin,
    ArrayPrototypePop,
    ArrayPrototypeSlice,
    StringPrototypeCharCodeAt,
    StringPrototypeIncludes,
    StringPrototypeIndexOf,
    StringPrototypeRepeat,
    StringPrototypeSlice,
    StringPrototypeSplit,
    StringPrototypeToLowerCase,
    StringPrototypeToUpperCase,
    TypeError
  } = primordials;
  const sep = "\\";
  const delimiter = ";";
  const WINDOWS_RESERVED_NAMES = [
    "CON",
    "PRN",
    "AUX",
    "NUL",
    "COM1",
    "COM2",
    "COM3",
    "COM4",
    "COM5",
    "COM6",
    "COM7",
    "COM8",
    "COM9",
    "LPT1",
    "LPT2",
    "LPT3",
    "LPT4",
    "LPT5",
    "LPT6",
    "LPT7",
    "LPT8",
    "LPT9",
    "COM\xB9",
    "COM\xB2",
    "COM\xB3",
    "LPT\xB9",
    "LPT\xB2",
    "LPT\xB3"
  ];
  function isWindowsReservedName(path, colonIndex) {
    const devicePart = StringPrototypeToUpperCase(
      StringPrototypeSlice(path, 0, colonIndex)
    );
    return ArrayPrototypeIncludes(WINDOWS_RESERVED_NAMES, devicePart);
  }
  function resolve(...pathSegments) {
    let resolvedDevice = "";
    let resolvedTail = "";
    let resolvedAbsolute = false;
    for (let i = pathSegments.length - 1; i >= -1; i--) {
      let path;
      if (i >= 0) {
        path = pathSegments[i];
      } else if (!resolvedDevice) {
        path = "/";
        if (pathSegments.length === 0 || pathSegments.length === 1 && (pathSegments[0] === "" || pathSegments[0] === ".") && isPathSeparator(StringPrototypeCharCodeAt(path, 0))) {
          return path;
        }
      } else {
        path = "/";
        if (path === void 0 || StringPrototypeToLowerCase(StringPrototypeSlice(path, 0, 2)) !== StringPrototypeToLowerCase(resolvedDevice) && StringPrototypeCharCodeAt(path, 2) === CHAR_BACKWARD_SLASH) {
          path = `${resolvedDevice}\\`;
        }
      }
      assertPath(path);
      const len = path.length;
      let rootEnd = 0;
      let device = "";
      let isAbsolute2 = false;
      const code = StringPrototypeCharCodeAt(path, 0);
      if (len === 1) {
        if (isPathSeparator(code)) {
          rootEnd = 1;
          isAbsolute2 = true;
        }
      } else if (isPathSeparator(code)) {
        isAbsolute2 = true;
        if (isPathSeparator(StringPrototypeCharCodeAt(path, 1))) {
          let j = 2;
          let last = j;
          while (j < len && !isPathSeparator(StringPrototypeCharCodeAt(path, j))) {
            j++;
          }
          if (j < len && j !== last) {
            const firstPart = StringPrototypeSlice(path, last, j);
            last = j;
            while (j < len && isPathSeparator(StringPrototypeCharCodeAt(path, j))) {
              j++;
            }
            if (j < len && j !== last) {
              last = j;
              while (j < len && !isPathSeparator(StringPrototypeCharCodeAt(path, j))) {
                j++;
              }
              if (j === len || j !== last) {
                if (firstPart !== "." && firstPart !== "?") {
                  device = `\\\\${firstPart}\\${StringPrototypeSlice(path, last, j)}`;
                  rootEnd = j;
                } else {
                  device = `\\\\${firstPart}`;
                  rootEnd = 4;
                }
              }
            }
          }
        } else {
          rootEnd = 1;
        }
      } else if (isWindowsDeviceRoot(code) && StringPrototypeCharCodeAt(path, 1) === CHAR_COLON) {
        device = StringPrototypeSlice(path, 0, 2);
        rootEnd = 2;
        if (len > 2 && isPathSeparator(StringPrototypeCharCodeAt(path, 2))) {
          isAbsolute2 = true;
          rootEnd = 3;
        }
      }
      if (device.length > 0) {
        if (resolvedDevice.length > 0) {
          if (StringPrototypeToLowerCase(device) !== StringPrototypeToLowerCase(resolvedDevice)) {
            continue;
          }
        } else {
          resolvedDevice = device;
        }
      }
      if (resolvedAbsolute) {
        if (resolvedDevice.length > 0) {
          break;
        }
      } else {
        resolvedTail = `${StringPrototypeSlice(path, rootEnd)}\\${resolvedTail}`;
        resolvedAbsolute = isAbsolute2;
        if (isAbsolute2 && resolvedDevice.length > 0) {
          break;
        }
      }
    }
    resolvedTail = normalizeString(
      resolvedTail,
      !resolvedAbsolute,
      "\\",
      isPathSeparator
    );
    return resolvedAbsolute ? `${resolvedDevice}\\${resolvedTail}` : `${resolvedDevice}${resolvedTail}` || ".";
  }
  function normalize(path) {
    assertPath(path);
    const len = path.length;
    if (len === 0) return ".";
    let rootEnd = 0;
    let device;
    let isAbsolute2 = false;
    const code = StringPrototypeCharCodeAt(path, 0);
    if (len === 1) {
      return isPosixPathSeparator(code) ? "\\" : path;
    }
    if (isPathSeparator(code)) {
      isAbsolute2 = true;
      if (isPathSeparator(StringPrototypeCharCodeAt(path, 1))) {
        let j = 2;
        let last = j;
        while (j < len && !isPathSeparator(StringPrototypeCharCodeAt(path, j))) {
          j++;
        }
        if (j < len && j !== last) {
          const firstPart = StringPrototypeSlice(path, last, j);
          last = j;
          while (j < len && isPathSeparator(StringPrototypeCharCodeAt(path, j))) {
            j++;
          }
          if (j < len && j !== last) {
            last = j;
            while (j < len && !isPathSeparator(StringPrototypeCharCodeAt(path, j))) {
              j++;
            }
            if (j === len || j !== last) {
              if (firstPart === "." || firstPart === "?") {
                device = `\\\\${firstPart}`;
                rootEnd = 4;
                const colonIndex2 = StringPrototypeIndexOf(path, ":");
                const possibleDevice = StringPrototypeSlice(
                  path,
                  4,
                  colonIndex2 + 1
                );
                if (isWindowsReservedName(possibleDevice, possibleDevice.length - 1)) {
                  device = `\\\\?\\${possibleDevice}`;
                  rootEnd = 4 + possibleDevice.length;
                }
              } else if (j === len) {
                return `\\\\${firstPart}\\${StringPrototypeSlice(path, last)}\\`;
              } else {
                device = `\\\\${firstPart}\\${StringPrototypeSlice(path, last, j)}`;
                rootEnd = j;
              }
            }
          }
        }
      } else {
        rootEnd = 1;
      }
    } else {
      const colonIndex2 = StringPrototypeIndexOf(path, ":");
      if (colonIndex2 > 0) {
        if (isWindowsDeviceRoot(code) && colonIndex2 === 1) {
          device = StringPrototypeSlice(path, 0, 2);
          rootEnd = 2;
          if (len > 2 && isPathSeparator(StringPrototypeCharCodeAt(path, 2))) {
            isAbsolute2 = true;
            rootEnd = 3;
          }
        } else if (isWindowsReservedName(path, colonIndex2)) {
          device = StringPrototypeSlice(path, 0, colonIndex2 + 1);
          rootEnd = colonIndex2 + 1;
        }
      }
    }
    let tail = rootEnd < len ? normalizeString(
      StringPrototypeSlice(path, rootEnd),
      !isAbsolute2,
      "\\",
      isPathSeparator
    ) : "";
    if (tail.length === 0 && !isAbsolute2) {
      tail = ".";
    }
    if (tail.length > 0 && isPathSeparator(StringPrototypeCharCodeAt(path, len - 1))) {
      tail += "\\";
    }
    if (!isAbsolute2 && device === void 0 && StringPrototypeIncludes(path, ":")) {
      if (tail.length >= 2 && isWindowsDeviceRoot(StringPrototypeCharCodeAt(tail, 0)) && StringPrototypeCharCodeAt(tail, 1) === CHAR_COLON) {
        return `.\\${tail}`;
      }
      let index = StringPrototypeIndexOf(path, ":");
      do {
        if (index === len - 1 || isPathSeparator(StringPrototypeCharCodeAt(path, index + 1))) {
          return `.\\${tail}`;
        }
      } while ((index = StringPrototypeIndexOf(path, ":", index + 1)) !== -1);
    }
    const colonIndex = StringPrototypeIndexOf(path, ":");
    if (isWindowsReservedName(path, colonIndex)) {
      return `.\\${device ?? ""}${tail}`;
    }
    if (device === void 0) {
      return isAbsolute2 ? `\\${tail}` : tail;
    }
    return isAbsolute2 ? `${device}\\${tail}` : `${device}${tail}`;
  }
  function isAbsolute(path) {
    assertPath(path);
    const len = path.length;
    if (len === 0) return false;
    const code = StringPrototypeCharCodeAt(path, 0);
    if (isPathSeparator(code)) {
      return true;
    } else if (isWindowsDeviceRoot(code)) {
      if (len > 2 && StringPrototypeCharCodeAt(path, 1) === CHAR_COLON) {
        if (isPathSeparator(StringPrototypeCharCodeAt(path, 2))) return true;
      }
    }
    return false;
  }
  function join(...paths) {
    const pathsCount = paths.length;
    if (pathsCount === 0) return ".";
    let joined;
    let firstPart = null;
    for (let i = 0; i < pathsCount; ++i) {
      const path = paths[i];
      assertPath(path);
      if (path.length > 0) {
        if (joined === void 0) joined = firstPart = path;
        else joined += `\\${path}`;
      }
    }
    if (joined === void 0) return ".";
    let needsReplace = true;
    let slashCount = 0;
    assert(firstPart != null);
    if (isPathSeparator(StringPrototypeCharCodeAt(firstPart, 0))) {
      ++slashCount;
      const firstLen = firstPart.length;
      if (firstLen > 1) {
        if (isPathSeparator(StringPrototypeCharCodeAt(firstPart, 1))) {
          ++slashCount;
          if (firstLen > 2) {
            if (isPathSeparator(StringPrototypeCharCodeAt(firstPart, 2))) {
              ++slashCount;
            } else {
              needsReplace = false;
            }
          }
        }
      }
    }
    if (needsReplace) {
      for (; slashCount < joined.length; ++slashCount) {
        if (!isPathSeparator(StringPrototypeCharCodeAt(joined, slashCount))) {
          break;
        }
      }
      if (slashCount >= 2) {
        joined = `\\${StringPrototypeSlice(joined, slashCount)}`;
      }
    }
    let hasReservedComponent = false;
    {
      let segStart = 0;
      for (let i = 0; i <= joined.length; i++) {
        if (i === joined.length || isPathSeparator(StringPrototypeCharCodeAt(joined, i))) {
          if (i > segStart) {
            const seg = StringPrototypeSlice(joined, segStart, i);
            const colonIdx = StringPrototypeIndexOf(seg, ":");
            if (colonIdx !== -1 && isWindowsReservedName(seg, colonIdx)) {
              hasReservedComponent = true;
              break;
            }
          }
          segStart = i + 1;
        }
      }
    }
    if (hasReservedComponent) {
      const parts = StringPrototypeSplit(joined, "/");
      return ArrayPrototypeJoin(parts, "\\");
    }
    return normalize(joined);
  }
  function relative(from, to) {
    assertPath(from);
    assertPath(to);
    if (from === to) return "";
    const fromOrig = resolve(from);
    const toOrig = resolve(to);
    if (fromOrig === toOrig) return "";
    from = StringPrototypeToLowerCase(fromOrig);
    to = StringPrototypeToLowerCase(toOrig);
    if (from === to) return "";
    if (fromOrig.length !== from.length || toOrig.length !== to.length) {
      const fromSplit = StringPrototypeSplit(fromOrig, "\\");
      const toSplit = StringPrototypeSplit(toOrig, "\\");
      if (fromSplit[fromSplit.length - 1] === "") {
        ArrayPrototypePop(fromSplit);
      }
      if (toSplit[toSplit.length - 1] === "") {
        ArrayPrototypePop(toSplit);
      }
      const fromLen2 = fromSplit.length;
      const toLen2 = toSplit.length;
      const length2 = fromLen2 < toLen2 ? fromLen2 : toLen2;
      let i2;
      for (i2 = 0; i2 < length2; i2++) {
        if (StringPrototypeToLowerCase(fromSplit[i2]) !== StringPrototypeToLowerCase(toSplit[i2])) {
          break;
        }
      }
      if (i2 === 0) {
        return toOrig;
      } else if (i2 === length2) {
        if (toLen2 > length2) {
          return ArrayPrototypeJoin(ArrayPrototypeSlice(toSplit, i2), "\\");
        }
        if (fromLen2 > length2) {
          return StringPrototypeRepeat("..\\", fromLen2 - 1 - i2) + "..";
        }
        return "";
      }
      return StringPrototypeRepeat("..\\", fromLen2 - i2) + ArrayPrototypeJoin(ArrayPrototypeSlice(toSplit, i2), "\\");
    }
    let fromStart = 0;
    while (fromStart < from.length && StringPrototypeCharCodeAt(from, fromStart) === CHAR_BACKWARD_SLASH) {
      fromStart++;
    }
    let fromEnd = from.length;
    while (fromEnd - 1 > fromStart && StringPrototypeCharCodeAt(from, fromEnd - 1) === CHAR_BACKWARD_SLASH) {
      fromEnd--;
    }
    const fromLen = fromEnd - fromStart;
    let toStart = 0;
    while (toStart < to.length && StringPrototypeCharCodeAt(to, toStart) === CHAR_BACKWARD_SLASH) {
      toStart++;
    }
    let toEnd = to.length;
    while (toEnd - 1 > toStart && StringPrototypeCharCodeAt(to, toEnd - 1) === CHAR_BACKWARD_SLASH) {
      toEnd--;
    }
    const toLen = toEnd - toStart;
    const length = fromLen < toLen ? fromLen : toLen;
    let lastCommonSep = -1;
    let i = 0;
    for (; i < length; i++) {
      const fromCode = StringPrototypeCharCodeAt(from, fromStart + i);
      if (fromCode !== StringPrototypeCharCodeAt(to, toStart + i)) {
        break;
      } else if (fromCode === CHAR_BACKWARD_SLASH) {
        lastCommonSep = i;
      }
    }
    if (i !== length) {
      if (lastCommonSep === -1) {
        return toOrig;
      }
    } else {
      if (toLen > length) {
        if (StringPrototypeCharCodeAt(to, toStart + i) === CHAR_BACKWARD_SLASH) {
          return StringPrototypeSlice(toOrig, toStart + i + 1);
        }
        if (i === 2) {
          return StringPrototypeSlice(toOrig, toStart + i);
        }
      }
      if (fromLen > length) {
        if (StringPrototypeCharCodeAt(from, fromStart + i) === CHAR_BACKWARD_SLASH) {
          lastCommonSep = i;
        } else if (i === 2) {
          lastCommonSep = 3;
        }
      }
      if (lastCommonSep === -1) {
        lastCommonSep = 0;
      }
    }
    let out = "";
    for (i = fromStart + lastCommonSep + 1; i <= fromEnd; ++i) {
      if (i === fromEnd || StringPrototypeCharCodeAt(from, i) === CHAR_BACKWARD_SLASH) {
        out += out.length === 0 ? ".." : "\\..";
      }
    }
    toStart += lastCommonSep;
    if (out.length > 0) {
      return out + StringPrototypeSlice(toOrig, toStart, toEnd);
    }
    if (StringPrototypeCharCodeAt(toOrig, toStart) === CHAR_BACKWARD_SLASH) {
      ++toStart;
    }
    return StringPrototypeSlice(toOrig, toStart, toEnd);
  }
  function toNamespacedPath(path) {
    if (typeof path !== "string") return path;
    if (path.length === 0) return "";
    const resolvedPath = resolve(path);
    if (resolvedPath.length <= 2) {
      return path;
    }
    if (StringPrototypeCharCodeAt(resolvedPath, 0) === CHAR_BACKWARD_SLASH) {
      if (StringPrototypeCharCodeAt(resolvedPath, 1) === CHAR_BACKWARD_SLASH) {
        const code = StringPrototypeCharCodeAt(resolvedPath, 2);
        if (code !== CHAR_QUESTION_MARK && code !== CHAR_DOT) {
          return `\\\\?\\UNC\\${StringPrototypeSlice(resolvedPath, 2)}`;
        }
      }
    } else if (isWindowsDeviceRoot(StringPrototypeCharCodeAt(resolvedPath, 0)) && StringPrototypeCharCodeAt(resolvedPath, 1) === CHAR_COLON && StringPrototypeCharCodeAt(resolvedPath, 2) === CHAR_BACKWARD_SLASH) {
      return `\\\\?\\${resolvedPath}`;
    }
    return resolvedPath;
  }
  function dirname(path) {
    assertPath(path);
    const len = path.length;
    if (len === 0) return ".";
    let rootEnd = -1;
    let end = -1;
    let matchedSlash = true;
    let offset = 0;
    const code = StringPrototypeCharCodeAt(path, 0);
    if (len > 1) {
      if (isPathSeparator(code)) {
        rootEnd = offset = 1;
        if (isPathSeparator(StringPrototypeCharCodeAt(path, 1))) {
          let j = 2;
          let last = j;
          for (; j < len; ++j) {
            if (isPathSeparator(StringPrototypeCharCodeAt(path, j))) break;
          }
          if (j < len && j !== last) {
            last = j;
            for (; j < len; ++j) {
              if (!isPathSeparator(StringPrototypeCharCodeAt(path, j))) break;
            }
            if (j < len && j !== last) {
              last = j;
              for (; j < len; ++j) {
                if (isPathSeparator(StringPrototypeCharCodeAt(path, j))) break;
              }
              if (j === len) {
                return path;
              }
              if (j !== last) {
                rootEnd = offset = j + 1;
              }
            }
          }
        }
      } else if (isWindowsDeviceRoot(code)) {
        if (StringPrototypeCharCodeAt(path, 1) === CHAR_COLON) {
          rootEnd = offset = 2;
          if (len > 2) {
            if (isPathSeparator(StringPrototypeCharCodeAt(path, 2))) {
              rootEnd = offset = 3;
            }
          }
        }
      }
    } else if (isPathSeparator(code)) {
      return path;
    }
    for (let i = len - 1; i >= offset; --i) {
      if (isPathSeparator(StringPrototypeCharCodeAt(path, i))) {
        if (!matchedSlash) {
          end = i;
          break;
        }
      } else {
        matchedSlash = false;
      }
    }
    if (end === -1) {
      if (rootEnd === -1) return ".";
      else end = rootEnd;
    }
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
    if (path.length >= 2) {
      const drive = StringPrototypeCharCodeAt(path, 0);
      if (isWindowsDeviceRoot(drive)) {
        if (StringPrototypeCharCodeAt(path, 1) === CHAR_COLON) start = 2;
      }
    }
    if (ext !== void 0 && ext.length > 0 && ext.length <= path.length) {
      if (ext.length === path.length && ext === path) return "";
      let extIdx = ext.length - 1;
      let firstNonSlashEnd = -1;
      for (i = path.length - 1; i >= start; --i) {
        const code = StringPrototypeCharCodeAt(path, i);
        if (isPathSeparator(code)) {
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
      for (i = path.length - 1; i >= start; --i) {
        if (isPathSeparator(StringPrototypeCharCodeAt(path, i))) {
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
    let start = 0;
    let startDot = -1;
    let startPart = 0;
    let end = -1;
    let matchedSlash = true;
    let preDotState = 0;
    if (path.length >= 2 && StringPrototypeCharCodeAt(path, 1) === CHAR_COLON && isWindowsDeviceRoot(StringPrototypeCharCodeAt(path, 0))) {
      start = startPart = 2;
    }
    for (let i = path.length - 1; i >= start; --i) {
      const code = StringPrototypeCharCodeAt(path, i);
      if (isPathSeparator(code)) {
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
    return _format("\\", pathObject);
  }
  function parse(path) {
    assertPath(path);
    const ret = { root: "", dir: "", base: "", ext: "", name: "" };
    const len = path.length;
    if (len === 0) return ret;
    let rootEnd = 0;
    let code = StringPrototypeCharCodeAt(path, 0);
    if (len > 1) {
      if (isPathSeparator(code)) {
        rootEnd = 1;
        if (isPathSeparator(StringPrototypeCharCodeAt(path, 1))) {
          let j = 2;
          let last = j;
          for (; j < len; ++j) {
            if (isPathSeparator(StringPrototypeCharCodeAt(path, j))) break;
          }
          if (j < len && j !== last) {
            last = j;
            for (; j < len; ++j) {
              if (!isPathSeparator(StringPrototypeCharCodeAt(path, j))) break;
            }
            if (j < len && j !== last) {
              last = j;
              for (; j < len; ++j) {
                if (isPathSeparator(StringPrototypeCharCodeAt(path, j))) break;
              }
              if (j === len) {
                rootEnd = j;
              } else if (j !== last) {
                rootEnd = j + 1;
              }
            }
          }
        }
      } else if (isWindowsDeviceRoot(code)) {
        if (StringPrototypeCharCodeAt(path, 1) === CHAR_COLON) {
          rootEnd = 2;
          if (len > 2) {
            if (isPathSeparator(StringPrototypeCharCodeAt(path, 2))) {
              if (len === 3) {
                ret.root = ret.dir = path;
                return ret;
              }
              rootEnd = 3;
            }
          } else {
            ret.root = ret.dir = path;
            return ret;
          }
        }
      }
    } else if (isPathSeparator(code)) {
      ret.root = ret.dir = path;
      return ret;
    }
    if (rootEnd > 0) ret.root = StringPrototypeSlice(path, 0, rootEnd);
    let startDot = -1;
    let startPart = rootEnd;
    let end = -1;
    let matchedSlash = true;
    let i = path.length - 1;
    let preDotState = 0;
    for (; i >= rootEnd; --i) {
      code = StringPrototypeCharCodeAt(path, i);
      if (isPathSeparator(code)) {
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
        ret.base = ret.name = StringPrototypeSlice(path, startPart, end);
      }
    } else {
      ret.name = StringPrototypeSlice(path, startPart, startDot);
      ret.base = StringPrototypeSlice(path, startPart, end);
      ret.ext = StringPrototypeSlice(path, startDot, end);
    }
    if (startPart > 0 && startPart !== rootEnd) {
      ret.dir = StringPrototypeSlice(path, 0, startPart - 1);
    } else ret.dir = ret.root;
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
