use std::sync::{Arc, OnceLock};

use simien_core::Extension;

use crate::config::RuntimeConfig;
use crate::introspect_archive::IntrospectArchive;
use pool::{HandlerKey, IsolatePool, IsolateResponse, PoolConfig, RequestData, RequestTrace};

pub struct RuntimeEngine {
    pool: IsolatePool,
    archive: Option<IntrospectArchive>,
}

static ENGINE: OnceLock<Arc<RuntimeEngine>> = OnceLock::new();

impl RuntimeEngine {
    pub fn new(
        pool_config: PoolConfig,
        runtime_config: &RuntimeConfig,
        extensions_provider: Arc<dyn Fn() -> Vec<Extension> + Send + Sync>,
    ) -> Self {
        let pool = IsolatePool::new(pool_config, extensions_provider);
        let retention_days = runtime_config.introspect_retention_days();
        let archive = runtime_config.introspect_db_path().and_then(|path| {
            if retention_days == 0 {
                None
            } else {
                Some(IntrospectArchive::new(path, retention_days))
            }
        });

        Self { pool, archive }
    }

    pub fn pool(&self) -> &IsolatePool {
        &self.pool
    }

    pub fn archive(&self) -> Option<IntrospectArchive> {
        self.archive.clone()
    }

    pub async fn execute(
        &self,
        handler_key: HandlerKey,
        request_data: RequestData,
    ) -> Result<IsolateResponse, String> {
        self.pool.execute(handler_key, request_data).await
    }

    pub async fn drain_request_history_before(&self, cutoff_ms: u64) -> Vec<RequestTrace> {
        self.pool.drain_request_history_before(cutoff_ms).await
    }
}

pub fn set_engine(engine: Arc<RuntimeEngine>) -> Result<(), String> {
    ENGINE
        .set(engine)
        .map_err(|_| "RuntimeEngine already initialized".to_string())
}

pub fn engine() -> Option<&'static Arc<RuntimeEngine>> {
    ENGINE.get()
}
