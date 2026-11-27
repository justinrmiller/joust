//! The Dashboards view: tiles over a LanceDB Enterprise server's system
//! views (`system.public.*`), its jobs and its tables.
//!
//! Everything here is plain data: which tiles exist, the SQL each runs for a
//! time range, and how rows become a headline number, stacked bars or a
//! table. Loading happens in the app; drawing in `ui::dashboard`.

use std::fmt;
use std::sync::Arc;

use lancedb::arrow::arrow_array::{RecordBatch, StringArray};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema};

use crate::db::remote::{JobRow, strings};
use crate::db::{ResultSet, TableInfo, TableKind};
use crate::results::{ResultTable, human_bytes, thousands};

/// Most series a chart shows; smaller ones are summed into "other".
pub const MAX_SERIES: usize = 6;
/// Rows the table tiles ask for.
const TABLE_ROWS: usize = 20;
/// A table needs compaction when at least this many fragments are small
/// and they are at least half of its fragments.
const SMALL_FRAGMENTS: u64 = 4;
const MILLIS_PER_HOUR: i64 = 3_600_000;
const MILLIS_PER_DAY: i64 = 86_400_000;
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The time a dashboard covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Range {
    Day,
    #[default]
    Week,
    Month,
}

impl Range {
    pub const ALL: [Self; 3] = [Self::Day, Self::Week, Self::Month];

    /// How many bars a chart over the range has.
    pub fn buckets(self) -> i64 {
        match self {
            Self::Day => 24,
            Self::Week => 7,
            Self::Month => 30,
        }
    }

    /// What one bar covers: an hour over the last 24 hours, else a day.
    pub fn bucket(self) -> &'static str {
        match self {
            Self::Day => "hour",
            Self::Week | Self::Month => "day",
        }
    }

    fn bucket_millis(self) -> i64 {
        match self {
            Self::Day => MILLIS_PER_HOUR,
            Self::Week | Self::Month => MILLIS_PER_DAY,
        }
    }

    /// The earliest time (Unix milliseconds) the range includes: the start
    /// of its first bar, in whole UTC hours or days ending with the current
    /// one.
    pub fn cutoff_millis(self, now_millis: i64) -> i64 {
        let width = self.bucket_millis();
        now_millis.div_euclid(width) * width - (self.buckets() - 1) * width
    }

    /// One slot per bar, oldest first.
    pub fn slots(self, now_millis: i64) -> Vec<Slot> {
        let width = self.bucket_millis();
        let first = self.cutoff_millis(now_millis);
        (0..self.buckets())
            .map(|index| {
                let (year, month, day, minute) = civil(first + index * width);
                let date = format!("{year:04}-{month:02}-{day:02}");
                match self {
                    Self::Day => Slot {
                        key: format!("{date}T{:02}", minute / 60),
                        label: format!("{:02}:00", minute / 60),
                    },
                    Self::Week | Self::Month => Slot {
                        label: format!("{} {day}", MONTHS[month - 1]),
                        key: date,
                    },
                }
            })
            .collect()
    }
}

/// One bar of a chart over a range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// The start of the bucket as the server prints it, to the hour or day
    /// (`2026-11-24T13`, `2026-11-24`).
    pub key: String,
    /// `13:00`, `Nov 24`.
    pub label: String,
}

/// Year, month (1-12), day and minute of the day (UTC) of a Unix time in
/// milliseconds.
pub fn civil(millis: i64) -> (i64, usize, i64, i64) {
    let seconds = millis.div_euclid(1000);
    let days = seconds.div_euclid(86_400);
    let minute_of_day = seconds.rem_euclid(86_400) / 60;
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month as usize, day, minute_of_day)
}

impl fmt::Display for Range {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Day => "Last 24 hours",
            Self::Week => "Last 7 days",
            Self::Month => "Last 30 days",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TileId {
    Tables,
    Jobs,
    ActiveJobs,
    RefreshedRows,
    ServiceCpu,
    TableHealth,
    Storage,
    FailedJobs,
    SlowStatements,
}

/// How a tile shows its rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileView {
    /// A headline number and a note.
    Kpi,
    /// Stacked bars: one bar per hour or day of the range (a column named
    /// after [`Range::bucket`]), split by `series`, measuring `y`.
    Bars {
        series: &'static str,
        y: &'static str,
        unit: &'static str,
    },
    /// The rows in a grid.
    Table,
}

