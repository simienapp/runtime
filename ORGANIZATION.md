# How Simien is organized

Simien runs on its own JavaScript runtime: V8, a multi-core isolate pool, and native built-ins such as `simien:db`. Deno's runtime is winding down after Cloudflare's acquisition (announced 9 October 2026), so Simien owns the parts of Deno it builds on instead of depending on them. This page says where each part lives and how the parts connect.

The decision, the full list of forked crates and the plan are in [simienapp/core#1](https://github.com/simienapp/core/issues/1).

## Repositories

```
simienapp/
├── core       every Deno crate Simien owns, in one Cargo workspace
├── rusty_v8   GitHub fork of denoland/rusty_v8: the V8 bindings, mirrored and watched
├── runtime    this repo: the Simien runtime
├── cli        the `simien` CLI (alias `s`): projects, schema sync, local containers
└── site       simien.dev
```

How they depend on each other:

```
simienapp/rusty_v8 ──(v8)──▶ simienapp/core ──(simien_* crates)──▶ simienapp/runtime ──▶ simien CLI · apps
```

Until the `simien_*` crates are published on crates.io, this repo depends on `simienapp/core` by git. After that, it uses crates.io versions.

## simienapp/core

One workspace that publishes many crates, as Deno's own repository did. Folders keep Deno's paths, so upstream patches still apply to the same files.

| folder | upstream crate | published as |
|---|---|---|
| `libs/core` | `deno_core` | `simien_core` |
| `libs/ops` | `deno_ops` | `simien_ops` |
| `libs/serde_v8` | `serde_v8` | `simien_serde_v8` |
| `libs/deno_v8` | `deno_v8` | `simien_v8`, the one place V8 plugs in (it can also select QuickJS) |
| `libs/crypto` | `deno_crypto_provider` | `simien_crypto_provider` |
| `libs/deno_error` | `deno_error`, `deno_error_macro` | `simien_error`, `simien_error_macro` |
| `libs/deno_unsync` | `deno_unsync` | `simien_unsync` |
| `libs/deno_path_util` | `deno_path_util` | `simien_path_util` |
| `libs/boxed_error` | `boxed_error` | `simien_boxed_error` |
| `libs/wasm_dep_analyzer` | `wasm_dep_analyzer` | `simien_wasm_dep_analyzer` |
| `libs/deno_native_certs` | `deno_native_certs` | `simien_native_certs` |
| `ext/webidl` | `deno_webidl` | `simien_webidl` |
| `ext/web` | `deno_web` | `simien_web` |
| `ext/fetch` | `deno_fetch` | `simien_fetch` |
| `ext/crypto` | `deno_crypto` | `simien_crypto` |
| `ext/tls` | `deno_tls` | `simien_tls` |

- **Where it came from:**
  - Most of it is `denoland/deno` at v2.9.7 (16 September 2026, the last release before the acquisition), filtered with `git filter-repo` to the folders above, with history.
  - The six crates in `libs/` that had their own Deno repositories were added with `git subtree`, also with history.
- **Why one repo:** these crates change together. A `deno_core` change usually touches every extension in the same commit, so separate repositories would turn each fix into several coordinated releases. Deno moved `deno_core` into its main repository for the same reason.
- **Branches:**
  - `upstream` is Deno's filtered history and nothing else, tagged `upstream-v2.9.7` at the fork point.
  - `main` is ours: the workspace, the renames and the cuts, each as a normal PR.
- **Following Deno:**
  - A scheduled job compares each kept folder with `denoland/deno` and opens an issue when it changes, flagging security fixes.
  - The `simien_*` rename is a script driven by one name map. The same script translates an upstream patch before it's applied, so porting a fix is one command.
- **Licence:** MIT, as upstream (Copyright 2018-2026 the Deno authors). Our changes are MIT too.

### Kept and left out

Kept: the engine glue and the web APIs that make up WinterTC's Minimum Common API (streams, fetch, crypto, URL, encoding, events, timers…).

Left out:
- **`deno_napi` and the 35 crates behind it:** Deno's Node compatibility layer. No Node native addons, which is consistent with WinterTC.
- **`deno_task_shell`.**
- **`deno_permissions`:** this repo's `permissions` crate replaces it.
- **`deno_fs` and `deno_io`:** `simien_fetch` doesn't fetch `file:` URLs.
- **Not yet forked:**
  - `deno_core_icudata`: optional ICU data for `Intl`, still taken from crates.io.
  - `deno_net`: needed only if TCP sockets are added later.

## simienapp/rusty_v8

A GitHub fork of `denoland/rusty_v8`, kept separate because it is V8 itself: millions of lines of C++, its own build, and prebuilt binaries for each platform. Cloudflare keeps maintaining rusty_v8 for celld, and Simien will ask to help maintain the `v8` crate. If that doesn't happen, this fork becomes the one we build and publish. Either way, swapping V8 means changing one line in `simien_v8`.

## This repo (simienapp/runtime)

The runtime: V8 isolates spread across cores (`crates/pool`), the HTTP server, module loading, permissions and the CLI that runs apps. Apps follow WinterTC's Workers-style shape, `export default { fetch(request, env, ctx) }`. Simien features arrive as built-in modules (`import { db } from 'simien:db'`), backed by Rust in the same process.

It was revived from deka (history up to `dekaruntime/deka@08511f1b`). The deka-era cleanup comes after the switch to `simien_*` crates, as listed in core#1:
- Remove DekaScript, native UI, the PHP-era paths and the vendored React.
- Turn lints back on with a workspace `[lints]` table and zero warnings in CI.
- Keep command registration thin: commands parse arguments and call library code.
