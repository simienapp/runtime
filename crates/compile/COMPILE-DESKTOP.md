# Desktop packaging (first cut, deka#921)

`deka compile --desktop` turns a DekaScript web project into a native desktop
binary. The window is a Tauri-stack webview (wry + tao) showing the same React
output `deka serve` would send to a browser.

There is no npm. Deka does not generate a Node Tauri project and does not invoke
`@tauri-apps/cli`. The running `deka` executable *is* the packaging driver: it
runs the web build, snapshots the served HTML, and embeds those bytes in a copy
of itself.

## Command

```sh
deka compile --desktop [project] [--outfile <path>]
```

Requires a web project: `simien.json` with `type: "serve"`, `deka.lock`, `app/`,
`public/`. dsc is required (`DEKA_DSC` or beside `deka` / on `PATH`).

## Pipeline

1. `deka build --bundle` in the project (same client pipeline as web).
2. `deka serve` on an ephemeral port; GET `/` and each local asset referenced
   by `src=` / `href=`.
3. Embed the snapshot in a copy of the current executable (`RuntimeMode::Desktop`).

At launch the binary detects the desktop VFS, opens a wry/tao window, and serves
the snapshot over `deka://localhost/`. Client React runs in WebKit.

`DEKA_DESKTOP_DUMP=1` prints `document.body.innerText` prefixed by
`__DEKA_DESKTOP_DUMP__` and exits. That is the CI-viable render check.

## Host scope

Webview launch is macOS-only in this cut. Packaging the snapshot is host-agnostic.
No `.app` bundle, installer, or cross-target matrix.

## Findings (rfd#34 / rfd#60)

- The recoverable desktop lineage (stripped in `b25d7187`) was wry + tao, not a
  generated `src-tauri` crate. Linking that stack into Deka avoids npm and a
  second Cargo graph at `compile --desktop` time.
- App-router `deka build` still does not prerender HTML (framework pause). The
  snapshot therefore comes from live `deka serve` SSR, which is the path that
  already emits React HTML and `hydrateRoot` islands for the browser.
- A full `tauri` crate + `generate_context!` / `tauri.conf.json` shell would
  reintroduce a frontend-tooling project layout. First cut keeps the webview
  and drops that packaging surface.