/// One tile of the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tile {
    pub id: TileId,
    pub title: &'static str,
    /// Where the data comes from (shown under the tile).
    pub source: &'static str,
    pub view: TileView,
    /// Shown when the source has no rows in the range.
    pub empty: &'static str,
}

/// Every tile, in reading order: headline numbers, charts, then tables.
/// Slowest statements comes last: servers record statements only when
/// `query_stats.enabled` is set.
///
/// There are no compute (CPU/GPU-hour) tiles from `job_metering` yet: on
/// 1.15 servers with metering data, reading those columns fails (ENT-4352).
pub const TILES: &[Tile] = &[
    Tile {
        id: TileId::Tables,
        title: "Tables",
        source: "catalog",
        view: TileView::Kpi,
        empty: "No tables yet",
    },
    Tile {
        id: TileId::Jobs,
        title: "Jobs finished",
        source: "system.public.job_metering",
        view: TileView::Kpi,
        empty: "No jobs finished in this range",
    },
    Tile {
        id: TileId::ActiveJobs,
        title: "Active jobs",
        source: "SHOW JOBS EXTENDED",
        view: TileView::Kpi,
        empty: "No jobs",
    },
    Tile {
        id: TileId::RefreshedRows,
        title: "Rows refreshed per day, by Function",
        source: "system.public.function_refresh_metering",
        view: TileView::Bars {
            series: "function",
            y: "rows",
            unit: "rows",
        },
        empty: "No Function refreshes in this range.",
    },
    Tile {
        id: TileId::ServiceCpu,
        title: "Service CPU-hours per day (whole cluster)",
        source: "system.public.service_metering",
        view: TileView::Bars {
            series: "service",
            y: "cpu_hours",
            unit: "CPU-h",
        },
        empty: "No service usage in this range. The agent samples services on Kubernetes \
                deployments.",
    },
    Tile {
        id: TileId::TableHealth,
        title: "Table health",
        source: "SHOW STATS / SHOW INDEXES FOR TABLE",
        view: TileView::Table,
        empty: "No tables to check.",
    },
    Tile {
        id: TileId::Storage,
        title: "Storage by table (latest sample)",
        source: "system.public.storage_metering",
        view: TileView::Table,
        empty: "No storage samples yet. The agent samples table storage about once a day.",
    },
    Tile {
        id: TileId::FailedJobs,
        title: "Failed jobs",
        source: "system.public.job_metering",
        view: TileView::Table,
        empty: "No failed jobs in this range.",
    },
    Tile {
        id: TileId::SlowStatements,
        title: "Slowest statements (whole cluster)",
        source: "system.public.query_stats",
        view: TileView::Table,
        empty: "No statements recorded. Query statistics are off unless the query node sets \
                query_stats.enabled = true; reading them needs Select on system.public.query_stats.",
    },
];

impl TileId {
    pub fn tile(self) -> &'static Tile {
        TILES
            .iter()
            .find(|tile| tile.id == self)
            .expect("every tile id has a tile")
    }
}

