use deka_cli_core::{CommandSpec, Context, ParamSpec, Registry};
use deka_modules::modules::MODULES_DIR;

use crate::dsc as build_dsc;
use crate::publish as build_publish;
use std::fs;
use std::path::{Path, PathBuf};

use crate::{project, single_file};

const COMMAND: CommandSpec = CommandSpec {
    owner: "build",
    name: "build",
    category: "project",
    summary: "build a DekaScript file into a JavaScript module",
    aliases: &[],
    subcommands: &[],
    handler: cmd,
};

pub fn register(registry: &mut Registry) {
    registry.add_command(COMMAND);
    registry.add_flag(deka_cli_core::FlagSpec {
        name: "--bundle",
        aliases: &[],
        description: "bundle emitted JS into a single file (in-memory, no intermediate files)",
    });
    registry.add_flag(deka_cli_core::FlagSpec {
        name: "--minify",
        aliases: &[],
        description: "minify bundled output (only with --bundle)",
    });
    registry.add_param(ParamSpec {
        name: "--out",
        description: "output JavaScript file path",
    });
}

pub fn cmd(context: &Context) {
    if let Err(err) = run(context) {
        stdio::error("build", &err);
        std::process::exit(1);
    }
}

fn run(context: &Context) -> Result<(), String> {
    if let Some(first) = context.args.positionals.first() {
        if Path::new(first)
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("phpx"))
        {
            return Err(format!(
                "DekaScript uses .ds only; migrate '{}' before building it",
                first
            ));
        }
        if project::is_deka_source_path(Path::new(first)) {
            return single_file::run(context, first);
        }
    }

    run_web_project_build(context)
}

