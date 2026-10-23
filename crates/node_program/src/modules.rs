//! `simien:db` and `simien:schema` for a program run with `--data`: the
//! program's own module loader, with these two served from Simien.

use std::rc::Rc;

use simien_core::{
    ModuleLoadOptions, ModuleLoadReferrer, ModuleLoadResponse, ModuleLoader, ModuleSource, ModuleSourceCode,
    ModuleSpecifier, ModuleType, ResolutionKind, error::ModuleLoaderError,
};
use simien_error::JsErrorBox;

const DB: &str = "simien:db";
const SCHEMA: &str = "simien:schema";

pub struct WithData {
    inner: Rc<dyn ModuleLoader>,
}

impl WithData {
    pub fn new(inner: Rc<dyn ModuleLoader>) -> Self {
        Self { inner }
    }
}

impl ModuleLoader for WithData {
    fn resolve(&self, specifier: &str, referrer: &str, kind: ResolutionKind) -> Result<ModuleSpecifier, ModuleLoaderError> {
        if specifier == DB || specifier == SCHEMA {
            return ModuleSpecifier::parse(specifier).map_err(JsErrorBox::from_err);
        }
        self.inner.resolve(specifier, referrer, kind)
    }

    fn load(
        &self,
        specifier: &ModuleSpecifier,
        referrer: Option<&ModuleLoadReferrer>,
        options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let code = match specifier.as_str() {
            DB => data::modules::db_native(),
            SCHEMA => data::modules::SCHEMA.to_string(),
            _ => return self.inner.load(specifier, referrer, options),
        };
        ModuleLoadResponse::Sync(Ok(ModuleSource::new(
            ModuleType::JavaScript,
            ModuleSourceCode::String(code.into()),
            specifier,
            None,
        )))
    }
}
