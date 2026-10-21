//! `simien dev`'s schema sync, the Convex-style loop. Nothing is synced by hand.
//!
//! - schema.ts saved → plan against the database → guards → DDL and pg_zega
//!   tracking, in one transaction → regenerate `simien/_generated`.
//! - The database changed directly (psql, a migration tool) → schema.ts is
//!   rewritten from it, which then syncs like any other save.
//! - Anything that would break a table or orphan data stops with an error and
//!   changes nothing.

use std::path::Path;
use std::time::{Duration, Instant};

use fallible_iterator::FallibleIterator;
use postgres::{Client, NoTls};
use project::Project;
use schema::PlanError;
use schema::catalog::{Catalog, Unmanaged};

/// The channel `simien_ddl_notify` reports DDL on.
pub const CHANNEL: &str = "simien_schema";

/// The local stack's Postgres, as the project's superuser.
pub fn database_url(project: &Project) -> Result<String, String> {
    Ok(format!(
        "postgres://postgres:{}@127.0.0.1:{}/postgres",
        project.secret("postgres_password")?,
        project.ports.postgres
    ))
}

/// Opens a connection; `timeout` bounds the connect.
pub fn connect(url: &str, timeout: Duration) -> Result<Client, String> {
    let mut config: postgres::Config = url.parse().map_err(|err| format!("{err}"))?;
    config.connect_timeout(timeout);
    config.connect(NoTls).map_err(|err| format!("{err}"))
}

/// Waits until Postgres answers a query (it accepts connections before it's
/// ready for them), then returns that connection.
pub fn ready(url: &str, timeout: Duration) -> Result<Client, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let attempt = connect(url, Duration::from_secs(2))
            .and_then(|mut client| client.simple_query("select 1").map(|_| client).map_err(|e| e.to_string()));
        match attempt {
            Ok(client) => return Ok(client),
            Err(err) if Instant::now() > deadline => return Err(format!("Postgres isn't answering: {err}")),
            Err(_) => std::thread::sleep(Duration::from_millis(500)),
        }
    }
}

/// Every ordinary table in `public`, with columns, primary key and foreign keys.
const CATALOG: &str = r#"
select coalesce(json_agg(t), '[]')::text from (
  select c.relname as name,
    coalesce((select json_agg(json_build_object(
        'name', a.attname, 'type', format_type(a.atttypid, a.atttypmod), 'nullable', not a.attnotnull) order by a.attnum)
      from pg_attribute a where a.attrelid = c.oid and a.attnum > 0 and not a.attisdropped), '[]') as columns,
    coalesce((select json_agg(a.attname order by array_position(i.indkey::int2[], a.attnum))
      from pg_index i join pg_attribute a on a.attrelid = i.indrelid and a.attnum = any(i.indkey)
      where i.indrelid = c.oid and i.indisprimary), '[]') as "primaryKey",
    coalesce((select json_agg(json_build_object(
        'columns', (select json_agg(a.attname order by k.ord) from unnest(con.conkey) with ordinality k(n, ord)
                    join pg_attribute a on a.attrelid = con.conrelid and a.attnum = k.n),
        'table', rc.relname,
        'refColumns', (select json_agg(a.attname order by k.ord) from unnest(con.confkey) with ordinality k(n, ord)
                       join pg_attribute a on a.attrelid = con.confrelid and a.attnum = k.n)))
      from pg_constraint con join pg_class rc on rc.oid = con.confrelid
      where con.conrelid = c.oid and con.contype = 'f'), '[]') as "foreignKeys"
  from pg_class c join pg_namespace n on n.oid = c.relnamespace
  where n.nspname = 'public' and c.relkind = 'r'
  order by c.relname
) t"#;

pub fn catalog(client: &mut Client) -> Result<Catalog, String> {
    let row = client.query_one(CATALOG, &[]).map_err(|err| err.to_string())?;
    let text: String = row.get(0);
    let tables = serde_json::from_str(&text).map_err(|err| format!("catalog: {err}"))?;
    Ok(Catalog { tables })
}

/// Why a sync changed nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncError {
    /// `simien/schema.ts` doesn't exist.
    Missing,
    /// schema.ts is wrong at a line and column.
    Parse { message: String, line: usize, column: usize },
    /// schema.ts names a table Postgres has that isn't Simien's.
    Clash(String),
    /// Guards found data the change would break or orphan. Nothing was applied.
    Blocked(Vec<String>),
    /// Postgres or the file system failed.
    Failed(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Missing => f.write_str("simien/schema.ts is missing"),
            SyncError::Parse { message, .. } => f.write_str(message),
            SyncError::Clash(message) | SyncError::Failed(message) => f.write_str(message),
            SyncError::Blocked(blocked) => {
                let count = if blocked.len() == 1 { "a change".to_string() } else { format!("{} changes", blocked.len()) };
                write!(f, "schema.ts asks for {count} that would break or orphan data; nothing was applied")
            }
        }
    }
}

impl From<PlanError> for SyncError {
    fn from(error: PlanError) -> Self {
        match error {
            PlanError::Parse(e) => SyncError::Parse { message: e.message, line: e.line, column: e.column },
            PlanError::Clash(message) => SyncError::Clash(message),
        }
    }
}