fn run_web_project_build(context: &Context) -> Result<(), String> {
    let root_hint = context
        .args
        .positionals
        .first()
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir().map_err(|err| err.to_string())?);

    let project_root = project::resolve_project_root(&root_hint)?;
    build_publish::recover_interrupted_publish(&project_root);
    project::ensure_web_project_layout(&project_root)?;

    let app_dir = project_root.join("app");
    let public_dir = project_root.join("public");
    let entry_path = project::resolve_web_entry(&project_root)?;
    let entry_source = fs::read_to_string(&entry_path)
        .map_err(|err| format!("failed to read {}: {}", entry_path.display(), err))?;
    let hydration_enabled = has_hydration_component(&entry_source);
    let bundle = bundle_enabled(context);
    let minify = minify_enabled(context);

    // dsc is required. There is no in-process compiler, and raw app/ .ds is
    // not the server product. Prefer default emit into staging (so dist/ is
    // not created on a failed compile), then promote trees and copy host
    // static files (public/ → dist/client, prerender/worker/_redirects/…).
    let dsc = build_dsc::require_dsc()?;
    // The dist staging tree hosts the generated server entries while they
    // compile: it sits inside the project root at the same relative depth as
    // the serve-time compiler cache (`.cache/dekascript`), so the entries it
    // generates import project sources by identical relative specifiers, and
    // dsc resolves them against the real sources.
    let staged = build_publish::stage_dist(&project_root)?;
    let entries_dir = staged.root.join("entries");
    let staging =
        tempfile::tempdir().map_err(|err| format!("failed to create build staging dir: {err}"))?;
    let staging_root = staging.path();

    let src_dir = project_root.join("src");
    let api_dir = project_root.join("api");
    let emitted_src = src_dir.is_dir();
    let emitted_api = api_dir.is_dir();

    match build_dsc::emit_project(&project_root, staging_root)? {
        build_dsc::ProjectEmit::Default => {
            // Default emit already copies src/ non-.ds verbatim. Only fill
            // leftovers dsc left out (skip when the dest file already exists).
            if emitted_src {
                build_dsc::copy_non_ds_tree(&src_dir, &staging_root.join("src"), true)?;
            }
        }
        build_dsc::ProjectEmit::NeedsTranspileFallback => {
            build_dsc::emit_source_tree(&app_dir, &staging_root.join("app"), &project_root)?;
            if emitted_src {
                build_dsc::emit_source_tree(&src_dir, &staging_root.join("src"), &project_root)?;
            }
            if emitted_api {
                build_dsc::emit_source_tree(&api_dir, &staging_root.join("api"), &project_root)?;
            }
        }
    }

    // Dsc emits build-only entries separately from the runtime graph. Execute
    // and validate all of them before promoting staged output; the runtime
    // graph then imports only the materialized `deka:dev/<slot>` literals.
    let planned = build_dsc::collect_build_plans(
        &project_root,
        &[app_dir.as_path(), src_dir.as_path(), api_dir.as_path()],
    )?;

    // Plan the route manifest BEFORE any build entry executes or any route
    // renders: it is the pre-execution validation boundary (plan version,
    // slot shape, route disposition, output collisions). Non-app-router
    // projects do not use the manifest. The app/api scans are kept: the
    // publication step below builds the deployment descriptor from them —
    // the same source walks the manifest was planned from, never fresh ones
    // (deka#719).
    let app_scans =
        if runtime_core::dist::is_source_app_router_project(&project_root) {
            Some((
                runtime_core::dist::scan_app_dir(&app_dir),
                runtime_core::dist::scan_api_dir(&api_dir),
            ))
        } else {
            None
        };
    let mut manifest = app_scans
        .as_ref()
        .map(|(app_manifest, api_entries)| {
            runtime_core::dist::BuildManifest::plan(
                &project_root,
                &planned,
                build_dsc::dsc_identity(),
                app_manifest,
                api_entries,
            )
        })
        .transpose()?;

    // The build phase runs under the project's resolved security policy
    // (simien.json + CLI overrides); permitted local reads are recorded per
    // slot for targeted `deka dev` invalidation (deka#725).
    let materialized = crate::slots::materialize_planned_slots(
        &context.args.flags,
        &context.args.params,
        &project_root,
        staging_root,
        &planned,
        false,
        None,
        Some(dsc.clone()),
    )?;
    let values = materialized.values.clone();

    // StaticParams routes become concrete instances from the materialized
    // values (post-execution, pre-render).
    if let Some(manifest) = manifest.as_mut() {
        crate::slots::attach_observations(manifest, &materialized.observations);
        manifest.expand_static_params(&values)?;
    }

    // Everything below writes into the staged dist tree that atomically
    // replaces dist/ only after every step succeeds (deka#719).
    let dist_root = staged.root.join("dist");
    let dist_client = dist_root.join("client");
    let dist_server = dist_root.join("server");

    fs::create_dir_all(&dist_client)
        .map_err(|err| format!("failed to create {}: {}", dist_client.display(), err))?;

    // Build-time server entries (deka#762): generate the page/api/defer
    // router entries, compile them through dsc, and re-root the graph into
    // dist/server as source-free, loader-ready modules. The compiled entries
    // are what `deka serve` loads — serve no longer generates
    // .cache/dekascript/serve-entry.dsx for built projects.
    crate::server_entries::emit_server_entries(
        &crate::server_entries::ServerEntriesPlan {
            project_root: &project_root,
            staging_root,
            entries_dir: &entries_dir,
            dist_server: &dist_server,
            has_manifest: manifest.is_some(),
            emitted_src,
            emitted_api,
            dsc: &dsc,
        },
    )?;

    copy_dir_recursive(&public_dir, &dist_client)?;

    let islands_logical = dist_client.join("assets").join("islands.js");
    if pool::islands::emit_islands_bundle(&project_root, &dsc, &islands_logical)? {
        let bytes = fs::read(&islands_logical).map_err(|err| {
            format!(
                "failed to read {}: {err}",
                islands_logical.display()
            )
        })?;
        let digest = runtime_core::dist::sha256_hex(&bytes);
        let hash = digest.get(..10).unwrap_or(&digest);
        let hashed = dist_client
            .join("assets")
            .join(format!("islands.{hash}.js"));
        fs::rename(&islands_logical, &hashed).map_err(|err| {
            format!(
                "failed to hash islands bundle {} -> {}: {err}",
                islands_logical.display(),
                hashed.display()
            )
        })?;
    }

    let client_index = dist_client.join("index.html");
    if runtime_core::dist::is_source_app_router_project(&project_root) {
        // Static prerendering (deka build → dist HTML) is paused with the
        // framework (deka#881); the native branch no longer renders. The
        // non-native fallback still copies a hand-written index.html.
    } else {
        let index_src = project_root.join("index.html");
        if index_src.is_file() {
            fs::copy(&index_src, &client_index).map_err(|err| {
                format!(
                    "failed to copy {} -> {}: {}",
                    index_src.display(),
                    client_index.display(),
                    err
                )
            })?;
        }
        let index_raw = fs::read_to_string(&client_index)
            .map_err(|err| format!("failed to read {}: {}", client_index.display(), err))?;
        let template_html = extract_template_html(&entry_source).unwrap_or_default();
        let with_app = inject_app_html(&index_raw, &template_html);

        let final_index = if hydration_enabled {
            let dist_assets = dist_client.join("assets");
            fs::create_dir_all(&dist_assets)
                .map_err(|err| format!("failed to create {}: {}", dist_assets.display(), err))?;

            let client_js = dist_assets.join("main.js");
            if bundle {
                single_file::build_bundle_to_path(&entry_path, &client_js, minify)?;
            } else {
                single_file::build_to_path(&entry_path, &client_js)?;
            }

            if !bundle {
                let assets_importmap = dist_assets.join("importmap.json");
                let client_importmap = dist_client.join("importmap.json");
                if assets_importmap.is_file() {
                    fs::copy(&assets_importmap, &client_importmap).map_err(|err| {
                        format!(
                            "failed to copy {} -> {}: {}",
                            assets_importmap.display(),
                            client_importmap.display(),
                            err
                        )
                    })?;
                }
            }

            inject_web_bootstrap_tags(&with_app, true, bundle)
        } else {
            inject_web_bootstrap_tags(&with_app, false, bundle)
        };

        fs::write(&client_index, final_index)
            .map_err(|err| format!("failed to write {}: {}", client_index.display(), err))?;
    }

    let modules_dir = project_root.join(MODULES_DIR);
    if modules_dir.is_dir() {
        copy_dir_recursive(&modules_dir, &dist_server.join(MODULES_DIR))?;
        // The client import map references the @deka scope under /ds_modules/
        // (see `stdlib_prefix_target`), and dist/client is the static root the
        // browser fetches from — without this copy every prefix URL 404s in
        // production. Only the @deka scope is public-by-construction; @user
        // and legacy unscoped trees stay server-only.
        let scoped = modules_dir.join("@deka");
        if scoped.is_dir() {
            copy_dir_recursive(&scoped, &dist_client.join(MODULES_DIR).join("@deka"))?;
        }
    }
    for file in ["simien.json", "deka.lock"] {
        let src = project_root.join(file);
        if src.is_file() {
            let dst = dist_server.join(file);
            fs::copy(&src, &dst).map_err(|err| {
                format!(
                    "failed to copy {} -> {}: {}",
                    src.display(),
                    dst.display(),
                    err
                )
            })?;
        }
    }

    let want_trailing = read_trailing_slash(&project_root);
    let redirects = runtime_core::dist::cloudflare_redirects(want_trailing);
    fs::write(dist_root.join("_redirects"), redirects.as_bytes()).map_err(|err| {
        format!(
            "failed to write {}: {err}",
            dist_root.join("_redirects").display()
        )
    })?;
    fs::write(dist_client.join("_redirects"), redirects.as_bytes()).map_err(|err| {
        format!(
            "failed to write {}: {err}",
            dist_client.join("_redirects").display()
        )
    })?;

    let needs_worker = runtime_core::dist::project_needs_worker(&project_root);
    let mut worker_emitted = false;
    match read_serve_kind(&project_root)? {
        Some(ServeKind::Static) if needs_worker => {
            return Err(
                "serve.kind is \"static\" but api/ routes or server:defer islands exist; set serve.kind to \"worker\""
                    .to_string(),
            );
        }
        Some(ServeKind::Worker) => {
            write_cloudflare_worker(&project_root, &dist_root)?;
            worker_emitted = true;
        }
        None if needs_worker => {
            write_cloudflare_worker(&project_root, &dist_root)?;
            worker_emitted = true;
        }
        _ => {}
    }

    // RFD 24 §10.7: dist HTML references the content-hashed asset names and
    // inlines the client import map (assets/importmap.json) when one was emitted.
    // Inlining is not app-router-only: the web-bootstrap path used to inject a
    // `src=` import map browsers reject (deka#624).
    rewrite_dist_html_asset_urls(&dist_client)?;
    // The compiled server entries carry the same logical /assets URLs and the
    // generation-time importmap placeholder; bake the final hashed names and
    // the built import map in (the built entry is what serves production —
    // there is no serve-time rewrite pass for it).
    crate::server_entries::rewrite_server_entry_asset_urls(&dist_server, &dist_client)?;

    // dist/ must be deployable without .cache/ (deka#738 F7): ship the
    // materialized build-value modules in dist and rewrite deka:dev/
    // specifiers to relative paths (fails the build if one survives), and
    // verify every server-module relative specifier resolves inside
    // dist/server. Bare ui/* specifiers are paused-framework imports: they
    // are intentionally left unrewritten here.
    crate::server_graph::publish_build_values(
        &project_root,
        &dist_root,
        &dist_server,
        &planned,
    )?;

    // The deployment descriptor (manifest v2): routes, server entries, slots,
    // and every payload with its digest, anchored by the sha256 sidecar.
    // Server-entry provenance is the plan-time scans carried since the
    // manifest was planned — publication does not re-scan sources (deka#719).
    let artifact = match (manifest.as_ref(), app_scans.as_ref()) {
        (Some(plan), Some((app_manifest, api_entries))) => Some(
            build_publish::build_artifact_manifest(
                plan,
                &project_root,
                &dist_root,
                worker_emitted,
                want_trailing,
                app_manifest,
                api_entries,
            )?,
        ),
        _ => None,
    };

    // The staged tree is complete: hash its artifacts into the manifests,
    // persist them, atomically replace dist/, then print the route table.
    build_publish::finalize(&project_root, &staged, manifest.as_mut(), artifact)?;

    // publish renamed the staged tree into place; report the real dist paths.
    let dist_root = project_root.join("dist");
    let dist_client = dist_root.join("client");
    let dist_server = dist_root.join("server");

    let report = format!(
        "built web project {}\n  client: {}\n  server: {}\n  hydration: {}",
        project_root.display(),
        dist_client.display(),
        dist_server.display(),
        if hydration_enabled { "enabled" } else { "disabled" }
    );
    stdio::success(&report);
    Ok(())
}

