//! LanceDB access: connection, catalog introspection and SQL execution.
//!
//! SQL runs on DataFusion. Every LanceDB table is registered as a DataFusion
//! table provider, so ordinary `SELECT`s (joins, aggregates, window functions,
//! ...) work across tables, and `INSERT INTO` writes through to Lance. A few
//! statements are intercepted so they act on LanceDB rather than on
//! DataFusion's in-memory catalog:
//!
//! * `CREATE [OR REPLACE] TABLE t AS SELECT ...` materialises a Lance table.
//! * `DROP TABLE t` drops the Lance table.

pub mod functions;
pub mod import;
pub mod media_udf;
pub mod plan;
pub mod sample;

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures::StreamExt;
use lancedb::arrow::arrow_array::RecordBatch;
use lancedb::arrow::arrow_schema::{DataType, SchemaRef};
use lancedb::arrow::arrow_select::concat::concat_batches;
use lancedb::connection::Connection;
use lancedb::database::CreateTableMode;
use lancedb::datafusion::logical_expr::{DdlStatement, LogicalPlan};
use lancedb::datafusion::physical_plan::display::DisplayableExecutionPlan;
use lancedb::datafusion::physical_plan::{ExecutionPlan, execute_stream};
use lancedb::datafusion::prelude::{SessionConfig, SessionContext};
use lancedb::datafusion::sql::parser::DFParserBuilder;
use lancedb::datafusion::sql::sqlparser::dialect::GenericDialect;
use lancedb::index::Index;
use lancedb::index::scalar::FtsIndexBuilder;
use lancedb::table::datafusion::BaseTableAdapter;
use lancedb::table::datafusion::udtf::fts::FtsTableFunction;

use functions::{RegisteredTable, TableRegistry, VectorSearchFunction, is_vector_type};
use plan::PlanNode;

/// An open LanceDB database plus the SQL session that queries it.
///
/// Cheap to clone; all clones share the same connection and session.
#[derive(Clone)]
pub struct Database {
    inner: Arc<Inner>,
}

struct Inner {
    uri: String,
    connection: Connection,
    session: SessionContext,
    registry: TableRegistry,
}

impl fmt::Debug for Database {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Database")
            .field("uri", &self.inner.uri)
            .finish_non_exhaustive()
    }
}

/// Catalog entry describing one LanceDB table.
#[derive(Debug, Clone, PartialEq)]
pub struct TableInfo {
    pub name: String,
    pub columns: Vec<ColumnInfo>,
    pub num_rows: usize,
    pub version: u64,
    pub size_bytes: usize,
    pub indices: Vec<IndexInfo>,
}

/// A column in a [`TableInfo`].
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

impl ColumnInfo {
    /// Whether the column holds embedding vectors.
    pub fn is_vector(&self) -> bool {
        is_vector_type(&self.data_type)
    }

    /// Whether the column holds text (candidate for a full-text index).
    pub fn is_text(&self) -> bool {
        matches!(
            self.data_type,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
        )
    }
}

/// A LanceDB index on a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexInfo {
    pub name: String,
    pub kind: String,
    pub columns: Vec<String>,
}

/// Index flavours joust can create from the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    /// Let LanceDB choose (IVF-PQ for vectors, B-tree for scalars).
    Auto,
    /// Inverted index enabling `fts(...)` queries on a text column.
    FullText,
}

/// Everything produced by running one editor submission (1+ statements).
#[derive(Debug, Clone)]
pub struct QueryOutcome {
    /// Rows returned by the last statement that produced a result set.
    pub result: Option<ResultSet>,
    /// Executed physical plan of the last statement, with runtime metrics.
    pub plan: Option<PlanNode>,
    /// Optimised logical plan of the last statement.
    pub logical_plan: String,
    /// Physical plan (with metrics) of the last statement as indented text.
    pub physical_plan: String,
    /// Number of statements executed.
    pub statements: usize,
    /// Human-readable notes (e.g. "Created LanceDB table `t` (10 rows)").
    pub notes: Vec<String>,
    /// Whether the set of tables (or their contents) may have changed.
    pub catalog_changed: bool,
    /// Wall-clock time for the whole submission.
    pub elapsed: Duration,
}

/// A materialised query result.
#[derive(Debug, Clone)]
pub struct ResultSet {
    /// All returned rows in a single batch (empty batch if no rows).
    pub batch: RecordBatch,
    /// True when more rows were available than the configured row limit.
    pub truncated: bool,
}

