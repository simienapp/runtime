use super::bridge_metrics::record_bridge_proto_metric;
use super::db_pg::{json_to_pg_param, pg_cell_to_json, with_pg_client};
use super::security::enforce_db;
use super::*;
use postgres::types::ToSql;

pub(super) struct DbConn {
    key: String,
    config: DbDriverConfig,
}

#[derive(Clone)]
pub(super) struct PgConnConfig {
    pub(super) host: String,
    pub(super) port: u16,
    pub(super) database: String,
    pub(super) user: String,
    pub(super) password: String,
}

#[derive(Clone)]
pub(super) struct SqliteConnConfig {
    path: String,
}

#[derive(Clone)]
pub(super) struct MysqlConnConfig {
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
}

#[derive(Clone)]
pub(super) enum DbDriverConfig {
    Postgres(PgConnConfig),
    Sqlite(SqliteConnConfig),
    Mysql(MysqlConnConfig),
}

impl DbDriverConfig {
    fn driver_name(&self) -> &'static str {
        match self {
            DbDriverConfig::Postgres(_) => "postgres",
            DbDriverConfig::Sqlite(_) => "sqlite",
            DbDriverConfig::Mysql(_) => "mysql",
        }
    }
}

#[derive(Clone, serde::Serialize)]
pub(super) struct DbMetric {
    calls: u64,
    errors: u64,
    total_ms: u64,
}

pub(super) struct DbState {
    next_handle: u64,
    handles: HashMap<u64, DbConn>,
    key_to_handle: HashMap<String, u64>,
    statement_cache: HashMap<u64, HashSet<String>>,
    statement_cache_hits: u64,
    statement_cache_misses: u64,
    metrics: HashMap<String, DbMetric>,
}

impl DbState {
    fn new() -> Self {
        Self {
            next_handle: 1,
            handles: HashMap::new(),
            key_to_handle: HashMap::new(),
            statement_cache: HashMap::new(),
            statement_cache_hits: 0,
            statement_cache_misses: 0,
            metrics: HashMap::new(),
        }
    }

    fn record_metric(&mut self, action: &str, driver: &str, elapsed_ms: u64, is_error: bool) {
        let key = format!("{}:{}", action, driver);
        let metric = self.metrics.entry(key).or_insert(DbMetric {
            calls: 0,
            errors: 0,
            total_ms: 0,
        });
        metric.calls += 1;
        if is_error {
            metric.errors += 1;
        }
        metric.total_ms = metric.total_ms.saturating_add(elapsed_ms);
    }

    fn touch_statement_cache(&mut self, handle: u64, sql: &str) {
        let entry = self.statement_cache.entry(handle).or_default();
        if entry.contains(sql) {
            self.statement_cache_hits = self.statement_cache_hits.saturating_add(1);
            return;
        }
        entry.insert(sql.to_string());
        self.statement_cache_misses = self.statement_cache_misses.saturating_add(1);
    }

    fn statement_cache_entries(&self) -> u64 {
        self.statement_cache
            .values()
            .map(|set| set.len() as u64)
            .sum::<u64>()
    }
}

static DB_STATE: OnceLock<Mutex<DbState>> = OnceLock::new();

pub(super) fn db_state() -> &'static Mutex<DbState> {
    DB_STATE.get_or_init(|| Mutex::new(DbState::new()))
}

pub(super) fn sanitize_conn_value(value: &str) -> String {
    value
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

fn with_sqlite_conn<T>(
    cfg: SqliteConnConfig,
    f: impl FnOnce(&SqliteConnection) -> Result<T, simien_core::error::CoreError> + Send + 'static,
) -> Result<T, simien_core::error::CoreError>
where
    T: Send + 'static,
{
    std::thread::spawn(move || {
        let path = sanitize_conn_value(&cfg.path);
        let conn = SqliteConnection::open(&path).map_err(|e| {
            simien_core::error::CoreError::from(std::io::Error::other(format!(
                "sqlite open failed: {} (path={})",
                e, path
            )))
        })?;
        f(&conn)
    })
    .join()
    .map_err(|_| {
        simien_core::error::CoreError::from(std::io::Error::other("db worker thread panicked"))
    })?
}

fn json_to_sqlite_value(value: &serde_json::Value) -> rusqlite::types::Value {
    match value {
        serde_json::Value::Null => rusqlite::types::Value::Null,
        serde_json::Value::Bool(v) => rusqlite::types::Value::Integer(if *v { 1 } else { 0 }),
        serde_json::Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                rusqlite::types::Value::Integer(i)
            } else if let Some(f) = v.as_f64() {
                rusqlite::types::Value::Real(f)
            } else {
                rusqlite::types::Value::Null
            }
        }
        serde_json::Value::String(v) => rusqlite::types::Value::Text(v.clone()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            rusqlite::types::Value::Text(value.to_string())
        }
    }
}

