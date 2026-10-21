//! Tests for the generated entry sources.
//!
//! Until deka#391 phase (b) makes codegen return a `Program`, the serve/defer
//! entry bodies can only be asserted as text. Where the assertion can be
//! pinned exactly (aliases, conditions, registry entries, error messages) it
//! is; whole-template checks state what they actually pin down.

use std::path::PathBuf;

use super::serve::{path_condition, wrap_layouts};
use super::{alias, exports_head, session_cookie_name};
use crate::dist::manifest::{FrameworkEntry, FrameworkEntryKind};
use crate::dist::{
    write_app_router_entry, write_defer_router_entry, write_worker_router_entry,
};

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "deka_{tag}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn entry(kind: FrameworkEntryKind, route: &str, file: &str) -> FrameworkEntry {
    FrameworkEntry {
        kind,
        route: route.into(),
        file: file.into(),
    }
}

#[test]
fn alias_is_injective_and_a_valid_identifier() {
    // Exact outputs pin the encoding; the pairs include chars that used to
    // share the `_x_` fallback (`/a.b` and `/a b` both aliased to
    // `Page_a_x_b`).
    assert_eq!(alias("Page", "/"), "Page_root");
    assert_eq!(alias("Page", "/blog/[slug]"), "Page_blog_s__l_slug_r_");
    assert_eq!(alias("Page", "/a.b"), "Page_a_x2e_b");
    let pairs: &[(&str, &str)] = &[
        ("/a/b", "/a_b"),
        ("/a-b", "/a_b"),
        ("/a.b", "/a b"),
        ("/a.b", "/a:b"),
    ];
    for (a, b) in pairs {
        assert_ne!(
            alias("Page", a),
            alias("Page", b),
            "aliases for {a:?} and {b:?} must not collide"
        );
    }
    assert!(
        alias("Page", "/blog/[slug]")
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "generated import aliases must be identifiers"
    );
}

#[test]
fn path_condition_is_exact_and_escapes_quotes() {
    assert_eq!(
        path_condition("/").unwrap(),
        "path == \"/\" || path == \"\""
    );
    assert_eq!(path_condition("/blog").unwrap(), "path == \"/blog\"");
    assert_eq!(
        path_condition("/blog/[slug]").unwrap(),
        "one_segment_after(path, \"/blog/\")"
    );
    // Exact equality, not `contains("\\\"")`: the generated condition must be
    // precisely this JSON-escaped comparison.
    assert_eq!(
        path_condition("/foo\"bar").unwrap(),
        r#"path == "/foo\"bar""#
    );
}

#[test]
fn dynamic_routes_must_be_a_single_trailing_param() {
    assert!(path_condition("/blog/[slug]").is_ok());
    assert!(path_condition("/blog/[id]/comments").is_err());
    assert!(path_condition("/[a]/[b]").is_err());
}

#[test]
fn page_call_passes_slug_from_last_segment() {
    assert_eq!(super::serve::page_call("/", "Page_root"), "<Page_root />");
    assert_eq!(
        super::serve::page_call("/blog/[slug]", "Page_blog_s__l_slug_r_"),
        "<Page_blog_s__l_slug_r_ slug={last_segment(path)} />"
    );
}

#[test]
fn wrap_layouts_desugars_loading_around_child_segment() {
    let entries = vec![
        entry(FrameworkEntryKind::Layout, "/", "app/layout.dsx"),
        entry(FrameworkEntryKind::Loading, "/", "app/loading.dsx"),
        entry(FrameworkEntryKind::Layout, "/blog", "app/blog/layout.dsx"),
        entry(FrameworkEntryKind::Loading, "/blog", "app/blog/loading.dsx"),
        entry(FrameworkEntryKind::Page, "/blog", "app/blog/page.dsx"),
    ];
    // Exact tree: each loading wraps the *child* of its segment's layout, so
    // the root loading must not wrap the root layout chrome.
    assert_eq!(
        wrap_layouts(&entries, "/blog", "Page_blog"),
        "<Layout_root><Suspense fallback={<Loading_root />}><Layout_blog><Suspense fallback={<Loading_blog />}><Page_blog /></Suspense></Layout_blog></Suspense></Layout_root>"
    );
}

#[test]
fn wrap_layouts_applies_loading_without_a_layout_at_that_segment() {
    let entries = vec![
        entry(FrameworkEntryKind::Layout, "/", "app/layout.dsx"),
        entry(FrameworkEntryKind::Loading, "/blog", "app/blog/loading.dsx"),
        entry(
            FrameworkEntryKind::Page,
            "/blog/post",
            "app/blog/post/page.dsx",
        ),
    ];
    assert_eq!(
        wrap_layouts(&entries, "/blog/post", "Page_blog_post"),
        "<Layout_root><Suspense fallback={<Loading_blog />}><Page_blog_post /></Suspense></Layout_root>"
    );
}