/// The SQL a tile runs in `database` for `range` at `now_millis`, or `None`
/// for a tile computed from the catalog, the jobs list or table statistics.
/// The metering views cover the whole cluster; tiles about jobs, refreshes
/// and storage keep to `database`, and service usage is cluster-wide.
pub fn tile_sql(id: TileId, range: Range, now_millis: i64, database: &str) -> Option<String> {
    let cutoff = range.cutoff_millis(now_millis);
    // Some rows (create_function jobs on 1.15) record the database under its
    // unpadded base64url name, so both spellings count.
    let encoded = {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(database)
    };
    let database = format!(
        "database_name IN ('{}', '{}')",
        database.replace('\'', "''"),
        encoded
    );
    let bucket = range.bucket();
    let day =
        |column: &str| format!("date_trunc('{bucket}', to_timestamp_millis({column})) AS {bucket}");
    Some(match id {
        TileId::Tables | TileId::ActiveJobs | TileId::TableHealth => return None,
        TileId::Jobs => format!(
            "SELECT count(*) AS jobs, \
             sum(CASE WHEN state IN ('failed', 'timed_out') THEN 1 ELSE 0 END) AS failed \
             FROM system.public.job_metering WHERE {database} AND ended_at_millis >= {cutoff}"
        ),
        TileId::RefreshedRows => format!(
            "SELECT {}, function_id AS function, sum(rows_assigned) AS rows \
             FROM system.public.function_refresh_metering \
             WHERE {database} AND finished_at_millis >= {cutoff} \
             GROUP BY 1, 2 ORDER BY 1, 2",
            day("finished_at_millis")
        ),
        TileId::SlowStatements => format!(
            "SELECT query, calls, round(mean_exec_time_ms, 1) AS mean_ms, \
             round(total_exec_time_ms, 1) AS total_ms, rows_scanned \
             FROM system.public.query_stats ORDER BY total_exec_time_ms DESC LIMIT {TABLE_ROWS}"
        ),
        TileId::ServiceCpu => format!(
            "SELECT {}, service, round(sum(total_cpu_millis) / 3600000.0, 4) AS cpu_hours \
             FROM system.public.service_metering WHERE window_start_millis >= {cutoff} \
             GROUP BY 1, 2 ORDER BY 1, 2",
            day("window_start_millis")
        ),
        TileId::Storage => format!(
            "SELECT table_id, \
             round(current_bytes / 1073741824.0, 2) AS current_gib, \
             round(retained_bytes / 1073741824.0, 2) AS retained_gib, retained_versions, \
             num_rows FROM (SELECT *, row_number() OVER (PARTITION BY table_id \
             ORDER BY window_start_millis DESC) AS latest FROM system.public.storage_metering \
             WHERE {database}) \
             WHERE latest = 1 ORDER BY retained_bytes DESC LIMIT {TABLE_ROWS}"
        ),
        TileId::FailedJobs => format!(
            "SELECT to_timestamp_millis(ended_at_millis) AS ended, job_type, table_name, job_id \
             FROM system.public.job_metering WHERE {database} \
             AND state IN ('failed', 'timed_out') AND ended_at_millis >= {cutoff} \
             ORDER BY ended_at_millis DESC LIMIT {TABLE_ROWS}"
        ),
    })
}

/// What a loaded tile shows.
#[derive(Debug, Clone)]
pub enum TileContent {
    Kpi {
        value: String,
        note: String,
    },
    Bars(Bars),
    Table(ResultTable),
    /// The source had no rows in the range.
    Empty,
}

/// Turns a tile's rows, loaded for `range` at `now_millis`, into what it
/// shows.
pub fn content(tile: &Tile, batches: &[RecordBatch], range: Range, now_millis: i64) -> TileContent {
    let rows: usize = batches.iter().map(RecordBatch::num_rows).sum();
    match tile.view {
        TileView::Kpi => kpi(tile.id, batches),
        TileView::Bars { .. } if rows == 0 => TileContent::Empty,
        TileView::Bars { series, y, unit } => {
            let slots = range.slots(now_millis);
            let bars = Bars::from_rows(batches, range.bucket(), series, y, unit, &slots);
            if bars.x.is_empty() || bars.series.is_empty() {
                TileContent::Empty
            } else {
                TileContent::Bars(bars)
            }
        }
        TileView::Table => match table(batches) {
            Some(table) => TileContent::Table(table),
            None => TileContent::Empty,
        },
    }
}

/// A headline number from a KPI tile's single row.
fn kpi(id: TileId, batches: &[RecordBatch]) -> TileContent {
    let number = |name: &str| {
        strings(batches, name)
            .first()
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0)
    };
    let (value, note) = match id {
        TileId::Jobs => {
            let (jobs, failed) = (number("jobs"), number("failed"));
            if jobs == 0.0 {
                return TileContent::Empty;
            }
            (
                format!("{} jobs", thousands(jobs as usize)),
                format!("{} failed", thousands(failed as usize)),
            )
        }
        _ => return TileContent::Empty,
    };
    TileContent::Kpi { value, note }
}