fn sqlite_cell_to_json(row: &rusqlite::Row<'_>, idx: usize) -> serde_json::Value {
    match row.get_ref(idx) {
        Ok(SqliteValueRef::Null) => serde_json::Value::Null,
        Ok(SqliteValueRef::Integer(v)) => serde_json::Value::Number(serde_json::Number::from(v)),
        Ok(SqliteValueRef::Real(v)) => serde_json::Number::from_f64(v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Ok(SqliteValueRef::Text(v)) => {
            serde_json::Value::String(String::from_utf8_lossy(v).to_string())
        }
        Ok(SqliteValueRef::Blob(v)) => serde_json::Value::String(format!("{:?}", v)),
        Err(_) => serde_json::Value::Null,
    }
}

fn with_mysql_conn<T>(
    cfg: MysqlConnConfig,
    f: impl FnOnce(&mut mysql::PooledConn) -> Result<T, simien_core::error::CoreError> + Send + 'static,
) -> Result<T, simien_core::error::CoreError>
where
    T: Send + 'static,
{
    std::thread::spawn(move || {
        let host = sanitize_conn_value(&cfg.host);
        let user = sanitize_conn_value(&cfg.user);
        let database = sanitize_conn_value(&cfg.database);
        let password = sanitize_conn_value(&cfg.password);

        let opts = OptsBuilder::new()
            .ip_or_hostname(Some(host.clone()))
            .tcp_port(cfg.port)
            .user(Some(user.clone()))
            .pass(Some(password.clone()))
            .db_name(Some(database.clone()))
            .tcp_connect_timeout(Some(Duration::from_secs(3)))
            .read_timeout(Some(Duration::from_secs(5)))
            .write_timeout(Some(Duration::from_secs(5)));

        let pool = MyPool::new(opts).map_err(|e| {
            simien_core::error::CoreError::from(std::io::Error::other(format!(
                "mysql pool failed: {} (host={}, port={}, database={}, user={})",
                e, host, cfg.port, database, user
            )))
        })?;
        let mut conn = pool.get_conn().map_err(|e| {
            simien_core::error::CoreError::from(std::io::Error::other(format!(
                "mysql connect failed: {}",
                e
            )))
        })?;
        f(&mut conn)
    })
    .join()
    .map_err(|_| {
        simien_core::error::CoreError::from(std::io::Error::other("db worker thread panicked"))
    })?
}

fn json_to_mysql_value(value: &serde_json::Value) -> MyValue {
    match value {
        serde_json::Value::Null => MyValue::NULL,
        serde_json::Value::Bool(v) => MyValue::Int(if *v { 1 } else { 0 }),
        serde_json::Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                MyValue::Int(i)
            } else if let Some(u) = v.as_u64() {
                MyValue::UInt(u)
            } else if let Some(f) = v.as_f64() {
                MyValue::Double(f)
            } else {
                MyValue::NULL
            }
        }
        serde_json::Value::String(v) => MyValue::Bytes(v.clone().into_bytes()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            MyValue::Bytes(value.to_string().into_bytes())
        }
    }
}

fn mysql_value_to_json(value: &MyValue) -> serde_json::Value {
    match value {
        MyValue::NULL => serde_json::Value::Null,
        MyValue::Bytes(v) => serde_json::Value::String(String::from_utf8_lossy(v).to_string()),
        MyValue::Int(v) => serde_json::Value::Number(serde_json::Number::from(*v)),
        MyValue::UInt(v) => serde_json::Value::Number(serde_json::Number::from(*v)),
        MyValue::Float(v) => serde_json::Number::from_f64(*v as f64)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        MyValue::Double(v) => serde_json::Number::from_f64(*v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        MyValue::Date(y, m, d, h, i, s, micros) => serde_json::Value::String(format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:06}",
            y, m, d, h, i, s, micros
        )),
        MyValue::Time(neg, days, h, i, s, micros) => serde_json::Value::String(format!(
            "{}{} {:02}:{:02}:{:02}.{:06}",
            if *neg { "-" } else { "" },
            days,
            h,
            i,
            s,
            micros
        )),
    }
}