/// Applies a plan all or nothing: guards first (they only read), then the DDL,
/// pg_zega tracking and reloads, in one transaction. `simien.syncing` keeps our
/// own DDL from coming back as a "the database changed" notification.
pub fn apply(client: &mut Client, plan: &schema::plan::Plan) -> Result<(), SyncError> {
    let failed = |err: postgres::Error| SyncError::Failed(err.to_string());
    let mut tx = client.transaction().map_err(failed)?;
    tx.batch_execute("select set_config('simien.syncing', 'on', true)").map_err(failed)?;
    let mut blocked = Vec::new();
    for guard in &plan.guards {
        let rows = tx.query(guard.sql.as_str(), &[]).map_err(failed)?;
        if rows.first().and_then(|row| row.try_get::<_, bool>(0).ok()) == Some(true) {
            blocked.push(guard.error.clone());
        }
    }
    if !blocked.is_empty() {
        return Err(SyncError::Blocked(blocked));
    }
    for statement in &plan.statements {
        tx.batch_execute(statement).map_err(failed)?;
    }
    tx.commit().map_err(failed)
}

/// What one sync did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Synced {
    pub changes: Vec<String>,
    pub warnings: Vec<String>,
    /// Whether `simien/_generated/dataModel.ts` changed.
    pub generated: bool,
}

/// One sync: schema.ts → database → generated code. Changes nothing on an error.
pub fn sync_once(project: &Project, client: &mut Client) -> Result<Synced, SyncError> {
    let file = project.schema_file();
    if !file.exists() {
        return Err(SyncError::Missing);
    }
    let text = std::fs::read_to_string(&file).map_err(|err| SyncError::Failed(format!("{}: {err}", file.display())))?;
    let current = catalog(client).map_err(SyncError::Failed)?;
    let planned = schema::plan_from(&text, &current)?;
    if !planned.plan.statements.is_empty() {
        apply(client, &planned.plan)?;
    }
    let generated = write_generated(project, &planned.data_model).map_err(SyncError::Failed)?;
    Ok(Synced {
        changes: planned.plan.changes,
        warnings: planned.plan.warnings,
        generated,
    })
}

fn write_generated(project: &Project, data_model: &str) -> Result<bool, String> {
    let dir = project.generated_dir();
    project::write_schema_types(&dir)?;
    project::write_if_changed(&dir.join("dataModel.ts"), data_model)
}

/// What a direct database change did to schema.ts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pulled {
    /// Tables in Postgres that aren't Simien's, so schema.ts leaves them out.
    pub unmanaged: Vec<Unmanaged>,
}

/// After a direct change in the database: rewrites schema.ts (and the data
/// model) from it when the two now disagree. `None` when they still agree.
pub fn pull_from_database(project: &Project, client: &mut Client) -> Result<Option<Pulled>, SyncError> {
    let current = catalog(client).map_err(SyncError::Failed)?;
    let file = project.schema_file();
    let text = std::fs::read_to_string(&file).unwrap_or_default();
    let differs = match schema::plan_from(&text, &current) {
        Ok(planned) => !planned.plan.statements.is_empty(),
        Err(_) => true,
    };
    if !differs {
        return Ok(None);
    }
    let pulled = schema::pull(&current);
    project::write_if_changed(&file, &pulled.schema_ts).map_err(SyncError::Failed)?;
    write_generated(project, &pulled.data_model).map_err(SyncError::Failed)?;
    Ok(Some(Pulled { unmanaged: pulled.unmanaged }))
}

/// Installs the event trigger that reports DDL from anyone but the sync itself.
pub fn install_notifier(client: &mut Client) -> Result<(), String> {
    client
        .batch_execute(&format!(
            r#"
    create or replace function simien_ddl_notify() returns event_trigger language plpgsql as $$
    begin
      if coalesce(current_setting('simien.syncing', true), '') <> 'on' then
        perform pg_notify('{CHANNEL}', tg_tag);
      end if;
    end $$;
    do $$ begin
      if not exists (select 1 from pg_event_trigger where evtname = 'simien_ddl_notify') then
        create event trigger simien_ddl_notify on ddl_command_end execute function simien_ddl_notify();
      end if;
    end $$;"#
        ))
        .map_err(|err| err.to_string())
}

/// A connection listening for direct schema changes.
pub struct Listener {
    client: Client,
}

impl Listener {
    pub fn new(url: &str) -> Result<Self, String> {
        let mut client = connect(url, Duration::from_secs(5))?;
        client.batch_execute(&format!("listen {CHANNEL}")).map_err(|err| err.to_string())?;
        Ok(Self { client })
    }

    /// The next change's command tag (`CREATE TABLE`, …), waiting up to `wait`.
    pub fn next(&mut self, wait: Duration) -> Result<Option<String>, String> {
        let mut notifications = self.client.notifications();
        let next = notifications.timeout_iter(wait).next();
        next.map(|note| note.map(|note| note.payload().to_string()))
            .map_err(|err| err.to_string())
    }
}

/// `simien/schema.ts` relative to the project, for messages.
pub fn schema_path(project: &Project) -> String {
    project
        .schema_file()
        .strip_prefix(&project.root)
        .unwrap_or(Path::new("simien/schema.ts"))
        .display()
        .to_string()
}