fn bundle_enabled(context: &Context) -> bool {
    context.args.flags.get("--bundle").copied().unwrap_or(false)
}

fn minify_enabled(context: &Context) -> bool {
    context.args.flags.get("--minify").copied().unwrap_or(false)
}

pub(crate) fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
    if fs::read_dir(src)
        .map_err(|err| format!("failed to read {}: {}", src.display(), err))?
        .next()
        .is_none()
    {
        return Ok(());
    }
    fs::create_dir_all(dst)
        .map_err(|err| format!("failed to create {}: {}", dst.display(), err))?;
    let entries =
        fs::read_dir(src).map_err(|err| format!("failed to read {}: {}", src.display(), err))?;

    for entry in entries {
        let entry = entry.map_err(|err| format!("read_dir entry error: {}", err))?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|err| format!("file_type error for {}: {}", src_path.display(), err))?;

        if file_type.is_dir() {
            // deka#1065: ds_modules/.cache is the compiler's own scratch
            // space (dsc's transpile dump, build manifests/values) — never
            // part of the artifact. Shipping it duplicated dsc's raw,
            // un-rewritten output into dist/server/ds_modules/.cache/...,
            // which then failed the server-jail import-linkage check because
            // that copy's specifiers were never rerooted.
            if entry.file_name() == ".cache" {
                continue;
            }
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|err| format!("failed to create {}: {}", parent.display(), err))?;
            }
            fs::copy(&src_path, &dst_path).map_err(|err| {
                format!(
                    "failed to copy {} -> {}: {}",
                    src_path.display(),
                    dst_path.display(),
                    err
                )
            })?;
        }
    }

    Ok(())
}

