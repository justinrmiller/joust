//! A LanceDB Enterprise database. The catalog comes over REST; SQL runs on
//! the server over Flight SQL (`Connection::execute_query_async`).
//!
//! Connecting probes what the server supports (`Capabilities`). When the SQL
//! endpoint is unreachable joust still browses: the catalog and table
//! previews only need REST.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::{Stream, StreamExt, TryStreamExt};
use lancedb::arrow::arrow_array::RecordBatch;
use lancedb::arrow::arrow_cast::display::{ArrayFormatter, FormatOptions};
use lancedb::arrow::arrow_schema::SchemaRef;
use lancedb::arrow::arrow_select::concat::concat_batches;
use lancedb::connection::Connection;
use lancedb::index::Index;
use lancedb::index::scalar::FtsIndexBuilder;
use lancedb::listing::ListingOptions;
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::remote::{
    AuthorizationCodeOptions, ClientAuthMethod, ClientConfig, HeaderProvider, OAuthConfig,
    OAuthFlow, OAuthSession, TlsConfig, TokenCacheOptions,
};
use lancedb::sql::Query;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::plan::PlanNode;
use super::statements::split_statements;
use super::{IndexKind, QueryOutcome, ResultSet, TableInfo, TableKind, TableRef};
use crate::enterprise::dashboard::TableHealth;
use crate::enterprise::profiles::{self, ClientAuth, Credential, OidcFlow, OidcSettings};
use crate::enterprise::profiles::{OIDC_CLIENT_SECRET_ENV, ResolvedCredential};
use crate::enterprise::{Check, EnterpriseTarget, format_namespace};

/// Required by the client even with a host override, where it is unused.
const REGION: &str = "us-east-1";
/// Namespaces nested deeper than this are not listed.
const MAX_NAMESPACE_DEPTH: usize = 4;
/// Most namespaces listed: a guard against very large catalogs.
const MAX_NAMESPACES: usize = 500;
/// Tables described at once.
const DESCRIBE_CONCURRENCY: usize = 8;
/// Where a paged `SHOW` statement puts the token for its next page.
const NEXT_PAGE_TOKEN: &str = "lancedb.next_page_token";
const SHOW_PAGE_SIZE: usize = 1000;
const MAX_SHOW_PAGES: usize = 50;
/// First keywords of statements that change the catalog.
const CATALOG_KEYWORDS: &[&str] = &[
    "CREATE", "DROP", "ALTER", "INSERT", "UPDATE", "DELETE", "MERGE", "REFRESH", "OPTIMIZE",
    "VACUUM",
];
/// Most tables the table health tile checks.
pub const MAX_HEALTH_TABLES: usize = 100;
/// Longest error text shown in a connection check.
const MAX_CHECK_ERROR: usize = 160;
/// The reserved database holding the metering views (reachable over SQL only).
const SYSTEM_DATABASE: &str = "system";

/// A connected LanceDB Enterprise database. Cheap to clone.
#[derive(Clone)]
pub struct RemoteDatabase {
    inner: Arc<Inner>,
}

struct Inner {
    target: EnterpriseTarget,
    rest_url: String,
    sql_url: String,
    uri: String,
    connection: Connection,
    capabilities: Capabilities,
    credential_source: String,
    /// The statement running now, so it can be cancelled.
    running: Mutex<Option<Arc<Query>>>,
    rows_received: AtomicUsize,
}

impl fmt::Debug for RemoteDatabase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteDatabase")
            .field("target", &self.inner.target.label())
            .finish_non_exhaustive()
    }
}

/// What the server turned out to support when joust connected.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// From `SHOW SERVER VERSION` (servers 1.13 and later).
    pub server_version: Option<String>,
    /// Why SQL is unavailable; `None` when it works.
    pub sql_error: Option<String>,
    /// Whether Functions are enabled (`None`: unknown without SQL).
    pub functions: Option<bool>,
    /// Whether the `system.public` metering views are readable.
    pub system_views: Option<bool>,
    /// From `SHOW DATABASES`.
    pub databases: Vec<String>,
}

/// One version of a Function, from `SHOW FUNCTIONS HISTORY`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FunctionInfo {
    pub name: String,
    pub version: String,
    pub created_at_millis: Option<i64>,
    pub python_version: String,
    /// `(x int64) -> int64`.
    pub signature: String,
    /// Packages as declared (`pip = [...]`).
    pub packages: String,
    /// Packages as resolved when the image was built.
    pub resolved_packages: String,
}

/// One Function version in detail, from `DESCRIBE FUNCTION`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FunctionDetail {
    pub name: String,
    pub version: String,
    pub source: Option<String>,
    pub python_version: Option<String>,
    /// How dependencies are installed: `pip` or `conda`.
    pub environment: Option<String>,
    /// Resolved packages, `name==version`.
    pub packages: Vec<String>,
    /// Parameters, `x int64 NOT NULL`.
    pub inputs: Vec<String>,
    /// The result type.
    pub output: String,
    pub image_digest: Option<String>,
    pub disabled: bool,
}

impl FunctionDetail {
    /// Reads the JSON definition `DESCRIBE FUNCTION` returns.
    fn from_definition(json: &str) -> Result<Self> {
        let value: Value =
            serde_json::from_str(json).context("the Function definition is not JSON")?;
        let text = |pointer: &str| {
            value.pointer(pointer).and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
        };
        let list = |pointer: &str| {
            value
                .pointer(pointer)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        let source = match (text("/source/encoding").as_deref(), text("/source/data")) {
            (Some("base64"), Some(data)) => {
                let bytes = BASE64
                    .decode(data.trim())
                    .context("the Function source is not valid base64")?;
                Some(String::from_utf8_lossy(&bytes).into_owned())
            }
            (_, data) => data,
        };
        let packages = list("/resolved_environment/packages")
            .iter()
            .filter_map(|package| {
                Some(format!(
                    "{}=={}",
                    package.get("name")?.as_str()?,
                    package.get("version")?.as_str()?
                ))
            })
            .collect();
        let typed = |field: &Value| {
            let name = field.get("name")?.as_str()?;
            let arrow_type = field.get("arrow_type")?.as_str()?;
            let nullable = field
                .get("nullable")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            Some(format!(
                "{name} {arrow_type}{}",
                if nullable { "" } else { " NOT NULL" }
            ))
        };
        let inputs = list("/signature/inputs").iter().filter_map(typed).collect();
        let output = text("/signature/output/arrow_type").unwrap_or_else(|| {
            let fields: Vec<String> = list("/signature/output/fields")
                .iter()
                .filter_map(typed)
                .collect();
            format!("struct<{}>", fields.join(", "))
        });
        Ok(Self {
            name: text("/name").unwrap_or_default(),
            version: text("/version").unwrap_or_default(),
            source,
            python_version: text("/runtime/python_version"),
            environment: text("/runtime/environment/kind"),
            packages,
            inputs,
            output,
            image_digest: text("/image/manifest_digest"),
            disabled: value
                .pointer("/disabled")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// A Function as `SHOW FUNCTIONS HISTORY` lists it. A namespaced one is
/// listed as `ns$name` (the server's path notation), which SQL writes in the
/// schema position: `ns.name`.
pub fn function_ref(listed: &str) -> TableRef {
    let mut parts: Vec<String> = listed.split('$').map(str::to_string).collect();
    let name = parts.pop().unwrap_or_default();
    TableRef {
        namespace: parts,
        name,
    }
}

/// Freshness of a Function column, from `SHOW COLUMN STATUS ON <table>`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ColumnStatus {
    pub column: String,
    pub function: String,
    /// The Function's namespace path (empty: the root).
    pub function_namespace: Vec<String>,
    pub function_version: String,
    pub rows: u64,
    pub fresh: u64,
    pub stale: u64,
    pub never_computed: u64,
    pub failed: u64,
    /// State of the last refresh job (`succeeded`, `failed`, …).
    pub last_refresh_state: String,
    /// Automatic refresh (`up_to_date`, `ready`, …).
    pub auto_refresh_state: String,
}

impl ColumnStatus {
    /// The Function as `SHOW FUNCTIONS HISTORY` lists it (`ns$name`).
    pub fn listed_function(&self) -> String {
        self.function_namespace
            .iter()
            .chain([&self.function])
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join("$")
    }
}

/// One job, from `SHOW JOBS EXTENDED`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JobRow {
    pub job_id: String,
    pub job_type: String,
    pub table: String,
    /// `running`, `succeeded`, `failed`, `cancelled`, …
    pub state: String,
    pub paused: bool,
    pub created_at_millis: Option<i64>,
    pub completed_at_millis: Option<i64>,
    pub duration_millis: Option<i64>,
    pub phase: String,
    pub queue_reason: String,
    pub rows_planned: Option<u64>,
    pub rows_computed: Option<u64>,
    pub rows_failed: Option<u64>,
    pub active_workers: Option<u64>,
    pub workers_desired: Option<u64>,
    pub failure_message: String,
    /// `name v3` for a Function refresh.
    pub function: String,
}

impl JobRow {
    /// Whether the job has not finished (running, queued or paused).
    pub fn is_active(&self) -> bool {
        !matches!(
            self.state.as_str(),
            "succeeded" | "failed" | "cancelled" | "canceled" | "timed_out" | "done"
        )
    }

    /// Whether the job ended without finishing its work (failed or timed
    /// out).
    pub fn failed(&self) -> bool {
        matches!(self.state.as_str(), "failed" | "timed_out")
    }

    /// Share of planned rows computed, when the job reports rows.
    pub fn fraction(&self) -> Option<f64> {
        match (self.rows_computed, self.rows_planned) {
            (Some(done), Some(planned)) if planned > 0 => Some(done as f64 / planned as f64),
            _ => None,
        }
    }
}

/// What can be done to a running job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobAction {
    Pause,
    Resume,
    Cancel,
}

/// A job in detail: `DESCRIBE JOB … FORMAT JSON` and `SHOW JOB EVENTS`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobDetail {
    pub job_id: String,
    pub state: String,
    pub total_millis: Option<u64>,
    /// Counts the server reports (`rows`, `splits`, `workers`, …).
    pub facts: Vec<(String, String)>,
    pub timeline: Vec<JobPhase>,
    /// Newest first.
    pub events: Vec<JobEvent>,
    pub failure: Option<String>,
}

/// A phase of a job's timeline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobPhase {
    pub name: String,
    pub at_millis: u64,
    pub millis: u64,
}

/// One event a job recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobEvent {
    pub seq: u64,
    pub state: String,
    pub at_millis: Option<i64>,
    /// Who wrote it (`registry-worker-0`, a worker pod, …).
    pub by: String,
    pub rows: Option<u64>,
}