/// The Tables headline, from the catalog.
pub fn tables_kpi(catalog: &[TableInfo]) -> TileContent {
    let tables: Vec<&TableInfo> = catalog
        .iter()
        .filter(|table| table.kind != TableKind::View)
        .collect();
    if tables.is_empty() {
        return TileContent::Empty;
    }
    let rows: usize = tables.iter().map(|table| table.num_rows).sum();
    let bytes: usize = tables.iter().map(|table| table.size_bytes).sum();
    TileContent::Kpi {
        value: format!("{} tables", tables.len()),
        note: format!("{} rows · {}", thousands(rows), human_bytes(bytes)),
    }
}

/// The Active jobs headline, from the jobs list.
pub fn active_jobs_kpi(jobs: &[JobRow]) -> TileContent {
    let active: Vec<&JobRow> = jobs.iter().filter(|job| job.is_active()).collect();
    let paused = active.iter().filter(|job| job.paused).count();
    TileContent::Kpi {
        value: format!("{} active", active.len()),
        note: if paused == 0 {
            format!("{} jobs listed", thousands(jobs.len()))
        } else {
            format!("{paused} paused")
        },
    }
}

/// Rows in a grid, or `None` when there are none.
pub fn table(batches: &[RecordBatch]) -> Option<ResultTable> {
    let first = batches.iter().find(|batch| batch.num_rows() > 0)?;
    let batch =
        lancedb::arrow::arrow_select::concat::concat_batches(&first.schema(), batches).ok()?;
    Some(ResultTable::new(Arc::new(ResultSet {
        batch,
        truncated: false,
    })))
}

/// Stacked bars: one bar per `x` (in order), split by series.
#[derive(Debug, Clone, PartialEq)]
pub struct Bars {
    /// Bar labels (`Nov 24` for days).
    pub x: Vec<String>,
    /// Series names, alphabetically; the last may be "other".
    pub series: Vec<String>,
    /// `values[series][x]`.
    pub values: Vec<Vec<f64>>,
    pub unit: &'static str,
}

impl Bars {
    /// Pivots `(x, series, y)` rows into bars, keeping the largest series
    /// (at most [`MAX_SERIES`] bars' worth) and summing the rest into "other".
    /// With `slots`, there is one bar per slot (empty where no row falls in
    /// it, and rows outside every slot are left out); without, one per
    /// distinct `x`.
    pub fn from_rows(
        batches: &[RecordBatch],
        x: &str,
        series: &str,
        y: &str,
        unit: &'static str,
        slots: &[Slot],
    ) -> Self {
        let xs = strings(batches, x);
        let names = strings(batches, series);
        let ys: Vec<f64> = strings(batches, y)
            .iter()
            .map(|value| value.parse().unwrap_or(0.0))
            .collect();

        // The bar each row belongs to.
        let (labels, columns): (Vec<String>, Vec<Option<usize>>) = if slots.is_empty() {
            let mut keys = xs.clone();
            keys.sort();
            keys.dedup();
            let columns = xs.iter().map(|x| keys.binary_search(x).ok()).collect();
            (keys.iter().map(|key| day_label(key)).collect(), columns)
        } else {
            let columns = xs
                .iter()
                .map(|x| {
                    // `2026-11-24 13:00:00` and `2026-11-24T13:00:00` alike.
                    let x = x.replacen(' ', "T", 1);
                    slots.iter().position(|slot| x.starts_with(&slot.key))
                })
                .collect();
            (
                slots.iter().map(|slot| slot.label.clone()).collect(),
                columns,
            )
        };
        let rows: Vec<(usize, &str, f64)> = columns
            .iter()
            .zip(&names)
            .zip(&ys)
            .filter_map(|((column, name), value)| {
                let name = if name.is_empty() { "(none)" } else { name };
                Some(((*column)?, name, *value))
            })
            .collect();

        let mut totals: Vec<(String, f64)> = Vec::new();
        for (_, name, value) in &rows {
            match totals.iter_mut().find(|(existing, _)| existing == name) {
                Some((_, total)) => *total += value,
                None => totals.push((name.to_string(), *value)),
            }
        }
        totals.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        // The largest series keep their own colour; the rest fold into
        // "other". Kept series are listed alphabetically so a series keeps
        // its colour slot when the range changes the ranking.
        let mut kept: Vec<String> = totals.iter().map(|(name, _)| name.clone()).collect();
        let folded = kept.len() > MAX_SERIES;
        if folded {
            kept.truncate(MAX_SERIES - 1);
        }
        kept.sort();
        if folded {
            kept.push("other".to_string());
        }

        let mut values = vec![vec![0.0; labels.len()]; kept.len()];
        for (column, name, value) in rows {
            let row = kept
                .iter()
                .position(|kept| kept == name)
                .unwrap_or(kept.len() - 1);
            values[row][column] += value;
        }
        Self {
            x: labels,
            series: kept,
            values,
            unit,
        }
    }