pub(super) fn db_call_impl(
    action: String,
    args: serde_json::Value,
) -> Result<serde_json::Value, simien_core::error::CoreError> {
    let err = |msg: String| {
        simien_core::error::CoreError::from(std::io::Error::new(std::io::ErrorKind::Other, msg))
    };

    let args_obj = args.as_object().cloned().unwrap_or_default();
    match action.as_str() {
        "open" => {
            let started = Instant::now();
            let raw_driver = args_obj
                .get("driver")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let driver = raw_driver
                .split('\0')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let cfg = args_obj
                .get("config")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();

            let (key, driver_cfg) = match driver.as_str() {
                d if d.starts_with("postgres") => {
                    let host = cfg
                        .get("host")
                        .and_then(|v| v.as_str())
                        .unwrap_or("127.0.0.1")
                        .trim_matches('\0')
                        .to_string();
                    let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(5432);
                    let user = cfg
                        .get("user")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim_matches('\0')
                        .to_string();
                    let password = cfg
                        .get("password")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim_matches('\0')
                        .to_string();
                    let database = cfg
                        .get("database")
                        .and_then(|v| v.as_str())
                        .or_else(|| cfg.get("dbname").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .trim_matches('\0')
                        .to_string();
                    (
                        format!(
                            "postgres://{}:{}@{}:{}/{}",
                            user, password, host, port, database
                        ),
                        DbDriverConfig::Postgres(PgConnConfig {
                            host,
                            port: port as u16,
                            database,
                            user,
                            password,
                        }),
                    )
                }
                d if d.starts_with("sqlite") => {
                    let path = cfg
                        .get("path")
                        .and_then(|v| v.as_str())
                        .or_else(|| cfg.get("file").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .trim_matches('\0')
                        .to_string();
                    if path.is_empty() {
                        return Ok(serde_json::json!({
                            "ok": false,
                            "error": "sqlite open requires config.path"
                        }));
                    }
                    (
                        format!("sqlite://{}", path),
                        DbDriverConfig::Sqlite(SqliteConnConfig { path }),
                    )
                }
                d if d.starts_with("mysql") => {
                    let host = cfg
                        .get("host")
                        .and_then(|v| v.as_str())
                        .unwrap_or("127.0.0.1")
                        .trim_matches('\0')
                        .to_string();
                    let port = cfg.get("port").and_then(|v| v.as_u64()).unwrap_or(3306);
                    let user = cfg
                        .get("user")
                        .and_then(|v| v.as_str())
                        .unwrap_or("root")
                        .trim_matches('\0')
                        .to_string();
                    let password = cfg
                        .get("password")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .trim_matches('\0')
                        .to_string();
                    let database = cfg
                        .get("database")
                        .and_then(|v| v.as_str())
                        .or_else(|| cfg.get("dbname").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .trim_matches('\0')
                        .to_string();
                    (
                        format!(
                            "mysql://{}:{}@{}:{}/{}",
                            user, password, host, port, database
                        ),
                        DbDriverConfig::Mysql(MysqlConnConfig {
                            host,
                            port: port as u16,
                            database,
                            user,
                            password,
                        }),
                    )
                }
                _ => {
                    return Ok(serde_json::json!({
                        "ok": false,
                        "error": format!("unsupported driver '{}'", driver)
                    }));
                }
            };

            {
                let state = db_state()
                    .lock()
                    .map_err(|_| err("db lock poisoned".to_string()))?;
                if let Some(handle) = state.key_to_handle.get(&key).copied() {
                    drop(state);
                    if let Ok(mut state) = db_state().lock() {
                        state.record_metric(
                            "open",
                            &driver,
                            started.elapsed().as_millis() as u64,
                            false,
                        );
                    }
                    return Ok(serde_json::json!({
                        "ok": true,
                        "handle": handle,
                        "reused": true
                    }));
                }
            }

            let mut state = db_state()
                .lock()
                .map_err(|_| err("db lock poisoned".to_string()))?;
            if let Some(handle) = state.key_to_handle.get(&key).copied() {
                state.record_metric("open", &driver, started.elapsed().as_millis() as u64, false);
                return Ok(serde_json::json!({
                    "ok": true,
                    "handle": handle,
                    "reused": true
                }));
            }
            let handle = state.next_handle;
            state.next_handle += 1;
            state.handles.insert(
                handle,
                DbConn {
                    key: key.clone(),
                    config: driver_cfg,
                },
            );
            state.key_to_handle.insert(key, handle);
            state.record_metric("open", &driver, started.elapsed().as_millis() as u64, false);
            Ok(serde_json::json!({
                "ok": true,
                "handle": handle,
                "reused": false
            }))
        }
        "query" => {
            let started = Instant::now();
            let handle = args_obj
                .get("handle")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| err("query: missing handle".to_string()))?;
            let sql = args_obj
                .get("sql")
                .and_then(|v| v.as_str())
                .ok_or_else(|| err("query: missing sql".to_string()))?;
            let params = args_obj
                .get("params")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            let driver_cfg = {
                let mut state = db_state()
                    .lock()
                    .map_err(|_| err("db lock poisoned".to_string()))?;
                let driver_name = {
                    let conn = state
                        .handles
                        .get(&handle)
                        .ok_or_else(|| err(format!("query: unknown handle {}", handle)))?;
                    conn.config.clone()
                };
                state.touch_statement_cache(handle, sql);
                driver_name
            };
            let driver_name = driver_cfg.driver_name();
            let sql = sql.to_string();
            let out_rows_result = match driver_cfg {
                DbDriverConfig::Postgres(cfg) => with_pg_client(cfg, move |client| {
                    let boxed: Vec<Box<dyn ToSql + Sync>> =
                        params.iter().map(json_to_pg_param).collect();
                    let refs: Vec<&(dyn ToSql + Sync)> = boxed.iter().map(|v| v.as_ref()).collect();
                    let rows = client.query(&sql, &refs).map_err(|e| {
                        simien_core::error::CoreError::from(std::io::Error::other(format!(
                            "postgres query failed: {}",
                            e
                        )))
                    })?;

                    let mut out_rows = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let mut obj = serde_json::Map::new();
                        for idx in 0..row.len() {
                            let name = row.columns()[idx].name().to_string();
                            obj.insert(name, pg_cell_to_json(row, idx));
                        }
                        out_rows.push(serde_json::Value::Object(obj));
                    }
                    Ok(out_rows)
                })?,
                DbDriverConfig::Sqlite(cfg) => with_sqlite_conn(cfg, move |conn| {
                    let mut stmt = conn.prepare(&sql).map_err(|e| {
                        simien_core::error::CoreError::from(std::io::Error::other(format!(
                            "sqlite prepare failed: {}",
                            e
                        )))
                    })?;
                    let sqlite_params: Vec<rusqlite::types::Value> =
                        params.iter().map(json_to_sqlite_value).collect();
                    let mut rows = stmt
                        .query(sqlite_params_from_iter(sqlite_params.iter()))
                        .map_err(|e| {
                            simien_core::error::CoreError::from(std::io::Error::other(format!(
                                "sqlite query failed: {}",
                                e
                            )))
                        })?;

                    let mut out_rows = Vec::new();
                    while let Some(row) = rows.next().map_err(|e| {
                        simien_core::error::CoreError::from(std::io::Error::other(format!(
                            "sqlite row fetch failed: {}",
                            e
                        )))
                    })? {
                        let mut obj = serde_json::Map::new();
                        let row_ref = row.as_ref();
                        for idx in 0..row_ref.column_count() {
                            let name = row_ref.column_name(idx).unwrap_or("").to_string();
                            obj.insert(name, sqlite_cell_to_json(row, idx));
                        }
                        out_rows.push(serde_json::Value::Object(obj));
                    }
                    Ok(out_rows)
                })?,
                DbDriverConfig::Mysql(cfg) => with_mysql_conn(cfg, move |conn| {
                    let mysql_params =
                        MyParams::Positional(params.iter().map(json_to_mysql_value).collect());
                    let rows: Vec<mysql::Row> = conn.exec(&sql, mysql_params).map_err(|e| {
                        simien_core::error::CoreError::from(std::io::Error::other(format!(
                            "mysql query failed: {}",
                            e
                        )))
                    })?;

                    let mut out_rows = Vec::with_capacity(rows.len());
                    for row in &rows {
                        let mut obj = serde_json::Map::new();
                        let cols = row.columns_ref();
                        for (idx, col) in cols.iter().enumerate() {
                            let name = col.name_str().to_string();
                            let value = row
                                .as_ref(idx)
                                .map(mysql_value_to_json)
                                .unwrap_or(serde_json::Value::Null);
                            obj.insert(name, value);
                        }
                        out_rows.push(serde_json::Value::Object(obj));
                    }
                    Ok(out_rows)
                })?,
            };
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let mut metric_state = db_state()
                .lock()
                .map_err(|_| err("db lock poisoned".to_string()))?;
            metric_state.record_metric("query", driver_name, elapsed_ms, false);
            drop(metric_state);
            let out_rows = out_rows_result;

            Ok(serde_json::json!({
                "ok": true,
                "rows": out_rows
            }))
        }
        "exec" => {
            let started = Instant::now();
            let handle = args_obj
                .get("handle")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| err("exec: missing handle".to_string()))?;
            let sql = args_obj
                .get("sql")
                .and_then(|v| v.as_str())
                .ok_or_else(|| err("exec: missing sql".to_string()))?;
            let params = args_obj
                .get("params")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();

            let driver_cfg = {
                let mut state = db_state()
                    .lock()
                    .map_err(|_| err("db lock poisoned".to_string()))?;
                let driver_name = {
                    let conn = state
                        .handles
                        .get(&handle)
                        .ok_or_else(|| err(format!("exec: unknown handle {}", handle)))?;
                    conn.config.clone()
                };
                state.touch_statement_cache(handle, sql);
                driver_name
            };
            let driver_name = driver_cfg.driver_name();
            let sql = sql.to_string();
            let affected_result = match driver_cfg {
                DbDriverConfig::Postgres(cfg) => with_pg_client(cfg, move |client| {
                    let boxed: Vec<Box<dyn ToSql + Sync>> =
                        params.iter().map(json_to_pg_param).collect();
                    let refs: Vec<&(dyn ToSql + Sync)> = boxed.iter().map(|v| v.as_ref()).collect();
                    client.execute(&sql, &refs).map_err(|e| {
                        simien_core::error::CoreError::from(std::io::Error::other(format!(
                            "postgres exec failed: {}",
                            e
                        )))
                    })
                })?,
                DbDriverConfig::Sqlite(cfg) => with_sqlite_conn(cfg, move |conn| {
                    let mut stmt = conn.prepare(&sql).map_err(|e| {
                        simien_core::error::CoreError::from(std::io::Error::other(format!(
                            "sqlite prepare failed: {}",
                            e
                        )))
                    })?;
                    let sqlite_params: Vec<rusqlite::types::Value> =
                        params.iter().map(json_to_sqlite_value).collect();
                    let changed = stmt
                        .execute(sqlite_params_from_iter(sqlite_params.iter()))
                        .map_err(|e| {
                            simien_core::error::CoreError::from(std::io::Error::other(format!(
                                "sqlite exec failed: {}",
                                e
                            )))
                        })?;
                    Ok(changed as u64)
                })?,
                DbDriverConfig::Mysql(cfg) => with_mysql_conn(cfg, move |conn| {
                    let mysql_params =
                        MyParams::Positional(params.iter().map(json_to_mysql_value).collect());
                    let result = conn.exec_iter(&sql, mysql_params).map_err(|e| {
                        simien_core::error::CoreError::from(std::io::Error::other(format!(
                            "mysql exec failed: {}",
                            e
                        )))
                    })?;
                    Ok(result.affected_rows())
                })?,
            };
            let elapsed_ms = started.elapsed().as_millis() as u64;
            let mut metric_state = db_state()
                .lock()
                .map_err(|_| err("db lock poisoned".to_string()))?;
            metric_state.record_metric("exec", driver_name, elapsed_ms, false);
            drop(metric_state);
            let affected = affected_result;

            Ok(serde_json::json!({
                "ok": true,
                "affected_rows": affected
            }))
        }
        "begin" => {
            // Transaction scope across multiple calls is not supported in stateless mode.
            Ok(serde_json::json!({ "ok": true }))
        }
        "commit" => Ok(serde_json::json!({ "ok": true })),
        "rollback" => Ok(serde_json::json!({ "ok": true })),
        "close" => {
            let started = Instant::now();
            let handle = args_obj
                .get("handle")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| err("close: missing handle".to_string()))?;
            let mut state = db_state()
                .lock()
                .map_err(|_| err("db lock poisoned".to_string()))?;
            if let Some(conn) = state.handles.remove(&handle) {
                state.record_metric(
                    "close",
                    conn.config.driver_name(),
                    started.elapsed().as_millis() as u64,
                    false,
                );
                state.key_to_handle.remove(&conn.key);
                state.statement_cache.remove(&handle);
            }
            Ok(serde_json::json!({ "ok": true }))
        }
        "stats" => {
            let state = db_state()
                .lock()
                .map_err(|_| err("db lock poisoned".to_string()))?;
            let mut handles_by_driver: HashMap<String, u64> = HashMap::new();
            for conn in state.handles.values() {
                let key = conn.config.driver_name().to_string();
                let prev = handles_by_driver.get(&key).copied().unwrap_or(0);
                handles_by_driver.insert(key, prev + 1);
            }

            let mut metrics = serde_json::Map::new();
            for (key, metric) in &state.metrics {
                let avg_ms = if metric.calls == 0 {
                    0
                } else {
                    metric.total_ms / metric.calls
                };
                metrics.insert(
                    key.clone(),
                    serde_json::json!({
                        "calls": metric.calls,
                        "errors": metric.errors,
                        "total_ms": metric.total_ms,
                        "avg_ms": avg_ms
                    }),
                );
            }

            Ok(serde_json::json!({
                "ok": true,
                "active_handles": state.handles.len() as u64,
                "handles_by_driver": handles_by_driver,
                "statement_cache_entries": state.statement_cache_entries(),
                "statement_cache_hits": state.statement_cache_hits,
                "statement_cache_misses": state.statement_cache_misses,
                "metrics": metrics
            }))
        }
        _ => Ok(serde_json::json!({
            "ok": false,
            "error": format!("unknown db action '{}'", action)
        })),
    }
}

