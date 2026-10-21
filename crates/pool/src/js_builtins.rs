//! Production React builtins for `@js/react*` (rfd#64 amendment 2).
//!
//! The resolver serves these specifiers in run/serve/bundle with no install
//! and no `simien.json` entry. Development React stays feature-gated under
//! `crates/http/vendor/react/`.
//!
//! Seams live in sibling modules:
//!
//! - [`vendor`]: pinned production CJS sources, hashes, and export lists.
//! - [`specs`]: user-facing specifiers, URL mapping, and reachable files.
//! - [`rewrite`]: dsc stub rewrite and host-owned CJS inlining.
//! - [`esm`]: ESM wrappers around the vendored CJS factories.
//! - [`islands`]: `client:*` host wrap and live-signal SSR prerender.

mod esm;
mod islands;
mod rewrite;
mod specs;
mod vendor;

pub use esm::esm_for_file;
pub use rewrite::{DscExternalStub, DscTranspileRewrite, inline_into, rewrite_for_dsc_transpile};
pub use specs::{
    DEV_BYTE_PROBES, URL_PREFIX, USER_SPECS, file_for_user_spec, filter_imports, is_builtin,
    is_builtin_url, load_esm, reachable_files, resolve_specifier, source_imports_builtins,
    url_for_file,
};
pub use vendor::REACT_VERSION;

#[cfg(test)]
#[path = "js_builtins_tests.rs"]
mod tests;