impl Database {
    /// Connects to the LanceDB database at `uri` (a directory or object store URI).
    pub async fn open(uri: &str) -> Result<Self> {
        let connection = lancedb::connect(uri)
            .execute()
            .await
            .with_context(|| format!("could not open LanceDB database at {uri}"))?;

        let config = SessionConfig::new()
            .with_information_schema(true)
            .with_default_catalog_and_schema("lancedb", "main");
        let session = SessionContext::new_with_config(config);
        let registry = TableRegistry::default();
        session.register_udtf(
            "vector_search",
            Arc::new(VectorSearchFunction::new(registry.clone())),
        );
        session.register_udtf(
            "fts",
            Arc::new(FtsTableFunction::new(Arc::new(registry.clone()))),
        );
        media_udf::register(&session);

        let database = Self {
            inner: Arc::new(Inner {
                uri: uri.to_string(),
                connection,
                session,
                registry,
            }),
        };
        // Validate the location eagerly so a bad path fails at open time.
        database.table_names().await?;
        Ok(database)
    }

    /// The URI this database was opened with.
    pub fn uri(&self) -> &str {
        &self.inner.uri
    }

    async fn table_names(&self) -> Result<Vec<String>> {
        let names = self
            .inner
            .connection
            .table_names()
            .execute()
            .await
            .context("could not list tables")?;
        Ok(names)
    }