fn inject_web_bootstrap_tags(
    index_html: &str,
    hydration_enabled: bool,
    bundle_enabled: bool,
) -> String {
    // Never inject a `src=` import map. Browsers reject that attribute on
    // `<script type="importmap">` (deka#624). The dist-HTML rewrite inlines
    // `assets/importmap.json` when the file exists. Strip both historical
    // spellings so a missed swap cannot ship the invalid tag.
    let stale_import_maps = [
        r#"<script type="importmap" src="/importmap.json"></script>"#,
        runtime_core::dist::CLIENT_IMPORTMAP_PLACEHOLDER_TAG,
    ];
    let module_tag = r#"<script type="module" src="/assets/main.js"></script>"#;

    let mut out = index_html.to_string();

    if !hydration_enabled {
        for tag in stale_import_maps {
            out = out.replace(tag, "");
        }
        out = out.replace(module_tag, "");
        return out;
    }

    if bundle_enabled {
        for tag in stale_import_maps {
            out = out.replace(tag, "");
        }
    }

    if !out.contains(module_tag) {
        if out.contains("</body>") {
            out = out.replace("</body>", &format!("  {}\n</body>", module_tag));
        } else {
            out.push('\n');
            out.push_str(module_tag);
            out.push('\n');
        }
    }

    out
}

