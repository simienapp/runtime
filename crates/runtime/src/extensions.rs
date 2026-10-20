use simien_core::Extension;
use engine::config::ServeMode;
use platform_server::extensions_for_php_server;

pub(crate) fn extensions_for_mode(mode: &ServeMode) -> Vec<Extension> {
    let _ = mode;
    extensions_for_php_server()
}