#[derive(Clone, Copy)]
pub(super) enum DbProtoActionKind {
    Open,
    Query,
    QueryOne,
    Exec,
    Begin,
    Commit,
    Rollback,
    Close,
    Stats,
}

pub(super) fn db_json_to_proto_value(value: &serde_json::Value) -> proto::bridge_v1::Value {
    use proto::bridge_v1::value::Kind;
    let kind = match value {
        serde_json::Value::Null => Some(Kind::NullValue(proto::bridge_v1::NullValue {})),
        serde_json::Value::Bool(v) => Some(Kind::BoolValue(*v)),
        serde_json::Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                Some(Kind::IntValue(i))
            } else if let Some(f) = v.as_f64() {
                Some(Kind::FloatValue(f))
            } else {
                Some(Kind::StringValue(v.to_string()))
            }
        }
        serde_json::Value::String(v) => Some(Kind::StringValue(v.clone())),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            Some(Kind::StringValue(value.to_string()))
        }
    };
    proto::bridge_v1::Value { kind }
}

pub(super) fn db_proto_to_json_value(value: &proto::bridge_v1::Value) -> serde_json::Value {
    use proto::bridge_v1::value::Kind;
    match value.kind.as_ref() {
        Some(Kind::NullValue(_)) => serde_json::Value::Null,
        Some(Kind::BoolValue(v)) => serde_json::Value::Bool(*v),
        Some(Kind::IntValue(v)) => serde_json::Value::Number((*v).into()),
        Some(Kind::FloatValue(v)) => serde_json::Number::from_f64(*v)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(Kind::StringValue(v)) => serde_json::Value::String(v.clone()),
        Some(Kind::BytesValue(v)) => serde_json::Value::Array(
            v.iter()
                .map(|b| serde_json::Value::Number((*b as u64).into()))
                .collect(),
        ),
        None => serde_json::Value::Null,
    }
}

