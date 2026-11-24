//! Phase 0 probe: exercises the lancedb crate's Enterprise client against a
//! running LanceDB Enterprise server and reports what works.
//!
//! Every check runs independently and prints `OK` or `ERR` with its timing
//! and output, so one failure does not hide the rest. See
//! `docs/enterprise-plan.md` (Phase 0) for what the results decided.
//!
//! ```sh
//! # A debug-built server needs bigger thread stacks (as Sophon's own tests do).
//! RUST_MIN_STACK=67108864 lancedb server --data-dir /tmp/joust-ent \
//!     --database joust --database-manifest-enabled   # REST :10024, SQL :10025
//! cargo run --example enterprise_probe
//! ```
//!
//! Environment (all optional):
//! * `JOUST_PROBE_REST` — REST endpoint (default `http://127.0.0.1:10024`)
//! * `JOUST_PROBE_SQL` — Flight SQL endpoint (default: the REST port + 1)
//! * `JOUST_PROBE_DB` — database name (default `joust`)
//! * `LANCEDB_API_KEY` — API key (default `sk_localtest`)

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use futures::TryStreamExt;
use lancedb::Connection;
use lancedb::arrow::arrow_array::{
    FixedSizeListArray, Float32Array, Int64Array, RecordBatch, StringArray,
};
use lancedb::arrow::arrow_cast::display::{ArrayFormatter, FormatOptions};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, SchemaRef};
use lancedb::database::CreateTableMode;
use lancedb::listing::ListingOptions;
use lancedb::sql::Query;

const TABLE: &str = "probe_items";
const ROWS: usize = 100_000;
const DIM: i32 = 8;
const SHOWN_ROWS: usize = 8;
/// Sums over a ~2 × 10^8-row join: seconds on a debug-built server.
const LONG_SQL: &str = "SELECT sum(a.id + b.id) AS s FROM probe_items a \
                        JOIN probe_items b ON a.id % 50 = b.id % 50";
/// A ~10^9-row join, and a different statement so the server cannot answer it
/// from a stored result of the progress check.
const CANCEL_SQL: &str = "SELECT sum(a.id + b.id) AS s FROM probe_items a \
                          JOIN probe_items b ON a.id % 10 = b.id % 10";

/// Statements whose output shapes the plan's SQL-first design.
const STATEMENTS: &[&str] = &[
    "SHOW SERVER VERSION",
    "SHOW DATABASES",
    "SHOW NAMESPACES",
    "SHOW TABLES",
    "SHOW STATS FOR TABLE probe_items",
    "SHOW INDEXES FOR TABLE probe_items",
    "SHOW FUNCTIONS",
    "SHOW JOBS",
    "SHOW BUILTIN FUNCTIONS",
    "SHOW MATERIALIZED VIEWS",
    "SELECT * FROM system.public.job_metering LIMIT 3",
    "SELECT * FROM system.public.service_metering LIMIT 3",
    "SELECT * FROM system.public.query_stats LIMIT 3",
    "SELECT id, distance(vec, [0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5]) AS d \
     FROM probe_items ORDER BY d LIMIT 5",
    "SELECT 1 AS a; SELECT 2 AS b",
];

/// A result: its schema (known even when no batch arrives) and its batches.
type Rows = (SchemaRef, Vec<RecordBatch>);

struct Probe {
    conn: Connection,
    results: Vec<(String, bool, Duration)>,
}

impl Probe {
    /// Runs one check, printing its outcome; never aborts the probe.
    async fn check<F, Fut>(&mut self, name: &str, run: F)
    where
        F: FnOnce(Connection) -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        let started = Instant::now();
        let outcome = run(self.conn.clone()).await;
        let elapsed = started.elapsed();
        let ok = outcome.is_ok();
        let tag = if ok { "OK " } else { "ERR" };
        println!("\n== [{tag}] {name} ({} ms)", elapsed.as_millis());
        let body = outcome.unwrap_or_else(|error| format!("{error:#}"));
        for line in body.lines() {
            println!("   {line}");
        }
        self.results.push((name.to_string(), ok, elapsed));
    }

    fn summary(&self) {
        println!("\n== Summary");
        for (name, ok, elapsed) in &self.results {
            let tag = if *ok { "OK " } else { "ERR" };
            println!("   [{tag}] {name:<58} {:>7} ms", elapsed.as_millis());
        }
    }
}

