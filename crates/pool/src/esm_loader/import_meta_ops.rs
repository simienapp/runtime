//! Native op backing `import.meta.resolve()` (rfd#12 amendment, deka#1139).
//!
//! `resolve()` must follow "the same lockfile-first resolution as imports"
//! — the exact rule [`super::PhpxEsmLoader::resolve_path`] already applies to
//! every `import` statement — so this is a thin op that borrows the
//! isolate's own loader out of [`OpState`] and calls straight into it,
//! rather than reimplementing any resolution logic in JavaScript. The
//! isolate that owns a `handler_entry` puts a clone of its `PhpxEsmLoader`
//! into `OpState` once, at construction (`worker_core.rs`); an isolate with
//! no loader (no `handler_entry`) never installs this extension's state, and
//! the op is simply unreachable — `import.meta.resolve` is only ever
//! generated for DekaScript/JS modules, which only load through a loader.

use std::cell::RefCell;
use std::rc::Rc;

use simien_core::OpState;
use simien_core::op2;

use super::PhpxEsmLoader;

#[op2]
#[string]
fn op_deka_import_meta_resolve(
    op_state: Rc<RefCell<OpState>>,
    #[string] specifier: String,
    #[string] referrer: String,
) -> Result<String, simien_core::error::CoreError> {
    let op_state = op_state.borrow();
    let loader = op_state.borrow::<PhpxEsmLoader>();
    loader
        .resolve_for_import_meta(&specifier, &referrer)
        .map_err(|err| simien_core::error::CoreError::from(std::io::Error::other(err)))
}

simien_core::extension!(deka_import_meta, ops = [op_deka_import_meta_resolve],);

pub fn init() -> simien_core::Extension {
    deka_import_meta::init()
}