#[test]
fn exports_head_does_not_match_header() {
    let tmp = tmp_dir("head");
    let path = tmp.join("page.dsx");
    std::fs::write(&path, "export fn header() { return 1 }\n").unwrap();
    assert!(!exports_head(&path));
    std::fs::write(&path, "export fn head() { return <title>x</title> }\n").unwrap();
    assert!(exports_head(&path));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn session_cookie_name_reads_serve_config() {
    let tmp = tmp_dir("sid_cfg");
    assert_eq!(session_cookie_name(&tmp), "deka_sid");
    std::fs::write(
        tmp.join("simien.json"),
        "{ \"serve\": { \"sessionCookie\": \"sid\" } }\n",
    )
    .unwrap();
    assert_eq!(session_cookie_name(&tmp), "sid");
    std::fs::write(
        tmp.join("simien.json"),
        "{ \"serve\": { \"sessionCookie\": \"\" } }\n",
    )
    .unwrap();
    assert_eq!(session_cookie_name(&tmp), "");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn generated_serve_entry_merges_head_and_passes_slug() {
    let tmp = tmp_dir("gen");
    std::fs::create_dir_all(tmp.join("app/blog/[slug]")).unwrap();
    std::fs::write(
        tmp.join("index.html"),
        "<!doctype html><html><head><!--deka-head--></head><body><div id=\"app\"><!--deka-app--></div><!--deka-scripts--></body></html>\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("app/layout.dsx"),
        "interface LayoutProps { children: ReactNode }\nexport fn Layout(props: LayoutProps) {\n    return <main>{props.children}</main>;\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("app/page.dsx"),
        "export fn head() {\n    return <title>Head Merge</title>;\n}\nexport fn Page() {\n    return <section><h1>Home</h1></section>;\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("app/blog/[slug]/page.dsx"),
        "interface PageProps { slug: string }\nexport fn Page(props: PageProps) {\n    return <article>{props.slug}</article>;\n}\n",
    )
    .unwrap();
    let entry = write_app_router_entry(&tmp).expect("generate serve-entry");
    let source = std::fs::read_to_string(&entry).expect("read serve-entry");
    // Text assertions until phase (b) gives us the Program to inspect: these
    // pin the head-merge call chain and the dynamic-segment prop passing.
    assert!(
        source.contains("head_html(head_root())"),
        "generated entry should merge page head(): {source}"
    );
    assert!(
        source.contains("title: title_from_head(headHtml)"),
        "fragment payload must include the merged title (RFD 24 §8.4): {source}"
    );
    assert!(
        source.contains("slug={last_segment(path)}"),
        "generated [slug] page call should pass params: {source}"
    );
    assert!(
        source.contains("renderToStreamHtml")
            && source.contains("async fn App")
            && !source.contains("renderToStringAsync")
            && !source.contains("text/x-deka-static"),
        "serve entry must retain request streaming without static-build behavior: {source}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn generated_serve_entry_imports_suspense_for_loading() {
    let tmp = tmp_dir("loading");
    std::fs::create_dir_all(tmp.join("app")).unwrap();
    std::fs::write(
        tmp.join("index.html"),
        "<!doctype html><html><head><!--deka-head--></head><body><div id=\"app\"><!--deka-app--></div><!--deka-scripts--></body></html>\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("app/layout.dsx"),
        "interface LayoutProps { children: ReactNode }\nexport fn Layout(props: LayoutProps) {\n    return <main>{props.children}</main>;\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("app/page.dsx"),
        "export fn Page() {\n    return <section><h1>Home</h1></section>;\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("app/loading.dsx"),
        "export fn Loading() {\n    return <p>Loading...</p>;\n}\n",
    )
    .unwrap();
    let entry = write_app_router_entry(&tmp).expect("generate serve-entry");
    let source = std::fs::read_to_string(&entry).expect("read serve-entry");
    assert!(
        source.contains("import { Suspense } from \"ui/suspense\""),
        "generated entry should import Suspense: {source}"
    );
    assert!(
        source.contains("<Suspense fallback={<Loading_root />}"),
        "generated entry should desugar loading.dsx: {source}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn app_router_entry_fails_build_when_defer_lacks_fallback() {
    let tmp = tmp_dir("defer_build");
    std::fs::create_dir_all(tmp.join("app")).unwrap();
    std::fs::write(
        tmp.join("index.html"),
        "<!doctype html><html><head><!--deka-head--></head><body><div id=\"app\"><!--deka-app--></div><!--deka-scripts--></body></html>\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("app/layout.dsx"),
        "interface LayoutProps { children: ReactNode }\nexport fn Layout(props: LayoutProps) {\n    return <main>{props.children}</main>;\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("app/page.dsx"),
        "export fn Page() {\n    return <Cart server:defer />;\n}\n",
    )
    .unwrap();
    // The build error is the effect under test; pin its exact message rather
    // than a `contains` fragment.
    let err = write_app_router_entry(&tmp)
        .expect_err("server:defer without fallback must fail the build");
    assert_eq!(
        err,
        "server:defer requires a child with slot=\"fallback\" (Cart)"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn defer_entry_binds_batch_to_secret_and_cookie() {
    let tmp = tmp_dir("defer_gen");
    std::fs::create_dir_all(tmp.join("app")).unwrap();
    std::fs::write(
        tmp.join("app/page.dsx"),
        "export fn Badge() { return <strong>42</strong> }\nexport fn Page() { return <Badge server:defer><span slot=\"fallback\">.</span></Badge> }\n",
    )
    .unwrap();
    let entry = write_defer_router_entry(&tmp).expect("write defer-entry");
    let source = std::fs::read_to_string(&entry).expect("read defer-entry");
    assert!(
        source.contains("import { runDeferBatch } from \"ui/server\""),
        "{source}"
    );
    // The registry entry is deterministic; pin it exactly.
    assert!(
        source.contains(r#"{ "Badge": Defer_0 }"#),
        "component registry must map the component name to its import alias: {source}"
    );
    assert!(source.contains("runDeferBatch(request.body"), "{source}");
    assert!(source.contains("\"deka_sid\""), "{source}");
    // Regression (deka#636-era): cache-control is set by the batch runner,
    // not smuggled into request headers.
    assert!(
        !source.contains("headers: { \\\"cache-control\\\""),
        "{source}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn worker_entry_calls_run_api_router() {
    let tmp = tmp_dir("worker_gen");
    let entry = write_worker_router_entry(&tmp).expect("write worker-entry");
    let source = std::fs::read_to_string(&entry).expect("read worker-entry");
    assert!(
        source.contains("import { runApiRouter } from \"ui/router\""),
        "{source}"
    );
    assert!(source.contains("runApiRouter(request,"), "{source}");
    // Regression guard for deka#398: framework middleware is gone, so the
    // generated worker entry must not reference it.
    assert!(
        !source.contains("middleware"),
        "worker entry must not reference middleware: {source}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn generated_serve_response_matches_golden() {
    let tmp = tmp_dir("response_golden");
    std::fs::create_dir_all(tmp.join("app")).unwrap();
    std::fs::write(tmp.join("index.html"), "<!doctype html><html><head><!--deka-head--></head><body><!--deka-app--><!--deka-scripts--></body></html>").unwrap();
    std::fs::write(
        tmp.join("app/page.dsx"),
        "export fn Page() { return <p>Hello</p> }\n",
    )
    .unwrap();
    std::fs::write(tmp.join("app/layout.dsx"), "interface Props { children: ReactNode }\nexport fn Layout(props: Props) { return <main>{props.children}</main> }\n").unwrap();
    let entry = write_app_router_entry(&tmp).unwrap();
    let source = std::fs::read_to_string(entry).unwrap();
    let respond = source
        .split("async fn respond(")
        .nth(1)
        .unwrap()
        .split("\nasync fn App(")
        .next()
        .unwrap();
    assert_eq!(
        format!("async fn respond({respond}"),
        include_str!("goldens/serve_response.dsx")
    );
    std::fs::remove_dir_all(tmp).unwrap();
}

#[test]
fn generated_serve_entry_preserves_defer_shell_wiring() {
    // deka#718 Phase A: classifying the route ◐ must not disturb the defer
    // wiring the document already had — the shell still loads the defer
    // asset, binds the request to the defer secret/session, and generates
    // the /_deka/defer batch entry alongside the serve entry.
    let tmp = tmp_dir("defer_serve");
    std::fs::create_dir_all(tmp.join("app")).unwrap();
    std::fs::write(
        tmp.join("index.html"),
        "<!doctype html><html><head></head><body><div id=\"app\"></div></body></html>\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("app/layout.dsx"),
        "interface LayoutProps { children: ReactNode }\nexport fn Layout(props: LayoutProps) {\n    return <main>{props.children}</main>;\n}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("app/page.dsx"),
        "export fn Page() {\n    return <Cart server:defer><CartSkeleton slot=\"fallback\" /></Cart>;\n}\n",
    )
    .unwrap();

    let entry = write_app_router_entry(&tmp).expect("generate serve-entry");
    let source = std::fs::read_to_string(&entry).expect("read serve-entry");
    assert!(
        source.contains("/assets/islands-defer.js"),
        "the document must keep loading the defer asset: {source}"
    );
    assert!(
        source.contains("bindDefer"),
        "the request must stay bound to the defer secret/session: {source}"
    );

    // The /_deka/defer batch entry is generated next to the serve entry and
    // still binds the deferred component registry.
    let defer_entry = crate::dist::compiler_cache_dir(&tmp).join("defer-entry.dsx");
    assert!(defer_entry.is_file(), "defer-entry.dsx must be generated");
    let defer_source = std::fs::read_to_string(&defer_entry).expect("read defer-entry");
    assert!(
        defer_source.contains("runDeferBatch"),
        "defer entry must still run the batch: {defer_source}"
    );
    assert!(
        defer_source.contains(r#"{ "Cart": Defer_0 }"#),
        "defer entry must register the Cart island: {defer_source}"
    );
    let _ = std::fs::remove_dir_all(&tmp);
}