impl JobDetail {
    /// Reads `DESCRIBE JOB … FORMAT JSON` and the job's events.
    fn from_json(json: &str, events: Vec<JobEvent>) -> Result<Self> {
        let value: Value = serde_json::from_str(json).context("the job description is not JSON")?;
        let text = |pointer: &str| {
            value
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let mut facts: Vec<(String, String)> = value
            .pointer("/facts")
            .and_then(Value::as_object)
            .map(|facts| {
                facts
                    .iter()
                    .map(|(name, value)| (name.replace('_', " "), plain_json(value)))
                    .collect()
            })
            .unwrap_or_default();
        for key in ["tasks", "workers"] {
            // The profile's own facts win over its top-level counters.
            if facts.iter().any(|(name, _)| name == key) {
                continue;
            }
            if let Some(count) = value.get(key).filter(|v| v.is_number()) {
                facts.push((key.to_string(), plain_json(count)));
            }
        }
        let timeline = value
            .pointer("/timeline")
            .and_then(Value::as_array)
            .map(|phases| {
                phases
                    .iter()
                    .filter_map(|phase| {
                        Some(JobPhase {
                            name: phase.get("phase")?.as_str()?.to_string(),
                            at_millis: phase.get("at_millis").and_then(Value::as_u64).unwrap_or(0),
                            millis: phase.get("millis").and_then(Value::as_u64).unwrap_or(0),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let failure = text("/failure/message")
            .or_else(|| text("/failure"))
            .or_else(|| text("/failure_message"));
        Ok(Self {
            job_id: text("/job_id").unwrap_or_default(),
            // A running job's profile can have an empty state: fall back to
            // the job's own state, then to its newest event.
            state: text("/state")
                .filter(|state| !state.is_empty())
                .or_else(|| text("/job_state").map(|state| state.to_lowercase()))
                .or_else(|| events.first().map(|event| event.state.clone()))
                .unwrap_or_default(),
            total_millis: value.pointer("/total_millis").and_then(Value::as_u64),
            facts,
            timeline,
            events,
            failure,
        })
    }
}

/// A JSON scalar as plain text.
fn plain_json(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Columns of a result read as text, by name.
struct Rows {
    columns: HashMap<String, Vec<String>>,
    len: usize,
}

impl Rows {
    fn new(batches: &[RecordBatch], names: &[&str]) -> Self {
        let columns: HashMap<String, Vec<String>> = names
            .iter()
            .map(|name| (name.to_string(), strings(batches, name)))
            .collect();
        Self {
            len: batches.iter().map(RecordBatch::num_rows).sum(),
            columns,
        }
    }

    fn text(&self, name: &str, row: usize) -> String {
        self.columns
            .get(name)
            .and_then(|values| values.get(row))
            .cloned()
            .unwrap_or_default()
    }

    fn number<T: std::str::FromStr>(&self, name: &str, row: usize) -> Option<T> {
        self.text(name, row).parse().ok()
    }
}

impl Capabilities {
    pub fn sql(&self) -> bool {
        self.sql_error.is_none()
    }
}

impl RemoteDatabase {
    /// Connects to `target` and probes the server.
    pub async fn connect(target: EnterpriseTarget, credential: ResolvedCredential) -> Result<Self> {
        let (rest_url, sql_url) = target.endpoints()?;
        let connection =
            open_connection(&target, &rest_url, &sql_url, &credential.credential).await?;
        // The catalog comes over REST, so REST has to work.
        connection
            .list_tables(&target.namespace, ListingOptions::default().page_limit(1))
            .next_page()
            .await
            .with_context(|| {
                format!(
                    "could not list tables at {rest_url} (database {})",
                    target.database
                )
            })?;
        let capabilities = probe(&connection, &target.namespace).await;
        Ok(Self {
            inner: Arc::new(Inner {
                uri: format!("db://{}", target.database),
                target,
                rest_url,
                sql_url,
                connection,
                capabilities,
                credential_source: credential.source,
                running: Mutex::new(None),
                rows_received: AtomicUsize::new(0),
            }),
        })
    }

    pub fn target(&self) -> &EnterpriseTarget {
        &self.inner.target
    }

    /// `db://<database>`.
    pub fn uri(&self) -> &str {
        &self.inner.uri
    }

    pub fn capabilities(&self) -> &Capabilities {
        &self.inner.capabilities
    }

    pub fn connection(&self) -> &Connection {
        &self.inner.connection
    }

    /// The connection check, line by line.
    pub fn checks(&self) -> Vec<Check> {
        let caps = &self.inner.capabilities;
        let mut checks = vec![Check::ok(
            "REST API",
            format!(
                "{} · signed in with {}",
                self.inner.rest_url, self.inner.credential_source
            ),
        )];
        checks.push(match &caps.sql_error {
            None => Check::ok(
                "SQL",
                format!("{} · queries run on the server", self.inner.sql_url),
            ),
            Some(error) => Check::unavailable(
                "SQL",
                format!(
                    "{} is unreachable ({error}); browsing still works",
                    self.inner.sql_url
                ),
            ),
        });
        checks.push(match &caps.server_version {
            Some(version) => Check::ok("Server version", version.clone()),
            None => Check::unavailable(
                "Server version",
                "unknown (SHOW SERVER VERSION needs 1.13 or later)",
            ),
        });
        if caps.sql() {
            checks.push(if caps.functions == Some(true) {
                Check::ok("Functions", "enabled")
            } else {
                Check::unavailable(
                    "Functions",
                    "off on this server (they need database_manifest_enabled)",
                )
            });
            checks.push(if caps.system_views == Some(true) {
                Check::ok("System views", "metering views are readable")
            } else {
                Check::unavailable("System views", "not readable with these credentials")
            });
        }
        checks
    }

    /// Every namespace's tables (REST), plus views and materialized views
    /// when SQL is available.
    pub async fn sync_catalog(&self) -> Result<Vec<TableInfo>> {
        let connection = &self.inner.connection;
        let mut tables = Vec::new();
        for namespace in list_namespaces(connection).await? {
            let names: Vec<String> = connection
                .list_tables(&namespace, ListingOptions::default())
                .try_collect()
                .await
                .with_context(|| format!("could not list tables in {}", shown(&namespace)))?;
            tables.extend(names.into_iter().map(|name| TableRef {
                namespace: namespace.clone(),
                name,
            }));
        }
        let (views, materialized) = if self.inner.capabilities.sql() {
            tokio::join!(
                self.show_objects("SHOW VIEWS"),
                self.show_objects("SHOW MATERIALIZED VIEWS")
            )
        } else {
            Default::default()
        };
        let materialized: HashSet<TableRef> = materialized.into_iter().collect();

        let mut infos: Vec<TableInfo> = futures::stream::iter(tables)
            .map(|table| describe(connection, table))
            .buffered(DESCRIBE_CONCURRENCY)
            .try_collect()
            .await?;
        for info in &mut infos {
            if materialized.contains(&info.table_ref()) {
                info.kind = TableKind::MaterializedView;
            }
        }
        infos.extend(views.into_iter().map(|view| TableInfo {
            namespace: view.namespace,
            name: view.name,
            kind: TableKind::View,
            columns: Vec::new(),
            num_rows: 0,
            version: 0,
            size_bytes: 0,
            indices: Vec::new(),
        }));
        infos.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
        Ok(infos)
    }

    /// Runs the statements in `sql` on the server, one request each, and
    /// returns the last result set (at most `row_limit` rows per statement).
    /// With `collect_plan`, a final query also runs under `EXPLAIN ANALYZE`
    /// (executing it a second time) to fill the plan view.
    pub async fn run_sql(
        &self,
        sql: &str,
        row_limit: usize,
        collect_plan: bool,
    ) -> Result<QueryOutcome> {
        if let Some(error) = &self.inner.capabilities.sql_error {
            bail!(
                "SQL is unavailable: the server's SQL endpoint {} is unreachable ({error}). \
                 Browsing and table previews still work.",
                self.inner.sql_url
            );
        }
        let statements = split_statements(sql);
        if statements.is_empty() {
            bail!("nothing to run: the editor contains no SQL statements");
        }
        let started = Instant::now();
        self.inner.rows_received.store(0, Ordering::Relaxed);
        let count = statements.len();
        let mut last: Option<QueryOutcome> = None;
        let mut catalog_changed = false;
        for (index, statement) in statements.iter().enumerate() {
            let outcome = self
                .run_statement(statement, row_limit)
                .await
                .map_err(|error| {
                    if count == 1 {
                        error
                    } else {
                        error.context(format!("statement {} of {count} failed", index + 1))
                    }
                })?;
            catalog_changed |= outcome.catalog_changed;
            if outcome.result.is_some() || last.is_none() {
                last = Some(outcome);
            }
        }
        let mut outcome = last.expect("at least one statement ran");
        outcome.statements = count;
        outcome.catalog_changed = catalog_changed;
        if collect_plan && let Some(statement) = statements.last().filter(|s| is_query(s)) {
            match self.explain(statement).await {
                Ok((text, plan)) => {
                    outcome.physical_plan = text;
                    outcome.plan = plan;
                }
                Err(error) => outcome
                    .notes
                    .push(format!("No plan: {}", short_error(&error))),
            }
        }
        outcome.elapsed = started.elapsed();
        Ok(outcome)
    }

    /// Runs one statement, registering it so [`Self::cancel`] can stop it.
    async fn run_statement(&self, statement: &str, row_limit: usize) -> Result<QueryOutcome> {
        let started = Instant::now();
        let query = Arc::new(
            self.inner
                .connection
                .execute_query_async(statement)
                .default_namespace_path(self.inner.target.namespace.clone())
                .execute()
                .await?,
        );
        *self.running() = Some(query.clone());
        let read = async {
            let mut reader = query.reader().await?;
            let schema = reader.schema();
            let (batches, truncated) =
                collect_limited(&mut reader, row_limit, Some(&self.inner.rows_received)).await?;
            // Dropping the reader stops a server that is still streaming rows.
            drop(reader);
            Ok::<_, anyhow::Error>((schema, batches, truncated))
        }
        .await;
        self.running()
            .take_if(|running| Arc::ptr_eq(running, &query));
        let (schema, batches, truncated) = read?;
        let token = is_show(statement)
            .then(|| next_page_token(&schema, &batches))
            .flatten();
        let mut outcome = outcome(
            schema,
            batches,
            truncated,
            is_catalog_statement(statement),
            started.elapsed(),
        )?;
        outcome.continuation = token.map(|token| continuation_sql(statement, &token));
        Ok(outcome)
    }

    fn running(&self) -> std::sync::MutexGuard<'_, Option<Arc<Query>>> {
        self.inner
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Asks the server to stop the statement that is running, if any.
    pub async fn cancel(&self) -> Result<()> {
        let running = self.running().take();
        if let Some(query) = running {
            query.cancel().await?;
        }
        Ok(())
    }

    /// Rows received so far by the current (or last) run.
    pub fn rows_received(&self) -> usize {
        self.inner.rows_received.load(Ordering::Relaxed)
    }

    /// The text and tree of `EXPLAIN ANALYZE <statement>`.
    async fn explain(&self, statement: &str) -> Result<(String, Option<PlanNode>)> {
        let (_, batches) = query(
            &self.inner.connection,
            &self.inner.target.namespace,
            &format!("EXPLAIN ANALYZE {statement}"),
        )
        .await?;
        let kinds = strings(&batches, "plan_type");
        let plans = strings(&batches, "plan");
        let rows: Vec<_> = kinds.iter().zip(&plans).collect();
        let text = rows
            .iter()
            .find(|(kind, _)| kind.contains("Metrics"))
            .or(rows.first())
            .map(|(_, plan)| (*plan).clone())
            .unwrap_or_default();
        let plan = PlanNode::from_indent_text(&text);
        Ok((text, plan))
    }

    /// Every version of every Function (`SHOW FUNCTIONS HISTORY`), the
    /// newest version first within each name. Empty when Functions are off.
    pub async fn functions(&self) -> Result<Vec<FunctionInfo>> {
        if self.inner.capabilities.functions != Some(true) {
            return Ok(Vec::new());
        }
        let batches = self.show_pages("SHOW FUNCTIONS HISTORY").await?;
        let column = |name: &str| strings(&batches, name);
        let (versions, created, pythons) = (
            column("version"),
            column("created_at_millis"),
            column("python_version"),
        );
        let (signatures, packages, resolved) = (
            column("signature"),
            column("packages"),
            column("resolved_packages"),
        );
        let at = |values: &[String], i: usize| values.get(i).cloned().unwrap_or_default();
        let mut functions: Vec<FunctionInfo> = column("name")
            .into_iter()
            .enumerate()
            .map(|(i, name)| FunctionInfo {
                name,
                version: at(&versions, i),
                created_at_millis: created.get(i).and_then(|value| value.parse().ok()),
                python_version: at(&pythons, i),
                signature: at(&signatures, i),
                packages: at(&packages, i),
                resolved_packages: at(&resolved, i),
            })
            .collect();
        functions.sort_by(|a, b| {
            a.name
                .cmp(&b.name)
                .then_with(|| version_order(&b.version).cmp(&version_order(&a.version)))
        });
        Ok(functions)
    }

    /// One Function version in detail, with its source.
    pub async fn describe_function(&self, name: &str, version: &str) -> Result<FunctionDetail> {
        let statement = format!(
            "DESCRIBE FUNCTION {} WITH (version = '{}', include_source = true)",
            function_ref(name).sql_name(),
            version.replace('\'', "''")
        );
        let (_, batches) = query(
            &self.inner.connection,
            &self.inner.target.namespace,
            &statement,
        )
        .await?;
        let definition = strings(&batches, "definition")
            .into_iter()
            .next()
            .context("the server returned no definition")?;
        FunctionDetail::from_definition(&definition)
    }

    /// Freshness of `table`'s Function columns.
    pub async fn column_status(&self, table: &TableRef) -> Result<Vec<ColumnStatus>> {
        let statement = format!("SHOW COLUMN STATUS ON {}", table.sql_name());
        let (_, batches) = query(
            &self.inner.connection,
            &self.inner.target.namespace,
            &statement,
        )
        .await?;
        Ok(column_statuses(&batches))
    }

    /// The first `limit` rows of `table` over REST (no SQL needed).
    pub async fn preview(&self, table: &TableRef, limit: usize) -> Result<QueryOutcome> {
        let started = Instant::now();
        let handle = open_table(&self.inner.connection, table).await?;
        let stream = handle.query().limit(limit).execute().await?;
        let schema = stream.schema();
        let batches = stream.try_collect().await?;
        outcome(schema, batches, false, false, started.elapsed())
    }

    /// Starts an index build on `table.column` (the server runs it as a job).
    pub async fn create_index(
        &self,
        table: &TableRef,
        column: &str,
        kind: IndexKind,
    ) -> Result<String> {
        let handle = open_table(&self.inner.connection, table).await?;
        let index = match kind {
            IndexKind::Auto => Index::Auto,
            IndexKind::FullText => Index::FTS(FtsIndexBuilder::default()),
        };
        handle
            .create_index(&[column], index)
            .execute()
            .await
            .with_context(|| format!("could not index {}.{column}", table.key()))?;
        Ok(format!(
            "Started an index build on {}.{column}; the server runs it as a job",
            table.key()
        ))
    }

    /// Every job the server lists, newest first.
    pub async fn jobs(&self) -> Result<Vec<JobRow>> {
        Ok(job_rows(&self.sql_batches("SHOW JOBS EXTENDED").await?))
    }

    /// One job's timeline, facts and events.
    pub async fn job_detail(&self, job_id: &str) -> Result<JobDetail> {
        let id = job_id.replace('\'', "''");
        let describe = format!("DESCRIBE JOB '{id}' FORMAT JSON");
        let show_events = format!("SHOW JOB EVENTS '{id}'");
        let (description, events) =
            tokio::join!(self.sql_batches(&describe), self.sql_batches(&show_events),);
        job_detail_of(job_id, &description?, &events.unwrap_or_default())
    }

    /// Pauses, resumes or cancels a job.
    pub async fn job_action(&self, job_id: &str, action: JobAction) -> Result<String> {
        let connection = &self.inner.connection;
        Ok(match action {
            JobAction::Pause => format!("Pause: {:?}", connection.pause_job(job_id).await?),
            JobAction::Resume => format!("Resume: {:?}", connection.resume_job(job_id).await?),
            JobAction::Cancel => {
                if connection.cancel_job(job_id).await? {
                    "Cancel requested".to_string()
                } else {
                    "No such job".to_string()
                }
            }
        })
    }

    /// Size, fragment and index statistics of `tables` (at most
    /// [`MAX_HEALTH_TABLES`]).
    pub async fn table_health(&self, tables: &[TableRef]) -> Result<Vec<TableHealth>> {
        let checked: Vec<Result<TableHealth>> =
            futures::stream::iter(tables.iter().take(MAX_HEALTH_TABLES).cloned())
                .map(|table| async move {
                    let name = table.sql_name();
                    let show_stats = format!("SHOW STATS FOR TABLE {name}");
                    let show_indexes = format!("SHOW INDEXES FOR TABLE {name}");
                    let (stats, indexes) = tokio::join!(
                        self.sql_batches(&show_stats),
                        self.sql_batches(&show_indexes),
                    );
                    Ok(table_health_of(
                        table.key(),
                        &stats?,
                        indexes.ok().as_deref(),
                    ))
                })
                .buffered(DESCRIBE_CONCURRENCY)
                .collect()
                .await;
        checked_tables(checked)
    }

    /// Runs one statement and returns every batch (dashboard tiles).
    pub async fn sql_batches(&self, statement: &str) -> Result<Vec<RecordBatch>> {
        if let Some(error) = &self.inner.capabilities.sql_error {
            bail!("SQL is unavailable on this server ({error})");
        }
        let (_, batches) = query(
            &self.inner.connection,
            &self.inner.target.namespace,
            statement,
        )
        .await?;
        Ok(batches)
    }

    /// Every page of a paged `SHOW` statement.
    async fn show_pages(&self, statement: &str) -> Result<Vec<RecordBatch>> {
        let mut all = Vec::new();
        let mut next = format!("{statement} LIMIT {SHOW_PAGE_SIZE}");
        for _ in 0..MAX_SHOW_PAGES {
            let (schema, batches) =
                query(&self.inner.connection, &self.inner.target.namespace, &next).await?;
            let token = next_page_token(&schema, &batches);
            all.extend(batches);
            match token {
                Some(token) => next = continuation_sql(statement, &token),
                None => break,
            }
        }
        Ok(all)
    }

    /// `(namespace, name)` rows of a `SHOW VIEWS`-style statement. Empty
    /// when the server does not know the statement.
    async fn show_objects(&self, statement: &str) -> Vec<TableRef> {
        let Ok(batches) = self.show_pages(statement).await else {
            return Vec::new();
        };
        let namespaces = strings(&batches, "namespace_path");
        strings(&batches, "name")
            .into_iter()
            .zip(namespaces)
            .map(|(name, path)| TableRef {
                namespace: serde_json::from_str(&path).unwrap_or_default(),
                name,
            })
            .collect()
    }
}

/// Builds the client for `target` with `credential`.
async fn open_connection(
    target: &EnterpriseTarget,
    rest_url: &str,
    sql_url: &str,
    credential: &Credential,
) -> Result<Connection> {
    let mut config = ClientConfig::default();
    let path = |p: &Option<String>| {
        p.as_deref()
            .map(|p| profiles::expand_home(p).to_string_lossy().into_owned())
    };
    if target.ca_cert.is_some() || target.client_cert.is_some() || target.client_key.is_some() {
        config.tls_config = Some(TlsConfig {
            cert_file: path(&target.client_cert),
            key_file: path(&target.client_key),
            ssl_ca_cert: path(&target.ca_cert),
            assert_hostname: true,
        });
    }
    let builder = lancedb::connect(&format!("db://{}", target.database))
        .region(REGION)
        .host_override(rest_url)
        .sql_host_override(sql_url);
    let builder = match credential {
        Credential::ApiKey(key) => builder.api_key(key),
        Credential::OidcToken(token) => {
            config.header_provider = Some(Arc::new(BearerToken(token.clone())));
            // An empty key sends no `x-api-key`; the token authenticates.
            builder.api_key("")
        }
        Credential::OAuth { profile, settings } => {
            let sessions = profiles::lancedb_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("auth");
            builder.oauth_config(oauth_config(profile, settings, &sessions).await?)
        }
    };
    builder
        .client_config(config)
        .execute()
        .await
        .context("could not set up the connection")
}

/// Sends an OIDC access token the way the server expects one.
struct BearerToken(String);

impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BearerToken(…)")
    }
}

#[async_trait::async_trait]
impl HeaderProvider for BearerToken {
    async fn get_headers(&self) -> lancedb::Result<HashMap<String, String>> {
        Ok(bearer_headers(&self.0))
    }
}

fn bearer_headers(token: &str) -> HashMap<String, String> {
    HashMap::from([
        ("authorization".to_string(), format!("Bearer {token}")),
        ("x-lancedb-credential-type".to_string(), "oidc".to_string()),
    ])
}

/// The SDK's OAuth settings for a profile's `[oidc]` table. Sessions are
/// cached under `sessions` the way the `lancedb` CLI caches them
/// (`~/.lancedb/auth/<sha256(profile)>`), so a CLI sign-in carries over.
async fn oauth_config(
    profile: &str,
    settings: &OidcSettings,
    sessions: &Path,
) -> Result<OAuthConfig> {
    let client_secret = match std::env::var(OIDC_CLIENT_SECRET_ENV)
        .ok()
        .filter(|secret| !secret.trim().is_empty())
    {
        Some(secret) => Some(secret.trim().to_string()),
        None => match &settings.client_secret_file {
            Some(path) => {
                let path = profiles::expand_home(&path.to_string_lossy());
                let secret = tokio::fs::read_to_string(&path).await.with_context(|| {
                    format!("could not read the client secret file {}", path.display())
                })?;
                Some(secret.trim().to_string())
            }
            None => None,
        },
    };
    let flow = match settings.flow {
        OidcFlow::ClientCredentials => OAuthFlow::ClientCredentials,
        OidcFlow::AuthorizationCode => {
            let mut options = AuthorizationCodeOptions::new();
            if let Some(uri) = &settings.redirect_uri {
                options = options.redirect_uri(uri);
            }
            if let Some(port) = settings.callback_port {
                options = options.callback_port(port);
            }
            OAuthFlow::AuthorizationCode(options)
        }
        OidcFlow::DeviceCode => OAuthFlow::DeviceCode,
    };
    let config = OAuthConfig {
        issuer_url: settings.issuer_url.clone(),
        client_id: settings.client_id.clone(),
        client_secret,
        scopes: settings.scopes.clone(),
        resource: settings.resource.clone(),
        audience: settings.audience.clone(),
        flow,
        client_auth_method: settings.client_auth_method.map(|method| match method {
            ClientAuth::None => ClientAuthMethod::None,
            ClientAuth::ClientSecretBasic => ClientAuthMethod::ClientSecretBasic,
            ClientAuth::ClientSecretPost => ClientAuthMethod::ClientSecretPost,
        }),
        refresh_buffer_secs: settings.refresh_buffer_secs,
        token_cache: (settings.flow != OidcFlow::ClientCredentials)
            .then(|| TokenCacheOptions::new().cache_dir(session_dir(sessions, profile))),
    };
    if settings.flow == OidcFlow::DeviceCode {
        // The device flow prints its code on stderr, which a window cannot
        // show; only reuse a session the CLI already has.
        let status = OAuthSession::new(config.clone())?.status().await?;
        if !status.refreshable {
            bail!(
                "device-code sign-in shows its code in a terminal, which joust cannot display. \
                 Run `lancedb auth login --profile {profile}` once, then connect again"
            );
        }
    }
    Ok(config)
}

/// `profile`'s session folder under `sessions`, named as the CLI names it
/// (profile names are arbitrary, so they are hashed).
fn session_dir(sessions: &Path, profile: &str) -> PathBuf {
    sessions.join(format!("{:x}", Sha256::digest(profile.as_bytes())))
}

/// Asks the server what it supports. Never fails: SQL problems become
/// `sql_error`.
async fn probe(connection: &Connection, namespace: &[String]) -> Capabilities {
    if let Err(error) = query(connection, namespace, "SELECT 1").await {
        return Capabilities {
            sql_error: Some(short_error(&error)),
            ..Capabilities::default()
        };
    }
    let (version, databases, functions, system) = tokio::join!(
        query(connection, namespace, "SHOW SERVER VERSION"),
        query(connection, namespace, "SHOW DATABASES"),
        query(connection, namespace, "SHOW FUNCTIONS"),
        query(
            connection,
            namespace,
            "SELECT * FROM system.public.job_metering LIMIT 0"
        ),
    );
    Capabilities {
        server_version: version
            .ok()
            .and_then(|(_, batches)| strings(&batches, "release").into_iter().next()),
        sql_error: None,
        functions: Some(functions.is_ok()),
        system_views: Some(system.is_ok()),
        databases: databases
            .map(|(_, batches)| strings(&batches, "name"))
            .unwrap_or_default()
            .into_iter()
            .filter(|name| is_user_database(name))
            .collect(),
    }
}

/// Whether a database can be opened over REST: not the reserved `system`
/// one (SQL only) or an internal `_` one.
fn is_user_database(name: &str) -> bool {
    name != SYSTEM_DATABASE && !name.starts_with('_')
}

/// Runs one statement on the server and reads all of it.
async fn query(
    connection: &Connection,
    namespace: &[String],
    statement: &str,
) -> Result<(SchemaRef, Vec<RecordBatch>)> {
    let query = connection
        .execute_query_async(statement)
        .default_namespace_path(namespace.to_vec())
        .execute()
        .await?;
    let reader = query.reader().await?;
    let schema = reader.schema();
    Ok((schema, reader.try_collect().await?))
}

/// Every namespace, breadth first from the root (which comes first).
async fn list_namespaces(connection: &Connection) -> Result<Vec<Vec<String>>> {
    let mut found = vec![Vec::new()];
    let mut frontier: Vec<Vec<String>> = vec![Vec::new()];
    for _ in 0..MAX_NAMESPACE_DEPTH {
        let mut next = Vec::new();
        for parent in &frontier {
            let children: Vec<String> = connection
                .list_namespaces(parent, ListingOptions::default())
                .try_collect()
                .await
                .with_context(|| format!("could not list namespaces in {}", shown(parent)))?;
            next.extend(children.into_iter().map(|child| {
                let mut path = parent.clone();
                path.push(child);
                path
            }));
        }
        if next.is_empty() {
            break;
        }
        found.extend(next.iter().cloned());
        if found.len() >= MAX_NAMESPACES {
            break;
        }
        frontier = next;
    }
    found.truncate(MAX_NAMESPACES);
    Ok(found)
}

async fn open_table(connection: &Connection, table: &TableRef) -> Result<lancedb::Table> {
    connection
        .open_table(&table.name)
        .namespace(table.namespace.clone())
        .execute()
        .await
        .with_context(|| format!("could not open table {}", table.key()))
}

/// A table's catalog entry: its schema, size and indexes.
async fn describe(connection: &Connection, table: TableRef) -> Result<TableInfo> {
    let handle = open_table(connection, &table).await?;
    let schema = handle.schema().await?;
    let (rows, version, stats, indices) = tokio::join!(
        handle.count_rows(None),
        handle.version(),
        handle.stats(),
        handle.list_indices()
    );
    Ok(TableInfo {
        namespace: table.namespace,
        name: table.name,
        kind: TableKind::Table,
        columns: super::columns_of(&schema),
        num_rows: rows.unwrap_or_default(),
        version: version.unwrap_or_default(),
        size_bytes: stats.map(|s| s.total_bytes).unwrap_or_default(),
        indices: indices
            .unwrap_or_default()
            .into_iter()
            .map(super::index_info)
            .collect(),
    })
}

/// Reads `stream` until `row_limit` rows; true when rows were left over.
async fn collect_limited<E>(
    stream: &mut (impl Stream<Item = Result<RecordBatch, E>> + Unpin),
    row_limit: usize,
    received: Option<&AtomicUsize>,
) -> Result<(Vec<RecordBatch>, bool)>
where
    E: std::error::Error + Send + Sync + 'static,
{
    let mut batches = Vec::new();
    let mut rows = 0usize;
    while let Some(batch) = stream.next().await {
        let batch = batch?;
        if let Some(received) = received {
            received.fetch_add(batch.num_rows(), Ordering::Relaxed);
        }
        let remaining = row_limit.saturating_sub(rows);
        if batch.num_rows() > remaining {
            if remaining > 0 {
                batches.push(batch.slice(0, remaining));
            }
            return Ok((batches, true));
        }
        rows += batch.num_rows();
        batches.push(batch);
    }
    Ok((batches, false))
}

/// A query outcome for a single statement that ran elsewhere (no plan).
fn outcome(
    schema: SchemaRef,
    batches: Vec<RecordBatch>,
    truncated: bool,
    catalog_changed: bool,
    elapsed: Duration,
) -> Result<QueryOutcome> {
    let batch = match batches.first() {
        Some(first) => concat_batches(&first.schema(), &batches)?,
        None => RecordBatch::new_empty(schema),
    };
    Ok(QueryOutcome {
        result: (!batch.schema().fields().is_empty()).then_some(ResultSet { batch, truncated }),
        plan: None,
        logical_plan: String::new(),
        physical_plan: String::new(),
        statements: 1,
        notes: Vec::new(),
        catalog_changed,
        elapsed,
        continuation: None,
    })
}

/// The values of column `name` as text (nulls are empty).
pub fn strings(batches: &[RecordBatch], name: &str) -> Vec<String> {
    let options = FormatOptions::default();
    let mut values = Vec::new();
    for batch in batches {
        let Some(column) = batch.column_by_name(name) else {
            continue;
        };
        let Ok(formatter) = ArrayFormatter::try_new(column.as_ref(), &options) else {
            continue;
        };
        values.extend((0..batch.num_rows()).map(|row| formatter.value(row).to_string()));
    }
    values
}

/// Jobs from `SHOW JOBS EXTENDED`, newest first.
fn job_rows(batches: &[RecordBatch]) -> Vec<JobRow> {
    let rows = Rows::new(
        batches,
        &[
            "job_id",
            "job_type",
            "table",
            "state",
            "paused",
            "created_at_millis",
            "completed_at_millis",
            "duration_millis",
            "phase",
            "queue_reason",
            "rows_planned",
            "rows_computed",
            "rows_failed",
            "active_workers",
            "workers_desired",
            "failure_message",
            "function_name",
            "function_version",
        ],
    );
    let mut jobs: Vec<JobRow> = (0..rows.len)
        .map(|i| {
            let function = rows.text("function_name", i);
            let version = rows.text("function_version", i);
            JobRow {
                job_id: rows.text("job_id", i),
                job_type: rows.text("job_type", i),
                table: rows.text("table", i),
                state: rows.text("state", i),
                paused: rows.text("paused", i) == "true",
                created_at_millis: rows.number("created_at_millis", i),
                completed_at_millis: rows.number("completed_at_millis", i),
                duration_millis: rows.number("duration_millis", i),
                phase: rows.text("phase", i),
                queue_reason: rows.text("queue_reason", i),
                rows_planned: rows.number("rows_planned", i),
                rows_computed: rows.number("rows_computed", i),
                rows_failed: rows.number("rows_failed", i),
                active_workers: rows.number("active_workers", i),
                workers_desired: rows.number("workers_desired", i),
                failure_message: rows.text("failure_message", i),
                function: match (function.is_empty(), version.is_empty()) {
                    (true, _) => String::new(),
                    (false, true) => function,
                    (false, false) => format!("{function} v{version}"),
                },
            }
        })
        .collect();
    jobs.sort_by_key(|job| std::cmp::Reverse(job.created_at_millis));
    jobs
}

/// A job's detail from `DESCRIBE JOB … FORMAT JSON` and `SHOW JOB EVENTS`.
fn job_detail_of(
    job_id: &str,
    description: &[RecordBatch],
    events: &[RecordBatch],
) -> Result<JobDetail> {
    // A job only has a profile once it has started; until then the server
    // returns no row.
    let json = description.first().and_then(|batch| {
        let name = batch.schema().field(0).name().clone();
        strings(std::slice::from_ref(batch), &name)
            .into_iter()
            .next()
    });
    let rows = Rows::new(
        events,
        &[
            "event_seq",
            "state",
            "updated_at_millis",
            "updated_by",
            "rows_processed",
        ],
    );
    let mut events: Vec<JobEvent> = (0..rows.len)
        .map(|i| JobEvent {
            seq: rows.number("event_seq", i).unwrap_or(0),
            state: rows.text("state", i),
            at_millis: rows.number("updated_at_millis", i),
            by: rows.text("updated_by", i),
            rows: rows.number("rows_processed", i),
        })
        .collect();
    events.sort_by_key(|event| std::cmp::Reverse(event.seq));
    match json {
        Some(json) => JobDetail::from_json(&json, events),
        None if events.is_empty() => bail!("no job {job_id}"),
        None => Ok(JobDetail {
            job_id: job_id.to_string(),
            state: events[0].state.clone(),
            events,
            ..JobDetail::default()
        }),
    }
}

/// One table's health from `SHOW STATS FOR TABLE` and, when it could be
/// read, `SHOW INDEXES FOR TABLE`.
fn table_health_of(
    table: String,
    stats: &[RecordBatch],
    indexes: Option<&[RecordBatch]>,
) -> TableHealth {
    let stats = Rows::new(
        stats,
        &[
            "num_rows",
            "total_bytes",
            "num_fragments",
            "num_small_fragments",
            "num_deleted_rows",
            "num_versions",
            "num_indices",
        ],
    );
    let unindexed = indexes
        .map(|batches| {
            strings(batches, "num_unindexed_rows")
                .iter()
                .filter_map(|value| value.parse::<u64>().ok())
                .sum()
        })
        .unwrap_or(0);
    TableHealth {
        table,
        rows: stats.number("num_rows", 0).unwrap_or(0),
        bytes: stats.number("total_bytes", 0).unwrap_or(0),
        fragments: stats.number("num_fragments", 0).unwrap_or(0),
        small_fragments: stats.number("num_small_fragments", 0).unwrap_or(0),
        deleted_rows: stats.number("num_deleted_rows", 0).unwrap_or(0),
        versions: stats.number("num_versions", 0).unwrap_or(0),
        indexes: stats.number("num_indices", 0).unwrap_or(0),
        unindexed_rows: unindexed,
    }
}

/// The tables that could be checked; an error only when none could.
fn checked_tables(checked: Vec<Result<TableHealth>>) -> Result<Vec<TableHealth>> {
    let mut health = Vec::new();
    let mut first_error = None;
    for result in checked {
        match result {
            Ok(table) => health.push(table),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    match first_error {
        Some(error) if health.is_empty() => Err(error),
        _ => Ok(health),
    }
}

/// Rows of `SHOW COLUMN STATUS`.
fn column_statuses(batches: &[RecordBatch]) -> Vec<ColumnStatus> {
    let column = |name: &str| strings(batches, name);
    let text = |values: &[String], i: usize| values.get(i).cloned().unwrap_or_default();
    let count =
        |values: &[String], i: usize| values.get(i).and_then(|v| v.parse().ok()).unwrap_or(0);
    let namespaces = string_lists(batches, "function_namespace_path");
    let (functions, versions, rows) = (
        column("function_name"),
        column("function_version"),
        column("rows"),
    );
    let (fresh, stale, never, failed) = (
        column("fresh"),
        column("stale"),
        column("never_computed"),
        column("failed"),
    );
    let (last, auto) = (
        column("last_refresh_job_state"),
        column("auto_refresh_state"),
    );
    column("column")
        .into_iter()
        .enumerate()
        .map(|(i, name)| ColumnStatus {
            column: name,
            function: text(&functions, i),
            function_namespace: namespaces.get(i).cloned().unwrap_or_default(),
            function_version: text(&versions, i),
            rows: count(&rows, i),
            fresh: count(&fresh, i),
            stale: count(&stale, i),
            never_computed: count(&never, i),
            failed: count(&failed, i),
            last_refresh_state: text(&last, i),
            auto_refresh_state: text(&auto, i),
        })
        .collect()
}

/// A list-of-strings column read by name, one list per row: the server
/// sends namespace paths as `List<Utf8>`.
fn string_lists(batches: &[RecordBatch], name: &str) -> Vec<Vec<String>> {
    use lancedb::arrow::arrow_array::{Array, ListArray, StringArray};
    let mut lists = Vec::new();
    for batch in batches {
        let Some(column) = batch.column_by_name(name) else {
            continue;
        };
        let Some(list) = column.as_any().downcast_ref::<ListArray>() else {
            // Other encodings (JSON text, as the SHOW listings send paths).
            lists.extend(
                strings(std::slice::from_ref(batch), name)
                    .iter()
                    .map(|text| serde_json::from_str(text).unwrap_or_default()),
            );
            continue;
        };
        lists.extend((0..list.len()).map(|row| {
            if list.is_null(row) {
                return Vec::new();
            }
            let values = list.value(row);
            values
                .as_any()
                .downcast_ref::<StringArray>()
                .map(|parts| parts.iter().flatten().map(str::to_string).collect())
                .unwrap_or_default()
        }));
    }
    lists
}

fn next_page_token(schema: &SchemaRef, batches: &[RecordBatch]) -> Option<String> {
    schema
        .metadata()
        .get(NEXT_PAGE_TOKEN)
        .or_else(|| {
            batches
                .first()?
                .schema_ref()
                .metadata()
                .get(NEXT_PAGE_TOKEN)
        })
        .filter(|token| !token.is_empty())
        .cloned()
}

/// The first word of `sql` after comments, if any.
fn first_keyword(sql: &str) -> Option<&str> {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            break;
        }
    }
    rest.split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .filter(|word| !word.is_empty())
}

fn is_catalog_statement(sql: &str) -> bool {
    first_keyword(sql).is_some_and(|word| {
        CATALOG_KEYWORDS
            .iter()
            .any(|keyword| keyword.eq_ignore_ascii_case(word))
    })
}

fn is_query(sql: &str) -> bool {
    first_keyword(sql).is_some_and(|word| {
        ["SELECT", "WITH", "VALUES"]
            .iter()
            .any(|keyword| keyword.eq_ignore_ascii_case(word))
    })
}

fn is_show(sql: &str) -> bool {
    first_keyword(sql).is_some_and(|word| word.eq_ignore_ascii_case("SHOW"))
}

/// The statement fetching the page after `statement`'s: the same `SHOW …`
/// with `LIMIT n FROM '<token>'` (replacing any page clause it had).
fn continuation_sql(statement: &str, token: &str) -> String {
    let statement = without_page_clause(statement.trim().trim_end_matches(';').trim_end());
    // After a trailing `-- comment`, the clause goes on its own line so the
    // comment cannot swallow it.
    let separator = if statement
        .lines()
        .last()
        .is_some_and(|line| line.contains("--"))
    {
        "\n"
    } else {
        " "
    };
    format!(
        "{statement}{separator}LIMIT {SHOW_PAGE_SIZE} FROM '{}'",
        token.replace('\'', "''")
    )
}

/// `statement` without a trailing `LIMIT n` or `LIMIT n FROM '…'`.
fn without_page_clause(statement: &str) -> &str {
    let upper = statement.to_ascii_uppercase();
    // The last LIMIT that starts a word (after a space or a line break).
    let Some(at) = upper
        .rmatch_indices("LIMIT")
        .map(|(at, _)| at)
        .find(|&at| at > 0 && upper.as_bytes()[at - 1].is_ascii_whitespace())
    else {
        return statement;
    };
    let rest = &upper[at + "LIMIT".len()..];
    let count = rest.trim_start();
    let digits = count.chars().take_while(char::is_ascii_digit).count();
    let after = count[digits..].trim_start();
    if count.len() < rest.len() && digits > 0 && (after.is_empty() || after.starts_with("FROM '")) {
        statement[..at].trim_end()
    } else {
        statement
    }
}

/// Sort key for Function versions: numerically when they are numbers.
fn version_order(version: &str) -> (u64, &str) {
    (version.parse().unwrap_or(0), version)
}

/// A short, readable reason for a failed request.
fn short_error(error: &anyhow::Error) -> String {
    let text = format!("{error:#}");
    let lower = text.to_ascii_lowercase();
    if lower.contains("tcp connect error") || lower.contains("connection refused") {
        return "connection refused".to_string();
    }
    let text = text.split(", source:").next().unwrap_or(&text).trim();
    if text.chars().count() > MAX_CHECK_ERROR {
        format!(
            "{}…",
            text.chars().take(MAX_CHECK_ERROR).collect::<String>()
        )
    } else {
        text.to_string()
    }
}

/// `public`, or a namespace path in the server's notation.
fn shown(namespace: &[String]) -> String {
    if namespace.is_empty() {
        "public".to_string()
    } else {
        format_namespace(namespace)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enterprise::profiles::AuthMethod;
    use lancedb::arrow::arrow_array::{Int64Array, StringArray};
    use lancedb::arrow::arrow_schema::{DataType, Field, Schema};

    fn names_batch(metadata: Option<&str>) -> RecordBatch {
        let mut schema = Schema::new(vec![
            Field::new("namespace_path", DataType::Utf8, false),
            Field::new("name", DataType::Utf8, false),
        ]);
        if let Some(token) = metadata {
            schema = schema.with_metadata(HashMap::from([(NEXT_PAGE_TOKEN.into(), token.into())]));
        }
        RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(StringArray::from(vec!["[]", "[\"team\"]"])),
                Arc::new(StringArray::from(vec!["a", "b"])),
            ],
        )
        .unwrap()
    }

    #[test]
    fn reads_text_columns_and_page_tokens() {
        let batch = names_batch(Some("tok"));
        assert_eq!(strings(std::slice::from_ref(&batch), "name"), ["a", "b"]);
        assert!(strings(std::slice::from_ref(&batch), "missing").is_empty());
        assert_eq!(
            next_page_token(&batch.schema(), &[]).as_deref(),
            Some("tok")
        );
        let plain = names_batch(None);
        assert_eq!(
            next_page_token(&plain.schema(), &[batch]).as_deref(),
            Some("tok")
        );
        assert_eq!(
            next_page_token(&plain.schema(), std::slice::from_ref(&plain)),
            None
        );
    }

    #[test]
    fn builds_continuations_and_orders_versions() {
        assert_eq!(
            continuation_sql("SHOW TABLES;", "a'b"),
            "SHOW TABLES LIMIT 1000 FROM 'a''b'"
        );
        assert_eq!(
            continuation_sql("SHOW VIEWS LIMIT 5 FROM 'old'", "new"),
            "SHOW VIEWS LIMIT 1000 FROM 'new'"
        );
        assert_eq!(
            continuation_sql("show tables limit 20", "t"),
            "show tables LIMIT 1000 FROM 't'"
        );
        assert_eq!(
            continuation_sql("SHOW TABLES\nLIMIT 20", "t"),
            "SHOW TABLES LIMIT 1000 FROM 't'",
            "a page clause on its own line is replaced"
        );
        assert_eq!(
            continuation_sql("SHOW TABLES -- every table", "t"),
            "SHOW TABLES -- every table\nLIMIT 1000 FROM 't'",
            "a trailing comment does not swallow the clause"
        );
        assert_eq!(without_page_clause("SHOW X LIMIT ten"), "SHOW X LIMIT ten");
        assert_eq!(without_page_clause("SHOW XLIMIT 3"), "SHOW XLIMIT 3");
        assert_eq!(without_page_clause("SHOW X LIMIT3"), "SHOW X LIMIT3");
        assert_eq!(
            without_page_clause("SHOW X LIMIT 3 WHERE"),
            "SHOW X LIMIT 3 WHERE"
        );
        assert!(is_query("with x as (select 1) select * from x"));
        assert!(!is_query("SHOW TABLES") && is_show("show jobs"));
        let mut versions = vec!["2", "10", "1"];
        versions.sort_by_key(|v| std::cmp::Reverse(version_order(v)));
        assert_eq!(versions, ["10", "2", "1"]);
    }

    #[test]
    fn namespaced_functions_use_the_schema_position() {
        assert_eq!(function_ref("add_one").sql_name(), "add_one");
        assert_eq!(
            function_ref("models$image_embedding").sql_name(),
            "models.image_embedding"
        );
        assert_eq!(function_ref("a$b$f").sql_name(), "\"a$b\".f");
        let status = |namespace: &[&str]| ColumnStatus {
            function: "clip".into(),
            function_namespace: namespace.iter().map(|part| part.to_string()).collect(),
            ..ColumnStatus::default()
        };
        assert_eq!(status(&[]).listed_function(), "clip");
        assert_eq!(status(&["models"]).listed_function(), "models$clip");
        assert_eq!(status(&["a", "b"]).listed_function(), "a$b$clip");
    }

    #[test]
    fn reads_function_definitions() {
        let definition = r#"{
            "name": "embed", "version": 3, "disabled": true,
            "image": {"manifest_digest": "sha256:abc"},
            "runtime": {"python_version": "3.12", "environment": {"kind": "pip"}},
            "resolved_environment": {"packages": [{"name": "numpy", "version": "2.0"}, {"bad": 1}]},
            "signature": {
                "inputs": [{"name": "x", "arrow_type": "int64", "nullable": false},
                           {"name": "y", "arrow_type": "utf8"}],
                "output": {"kind": "struct", "fields": [{"name": "a", "arrow_type": "int64", "nullable": true}]}
            },
            "source": {"encoding": "base64", "data": "ZGVmIGYoKTogcGFzcw=="}
        }"#;
        let detail = FunctionDetail::from_definition(definition).unwrap();
        assert_eq!(
            (detail.name.as_str(), detail.version.as_str()),
            ("embed", "3")
        );
        assert_eq!(detail.source.as_deref(), Some("def f(): pass"));
        assert_eq!(detail.packages, ["numpy==2.0"]);
        assert_eq!(detail.inputs, ["x int64 NOT NULL", "y utf8"]);
        assert_eq!(detail.output, "struct<a int64>");
        assert_eq!(detail.environment.as_deref(), Some("pip"));
        assert_eq!(detail.image_digest.as_deref(), Some("sha256:abc"));
        assert!(detail.disabled);

        let plain = FunctionDetail::from_definition(
            r#"{"source": {"encoding": "utf8", "data": "x"}, "signature": {"output": {"arrow_type": "float32"}}}"#,
        )
        .unwrap();
        assert_eq!(
            (plain.source.as_deref(), plain.output.as_str()),
            (Some("x"), "float32")
        );
        assert!(!plain.disabled && plain.python_version.is_none());
        assert!(FunctionDetail::from_definition("not json").is_err());
        assert!(
            FunctionDetail::from_definition(r#"{"source": {"encoding": "base64", "data": "!!"}}"#)
                .is_err()
        );
    }

    #[test]
    fn recognises_statements() {
        assert!(is_user_database("joust"));
        assert!(!is_user_database("system") && !is_user_database("_jobs"));
        assert_eq!(first_keyword("  -- note\n/* x */ select 1"), Some("select"));
        assert_eq!(first_keyword("-- only a comment"), None);
        assert_eq!(first_keyword("/* open"), None);
        assert_eq!(first_keyword("   "), None);
        assert!(is_catalog_statement("create table t as select 1"));
        assert!(is_catalog_statement("-- c\nREFRESH COLUMN c ON t"));
        assert!(!is_catalog_statement("SELECT * FROM t"));
        assert!(!is_catalog_statement("SHOW TABLES"));
    }

    #[tokio::test]
    async fn limits_rows_and_builds_outcomes() {
        let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, false)]));
        let batch = |values: Vec<i64>| {
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(values))]).unwrap()
        };
        let batches = || {
            futures::stream::iter(vec![
                Ok::<_, std::io::Error>(batch(vec![1, 2])),
                Ok(batch(vec![3, 4, 5])),
            ])
        };
        let mut stream = batches();
        let (kept, truncated) = collect_limited(&mut stream, 3, None).await.unwrap();
        assert!(truncated);
        assert_eq!(kept.iter().map(RecordBatch::num_rows).sum::<usize>(), 3);
        let mut stream = batches();
        let (all, truncated) = collect_limited(&mut stream, 10, None).await.unwrap();
        assert_eq!((all.len(), truncated), (2, false));
        let mut stream = batches();
        let (kept, truncated) = collect_limited(&mut stream, 2, None).await.unwrap();
        assert_eq!((kept.len(), truncated), (1, true));
        let mut failing =
            futures::stream::iter(vec![Err::<RecordBatch, _>(std::io::Error::other("x"))]);
        assert!(collect_limited(&mut failing, 5, None).await.is_err());

        let outcome = outcome(schema.clone(), kept, true, true, Duration::ZERO).unwrap();
        let result = outcome.result.unwrap();
        assert_eq!((result.batch.num_rows(), result.truncated), (2, true));
        assert!(outcome.catalog_changed && outcome.plan.is_none());

        let empty = super::outcome(schema, Vec::new(), false, false, Duration::ZERO).unwrap();
        assert_eq!(
            empty.result.unwrap().batch.num_columns(),
            1,
            "the schema survives"
        );
        let nothing = super::outcome(
            Arc::new(Schema::empty()),
            Vec::new(),
            false,
            false,
            Duration::ZERO,
        );
        assert!(nothing.unwrap().result.is_none());
    }

    fn rows_batch(columns: Vec<(&str, lancedb::arrow::arrow_array::ArrayRef)>) -> RecordBatch {
        RecordBatch::try_from_iter(columns).unwrap()
    }

    fn texts(values: &[Option<&str>]) -> lancedb::arrow::arrow_array::ArrayRef {
        Arc::new(StringArray::from(values.to_vec()))
    }

    fn numbers(values: &[Option<i64>]) -> lancedb::arrow::arrow_array::ArrayRef {
        Arc::new(Int64Array::from(values.to_vec()))
    }

    #[test]
    fn jobs_are_read_newest_first() {
        use lancedb::arrow::arrow_array::BooleanArray;
        let batch = rows_batch(vec![
            ("job_id", texts(&[Some("j1-a"), Some("j1-b"), Some("j1-c")])),
            (
                "job_type",
                texts(&[
                    Some("reindex_fts"),
                    Some("function_refresh"),
                    Some("compaction"),
                ]),
            ),
            (
                "table",
                texts(&[Some("docs"), Some("images"), Some("docs")]),
            ),
            (
                "state",
                texts(&[Some("succeeded"), Some("running"), Some("queued")]),
            ),
            (
                "paused",
                Arc::new(BooleanArray::from(vec![false, true, false])),
            ),
            (
                "created_at_millis",
                numbers(&[Some(1_000), Some(3_000), Some(2_000)]),
            ),
            ("completed_at_millis", numbers(&[Some(1_500), None, None])),
            ("rows_planned", numbers(&[None, Some(200), None])),
            ("rows_computed", numbers(&[None, Some(50), None])),
            ("active_workers", numbers(&[None, Some(2), None])),
            (
                "queue_reason",
                texts(&[None, None, Some("waiting for a worker")]),
            ),
            (
                "function_name",
                texts(&[None, Some("embed"), Some("plain")]),
            ),
            ("function_version", texts(&[None, Some("2"), None])),
        ]);
        let jobs = job_rows(&[batch]);
        let ids: Vec<&str> = jobs.iter().map(|job| job.job_id.as_str()).collect();
        assert_eq!(ids, ["j1-b", "j1-c", "j1-a"]);
        assert!(jobs[0].paused && jobs[0].is_active());
        assert_eq!(jobs[0].function, "embed v2");
        assert_eq!(jobs[0].fraction(), Some(0.25));
        assert_eq!(jobs[0].active_workers, Some(2));
        assert_eq!(jobs[1].function, "plain", "a version is optional");
        assert_eq!(jobs[1].queue_reason, "waiting for a worker");
        assert_eq!(jobs[2].function, "");
        assert_eq!(jobs[2].completed_at_millis, Some(1_500));
        assert_eq!(jobs[2].fraction(), None);
        assert!(job_rows(&[]).is_empty());
    }

    #[test]
    fn job_details_come_from_the_profile_and_events() {
        let events = rows_batch(vec![
            ("event_seq", numbers(&[Some(0), Some(1)])),
            ("state", texts(&[Some("created"), Some("in_progress")])),
            ("updated_at_millis", numbers(&[Some(10), Some(20)])),
            ("updated_by", texts(&[Some("api"), Some("lance-agent:0")])),
            ("rows_processed", numbers(&[None, Some(5)])),
        ]);
        // The profile of a finished job, as a 1.15 server writes it.
        let profile = r#"{"job_id":"j1-ft","state":"","job_state":"DONE",
            "total_millis":391,"facts":{"rows_read":10,"note":"ok"},"tasks":4,
            "workers":1,"timeline":[{"phase":"queued","at_millis":0,"millis":314},
            {"phase":"commit","at_millis":314,"millis":77},{"millis":3}],
            "failure":{"message":"boom"}}"#;
        let description = rows_batch(vec![("profile", texts(&[Some(profile)]))]);
        let detail = job_detail_of(
            "j1-ft",
            std::slice::from_ref(&description),
            std::slice::from_ref(&events),
        )
        .unwrap();
        assert_eq!(detail.job_id, "j1-ft");
        assert_eq!(
            detail.state, "done",
            "an empty state falls back to job_state"
        );
        assert_eq!(detail.total_millis, Some(391));
        assert!(detail.facts.contains(&("rows read".into(), "10".into())));
        assert!(detail.facts.contains(&("note".into(), "ok".into())));
        assert!(detail.facts.contains(&("tasks".into(), "4".into())));
        assert!(detail.facts.contains(&("workers".into(), "1".into())));
        let twice = rows_batch(vec![(
            "profile",
            texts(&[Some(
                r#"{"state":"done","facts":{"workers":1},"workers":2}"#,
            )]),
        )]);
        let deduped = job_detail_of("j1-w", &[twice], &[]).unwrap();
        assert_eq!(
            deduped.facts,
            [("workers".to_string(), "1".to_string())],
            "one workers fact, the profile's own"
        );
        assert_eq!(
            detail.timeline.len(),
            2,
            "phases without a name are skipped"
        );
        assert_eq!(detail.timeline[1].millis, 77);
        assert_eq!(detail.failure.as_deref(), Some("boom"));
        assert_eq!(detail.events[0].seq, 1, "newest first");
        assert_eq!(detail.events[0].rows, Some(5));

        // Without a state at all, the newest event's.
        let bare = rows_batch(vec![("profile", texts(&[Some(r#"{"failure":"oops"}"#)]))]);
        let detail = job_detail_of("j1-x", &[bare], std::slice::from_ref(&events)).unwrap();
        assert_eq!(
            (detail.state.as_str(), detail.failure.as_deref()),
            ("in_progress", Some("oops"))
        );
        // A job that has not started has events but no profile.
        let none = rows_batch(vec![("profile", texts(&[]))]);
        let detail = job_detail_of(
            "j1-y",
            std::slice::from_ref(&none),
            std::slice::from_ref(&events),
        )
        .unwrap();
        assert_eq!(
            (detail.job_id.as_str(), detail.state.as_str()),
            ("j1-y", "in_progress")
        );
        assert!(detail.timeline.is_empty());
        assert!(job_detail_of("j1-z", &[none], &[]).is_err(), "no such job");
        let broken = rows_batch(vec![("profile", texts(&[Some("not json")]))]);
        assert!(job_detail_of("j1-z", &[broken], &[]).is_err());
    }

    #[test]
    fn table_health_reads_stats_and_index_coverage() {
        let stats = rows_batch(vec![
            ("num_rows", numbers(&[Some(100)])),
            ("total_bytes", numbers(&[Some(2_048)])),
            ("num_fragments", numbers(&[Some(5)])),
            ("num_small_fragments", numbers(&[Some(4)])),
            ("num_deleted_rows", numbers(&[Some(1)])),
            ("num_versions", numbers(&[Some(3)])),
            ("num_indices", numbers(&[Some(2)])),
        ]);
        let indexes = rows_batch(vec![("num_unindexed_rows", numbers(&[Some(10), Some(5)]))]);
        let health = table_health_of(
            "docs".into(),
            std::slice::from_ref(&stats),
            Some(std::slice::from_ref(&indexes)),
        );
        assert_eq!(
            (
                health.rows,
                health.bytes,
                health.fragments,
                health.small_fragments
            ),
            (100, 2_048, 5, 4)
        );
        assert_eq!(
            (health.deleted_rows, health.versions, health.indexes),
            (1, 3, 2)
        );
        assert_eq!(health.unindexed_rows, 15);
        let without = table_health_of("docs".into(), &[stats], None);
        assert_eq!(without.unindexed_rows, 0, "SHOW INDEXES failed");

        let ok = || Ok(TableHealth::default());
        let failed = || Err(anyhow::anyhow!("denied"));
        assert_eq!(checked_tables(vec![ok(), failed()]).unwrap().len(), 1);
        assert!(checked_tables(vec![failed(), failed()]).is_err());
        assert!(checked_tables(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn column_status_rows() {
        use lancedb::arrow::arrow_array::builder::{ListBuilder, StringBuilder};
        let mut paths = ListBuilder::new(StringBuilder::new());
        paths.values().append_value("models");
        paths.append(true);
        let batch = rows_batch(vec![
            ("column", texts(&[Some("embedding")])),
            ("function_name", texts(&[Some("image_embedding")])),
            ("function_namespace_path", Arc::new(paths.finish())),
            ("function_version", texts(&[Some("2")])),
            ("rows", numbers(&[Some(75)])),
            ("fresh", numbers(&[Some(70)])),
            ("stale", numbers(&[Some(3)])),
            ("never_computed", numbers(&[Some(1)])),
            ("failed", numbers(&[Some(1)])),
            ("last_refresh_job_state", texts(&[Some("succeeded")])),
            ("auto_refresh_state", texts(&[None])),
        ]);
        let statuses = column_statuses(&[batch]);
        assert_eq!(statuses.len(), 1);
        let status = &statuses[0];
        assert_eq!(status.listed_function(), "models$image_embedding");
        assert_eq!(
            (
                status.rows,
                status.fresh,
                status.stale,
                status.never_computed,
                status.failed
            ),
            (75, 70, 3, 1, 1)
        );
        assert_eq!(status.last_refresh_state, "succeeded");
        assert_eq!(status.auto_refresh_state, "");
        assert!(column_statuses(&[]).is_empty());
    }

    #[test]
    fn namespace_paths_are_read_from_string_lists() {
        use lancedb::arrow::arrow_array::StringArray;
        use lancedb::arrow::arrow_array::builder::{ListBuilder, StringBuilder};
        let mut paths = ListBuilder::new(StringBuilder::new());
        paths.values().append_value("models");
        paths.append(true);
        paths.append(true);
        paths.values().append_value("team");
        paths.values().append_value("models");
        paths.append(true);
        paths.append(false);
        let batch = RecordBatch::try_from_iter([
            ("path", Arc::new(paths.finish()) as _),
            (
                "text",
                Arc::new(StringArray::from(vec!["a", "b", "c", "d"])) as _,
            ),
        ])
        .unwrap();
        assert_eq!(
            string_lists(std::slice::from_ref(&batch), "path"),
            [
                vec!["models".to_string()],
                vec![],
                vec!["team".to_string(), "models".to_string()],
                vec![]
            ]
        );
        assert_eq!(
            string_lists(std::slice::from_ref(&batch), "text"),
            vec![Vec::<String>::new(); 4],
            "text that is not JSON gives empty paths"
        );
        let json = RecordBatch::try_from_iter([(
            "path",
            Arc::new(StringArray::from(vec![r#"["team","models"]"#])) as _,
        )])
        .unwrap();
        assert_eq!(string_lists(&[json], "path"), [vec!["team", "models"]]);
        assert!(string_lists(&[batch], "missing").is_empty());
        let status = ColumnStatus {
            function: "image_embedding".into(),
            function_namespace: vec!["models".into()],
            ..ColumnStatus::default()
        };
        assert_eq!(status.listed_function(), "models$image_embedding");
    }

    #[test]
    fn short_errors_and_names() {
        let refused = anyhow::anyhow!("tcp connect error: Connection refused (os error 61)");
        assert_eq!(short_error(&refused), "connection refused");
        let long = anyhow::anyhow!("{}, source: inner", "x".repeat(400));
        let text = short_error(&long);
        assert!(text.ends_with('…') && text.chars().count() == MAX_CHECK_ERROR + 1);
        assert_eq!(short_error(&anyhow::anyhow!("bad, source: y")), "bad");
        assert_eq!(shown(&[]), "public");
        assert_eq!(shown(&["a".into(), "b".into()]), "a$b");
        let root = Path::new("/sessions");
        assert!(session_dir(root, "prod").starts_with(root));
        assert_ne!(session_dir(root, "prod"), session_dir(root, "dev"));
    }

    #[tokio::test]
    async fn bearer_tokens_are_sent_as_oidc_credentials() {
        let provider = BearerToken("tok".into());
        assert_eq!(format!("{provider:?}"), "BearerToken(…)");
        let headers = provider.get_headers().await.unwrap();
        assert_eq!(headers["authorization"], "Bearer tok");
        assert_eq!(headers["x-lancedb-credential-type"], "oidc");
    }

    #[tokio::test]
    async fn oauth_settings_map_to_the_sdk() {
        let dir = tempfile::tempdir().unwrap();
        let secret = dir.path().join("secret");
        std::fs::write(&secret, "s3cret\n").unwrap();
        let mut settings = OidcSettings {
            flow: OidcFlow::AuthorizationCode,
            issuer_url: "https://idp.example.com".into(),
            client_id: "joust".into(),
            scopes: vec!["openid".into()],
            client_secret_file: Some(secret),
            client_auth_method: Some(ClientAuth::ClientSecretPost),
            resource: None,
            audience: Some("lancedb".into()),
            redirect_uri: Some("http://localhost:8400/callback".into()),
            callback_port: Some(8400),
            refresh_buffer_secs: Some(60),
        };
        let sessions = dir.path().join("auth");
        let config = oauth_config("sso", &settings, &sessions).await.unwrap();
        assert_eq!(config.client_secret.as_deref(), Some("s3cret"));
        assert!(matches!(config.flow, OAuthFlow::AuthorizationCode(_)));
        assert!(
            config.token_cache.is_some(),
            "browser sessions are cached like the CLI's"
        );

        settings.flow = OidcFlow::ClientCredentials;
        settings.client_auth_method = Some(ClientAuth::None);
        let config = oauth_config("sso", &settings, &sessions).await.unwrap();
        assert!(config.token_cache.is_none());
        settings.client_auth_method = Some(ClientAuth::ClientSecretBasic);
        settings.client_secret_file = Some(dir.path().join("missing"));
        assert!(oauth_config("sso", &settings, &sessions).await.is_err());

        // No cached session: device codes would go to stderr, so refuse.
        settings.flow = OidcFlow::DeviceCode;
        settings.client_secret_file = None;
        settings.client_auth_method = None;
        let error = oauth_config("sso", &settings, &sessions).await.unwrap_err();
        assert!(
            error.to_string().contains("lancedb auth login"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn unreachable_servers_fail_to_connect() {
        let target = EnterpriseTarget {
            rest_url: "http://127.0.0.1:9".into(),
            database: "d".into(),
            auth: AuthMethod::ApiKey,
            ..EnterpriseTarget::default()
        };
        let credential = ResolvedCredential {
            credential: Credential::OidcToken("t".into()),
            source: "test".into(),
        };
        let error = RemoteDatabase::connect(target.clone(), credential)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("could not list tables"),
            "{error:#}"
        );

        let tls = EnterpriseTarget {
            ca_cert: Some("/no/such/ca.pem".into()),
            ..target
        };
        let credential = ResolvedCredential {
            credential: Credential::ApiKey("k".into()),
            source: "test".into(),
        };
        assert!(RemoteDatabase::connect(tls, credential).await.is_err());
    }

    fn rows(outcome: &QueryOutcome) -> usize {
        outcome.result.as_ref().map_or(0, |r| r.batch.num_rows())
    }

    /// End to end against a running server, which needs the database
    /// manifest (for views):
    ///
    /// ```sh
    /// lancedb server --data-dir DIR --database joust --database-manifest-enabled
    /// JOUST_TEST_ENTERPRISE=http://127.0.0.1:10024 cargo test
    /// ```
    ///
    /// `JOUST_TEST_ENTERPRISE_DB` names the database (default `joust`).
    /// Skipped when `JOUST_TEST_ENTERPRISE` is unset.
    #[test]
    fn against_a_running_server() {
        let Some(server) = crate::test_support::enterprise_server() else {
            return;
        };
        let target = EnterpriseTarget {
            rest_url: server.rest.clone(),
            database: server.database.clone(),
            ..EnterpriseTarget::default()
        };
        crate::test_support::RUNTIME.block_on(server_flow(target));
    }

    async fn server_flow(target: EnterpriseTarget) {
        let key = || ResolvedCredential {
            credential: Credential::ApiKey(profiles::LOCAL_API_KEY.into()),
            source: "the test key".into(),
        };
        let db = RemoteDatabase::connect(target.clone(), key())
            .await
            .unwrap();
        let capabilities = db.capabilities().clone();
        assert!(capabilities.sql(), "{capabilities:?}");
        assert!(capabilities.server_version.is_some());
        assert!(capabilities.databases.contains(&target.database));
        assert!(!capabilities.databases.iter().any(|d| d == "system"));
        let checks = db.checks();
        assert!(checks.iter().take(3).all(|check| check.ok), "{checks:?}");
        assert!(format!("{db:?}").contains(&target.database));

        // Fixtures: a root table, a namespaced table and a view.
        let batch = |n: i64| {
            RecordBatch::try_new(
                Arc::new(Schema::new(vec![
                    Field::new("id", DataType::Int64, false),
                    Field::new("name", DataType::Utf8, false),
                ])),
                vec![
                    Arc::new(Int64Array::from_iter_values(0..n)),
                    Arc::new(StringArray::from_iter_values(
                        (0..n).map(|i| format!("item-{i}")),
                    )),
                ],
            )
            .unwrap()
        };
        // Tables with Function columns cannot be overwritten; drop it first.
        let _ = db.run_sql("DROP TABLE joust_it_items", 10, false).await;
        db.connection()
            .create_table("joust_it_items", batch(50))
            .mode(lancedb::database::CreateTableMode::Overwrite)
            .execute()
            .await
            .unwrap();
        // Already there on a re-run.
        let _ = db.run_sql("CREATE SCHEMA joust_it", 10, false).await;
        db.connection()
            .create_table("daily", batch(5))
            .namespace(vec!["joust_it".into()])
            .mode(lancedb::database::CreateTableMode::Overwrite)
            .execute()
            .await
            .unwrap();
        let _ = db.run_sql("DROP VIEW joust_it_view", 10, false).await;
        db.run_sql(
            "CREATE VIEW joust_it_view AS SELECT id FROM joust_it_items WHERE id < 3",
            10,
            false,
        )
        .await
        .unwrap();

        let catalog = db.sync_catalog().await.unwrap();
        let find = |key: &str| {
            catalog
                .iter()
                .find(|table| table.key() == key)
                .unwrap_or_else(|| {
                    let keys: Vec<_> = catalog.iter().map(TableInfo::key).collect();
                    panic!("no {key} in {keys:?}")
                })
        };
        assert_eq!(find("joust_it_items").num_rows, 50);
        assert_eq!(find("joust_it_items").columns.len(), 2);
        let daily = find("joust_it.daily");
        assert_eq!(
            (daily.num_rows, &daily.namespace[..]),
            (5, &["joust_it".to_string()][..])
        );
        assert_eq!(find("joust_it_view").kind, TableKind::View);

        // SQL on the server: row limits, namespaces, views, DDL.
        let outcome = db
            .run_sql("SELECT * FROM joust_it.daily", 3, false)
            .await
            .unwrap();
        assert!(outcome.result.as_ref().unwrap().truncated);
        assert_eq!(rows(&outcome), 3);
        let outcome = db
            .run_sql("SELECT * FROM joust_it_view", 100, false)
            .await
            .unwrap();
        assert_eq!(rows(&outcome), 3);
        assert!(db.run_sql("-- nothing to run", 10, false).await.is_err());
        // Several statements run one request each; the last result shows.
        let both = db
            .run_sql("SELECT 1 AS a; SELECT 2 AS b;", 10, false)
            .await
            .unwrap();
        assert_eq!((both.statements, rows(&both)), (2, 1));
        assert!(db.rows_received() >= 1);
        let error = db
            .run_sql("SELECT 1; SELECT * FROM joust_it_missing", 10, false)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("statement 2 of 2"),
            "{error:#}"
        );
        db.cancel().await.unwrap();

        // A paged SHOW offers its next page.
        let shown = db.run_sql("SHOW TABLES LIMIT 1", 10, false).await.unwrap();
        let next = shown
            .continuation
            .expect("more than one table: a next page");
        assert!(next.starts_with("SHOW TABLES LIMIT 1000 FROM '"), "{next}");
        assert!(db.run_sql(&next, 10, false).await.is_ok());

        // Plans come from EXPLAIN ANALYZE when asked for.
        let planned = db
            .run_sql(
                "SELECT name, count(*) AS n FROM joust_it_items GROUP BY name",
                100,
                true,
            )
            .await
            .unwrap();
        assert!(
            planned.physical_plan.contains("metrics="),
            "{}",
            planned.physical_plan
        );
        assert!(
            planned
                .plan
                .as_ref()
                .is_some_and(|plan| plan.node_count() > 1)
        );
        let unplanned = db.run_sql("SHOW TABLES", 10, true).await.unwrap();
        assert!(unplanned.plan.is_none(), "only queries are explained");

        // Computed columns, Functions and their freshness.
        db.run_sql(
            "ALTER TABLE joust_it_items ADD COLUMN id_x2 AS id * 2",
            10,
            false,
        )
        .await
        .unwrap();
        db.run_sql(crate::app::FUNCTION_TEMPLATE, 10, false)
            .await
            .expect("the new-Function template runs as written");
        db.run_sql(
            "CREATE OR REPLACE FUNCTION joust_it_add(x BIGINT NOT NULL) RETURNS BIGINT \
             LANGUAGE python AS $$\ndef joust_it_add(x: int) -> int:\n    return x + 1\n$$",
            10,
            false,
        )
        .await
        .unwrap();
        let functions = db.functions().await.unwrap();
        let latest = functions
            .iter()
            .find(|function| function.name == "joust_it_add")
            .expect("the Function is listed")
            .clone();
        assert_eq!(latest.signature, "(x int64) -> int64");
        let detail = db
            .describe_function(&latest.name, &latest.version)
            .await
            .unwrap();
        assert!(
            detail
                .source
                .as_deref()
                .unwrap()
                .contains("def joust_it_add")
        );
        assert_eq!(
            (detail.inputs.as_slice(), detail.output.as_str()),
            (&["x int64 NOT NULL".to_string()][..], "int64")
        );
        // The app's "new version" SQL is accepted as written.
        db.run_sql(&crate::app::function_sql(&latest.name, &detail), 10, false)
            .await
            .expect("the new-version SQL runs");
        assert!(db.describe_function("joust_it_add", "0").await.is_err());

        db.run_sql(
            &format!(
                "ALTER TABLE joust_it_items ADD COLUMN plus AS joust_it_add(id) WITH (version = '{}')",
                latest.version
            ),
            10,
            false,
        )
        .await
        .unwrap();
        let catalog = db.sync_catalog().await.unwrap();
        let items = catalog
            .iter()
            .find(|table| table.key() == "joust_it_items")
            .unwrap();
        let computed = |name: &str| {
            items
                .columns
                .iter()
                .find(|column| column.name == name)
                .and_then(|column| column.computed.clone())
        };
        assert_eq!(
            computed("id_x2"),
            Some(super::super::Computed::Expression("id * 2".into()))
        );
        assert_eq!(computed("plus"), Some(super::super::Computed::Function));
        assert_eq!(computed("id"), None);
        let statuses = db.column_status(&items.table_ref()).await.unwrap();
        let plus = statuses
            .iter()
            .find(|status| status.column == "plus")
            .expect("the Function column has a status");
        assert_eq!((plus.function.as_str(), plus.rows), ("joust_it_add", 50));
        let created = db
            .run_sql("CREATE TABLE joust_it_tmp AS SELECT 1 AS x", 10, false)
            .await
            .unwrap();
        assert!(created.catalog_changed);
        db.run_sql("DROP TABLE joust_it_tmp", 10, false)
            .await
            .unwrap();
        assert_eq!(rows(&db.preview(&daily.table_ref(), 2).await.unwrap()), 2);
        let note = db
            .create_index(&TableRef::from("joust_it_items"), "id", IndexKind::Auto)
            .await
            .unwrap();
        assert!(note.contains("job"), "{note}");
        let fts = db
            .create_index(
                &TableRef::from("joust_it_items"),
                "name",
                IndexKind::FullText,
            )
            .await;
        assert!(fts.is_ok(), "{fts:?}");

        // Jobs: the index builds above are listed and described.
        let jobs = db.jobs().await.unwrap();
        assert!(!jobs.is_empty(), "the index builds are jobs");
        assert!(
            jobs.windows(2)
                .all(|pair| pair[0].created_at_millis >= pair[1].created_at_millis),
            "newest first"
        );
        let job = &jobs[0];
        assert!(!job.job_type.is_empty() && !job.state.is_empty(), "{job:?}");
        let detail = db.job_detail(&job.job_id).await.unwrap();
        assert_eq!(detail.job_id, job.job_id);
        assert!(!detail.state.is_empty(), "{detail:?}");
        assert!(db.job_detail("joust-no-such-job").await.is_err());
        // Cancelling a job that does not exist is not an error.
        let missing = db
            .job_action(
                "j1-bt-000000000-joustmissing-joust:joust_it_items",
                JobAction::Cancel,
            )
            .await;
        assert_eq!(missing.as_deref().ok(), Some("No such job"), "{missing:?}");

        // Dashboards: table health, and every SQL tile parses and runs.
        let health = db
            .table_health(&[TableRef::from("joust_it_items")])
            .await
            .unwrap();
        assert_eq!(health.len(), 1);
        assert_eq!(health[0].rows, 50, "{health:?}");
        assert!(
            health[0].fragments >= 1 && health[0].bytes > 0,
            "{health:?}"
        );
        if capabilities.system_views == Some(true) {
            let (month, now) = (
                crate::enterprise::dashboard::Range::Month,
                crate::app::now_millis(),
            );
            for tile in crate::enterprise::dashboard::TILES {
                let Some(sql) =
                    crate::enterprise::dashboard::tile_sql(tile.id, month, now, &target.database)
                else {
                    continue;
                };
                let batches = db.sql_batches(&sql).await;
                let batches = batches.unwrap_or_else(|e| panic!("{}: {e:#}\n{sql}", tile.title));
                let _ = crate::enterprise::dashboard::content(tile, &batches, month, now);
            }
        }

        // An unreachable SQL endpoint leaves browsing working.
        let browse = RemoteDatabase::connect(
            EnterpriseTarget {
                sql_url: "grpc://127.0.0.1:9".into(),
                ..target
            },
            key(),
        )
        .await
        .unwrap();
        assert!(!browse.capabilities().sql());
        assert!(browse.checks().iter().any(|c| c.label == "SQL" && !c.ok));
        let catalog = browse.sync_catalog().await.unwrap();
        assert!(catalog.iter().all(|table| table.kind != TableKind::View));
        let preview = browse
            .preview(&TableRef::from("joust_it_items"), 4)
            .await
            .unwrap();
        assert_eq!(rows(&preview), 4);
        let error = browse.run_sql("SELECT 1", 10, false).await.unwrap_err();
        assert!(error.to_string().contains("Browsing"), "{error}");
    }
}