/// Reads a submitted query to the end.
async fn collect(query: &Query) -> lancedb::Result<Rows> {
    let reader = query.reader().await?;
    let schema = reader.schema();
    let batches = reader.try_collect().await?;
    Ok((schema, batches))
}

/// Runs `statement` on the server and reads every batch.
async fn sql(conn: &Connection, statement: &str) -> Result<Rows> {
    let query = conn.execute_query_async(statement).execute().await?;
    Ok(collect(&query).await?)
}

/// Formats up to `max_rows` rows as `| a | b |` lines under a schema line.
fn render((schema, batches): &Rows, max_rows: usize) -> Result<String> {
    let total: usize = batches.iter().map(RecordBatch::num_rows).sum();
    let columns: Vec<String> = schema
        .fields()
        .iter()
        .map(|field| format!("{} {}", field.name(), field.data_type()))
        .collect();
    let mut out = format!(
        "{total} rows in {} batch(es); columns: {}\n",
        batches.len(),
        columns.join(", ")
    );
    let options = FormatOptions::default();
    let mut shown = 0;
    for batch in batches {
        let formatters = batch
            .columns()
            .iter()
            .map(|column| ArrayFormatter::try_new(column.as_ref(), &options))
            .collect::<Result<Vec<_>, _>>()?;
        for row in 0..batch.num_rows() {
            if shown == max_rows {
                out.push_str("  …\n");
                return Ok(out);
            }
            let cells: Vec<String> = formatters
                .iter()
                .map(|formatter| clip(&formatter.value(row).to_string(), 70))
                .collect();
            out.push_str(&format!("  | {} |\n", cells.join(" | ")));
            shown += 1;
        }
    }
    Ok(out)
}

fn clip(text: &str, max: usize) -> String {
    let flat = text.replace('\n', "⏎");
    if flat.chars().count() <= max {
        flat
    } else {
        format!("{}…", flat.chars().take(max).collect::<String>())
    }
}

/// `ROWS` rows of `id`, `name` and an 8-d `vec`.
fn sample_batch() -> Result<RecordBatch> {
    let ids: Vec<i64> = (0..ROWS as i64).collect();
    let names: Vec<String> = ids.iter().map(|id| format!("item-{id}")).collect();
    let values: Vec<f32> = (0..ROWS)
        .flat_map(|row| (0..DIM as usize).map(move |d| ((row * 7 + d * 13) % 100) as f32 / 100.0))
        .collect();
    let item = Arc::new(Field::new("item", DataType::Float32, true));
    let vectors = FixedSizeListArray::try_new(
        item.clone(),
        DIM,
        Arc::new(Float32Array::from(values)),
        None,
    )?;
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("vec", DataType::FixedSizeList(item, DIM), true),
    ]));
    Ok(RecordBatch::try_new(
        schema,
        vec![
            Arc::new(Int64Array::from(ids)),
            Arc::new(StringArray::from(names)),
            Arc::new(vectors),
        ],
    )?)
}