pub(super) fn db_proto_request_to_action_payload(
    req: &proto::bridge_v1::DbRequest,
) -> Result<(String, serde_json::Value, DbProtoActionKind), simien_core::error::CoreError> {
    use proto::bridge_v1::db_request::Action;
    let Some(action) = req.action.as_ref() else {
        return Err(core_err("db proto request missing action"));
    };
    match action {
        Action::Open(open) => {
            let mut cfg = serde_json::Map::new();
            for item in &open.config {
                let value = item
                    .value
                    .as_ref()
                    .map(db_proto_to_json_value)
                    .unwrap_or(serde_json::Value::Null);
                cfg.insert(item.key.clone(), value);
            }
            Ok((
                "open".to_string(),
                serde_json::json!({
                    "driver": open.driver,
                    "config": cfg
                }),
                DbProtoActionKind::Open,
            ))
        }
        Action::Query(query) => Ok((
            "query".to_string(),
            serde_json::json!({
                "handle": query.handle,
                "sql": query.sql,
                "params": query.params.iter().map(db_proto_to_json_value).collect::<Vec<_>>()
            }),
            DbProtoActionKind::Query,
        )),
        Action::QueryOne(query) => Ok((
            "query".to_string(),
            serde_json::json!({
                "handle": query.handle,
                "sql": query.sql,
                "params": query.params.iter().map(db_proto_to_json_value).collect::<Vec<_>>()
            }),
            DbProtoActionKind::QueryOne,
        )),
        Action::Exec(exec) => Ok((
            "exec".to_string(),
            serde_json::json!({
                "handle": exec.handle,
                "sql": exec.sql,
                "params": exec.params.iter().map(db_proto_to_json_value).collect::<Vec<_>>()
            }),
            DbProtoActionKind::Exec,
        )),
        Action::Begin(h) => Ok((
            "begin".to_string(),
            serde_json::json!({ "handle": h.handle }),
            DbProtoActionKind::Begin,
        )),
        Action::Commit(h) => Ok((
            "commit".to_string(),
            serde_json::json!({ "handle": h.handle }),
            DbProtoActionKind::Commit,
        )),
        Action::Rollback(h) => Ok((
            "rollback".to_string(),
            serde_json::json!({ "handle": h.handle }),
            DbProtoActionKind::Rollback,
        )),
        Action::Close(h) => Ok((
            "close".to_string(),
            serde_json::json!({ "handle": h.handle }),
            DbProtoActionKind::Close,
        )),
        Action::Stats(_) => Ok((
            "stats".to_string(),
            serde_json::json!({}),
            DbProtoActionKind::Stats,
        )),
    }
}