    /// The height of each bar.
    pub fn totals(&self) -> Vec<f64> {
        (0..self.x.len())
            .map(|column| self.values.iter().map(|row| row[column]).sum())
            .collect()
    }
}

/// What a tile shows of a server error: the SQL error's message, without
/// request ids and error codes; the first line of anything else.
pub fn tile_error(text: &str) -> String {
    if let Some(start) = text.find("message: \"") {
        // Up to the quote that closes the message, unescaping as we go.
        let mut message = String::new();
        let mut chars = text[start + "message: \"".len()..].chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => message.extend(chars.next()),
                '"' => return message,
                c => message.push(c),
            }
        }
    }
    text.lines().next().unwrap_or(text).to_string()
}

/// `Nov 24` for an ISO date or timestamp; other text unchanged.
pub fn day_label(text: &str) -> String {
    let month = text.get(5..7).and_then(|m| m.parse::<usize>().ok());
    let day = text.get(8..10).and_then(|d| d.parse::<u32>().ok());
    match (text.as_bytes().get(4), month, day) {
        (Some(b'-'), Some(month @ 1..=12), Some(day)) => format!("{} {day}", MONTHS[month - 1]),
        _ => text.to_string(),
    }
}

/// Size, fragments and index coverage of one table (`SHOW STATS` and
/// `SHOW INDEXES FOR TABLE`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TableHealth {
    pub table: String,
    pub rows: u64,
    pub bytes: u64,
    pub fragments: u64,
    pub small_fragments: u64,
    pub deleted_rows: u64,
    pub versions: u64,
    pub indexes: u64,
    pub unindexed_rows: u64,
}

impl TableHealth {
    /// `needs compaction`, `index behind` or `ok`.
    pub fn status(&self) -> &'static str {
        if self.small_fragments >= SMALL_FRAGMENTS && self.small_fragments * 2 >= self.fragments {
            "needs compaction"
        } else if self.unindexed_rows > 0 {
            "index behind"
        } else {
            "ok"
        }
    }
}