fn has_hydration_component(source: &str) -> bool {
    source.contains("<Hydration") || source.contains("<Hydration/")
}

fn extract_template_html(_source: &str) -> Option<String> {
    // RFD 24: no frontmatter / trailing template capture.
    None
}

fn inject_app_html(index_html: &str, app_html: &str) -> String {
    if app_html.trim().is_empty() {
        return index_html.to_string();
    }

    let mount = "<div id=\"app\"></div>";
    if index_html.contains(mount) {
        return index_html.replacen(mount, &format!("<div id=\"app\">{}</div>", app_html), 1);
    }

    if index_html.contains("</body>") {
        return index_html.replacen("</body>", &format!("{}\n</body>", app_html), 1);
    }

    let mut out = index_html.to_string();
    out.push('\n');
    out.push_str(app_html);
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ServeKind {
    Static,
    Worker,
}

fn read_serve_kind(project_root: &Path) -> Result<Option<ServeKind>, String> {
    let Some(serve) = read_serve_object(project_root)? else {
        return Ok(None);
    };
    match serve.get("kind").and_then(|v| v.as_str()) {
        None => Ok(None),
        Some("static") => Ok(Some(ServeKind::Static)),
        Some("worker") => Ok(Some(ServeKind::Worker)),
        Some(other) => Err(format!(
            "simien.json serve.kind must be \"static\" or \"worker\", got {other:?}"
        )),
    }
}

fn read_trailing_slash(project_root: &Path) -> bool {
    read_serve_object(project_root)
        .ok()
        .flatten()
        .and_then(|serve| {
            serve
                .get("trailingSlash")
                .or_else(|| serve.get("trailing_slash"))
                .and_then(|v| v.as_bool())
        })
        .unwrap_or(false)
}

fn read_serve_object(project_root: &Path) -> Result<Option<serde_json::Value>, String> {
    let path = project_root.join("simien.json");
    if !path.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|err| format!("failed to parse {}: {err}", path.display()))?;
    Ok(value.get("serve").cloned())
}