    /// Re-reads the table list, re-registers every table with the SQL session
    /// and returns fresh catalog information.
    pub async fn sync_catalog(&self) -> Result<Vec<TableInfo>> {
        let names = self.table_names().await?;
        let session = &self.inner.session;

        // Drop registrations for tables that no longer exist.
        for old in self.inner.registry.names() {
            if !names.contains(&old) {
                session.deregister_table(quote_ident(&old).as_str())?;
            }
        }

        let mut registered = BTreeMap::new();
        let mut infos = Vec::with_capacity(names.len());
        for name in names {
            let table = self
                .inner
                .connection
                .open_table(&name)
                .execute()
                .await
                .with_context(|| format!("could not open table {name}"))?;

            let adapter = Arc::new(BaseTableAdapter::try_new(table.base_table().clone()).await?);
            let schema = lancedb::datafusion::catalog::TableProvider::schema(adapter.as_ref());
            let quoted = quote_ident(&name);
            session.deregister_table(quoted.as_str())?;
            session.register_table(quoted.as_str(), adapter.clone())?;

            let num_rows = table.count_rows(None).await.unwrap_or_default();
            let version = table.version().await.unwrap_or_default();
            let size_bytes = table
                .stats()
                .await
                .map(|s| s.total_bytes)
                .unwrap_or_default();
            let indices = table
                .list_indices()
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|index| IndexInfo {
                    name: index.name,
                    kind: index.index_type.to_string(),
                    columns: index.columns,
                })
                .collect();

            infos.push(TableInfo {
                name: name.clone(),
                columns: schema
                    .fields()
                    .iter()
                    .map(|field| ColumnInfo {
                        name: field.name().clone(),
                        data_type: field.data_type().clone(),
                        nullable: field.is_nullable(),
                    })
                    .collect(),
                num_rows,
                version,
                size_bytes,
                indices,
            });
            registered.insert(
                name,
                RegisteredTable {
                    table,
                    schema,
                    adapter,
                },
            );
        }
        self.inner.registry.replace(registered);
        Ok(infos)
    }

    /// Runs every statement in `sql`, returning the last result set.
    ///
    /// At most `row_limit` rows are materialised per statement.
    pub async fn run_sql(&self, sql: &str, row_limit: usize) -> Result<QueryOutcome> {
        let started = Instant::now();
        let statements = DFParserBuilder::new(sql)
            .with_dialect(&GenericDialect {})
            .build()?
            .parse_statements()?;
        if statements.is_empty() {
            bail!("nothing to run: the editor contains no SQL statements");
        }

        let mut outcome = QueryOutcome {
            result: None,
            plan: None,
            logical_plan: String::new(),
            physical_plan: String::new(),
            statements: statements.len(),
            notes: Vec::new(),
            catalog_changed: false,
            elapsed: Duration::ZERO,
        };

        for statement in statements {
            let state = self.inner.session.state();
            let logical = state.statement_to_plan(statement).await?;
            self.run_logical(logical, row_limit, &mut outcome).await?;
        }

        outcome.elapsed = started.elapsed();
        Ok(outcome)
    }

    async fn run_logical(
        &self,
        logical: LogicalPlan,
        row_limit: usize,
        outcome: &mut QueryOutcome,
    ) -> Result<()> {
        match &logical {
            LogicalPlan::Ddl(DdlStatement::CreateMemoryTable(create)) => {
                let name = create.name.table().to_string();
                let rows = self
                    .create_table_as(
                        &name,
                        (*create.input).clone(),
                        create.or_replace,
                        create.if_not_exists,
                    )
                    .await?;
                outcome.notes.push(match rows {
                    Some(rows) => format!("Created LanceDB table {name} ({rows} rows)"),
                    None => format!("Table {name} already exists; left unchanged"),
                });
                outcome.catalog_changed = true;
                return Ok(());
            }
            LogicalPlan::Ddl(DdlStatement::DropTable(drop))
                if self.inner.registry.contains(drop.name.table()) =>
            {
                let name = drop.name.table().to_string();
                self.inner
                    .connection
                    .drop_table(&name, &[])
                    .await
                    .with_context(|| format!("could not drop table {name}"))?;
                self.inner
                    .session
                    .deregister_table(quote_ident(&name).as_str())?;
                outcome.notes.push(format!("Dropped LanceDB table {name}"));
                outcome.catalog_changed = true;
                return Ok(());
            }
            LogicalPlan::Dml(_) | LogicalPlan::Ddl(_) => outcome.catalog_changed = true,
            _ => {}
        }

        let frame = self.inner.session.execute_logical_plan(logical).await?;
        let optimized = frame.clone().into_optimized_plan()?;
        let physical = frame.create_physical_plan().await?;
        let (batches, truncated) = self.collect_limited(physical.clone(), row_limit).await?;

        let schema = physical.schema();
        let batch = if batches.is_empty() {
            RecordBatch::new_empty(schema)
        } else {
            concat_batches(&schema, &batches)?
        };

        outcome.logical_plan = optimized.display_indent().to_string();
        outcome.physical_plan = DisplayableExecutionPlan::with_metrics(physical.as_ref())
            .indent(true)
            .to_string();
        outcome.plan = Some(PlanNode::from_execution_plan(&physical));
        if !batch.schema().fields().is_empty() {
            outcome.result = Some(ResultSet { batch, truncated });
        }
        Ok(())
    }

    /// Streams `plan`, keeping at most `row_limit` rows.
    async fn collect_limited(
        &self,
        plan: Arc<dyn ExecutionPlan>,
        row_limit: usize,
    ) -> Result<(Vec<RecordBatch>, bool)> {
        let mut stream = execute_stream(plan, self.inner.session.task_ctx())?;
        let mut batches = Vec::new();
        let mut rows = 0usize;
        let mut truncated = false;
        while let Some(batch) = stream.next().await {
            let batch = batch?;
            let remaining = row_limit.saturating_sub(rows);
            if batch.num_rows() > remaining {
                if remaining > 0 {
                    batches.push(batch.slice(0, remaining));
                }
                truncated = true;
                break;
            }
            rows += batch.num_rows();
            batches.push(batch);
        }
        Ok((batches, truncated))
    }

    /// Materialises `input` as a new LanceDB table. Returns `None` when the
    /// table existed and `IF NOT EXISTS` asked to leave it alone.
    async fn create_table_as(
        &self,
        name: &str,
        input: LogicalPlan,
        or_replace: bool,
        if_not_exists: bool,
    ) -> Result<Option<usize>> {
        if self.inner.registry.contains(name) {
            if if_not_exists {
                return Ok(None);
            }
            if !or_replace {
                bail!("table {name} already exists (use CREATE OR REPLACE TABLE)");
            }
        }

        let frame = self.inner.session.execute_logical_plan(input).await?;
        let schema: SchemaRef = Arc::new(frame.schema().as_arrow().clone());
        let batches: Vec<RecordBatch> = frame
            .collect()
            .await?
            .into_iter()
            .filter(|batch| batch.num_rows() > 0)
            .collect();
        let rows = batches.iter().map(RecordBatch::num_rows).sum();

        let mode = if or_replace {
            CreateTableMode::Overwrite
        } else {
            CreateTableMode::Create
        };
        let connection = &self.inner.connection;
        if batches.is_empty() {
            connection
                .create_empty_table(name, schema)
                .mode(mode)
                .execute()
                .await?;
        } else {
            connection
                .create_table(name, batches)
                .mode(mode)
                .execute()
                .await?;
        }
        Ok(Some(rows))
    }

    /// Imports the media files under `folder` into a new table.
    pub async fn import_media_folder(
        &self,
        folder: std::path::PathBuf,
    ) -> Result<import::ImportSummary> {
        let existing = self.table_names().await?;
        let table = import::table_name_for(&folder, &existing);
        let source = folder.clone();
        let (batch, skipped, referenced) =
            tokio::task::spawn_blocking(move || import::build_media_batch(&source)).await??;
        if batch.num_rows() == 0 {
            bail!(
                "no supported media files found in {} (images, audio, video and PDFs are imported)",
                folder.display()
            );
        }
        let imported = batch.num_rows();
        self.inner
            .connection
            .create_table(&table, batch)
            .execute()
            .await
            .with_context(|| format!("could not create table {table}"))?;
        Ok(import::ImportSummary {
            table,
            imported,
            skipped,
            referenced,
        })
    }

    /// Builds an index on `table.column`.
    pub async fn create_index(&self, table: &str, column: &str, kind: IndexKind) -> Result<String> {
        let entry = self
            .inner
            .registry
            .get(table)
            .ok_or_else(|| anyhow!("table {table} is not open"))?;
        let index = match kind {
            IndexKind::Auto => Index::Auto,
            IndexKind::FullText => Index::FTS(FtsIndexBuilder::default()),
        };
        entry
            .table
            .create_index(&[column], index)
            .execute()
            .await
            .with_context(|| format!("could not index {table}.{column}"))?;
        Ok(match kind {
            IndexKind::Auto => format!("Indexed {table}.{column}"),
            IndexKind::FullText => format!("Built full-text index on {table}.{column}"),
        })
    }
}