/// The Table health grid: tables needing attention first.
pub fn health_table(health: &[TableHealth]) -> TileContent {
    if health.is_empty() {
        return TileContent::Empty;
    }
    let mut sorted: Vec<&TableHealth> = health.iter().collect();
    sorted.sort_by_key(|table| (table.status() == "ok", table.table.clone()));
    let column = |name: &str, values: Vec<String>| {
        (
            Field::new(name, DataType::Utf8, false),
            Arc::new(StringArray::from(values)) as _,
        )
    };
    let (fields, arrays): (Vec<Field>, Vec<_>) = [
        column("table", sorted.iter().map(|t| t.table.clone()).collect()),
        column(
            "status",
            sorted.iter().map(|t| t.status().to_string()).collect(),
        ),
        column(
            "rows",
            sorted.iter().map(|t| thousands(t.rows as usize)).collect(),
        ),
        column(
            "size",
            sorted
                .iter()
                .map(|t| human_bytes(t.bytes as usize))
                .collect(),
        ),
        column(
            "fragments",
            sorted
                .iter()
                .map(|t| format!("{} ({} small)", t.fragments, t.small_fragments))
                .collect(),
        ),
        column(
            "unindexed rows",
            sorted
                .iter()
                .map(|t| thousands(t.unindexed_rows as usize))
                .collect(),
        ),
        column(
            "versions",
            sorted.iter().map(|t| t.versions.to_string()).collect(),
        ),
    ]
    .into_iter()
    .unzip();
    let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)
        .expect("columns have equal lengths");
    TileContent::Table(ResultTable::new(Arc::new(ResultSet {
        batch,
        truncated: false,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lancedb::arrow::arrow_array::{Float64Array, Int64Array};

    fn batch(columns: Vec<(&str, Arc<dyn lancedb::arrow::arrow_array::Array>)>) -> RecordBatch {
        RecordBatch::try_from_iter(columns).unwrap()
    }

    /// 2026-11-24 21:09 UTC.
    const NOW: i64 = 1_795_554_564_300;

    fn shown(tile: &Tile, batches: &[RecordBatch]) -> TileContent {
        content(tile, batches, Range::Week, NOW)
    }

    #[test]
    fn ranges_and_sql() {
        assert_eq!(Range::default(), Range::Week);
        assert_eq!(Range::Day.to_string(), "Last 24 hours");
        assert_eq!(civil(0), (1970, 1, 1, 0));
        assert_eq!(civil(951_782_400_000), (2000, 2, 29, 0));
        assert_eq!(civil(-60_000), (1969, 12, 31, 23 * 60 + 59));
        // Whole days (hours for the last 24 hours), ending with the current one.
        assert_eq!(civil(Range::Week.cutoff_millis(NOW)), (2026, 11, 18, 0));
        assert_eq!(civil(Range::Month.cutoff_millis(NOW)), (2026, 10, 26, 0));
        assert_eq!(
            civil(Range::Day.cutoff_millis(NOW)),
            (2026, 11, 23, 22 * 60)
        );
        let week = Range::Week.slots(NOW);
        assert_eq!(week.len(), 7);
        assert_eq!(
            week[0],
            Slot {
                key: "2026-11-18".into(),
                label: "Nov 18".into()
            }
        );
        assert_eq!(week[6].label, "Nov 24");
        let day = Range::Day.slots(NOW);
        assert_eq!(
            (day.len(), day[0].key.as_str(), day[23].label.as_str()),
            (24, "2026-11-23T22", "21:00")
        );
        assert_eq!(Range::Month.slots(NOW).len(), 30);

        for tile in TILES {
            assert_eq!(tile.id.tile(), tile);
            match tile_sql(tile.id, Range::Week, NOW, "demo") {
                Some(sql) => assert!(sql.contains("FROM "), "{sql}"),
                None => assert!(matches!(
                    tile.id,
                    TileId::Tables | TileId::ActiveJobs | TileId::TableHealth
                )),
            }
        }
        let cutoff = Range::Week.cutoff_millis(NOW);
        let failed = tile_sql(TileId::FailedJobs, Range::Week, NOW, "it's").unwrap();
        assert!(failed.contains(&format!(">= {cutoff}")), "{failed}");
        assert!(
            failed.contains("database_name IN ('it''s', 'aXQncw')"),
            "{failed}"
        );
        let storage = tile_sql(TileId::Storage, Range::Week, NOW, "demo").unwrap();
        assert!(
            storage.contains("WHERE database_name IN ('demo', 'ZGVtbw')"),
            "{storage}"
        );
        let service = tile_sql(TileId::ServiceCpu, Range::Week, NOW, "demo").unwrap();
        assert!(
            !service.contains("database_name"),
            "services are cluster-wide"
        );
        let hourly = tile_sql(TileId::ServiceCpu, Range::Day, NOW, "demo").unwrap();
        assert!(
            hourly.contains("date_trunc('hour', to_timestamp_millis(window_start_millis)) AS hour"),
            "{hourly}"
        );
    }

    #[test]
    fn server_errors_are_shortened() {
        let error = "Runtime error: SQL error (request_id=7d8): code: 'Client specified an \
                     invalid argument', message: \"validity read requires a \\\"filtered\\\" read\"";
        assert_eq!(
            tile_error(error),
            "validity read requires a \"filtered\" read"
        );
        assert_eq!(tile_error("table not found\nmore"), "table not found");
        assert_eq!(
            tile_error("x, message: \"bad\", hint: \"try X\""),
            "bad",
            "stops at the quote closing the message"
        );
    }

    #[test]
    fn kpis_read_their_row() {
        let jobs = batch(vec![
            ("jobs", Arc::new(Int64Array::from(vec![1_234]))),
            ("failed", Arc::new(Int64Array::from(vec![3]))),
        ]);
        let tile = TileId::Jobs.tile();
        assert!(matches!(
            shown(tile, &[jobs]),
            TileContent::Kpi { value, note } if value == "1,234 jobs" && note == "3 failed"
        ));
        let none = batch(vec![
            ("jobs", Arc::new(Int64Array::from(vec![0]))),
            ("failed", Arc::new(Int64Array::from(vec![0]))),
        ]);
        assert!(matches!(shown(tile, &[none]), TileContent::Empty));

        assert!(matches!(kpi(TileId::Storage, &[]), TileContent::Empty));
    }

    #[test]
    fn bars_pivot_series_and_fold_the_rest() {
        let days = vec!["2026-11-23T00:00:00", "2026-11-24T00:00:00"];
        let mut x = Vec::new();
        let mut series = Vec::new();
        let mut y = Vec::new();
        for (i, name) in ["a", "b", "c", "d", "e", "f", "g", "h"].iter().enumerate() {
            for day in &days {
                x.push(*day);
                series.push(*name);
                y.push((i + 1) as f64);
            }
        }
        x.push(days[0]);
        series.push("");
        y.push(0.5);
        let rows = batch(vec![
            ("day", Arc::new(StringArray::from(x))),
            ("kind", Arc::new(StringArray::from(series))),
            ("n", Arc::new(Float64Array::from(y))),
        ]);
        let bars = Bars::from_rows(std::slice::from_ref(&rows), "day", "kind", "n", "rows", &[]);
        assert_eq!(bars.x, ["Nov 23", "Nov 24"]);
        assert_eq!(bars.series.len(), MAX_SERIES);
        assert_eq!(
            bars.series[..5],
            ["d", "e", "f", "g", "h"],
            "the five largest, alphabetically, so colours follow the series"
        );
        assert_eq!(bars.series.last().unwrap(), "other");
        // a, b, c and the unnamed row fold into "other" on the first day.
        assert_eq!(bars.values[MAX_SERIES - 1], [1.0 + 2.0 + 3.0 + 0.5, 6.0]);
        assert_eq!(bars.totals(), [36.5, 36.0]);

        let tile = Tile {
            view: TileView::Bars {
                series: "kind",
                y: "n",
                unit: "rows",
            },
            ..*TileId::ServiceCpu.tile()
        };
        assert!(matches!(
            shown(&tile, std::slice::from_ref(&rows)),
            TileContent::Bars(_)
        ));
        assert!(matches!(
            shown(&tile, &[rows.slice(0, 0)]),
            TileContent::Empty
        ));
        assert!(
            matches!(
                shown(TileId::ServiceCpu.tile(), &[rows]),
                TileContent::Empty
            ),
            "the tile's own columns are missing"
        );
    }

    #[test]
    fn bars_have_one_slot_per_bucket() {
        let rows = batch(vec![
            (
                "day",
                Arc::new(StringArray::from(vec![
                    "2026-11-23 00:00:00",
                    "2026-11-24T00:00:00",
                    "2026-11-01T00:00:00",
                ])),
            ),
            ("kind", Arc::new(StringArray::from(vec!["a", "b", "old"]))),
            ("n", Arc::new(Float64Array::from(vec![1.0, 2.0, 50.0]))),
        ]);
        let slots = Range::Week.slots(NOW);
        let bars = Bars::from_rows(
            std::slice::from_ref(&rows),
            "day",
            "kind",
            "n",
            "rows",
            &slots,
        );
        assert_eq!(bars.x.len(), 7, "days without rows still get a bar");
        assert_eq!(
            bars.series,
            ["a", "b"],
            "rows outside the range are left out"
        );
        assert_eq!(bars.totals(), [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0]);

        let hours = batch(vec![
            (
                "hour",
                Arc::new(StringArray::from(vec!["2026-11-24T21:00:00"])),
            ),
            ("kind", Arc::new(StringArray::from(vec!["a"]))),
            ("n", Arc::new(Float64Array::from(vec![4.0]))),
        ]);
        let TileContent::Bars(bars) = content(
            TileId::ServiceCpu.tile(),
            &[batch(vec![
                ("hour", hours.column(0).clone()),
                ("service", hours.column(1).clone()),
                ("cpu_hours", hours.column(2).clone()),
            ])],
            Range::Day,
            NOW,
        ) else {
            panic!("an hourly chart");
        };
        assert_eq!((bars.x.len(), bars.x[23].as_str()), (24, "21:00"));
        assert_eq!(bars.values[0][23], 4.0);
    }

    #[test]
    fn labels_tables_and_health() {
        assert_eq!(day_label("2026-01-05"), "Jan 5");
        assert_eq!(day_label("2026-13-05"), "2026-13-05");
        assert_eq!(day_label("api"), "api");

        let rows = batch(vec![("q", Arc::new(StringArray::from(vec!["SELECT 1"])))]);
        assert!(matches!(
            shown(TileId::SlowStatements.tile(), std::slice::from_ref(&rows)),
            TileContent::Table(_)
        ));
        assert!(matches!(
            shown(TileId::SlowStatements.tile(), &[rows.slice(0, 0)]),
            TileContent::Empty
        ));

        let healthy = TableHealth {
            table: "b".into(),
            fragments: 10,
            small_fragments: 3,
            ..TableHealth::default()
        };
        let fragmented = TableHealth {
            table: "c".into(),
            fragments: 8,
            small_fragments: 4,
            ..TableHealth::default()
        };
        let behind = TableHealth {
            table: "a".into(),
            fragments: 20,
            small_fragments: 9,
            unindexed_rows: 5,
            ..TableHealth::default()
        };
        assert_eq!(healthy.status(), "ok");
        assert_eq!(fragmented.status(), "needs compaction");
        assert_eq!(
            behind.status(),
            "index behind",
            "9 of 20 small is under half"
        );
        let TileContent::Table(table) = health_table(&[healthy, fragmented, behind]) else {
            panic!("a table");
        };
        assert_eq!(table.row_count(), 3);
        assert_eq!(table.cell_text(0, 0), "a", "attention first, by name");
        assert_eq!(table.cell_text(2, 1), "ok");
        assert!(matches!(health_table(&[]), TileContent::Empty));
    }

    #[test]
    fn catalog_and_jobs_headlines() {
        let table = |name: &str, kind, rows, bytes| TableInfo {
            namespace: Vec::new(),
            name: name.into(),
            kind,
            columns: Vec::new(),
            num_rows: rows,
            version: 1,
            size_bytes: bytes,
            indices: Vec::new(),
        };
        let catalog = [
            table("a", TableKind::Table, 1_000, 2048),
            table("v", TableKind::View, 0, 0),
        ];
        assert!(matches!(
            tables_kpi(&catalog),
            TileContent::Kpi { value, note } if value == "1 tables" && note == "1,000 rows · 2.0 KB"
        ));
        assert!(matches!(tables_kpi(&catalog[1..]), TileContent::Empty));

        let job = |state: &str, paused| JobRow {
            state: state.into(),
            paused,
            ..JobRow::default()
        };
        assert!(matches!(
            active_jobs_kpi(&[job("running", false), job("succeeded", false)]),
            TileContent::Kpi { value, note } if value == "1 active" && note == "2 jobs listed"
        ));
        assert!(matches!(
            active_jobs_kpi(&[job("running", true)]),
            TileContent::Kpi { note, .. } if note == "1 paused"
        ));
    }
}