pub(super) fn db_action_payload_to_proto_request(
    action: &str,
    payload: &serde_json::Value,
) -> Result<proto::bridge_v1::DbRequest, simien_core::error::CoreError> {
    use proto::bridge_v1::db_request::Action;
    let args = payload.as_object().cloned().unwrap_or_default();
    let action = match action {
        "open" => {
            let driver = args
                .get("driver")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mut config = Vec::new();
            if let Some(cfg) = args.get("config").and_then(|v| v.as_object()) {
                for (key, val) in cfg {
                    config.push(proto::bridge_v1::NamedValue {
                        key: key.clone(),
                        value: Some(db_json_to_proto_value(val)),
                    });
                }
            }
            Action::Open(proto::bridge_v1::DbOpenRequest { driver, config })
        }
        "query" => {
            let handle = args.get("handle").and_then(|v| v.as_u64()).unwrap_or(0);
            let sql = args
                .get("sql")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let params = args
                .get("params")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(db_json_to_proto_value)
                .collect();
            Action::Query(proto::bridge_v1::DbQueryRequest {
                handle,
                sql,
                params,
            })
        }
        "query_one" => {
            let handle = args.get("handle").and_then(|v| v.as_u64()).unwrap_or(0);
            let sql = args
                .get("sql")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let params = args
                .get("params")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(db_json_to_proto_value)
                .collect();
            Action::QueryOne(proto::bridge_v1::DbQueryRequest {
                handle,
                sql,
                params,
            })
        }
        "exec" => {
            let handle = args.get("handle").and_then(|v| v.as_u64()).unwrap_or(0);
            let sql = args
                .get("sql")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let params = args
                .get("params")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(db_json_to_proto_value)
                .collect();
            Action::Exec(proto::bridge_v1::DbExecRequest {
                handle,
                sql,
                params,
            })
        }
        "begin" => Action::Begin(proto::bridge_v1::DbHandleRequest {
            handle: args.get("handle").and_then(|v| v.as_u64()).unwrap_or(0),
        }),
        "commit" => Action::Commit(proto::bridge_v1::DbHandleRequest {
            handle: args.get("handle").and_then(|v| v.as_u64()).unwrap_or(0),
        }),
        "rollback" => Action::Rollback(proto::bridge_v1::DbHandleRequest {
            handle: args.get("handle").and_then(|v| v.as_u64()).unwrap_or(0),
        }),
        "close" => Action::Close(proto::bridge_v1::DbHandleRequest {
            handle: args.get("handle").and_then(|v| v.as_u64()).unwrap_or(0),
        }),
        "stats" => Action::Stats(true),
        other => {
            return Err(core_err(format!("unsupported db proto action '{}'", other)));
        }
    };

    Ok(proto::bridge_v1::DbRequest {
        schema_version: 1,
        action: Some(action),
    })
}