/// Quotes an identifier for use in SQL when it is not a plain lower-case name.
pub fn quote_ident(name: &str) -> String {
    let plain = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lancedb::arrow::arrow_array::Array;
    use lancedb::arrow::arrow_array::cast::AsArray;
    use lancedb::arrow::arrow_array::types::Float32Type;

    async fn sample() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.lancedb");
        sample::create_sample_database(&path).await.unwrap();
        let db = Database::open(path.to_str().unwrap()).await.unwrap();
        (dir, db)
    }

    fn rows(outcome: &QueryOutcome) -> usize {
        outcome.result.as_ref().map_or(0, |r| r.batch.num_rows())
    }

    #[tokio::test]
    async fn catalog_describes_sample_tables() {
        let (_dir, db) = sample().await;
        let catalog = db.sync_catalog().await.unwrap();
        let names: Vec<_> = catalog.iter().map(|t| t.name.as_str()).collect();
        if crate::ffmpeg::available() {
            assert_eq!(names, ["events", "movies", "trailers"]);
        } else {
            assert_eq!(names, ["events", "movies"]);
        }

        let movies = &catalog[1];
        assert_eq!(movies.num_rows, 64);
        assert!(
            movies
                .columns
                .iter()
                .any(|c| c.name == "vector" && c.is_vector())
        );
        assert!(movies.indices.iter().any(|i| i.columns == ["title"]));
        assert_eq!(catalog[0].num_rows, sample::EVENT_ROWS);
    }

    #[tokio::test]
    async fn sql_returns_rows_and_an_instrumented_plan() {
        let (_dir, db) = sample().await;
        db.sync_catalog().await.unwrap();
        let outcome = db
            .run_sql(
                "SELECT event, count(*) AS n FROM events GROUP BY event ORDER BY n DESC",
                1_000,
            )
            .await
            .unwrap();
        assert_eq!(rows(&outcome), 5);
        let plan = outcome.plan.unwrap();
        assert!(plan.node_count() >= 3);
        assert_eq!(plan.output_rows, Some(5));
        assert!(outcome.physical_plan.contains("metrics="));
        assert!(!outcome.logical_plan.is_empty());
    }

    #[tokio::test]
    async fn vector_search_is_available_in_sql() {
        let (_dir, db) = sample().await;
        db.sync_catalog().await.unwrap();
        let outcome = db
            .run_sql(
                "SELECT title, _distance FROM vector_search('movies', 'vector', '[0.75, 0.1, 0, 0, 0, 0.55, 0, 0.35]', 5, 'cosine') ORDER BY _distance",
                1_000,
            )
            .await
            .unwrap();
        let result = outcome.result.unwrap();
        assert_eq!(result.batch.num_rows(), 5);
        assert_eq!(result.batch.num_columns(), 2);
        let distances = result.batch.column(1).as_primitive::<Float32Type>();
        assert!(distances.values().windows(2).all(|w| w[0] <= w[1]));
        let titles = result.batch.column(0).as_string::<i32>();
        assert!(
            (0..titles.len()).any(|i| titles.value(i).contains("Alien")),
            "expected an Alien film among the neighbours"
        );

        let error = db
            .run_sql(
                "SELECT * FROM vector_search('movies', 'title', '[1]', 5)",
                10,
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("vector_search needs"));
    }

    #[tokio::test]
    async fn full_text_search_is_available_in_sql() {
        let (_dir, db) = sample().await;
        db.sync_catalog().await.unwrap();
        let outcome = db
            .run_sql(
                r#"SELECT title FROM fts('movies', '{"match": {"column": "title", "terms": "hard"}}')"#,
                100,
            )
            .await
            .unwrap();
        assert_eq!(rows(&outcome), 5, "five Die Hard films");
    }

    #[tokio::test]
    async fn ddl_and_dml_write_through_to_lance() {
        let (_dir, db) = sample().await;
        db.sync_catalog().await.unwrap();

        let outcome = db
            .run_sql(
                "CREATE TABLE miyazaki AS SELECT title, year FROM movies WHERE director = 'Hayao Miyazaki'",
                100,
            )
            .await
            .unwrap();
        assert!(outcome.catalog_changed);
        assert!(outcome.notes[0].contains("11 rows"));
        let catalog = db.sync_catalog().await.unwrap();
        assert!(
            catalog
                .iter()
                .any(|t| t.name == "miyazaki" && t.num_rows == 11)
        );

        db.run_sql(
            "INSERT INTO miyazaki VALUES ('The Castle of Cagliostro', 1979)",
            10,
        )
        .await
        .unwrap();
        let outcome = db
            .run_sql("SELECT count(*) FROM miyazaki", 10)
            .await
            .unwrap();
        let count = outcome
            .result
            .unwrap()
            .batch
            .column(0)
            .as_primitive::<lancedb::arrow::arrow_array::types::Int64Type>()
            .value(0);
        assert_eq!(count, 12);

        db.run_sql("DROP TABLE miyazaki", 10).await.unwrap();
        let catalog = db.sync_catalog().await.unwrap();
        assert!(catalog.iter().all(|t| t.name != "miyazaki"));
    }

    #[tokio::test]
    async fn multiple_statements_and_row_limits() {
        let (_dir, db) = sample().await;
        db.sync_catalog().await.unwrap();
        let outcome = db
            .run_sql("SELECT 1; SELECT * FROM events", 1_234)
            .await
            .unwrap();
        assert_eq!(outcome.statements, 2);
        let result = outcome.result.unwrap();
        assert_eq!(result.batch.num_rows(), 1_234);
        assert!(result.truncated);

        assert!(db.run_sql("   ", 10).await.is_err());
        assert!(db.run_sql("SELEC nope", 10).await.is_err());
    }

    #[tokio::test]
    async fn built_in_example_queries_run() {
        let (_dir, db) = sample().await;
        db.sync_catalog().await.unwrap();
        let catalog = db.sync_catalog().await.unwrap();
        let welcome = db.run_sql(crate::app::WELCOME_SQL, 100).await.unwrap();
        assert_eq!(rows(&welcome), catalog.len(), "one row per table");
        for (title, sql) in crate::app::SAMPLE_EXAMPLES
            .iter()
            .filter(|(_, sql)| crate::app::example_available(sql, &catalog))
        {
            let outcome = db
                .run_sql(sql, 1_000)
                .await
                .unwrap_or_else(|e| panic!("example {title:?} failed: {e:#}"));
            assert!(rows(&outcome) > 0, "example {title:?} returned no rows");
        }
    }

    #[tokio::test]
    async fn media_functions_and_folder_import() {
        let (dir, db) = sample().await;
        db.sync_catalog().await.unwrap();

        let outcome = db
            .run_sql(
                "SELECT media_type(poster), image_width(poster), image_height(poster) FROM movies LIMIT 1",
                10,
            )
            .await
            .unwrap();
        let batch = outcome.result.unwrap().batch;
        assert_eq!(batch.column(0).as_string::<i32>().value(0), "image/png");
        let width = batch
            .column(1)
            .as_primitive::<lancedb::arrow::arrow_array::types::Int32Type>();
        assert_eq!(width.value(0), 120);

        // Import a folder with two images and a PDF, then search by colour.
        let folder = dir.path().join("Holiday Pics");
        std::fs::create_dir(&folder).unwrap();
        for (name, rgb) in [("red.png", [220u8, 20, 20]), ("blue.png", [20, 40, 220])] {
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(64, 48, image::Rgb(rgb)))
                .save(folder.join(name))
                .unwrap();
        }
        std::fs::write(folder.join("doc.pdf"), b"%PDF-1.7").unwrap();
        let summary = db.import_media_folder(folder).await.unwrap();
        assert_eq!(summary.table, "holiday_pics");
        assert_eq!((summary.imported, summary.skipped), (3, 0));

        db.sync_catalog().await.unwrap();
        let red = crate::media::color_vector(&image::DynamicImage::ImageRgb8(
            image::RgbImage::from_pixel(8, 8, image::Rgb([200, 30, 30])),
        ));
        let vector: Vec<String> = red.iter().map(|v| v.to_string()).collect();
        let outcome = db
            .run_sql(
                &format!(
                    "SELECT file_name FROM vector_search('holiday_pics', 'color_vector', '[{}]', 1)",
                    vector.join(",")
                ),
                10,
            )
            .await
            .unwrap();
        let names = outcome.result.unwrap().batch;
        assert_eq!(names.column(0).as_string::<i32>().value(0), "red.png");
    }

    #[tokio::test]
    async fn video_functions_and_import_when_ffmpeg_exists() {
        if !crate::ffmpeg::available() {
            eprintln!("skipping: ffmpeg not found");
            return;
        }
        let (dir, db) = sample().await;
        db.sync_catalog().await.unwrap();
        let outcome = db
            .run_sql(
                "SELECT media_duration(clip), media_codec(clip), media_width(clip), media_height(clip), media_type(clip) FROM trailers ORDER BY id LIMIT 1",
                10,
            )
            .await
            .unwrap();
        let batch = outcome.result.unwrap().batch;
        let seconds = batch
            .column(0)
            .as_primitive::<lancedb::arrow::arrow_array::types::Float64Type>();
        assert!((seconds.value(0) - sample::TRAILER_SECONDS).abs() < 0.2);
        let codec = batch.column(1).as_string::<i32>().value(0).to_string();
        assert!(codec == "H.264" || codec == "MPEG-4", "codec {codec}");
        let width = batch
            .column(2)
            .as_primitive::<lancedb::arrow::arrow_array::types::Int32Type>();
        assert_eq!(width.value(0), 320);
        assert_eq!(batch.column(4).as_string::<i32>().value(0), "video/mp4");

        // Import a folder holding a video: metadata, poster thumbnail, colours.
        let folder = dir.path().join("clips");
        std::fs::create_dir(&folder).unwrap();
        let clip =
            crate::ffmpeg::synthesize_clip(2.0, (160, 90), [0x101010, 0xff0000, 0x202020], 1)
                .unwrap();
        std::fs::write(folder.join("red.mp4"), &clip).unwrap();
        let summary = db.import_media_folder(folder).await.unwrap();
        assert_eq!((summary.imported, summary.referenced), (1, 0));
        db.sync_catalog().await.unwrap();
        let outcome = db
            .run_sql(
                "SELECT kind, width, height, duration_s, codec, thumbnail IS NOT NULL, color_vector IS NOT NULL, media_type(data) FROM clips",
                10,
            )
            .await
            .unwrap();
        let row = outcome.result.unwrap().batch;
        assert_eq!(row.column(0).as_string::<i32>().value(0), "video");
        let size = row
            .column(1)
            .as_primitive::<lancedb::arrow::arrow_array::types::Int32Type>();
        assert_eq!(size.value(0), 160);
        let duration = row
            .column(3)
            .as_primitive::<lancedb::arrow::arrow_array::types::Float64Type>();
        assert!((duration.value(0) - 2.0).abs() < 0.2);
        assert!(row.column(5).as_boolean().value(0), "poster thumbnail");
        assert!(row.column(6).as_boolean().value(0), "colour vector");
        assert_eq!(row.column(7).as_string::<i32>().value(0), "video/mp4");
    }

    #[tokio::test]
    async fn vector_search_arguments_are_validated() {
        let (_dir, db) = sample().await;
        db.sync_catalog().await.unwrap();
        for (sql, expected) in [
            ("vector_search('movies', 'vector')", "usage: vector_search"),
            (
                "vector_search(1, 'vector', '[1]')",
                "argument 'table' must be a string",
            ),
            (
                "vector_search('nope', 'vector', '[1]')",
                "no LanceDB table named 'nope'",
            ),
            (
                "vector_search('movies', 'nope', '[1]')",
                "has no column 'nope'",
            ),
            (
                "vector_search('movies', 'vector', '[1]', 0)",
                "k must be a positive integer",
            ),
            (
                "vector_search('movies', 'vector', '[1]', 'x')",
                "argument 'k' must be an integer",
            ),
            (
                "vector_search('movies', 'vector', '[1]', 3, 'manhattan')",
                "unknown distance metric",
            ),
            (
                "vector_search('movies', 'vector', 42)",
                "the query vector must be a literal",
            ),
            (
                "vector_search('movies', 'vector', '[1, x]')",
                "'x' is not a number",
            ),
            (
                "vector_search('movies', 'vector', make_array(1, NULL))",
                "vector values must be numbers, not NULL",
            ),
            // Volatile calls are not folded into a literal.
            (
                "vector_search('movies', 'vector', make_array(random(), 1))",
                "the query vector must be a literal",
            ),
            (
                "vector_search('movies', 'vector', '[1]', random())",
                "argument 'k' must be an integer literal",
            ),
        ] {
            let error = db
                .run_sql(&format!("SELECT * FROM {sql}"), 10)
                .await
                .map(|_| ())
                .unwrap_err();
            let text = format!("{error:#}");
            assert!(text.contains(expected), "{sql}: {text}");
        }

        // Vectors as strings, arrays (with negatives) and function calls agree.
        let vector = "0.75, 0.1, 0, 0, 0, 0.55, 0, -0.35";
        let mut titles = Vec::new();
        for argument in [
            format!("'[{vector}]'"),
            format!("[{vector}]"),
            format!("make_array({vector})"),
        ] {
            let outcome = db
                .run_sql(
                    &format!(
                        "SELECT title FROM vector_search('movies', 'vector', {argument}, 3, 'dot') ORDER BY _distance, title"
                    ),
                    10,
                )
                .await
                .unwrap_or_else(|e| panic!("{argument}: {e:#}"));
            let batch = outcome.result.unwrap().batch;
            let column = batch.column(0).as_string::<i32>();
            titles.push(
                (0..column.len())
                    .map(|i| column.value(i).to_string())
                    .collect::<Vec<_>>(),
            );
        }
        assert_eq!(titles[0].len(), 3);
        assert!(titles.iter().all(|t| *t == titles[0]), "{titles:?}");

        // Literal types folded from casts are accepted.
        for (k, table) in [
            ("arrow_cast(2, 'Int8')", "arrow_cast('movies', 'LargeUtf8')"),
            ("arrow_cast(2, 'Int64')", "'movies'"),
            ("arrow_cast(2, 'Int16')", "arrow_cast('movies', 'Utf8View')"),
            ("arrow_cast(2, 'Int32')", "'movies'"),
            ("arrow_cast(2, 'UInt8')", "'movies'"),
            ("arrow_cast(2, 'UInt16')", "'movies'"),
            ("arrow_cast(2, 'UInt32')", "'movies'"),
            ("arrow_cast(2, 'UInt64')", "'movies'"),
        ] {
            let sql = format!(
                "SELECT title FROM vector_search({table}, 'vector', arrow_cast('[1,0,0,0,0,0,0,0]', 'LargeUtf8'), {k})"
            );
            let sql = if k.contains("Int64") {
                // Also: a Utf8View vector and a fixed-size list literal.
                sql.replace(
                    "arrow_cast('[1,0,0,0,0,0,0,0]', 'LargeUtf8')",
                    "arrow_cast('[1,0,0,0,0,0,0,0]', 'Utf8View')",
                )
            } else if k.contains("UInt64") {
                sql.replace(
                    "arrow_cast('[1,0,0,0,0,0,0,0]', 'LargeUtf8')",
                    "arrow_cast([1.0,0,0,0,0,0,0,0], 'FixedSizeList(8, Float64)')",
                )
            } else {
                sql
            };
            let outcome = db
                .run_sql(&sql, 10)
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e:#}"));
            assert_eq!(rows(&outcome), 2, "{sql}");
        }
        for (sql, expected) in [
            (
                "vector_search('movies', 'vector', '[1]', arrow_cast('18446744073709551615', 'UInt64'))",
                "too large",
            ),
            (
                "vector_search('movies', 'vector', '[1]', 2.5)",
                "must be an integer literal",
            ),
            // LanceDB itself rejects a query of the wrong dimension.
            (
                "vector_search('movies', 'vector', '[1, 2]', 2)",
                "doesn't match",
            ),
        ] {
            let error = db
                .run_sql(&format!("SELECT * FROM {sql}"), 10)
                .await
                .map(|_| ())
                .unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{sql}: {error:#}");
        }

        // The default k is 10; projections and filters work on top.
        let outcome = db
            .run_sql(
                "SELECT year FROM vector_search('movies', 'vector', '[1,0,0,0,0,0,0,0]') WHERE year > 0",
                100,
            )
            .await
            .unwrap();
        assert_eq!(rows(&outcome), 10);
        assert_eq!(
            functions::parse_metric("hamming").unwrap(),
            lancedb::DistanceType::Hamming
        );
        assert_eq!(
            functions::parse_metric("euclidean").unwrap(),
            lancedb::DistanceType::L2
        );
    }

    #[tokio::test]
    async fn ddl_variants_and_statements_without_rows() {
        let (dir, db) = sample().await;
        db.sync_catalog().await.unwrap();

        let outcome = db
            .run_sql("CREATE TABLE IF NOT EXISTS movies AS SELECT 1 AS x", 10)
            .await
            .unwrap();
        assert_eq!(
            outcome.notes,
            ["Table movies already exists; left unchanged"]
        );
        let error = db
            .run_sql("CREATE TABLE movies AS SELECT 1 AS x", 10)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("CREATE OR REPLACE"));

        // Replacing, and creating from an empty result.
        for _ in 0..2 {
            let outcome = db
                .run_sql(
                    "CREATE OR REPLACE TABLE picks AS SELECT id FROM movies LIMIT 2",
                    10,
                )
                .await
                .unwrap();
            assert!(outcome.notes[0].contains("(2 rows)"));
            db.sync_catalog().await.unwrap();
        }
        let outcome = db
            .run_sql(
                "CREATE TABLE nothing AS SELECT id, title FROM movies WHERE id < 0",
                10,
            )
            .await
            .unwrap();
        assert!(outcome.notes[0].contains("(0 rows)"));
        let catalog = db.sync_catalog().await.unwrap();
        let nothing = catalog.iter().find(|t| t.name == "nothing").unwrap();
        assert_eq!((nothing.num_rows, nothing.columns.len()), (0, 2));

        // Zero-row selects keep their schema; SET returns no result set.
        let outcome = db.run_sql("SELECT * FROM nothing", 10).await.unwrap();
        let result = outcome.result.unwrap();
        assert_eq!(
            (result.batch.num_rows(), result.batch.num_columns()),
            (0, 2)
        );
        assert!(!result.truncated);
        let outcome = db
            .run_sql("SET datafusion.execution.batch_size = 4096", 10)
            .await
            .unwrap();
        assert!(outcome.result.is_none());
        let outcome = db.run_sql("EXPLAIN SELECT 1", 10).await.unwrap();
        assert!(rows(&outcome) > 0);

        // Views live in the SQL session only.
        let outcome = db
            .run_sql(
                "CREATE VIEW recent AS SELECT title FROM movies WHERE year > 2000",
                10,
            )
            .await
            .unwrap();
        assert!(outcome.catalog_changed);
        assert!(rows(&db.run_sql("SELECT * FROM recent", 100).await.unwrap()) > 0);
        db.run_sql("DROP VIEW recent", 10).await.unwrap();
        assert!(db.run_sql("SELECT * FROM recent", 10).await.is_err());

        // Tables dropped behind joust's back disappear on the next sync.
        let other = lancedb::connect(dir.path().join("sample.lancedb").to_str().unwrap())
            .execute()
            .await
            .unwrap();
        other.drop_table("picks", &[]).await.unwrap();
        let catalog = db.sync_catalog().await.unwrap();
        assert!(catalog.iter().all(|t| t.name != "picks"));
        assert!(db.run_sql("SELECT * FROM picks", 10).await.is_err());
    }

    #[tokio::test]
    async fn indexes_and_open_errors() {
        let (dir, db) = sample().await;
        db.sync_catalog().await.unwrap();
        assert_eq!(
            db.create_index("movies", "director", IndexKind::FullText)
                .await
                .unwrap(),
            "Built full-text index on movies.director"
        );
        assert_eq!(
            db.create_index("movies", "year", IndexKind::Auto)
                .await
                .unwrap(),
            "Indexed movies.year"
        );
        let error = db
            .create_index("movies", "nope", IndexKind::Auto)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("could not index movies.nope"));
        let error = db
            .create_index("nope", "id", IndexKind::Auto)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("not open"));
        let catalog = db.sync_catalog().await.unwrap();
        let movies = catalog.iter().find(|t| t.name == "movies").unwrap();
        assert_eq!(movies.indices.len(), 3);
        assert!(format!("{db:?}").contains("sample.lancedb"));

        // A regular file is not a database.
        let file = dir.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        assert!(Database::open(file.to_str().unwrap()).await.is_err());

        // Importing a folder without media fails clearly.
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        std::fs::write(empty.join("notes.txt"), "hi").unwrap();
        let error = db.import_media_folder(empty).await.unwrap_err();
        assert!(error.to_string().contains("no supported media files"));
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("movies"), "movies");
        assert_eq!(quote_ident("Movies"), "\"Movies\"");
        assert_eq!(quote_ident("my table"), "\"my table\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident("1st"), "\"1st\"");
    }
}