#[tokio::main]
async fn main() -> Result<()> {
    let rest = std::env::var("JOUST_PROBE_REST").unwrap_or("http://127.0.0.1:10024".into());
    let database = std::env::var("JOUST_PROBE_DB").unwrap_or("joust".into());
    let api_key = std::env::var("LANCEDB_API_KEY").unwrap_or("sk_localtest".into());

    // A region is required even with a host override; its value is unused then.
    let mut builder = lancedb::connect(&format!("db://{database}"))
        .region("us-east-1")
        .host_override(&rest)
        .api_key(&api_key);
    if let Ok(sql_host) = std::env::var("JOUST_PROBE_SQL") {
        builder = builder.sql_host_override(&sql_host);
    }
    let conn = builder
        .execute()
        .await
        .context("could not build the remote connection")?;
    println!("Probing {rest} (database {database}) with lancedb 0.41");

    let mut probe = Probe {
        conn,
        results: Vec::new(),
    };

    probe
        .check("REST: create table (overwrite)", |c| async move {
            let started = Instant::now();
            c.create_table(TABLE, sample_batch()?)
                .mode(CreateTableMode::Overwrite)
                .execute()
                .await?;
            Ok(format!(
                "wrote {ROWS} rows in {} ms",
                started.elapsed().as_millis()
            ))
        })
        .await;

    probe
        .check("REST: list tables (paginated Listing)", |c| async move {
            let mut listing = c.list_tables(&[], ListingOptions::default());
            let page = listing.next_page().await?;
            let names = page.map(|page| page.items).unwrap_or_default();
            Ok(format!("first page: {names:?}"))
        })
        .await;

    probe
        .check("REST: table stats, version, indices", |c| async move {
            let table = c.open_table(TABLE).execute().await?;
            let rows = table.count_rows(None).await?;
            let version = table.version().await?;
            let stats = table.stats().await?;
            let indices = table.list_indices().await?;
            Ok(format!(
                "rows {rows}, version {version}, {} bytes, {} fragments, indices {:?}",
                stats.total_bytes,
                stats.fragment_stats.num_fragments,
                indices.iter().map(|i| &i.name).collect::<Vec<_>>()
            ))
        })
        .await;

    probe
        .check("SQL: SELECT 1", |c| async move {
            render(&sql(&c, "SELECT 1 AS one").await?, SHOWN_ROWS)
        })
        .await;

    probe
        .check("SQL: stream a whole table", |c| async move {
            let started = Instant::now();
            let query = c
                .execute_query_async(format!("SELECT id, name FROM {TABLE}"))
                .execute()
                .await?;
            let mut reader = query.reader().await?;
            let mut first_batch = None;
            let (mut batches, mut rows) = (0usize, 0usize);
            while let Some(batch) = reader.try_next().await? {
                first_batch.get_or_insert_with(|| started.elapsed());
                batches += 1;
                rows += batch.num_rows();
            }
            if rows != ROWS {
                bail!("expected {ROWS} rows, got {rows}");
            }
            Ok(format!(
                "{rows} rows in {batches} batches; first batch after {} ms, all after {} ms",
                first_batch.unwrap_or_default().as_millis(),
                started.elapsed().as_millis()
            ))
        })
        .await;

    probe
        .check("SQL: parameterised query ($1)", |c| async move {
            let schema = Arc::new(Schema::new(vec![Field::new("p", DataType::Int64, false)]));
            let params = RecordBatch::try_new(schema, vec![Arc::new(Int64Array::from(vec![42]))])?;
            let query = c
                .execute_query_async(format!("SELECT id, name FROM {TABLE} WHERE id = $1"))
                .parameters(params)
                .execute()
                .await?;
            render(&collect(&query).await?, SHOWN_ROWS)
        })
        .await;

    probe
        .check(
            "SQL: progress from describe() during a query",
            |c| async move {
                let started = Instant::now();
                let query = Arc::new(c.execute_query_async(LONG_SQL).execute().await?);
                let reading = query.clone();
                let reader = tokio::spawn(async move { collect(&reading).await });
                let mut samples = Vec::new();
                while !reader.is_finished() && samples.len() < 120 {
                    let description = query.describe().await?;
                    samples.push(format!(
                        "{}ms {}:{:?}",
                        started.elapsed().as_millis(),
                        description.status,
                        description.progress
                    ));
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                let rows = reader.await??;
                Ok(format!(
                    "id {}; took {} ms\nsamples: {}\n{}",
                    query.id(),
                    started.elapsed().as_millis(),
                    samples.join(", "),
                    render(&rows, SHOWN_ROWS)?
                ))
            },
        )
        .await;

    probe
        .check("SQL: cancel a running query", |c| async move {
            let query = Arc::new(c.execute_query_async(CANCEL_SQL).execute().await?);
            let id = query.id();
            let reading = query.clone();
            let reader = tokio::spawn(async move { collect(&reading).await.map(|rows| rows.1.len()) });
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let before = query.describe().await?;
            let running = !reader.is_finished();
            let stop = Instant::now();
            let cancel = query.cancel().await;
            let mut out = format!(
                "id {id}\nbefore cancel: {} {:?}; reader still running: {running}\ncancel(): {:?}\n",
                before.status,
                before.progress,
                cancel.as_ref().map_err(ToString::to_string)
            );
            if cancel.is_err() && !reader.is_finished() {
                // Dropping the reader and every handle stops a directly
                // streamed query (the documented alternative to cancel).
                reader.abort();
                let _ = reader.await;
                drop(query);
                out.push_str("dropped the reader and the handle instead\n");
            } else {
                let ended = tokio::time::timeout(Duration::from_secs(60), reader)
                    .await
                    .context("the reader did not stop within 60 s of cancel")??;
                out.push_str(&format!("reader ended: {:?}\n", ended.map_err(|e| e.to_string())));
            }
            out.push_str(&format!("stopped {} ms after cancel\n", stop.elapsed().as_millis()));
            let probe_started = Instant::now();
            sql(&c, "SELECT 1 AS one").await?;
            out.push_str(&format!(
                "SELECT 1 afterwards answered in {} ms",
                probe_started.elapsed().as_millis()
            ));
            Ok(out)
        })
        .await;

    for &statement in STATEMENTS {
        probe
            .check(&format!("SQL: {}", clip(statement, 50)), |c| async move {
                render(&sql(&c, statement).await?, SHOWN_ROWS)
            })
            .await;
    }

    probe
        .check("SQL: EXPLAIN ANALYZE (plan text shape)", |c| async move {
            let statement =
                format!("EXPLAIN ANALYZE SELECT name, count(*) FROM {TABLE} GROUP BY name");
            let (_, batches) = sql(&c, &statement).await?;
            let mut out = String::new();
            for batch in &batches {
                let formatters = batch
                    .columns()
                    .iter()
                    .map(|column| {
                        ArrayFormatter::try_new(column.as_ref(), &FormatOptions::default())
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                for row in 0..batch.num_rows() {
                    for (field, formatter) in batch.schema().fields().iter().zip(&formatters) {
                        out.push_str(&format!("[{}]\n{}\n", field.name(), formatter.value(row)));
                    }
                }
            }
            Ok(out)
        })
        .await;

    probe
        .check("SQL: CREATE TABLE AS + DROP TABLE", |c| async move {
            let created = sql(
                &c,
                &format!("CREATE TABLE probe_ctas AS SELECT id FROM {TABLE} LIMIT 10"),
            )
            .await?;
            let dropped = sql(&c, "DROP TABLE probe_ctas").await?;
            Ok(format!(
                "{}{}",
                render(&created, SHOWN_ROWS)?,
                render(&dropped, SHOWN_ROWS)?
            ))
        })
        .await;

    probe
        .check("SQL: CREATE INDEX (returns a job?)", |c| async move {
            render(
                &sql(
                    &c,
                    &format!("CREATE INDEX probe_id_idx ON {TABLE} USING btree (id)"),
                )
                .await?,
                SHOWN_ROWS,
            )
        })
        .await;

    probe
        .check("REST: list_jobs (0.41, /v1/jobs/list)", |c| async move {
            let mut listing = c.list_jobs(ListingOptions::default());
            let jobs = listing
                .next_page()
                .await?
                .map(|page| page.items)
                .unwrap_or_default();
            Ok(jobs
                .iter()
                .map(|job| {
                    format!(
                        "{} {} {} {}",
                        job.job_id, job.job_type, job.table, job.state
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        })
        .await;

    probe
        .check(
            "REST: list_functions (0.41; route removed in 1.15?)",
            |c| async move {
                let mut listing = c.list_functions(&[], ListingOptions::default());
                let functions = listing
                    .next_page()
                    .await?
                    .map(|page| page.items)
                    .unwrap_or_default();
                Ok(format!("{} function(s)", functions.len()))
            },
        )
        .await;

    probe.summary();
    Ok(())
}