pub(super) fn db_json_rows_to_proto_rows(value: &serde_json::Value) -> Vec<proto::bridge_v1::Row> {
    let Some(rows) = value.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        if let Some(obj) = row.as_object() {
            let fields = obj
                .iter()
                .map(|(key, val)| proto::bridge_v1::NamedValue {
                    key: key.clone(),
                    value: Some(db_json_to_proto_value(val)),
                })
                .collect();
            out.push(proto::bridge_v1::Row { fields });
        }
    }
    out
}

pub(super) fn db_proto_rows_to_json(rows: &[proto::bridge_v1::Row]) -> serde_json::Value {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut obj = serde_json::Map::new();
        for field in &row.fields {
            let val = field
                .value
                .as_ref()
                .map(db_proto_to_json_value)
                .unwrap_or(serde_json::Value::Null);
            obj.insert(field.key.clone(), val);
        }
        out.push(serde_json::Value::Object(obj));
    }
    serde_json::Value::Array(out)
}

pub(super) fn db_json_response_to_proto(
    resp: &serde_json::Value,
    kind: DbProtoActionKind,
) -> proto::bridge_v1::DbResponse {
    use proto::bridge_v1::db_response::Action;
    let ok = resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    let error = resp
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let action = match kind {
        DbProtoActionKind::Open => {
            let handle = resp.get("handle").and_then(|v| v.as_u64()).unwrap_or(0);
            let reused = resp
                .get("reused")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Some(Action::Open(proto::bridge_v1::DbOpenResponse {
                handle,
                reused,
            }))
        }
        DbProtoActionKind::Query => {
            let rows =
                db_json_rows_to_proto_rows(resp.get("rows").unwrap_or(&serde_json::Value::Null));
            Some(Action::Query(proto::bridge_v1::DbRowsResponse { rows }))
        }
        DbProtoActionKind::QueryOne => {
            let mut rows =
                db_json_rows_to_proto_rows(resp.get("rows").unwrap_or(&serde_json::Value::Null));
            if rows.len() > 1 {
                rows.truncate(1);
            }
            Some(Action::QueryOne(proto::bridge_v1::DbRowsResponse { rows }))
        }
        DbProtoActionKind::Exec => {
            let affected_rows = resp
                .get("affected_rows")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            Some(Action::Exec(proto::bridge_v1::DbExecResponse {
                affected_rows,
            }))
        }
        DbProtoActionKind::Begin => Some(Action::Begin(proto::bridge_v1::DbUnitResponse { ok })),
        DbProtoActionKind::Commit => Some(Action::Commit(proto::bridge_v1::DbUnitResponse { ok })),
        DbProtoActionKind::Rollback => {
            Some(Action::Rollback(proto::bridge_v1::DbUnitResponse { ok }))
        }
        DbProtoActionKind::Close => Some(Action::Close(proto::bridge_v1::DbUnitResponse { ok })),
        DbProtoActionKind::Stats => {
            let active_handles = resp
                .get("active_handles")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let statement_cache_entries = resp
                .get("statement_cache_entries")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let statement_cache_hits = resp
                .get("statement_cache_hits")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let statement_cache_misses = resp
                .get("statement_cache_misses")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            let mut handles_by_driver = Vec::new();
            if let Some(obj) = resp.get("handles_by_driver").and_then(|v| v.as_object()) {
                for (driver, count_val) in obj {
                    let count = count_val.as_u64().unwrap_or(0);
                    handles_by_driver.push(proto::bridge_v1::DriverCount {
                        driver: driver.clone(),
                        count,
                    });
                }
            }

            let mut metrics = Vec::new();
            if let Some(obj) = resp.get("metrics").and_then(|v| v.as_object()) {
                for (key, metric_val) in obj {
                    let calls = metric_val
                        .get("calls")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let errors = metric_val
                        .get("errors")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let total_ms = metric_val
                        .get("total_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let avg_ms = metric_val
                        .get("avg_ms")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    metrics.push(proto::bridge_v1::NamedMetric {
                        key: key.clone(),
                        metric: Some(proto::bridge_v1::DbStatsMetric {
                            calls,
                            errors,
                            total_ms,
                            avg_ms,
                        }),
                    });
                }
            }

            Some(Action::Stats(proto::bridge_v1::DbStatsResponse {
                active_handles,
                handles_by_driver,
                metrics,
                statement_cache_entries,
                statement_cache_hits,
                statement_cache_misses,
            }))
        }
    };

    proto::bridge_v1::DbResponse {
        schema_version: 1,
        ok,
        error,
        action,
    }
}

pub(super) fn db_proto_response_to_json(resp: &proto::bridge_v1::DbResponse) -> serde_json::Value {
    use proto::bridge_v1::db_response::Action;
    let mut out = serde_json::Map::new();
    out.insert("ok".to_string(), serde_json::Value::Bool(resp.ok));
    if !resp.error.is_empty() {
        out.insert(
            "error".to_string(),
            serde_json::Value::String(resp.error.clone()),
        );
    }
    if let Some(action) = resp.action.as_ref() {
        match action {
            Action::Open(open) => {
                out.insert(
                    "handle".to_string(),
                    serde_json::Value::Number(open.handle.into()),
                );
                out.insert("reused".to_string(), serde_json::Value::Bool(open.reused));
            }
            Action::Query(rows) | Action::QueryOne(rows) => {
                out.insert("rows".to_string(), db_proto_rows_to_json(&rows.rows));
            }
            Action::Exec(exec) => {
                out.insert(
                    "affected_rows".to_string(),
                    serde_json::Value::Number(exec.affected_rows.into()),
                );
            }
            Action::Begin(unit)
            | Action::Commit(unit)
            | Action::Rollback(unit)
            | Action::Close(unit) => {
                out.insert("ok".to_string(), serde_json::Value::Bool(unit.ok));
            }
            Action::Stats(stats) => {
                out.insert(
                    "active_handles".to_string(),
                    serde_json::Value::Number(stats.active_handles.into()),
                );
                let mut by_driver = serde_json::Map::new();
                for item in &stats.handles_by_driver {
                    by_driver.insert(
                        item.driver.clone(),
                        serde_json::Value::Number(item.count.into()),
                    );
                }
                out.insert(
                    "handles_by_driver".to_string(),
                    serde_json::Value::Object(by_driver),
                );
                out.insert(
                    "statement_cache_entries".to_string(),
                    serde_json::Value::Number(stats.statement_cache_entries.into()),
                );
                out.insert(
                    "statement_cache_hits".to_string(),
                    serde_json::Value::Number(stats.statement_cache_hits.into()),
                );
                out.insert(
                    "statement_cache_misses".to_string(),
                    serde_json::Value::Number(stats.statement_cache_misses.into()),
                );

                let mut metrics = serde_json::Map::new();
                for metric in &stats.metrics {
                    let m = metric.metric.as_ref();
                    metrics.insert(
                        metric.key.clone(),
                        serde_json::json!({
                            "calls": m.map(|x| x.calls).unwrap_or(0),
                            "errors": m.map(|x| x.errors).unwrap_or(0),
                            "total_ms": m.map(|x| x.total_ms).unwrap_or(0),
                            "avg_ms": m.map(|x| x.avg_ms).unwrap_or(0),
                        }),
                    );
                }
                out.insert("metrics".to_string(), serde_json::Value::Object(metrics));
            }
        }
    }
    serde_json::Value::Object(out)
}

pub(super) fn db_call_proto_impl(request: &[u8]) -> Result<Vec<u8>, simien_core::error::CoreError> {
    let started = Instant::now();
    let req = proto::bridge_v1::DbRequest::decode(request)
        .map_err(|e| core_err(format!("db proto decode failed: {}", e)))?;
    let (action, payload, kind) = db_proto_request_to_action_payload(&req)?;
    let db_target = db_target_from_payload(&action, &payload);
    let target = db_target.as_deref().unwrap_or("*");
    enforce_db(Some(target))?;
    let response_json = db_call_impl(action, payload)?;
    let response = db_json_response_to_proto(&response_json, kind);
    let out = response.encode_to_vec();
    record_bridge_proto_metric(
        "db",
        request.len(),
        out.len(),
        started.elapsed().as_micros() as u64,
    );
    Ok(out)
}

pub(super) fn db_target_from_payload(action: &str, payload: &serde_json::Value) -> Option<String> {
    if action == "stats" {
        return Some("stats".to_string());
    }
    let obj = payload.as_object()?;
    if let Some(driver) = obj.get("driver").and_then(|v| v.as_str()) {
        let trimmed = driver.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let handle = obj.get("handle").and_then(|v| v.as_u64())?;
    let state = db_state().lock().ok()?;
    let conn = state.handles.get(&handle)?;
    Some(conn.config.driver_name().to_string())
}

#[op2]
#[buffer]
pub(super) fn op_php_db_call_proto(
    #[buffer] request: &[u8],
) -> Result<Vec<u8>, simien_core::error::CoreError> {
    db_call_proto_impl(request)
}

#[op2]
#[buffer]
pub(super) fn op_php_db_proto_encode(
    #[string] action: String,
    #[serde] payload: serde_json::Value,
) -> Result<Vec<u8>, simien_core::error::CoreError> {
    let request = db_action_payload_to_proto_request(&action, &payload)?;
    Ok(request.encode_to_vec())
}

#[op2]
#[serde]
pub(super) fn op_php_db_proto_decode(
    #[buffer] response: &[u8],
) -> Result<serde_json::Value, simien_core::error::CoreError> {
    let decoded = proto::bridge_v1::DbResponse::decode(response)
        .map_err(|e| core_err(format!("db proto decode response failed: {}", e)))?;
    Ok(db_proto_response_to_json(&decoded))
}