fn write_cloudflare_worker(project_root: &Path, dist_root: &Path) -> Result<(), String> {
    let entry = runtime_core::dist::write_worker_router_entry(project_root)?;
    let entry_source = fs::read_to_string(&entry)
        .map_err(|err| format!("failed to read {}: {err}", entry.display()))?;
    let graph_imports = deka_modules::ds_imports::paths(&entry_source);
    project::ensure_project_layout(project_root, None, &graph_imports)?;
    let bundled = build_dsc::transpile_bundle(project_root, &entry, false)?;
    let public_files = runtime_core::dist::collect_public_rel_paths(project_root);
    let public_json = serde_json::to_string(&public_files)
        .map_err(|err| format!("failed to encode public paths: {err}"))?;
    let want_trailing = if read_trailing_slash(project_root) {
        "true"
    } else {
        "false"
    };
    let source = format!(
        r#"// Generated by deka build. Worker in front of static assets (api/ and defer).
{bundled}

const PUBLIC_FILES = new Set({public_json});
const WANT_TRAILING = {want_trailing};

function canonicalizePath(pathname) {{
  let path = String(pathname || "/");
  if (!path.startsWith("/")) path = "/" + path;
  path = path.replace(/\/{{2,}}/g, "/");
  return path || "/";
}}

function dekaApiRequest(request, path, body) {{
  const headers = {{ accept: request.headers.get("accept") || "" }};
  try {{
    for (const [key, value] of request.headers.entries()) {{
      if (value != null) headers[String(key).toLowerCase()] = String(value);
    }}
  }} catch (_) {{}}
  return {{
    url: request.url,
    pathname: path,
    method: request.method,
    headers,
    body: body || "",
  }};
}}

function workerResponse(result, method) {{
  const status = result && result.status != null ? result.status : 200;
  const body = method === "HEAD" ? "" : (result && result.body != null ? result.body : "");
  const headers = {{}};
  const rawHeaders = result && result.headers ? result.headers : {{}};
  for (const key of Object.keys(rawHeaders)) {{
    if (rawHeaders[key] != null) headers[key] = String(rawHeaders[key]);
  }}
  return new Response(body, {{ status, headers }});
}}

export default {{
  async fetch(request, env) {{
    const url = new URL(request.url);
    let path = canonicalizePath(url.pathname);
    const method = request.method || "GET";
    const isGetHead = method === "GET" || method === "HEAD";
    const skipSlashRedirect = path === "/api" || path.startsWith("/api/") || path === "/_deka/defer" || path.startsWith("/_deka/");
    if (isGetHead && !skipSlashRedirect) {{
      if (path.length > 1 && path.endsWith("/")) {{
        if (!WANT_TRAILING) {{
          url.pathname = path.slice(0, -1);
          return Response.redirect(url.toString(), 301);
        }}
      }} else if (WANT_TRAILING && path.length > 1) {{
        url.pathname = path + "/";
        return Response.redirect(url.toString(), 301);
      }}
    }}
    if (path.length > 1 && path.endsWith("/")) path = path.slice(0, -1);
    const body = isGetHead ? "" : await request.text();
    const req = dekaApiRequest(request, path, body);
    if ((path === "/_deka/defer") && typeof DeferApp === "function") {{
      const result = await Promise.resolve(DeferApp(req));
      return workerResponse(result, method);
    }}
    if (PUBLIC_FILES.has(path) && env && env.ASSETS) {{
      return env.ASSETS.fetch(request);
    }}
    if (path === "/api" || path.startsWith("/api/")) {{
      const result = await Promise.resolve(App(req));
      return workerResponse(result, request.method);
    }}
    if (env && env.ASSETS) return env.ASSETS.fetch(request);
    return new Response("Not found", {{ status: 404 }});
  }}
}};
"#
    );
    let dest = dist_root.join("_worker.js");
    fs::write(&dest, source.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", dest.display()))?;
    Ok(())
}


/// Rewrite unhashed `/assets/...` URLs in every dist HTML file to the
/// content-hashed names emitted next to them, and wire the client import map
/// into documents that load hashed chunks.
/// Renames come from the shared collector in `runtime::asset_urls` — the same
/// source the server-entry rewrite uses, so dev and prod agree by construction.
/// The map is inlined: browsers reject the `src` form of the element, so
/// `assets/importmap.json` stays on disk as the tooling/test copy and the
/// document carries the JSON body. App-router and web-bootstrap share this
/// path (deka#624).
fn rewrite_dist_html_asset_urls(dist_client: &Path) -> Result<(), String> {
    let assets_dir = dist_client.join("assets");
    let mut renames: Vec<(String, String)> = Vec::new();
    runtime::collect_hashed_asset_renames(&assets_dir, &assets_dir, &mut renames)?;
    let importmap_tag = if assets_dir.join("importmap.json").is_file() {
        runtime::inline_importmap_tag(&assets_dir)?
    } else {
        None
    };
    rewrite_html_asset_urls(dist_client, &renames, importmap_tag.as_deref())?;
    Ok(())
}

fn rewrite_html_asset_urls(
    dir: &Path,
    renames: &[(String, String)],
    importmap_tag: Option<&str>,
) -> Result<(), String> {
    let Ok(reader) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in reader.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rewrite_html_asset_urls(&path, renames, importmap_tag)?;
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let mut html = fs::read_to_string(&path)
            .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
        let mut changed = false;
        for (logical, hashed) in renames {
            if html.contains(logical.as_str()) {
                html = html.replace(logical.as_str(), hashed.as_str());
                changed = true;
            }
        }
        if let Some(tag) = importmap_tag {
            // A prerendered app-router document carries the generation-time
            // placeholder (a `src` reference browsers reject); swap it for the
            // inline map rather than treating it as an existing import map.
            let placeholder = runtime_core::dist::CLIENT_IMPORTMAP_PLACEHOLDER_TAG;
            if html.contains(placeholder) {
                html = html.replace(placeholder, tag);
                changed = true;
            }
            if html.contains("/assets/") && !html.contains("type=\"importmap\"") {
                html = if html.contains("</head>") {
                    html.replacen("</head>", &format!("  {tag}\n</head>"), 1)
                } else {
                    format!("{tag}\n{html}")
                };
                changed = true;
            }
        }
        if changed {
            fs::write(&path, html.as_bytes())
                .map_err(|err| format!("failed to write {}: {err}", path.display()))?;
        }
    }
    Ok(())
}

