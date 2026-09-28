//! Presentation helpers for query results: cell formatting, sorting, sizing
//! and CSV export.

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use lancedb::arrow::arrow_array::cast::AsArray;
use lancedb::arrow::arrow_array::{Array, ArrayRef, RecordBatch, UInt32Array};
use lancedb::arrow::arrow_cast::display::{ArrayFormatter, FormatOptions};
use lancedb::arrow::arrow_ord::sort::{SortOptions, sort_to_indices};
use lancedb::arrow::arrow_schema::DataType;

use crate::db::ResultSet;

/// Formatting options shared by the grid, the cell inspector and CSV export.
pub fn format_options() -> FormatOptions<'static> {
    FormatOptions::default().with_null("NULL")
}

/// Formats a single value (binary values are described, not hex-dumped).
pub fn format_value(array: &dyn Array, row: usize) -> String {
    match CellFormatter::new(array) {
        Some(formatter) => formatter.format(row),
        None => "<unformattable>".to_string(),
    }
}

/// Whether a type holds raw bytes (candidate media).
pub fn is_binary(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Binary
            | DataType::LargeBinary
            | DataType::BinaryView
            | DataType::FixedSizeBinary(_)
    )
}

/// The bytes of a binary value, or `None` for nulls and non-binary arrays.
pub fn binary_value(array: &dyn Array, row: usize) -> Option<&[u8]> {
    if row >= array.len() || array.is_null(row) {
        return None;
    }
    match array.data_type() {
        DataType::Binary => Some(array.as_binary::<i32>().value(row)),
        DataType::LargeBinary => Some(array.as_binary::<i64>().value(row)),
        DataType::BinaryView => Some(array.as_binary_view().value(row)),
        DataType::FixedSizeBinary(_) => Some(array.as_fixed_size_binary().value(row)),
        _ => None,
    }
}

/// Formats cells of one column for display.
pub enum CellFormatter<'a> {
    Arrow(ArrayFormatter<'a>),
    /// Binary columns show a media description instead of a hex dump.
    Binary(&'a dyn Array),
}

impl<'a> CellFormatter<'a> {
    pub fn new(array: &'a dyn Array) -> Option<Self> {
        if is_binary(array.data_type()) {
            Some(Self::Binary(array))
        } else {
            ArrayFormatter::try_new(array, &format_options())
                .ok()
                .map(Self::Arrow)
        }
    }

    pub fn format(&self, row: usize) -> String {
        match self {
            Self::Arrow(formatter) => formatter.value(row).to_string(),
            Self::Binary(array) => match binary_value(*array, row) {
                Some(bytes) => crate::media::describe(bytes),
                None => "NULL".to_string(),
            },
        }
    }
}

/// Short, human-friendly name for an Arrow type.
pub fn type_label(data_type: &DataType) -> String {
    match data_type {
        DataType::Boolean => "bool".into(),
        DataType::Int8 => "int8".into(),
        DataType::Int16 => "int16".into(),
        DataType::Int32 => "int32".into(),
        DataType::Int64 => "int64".into(),
        DataType::UInt8 => "uint8".into(),
        DataType::UInt16 => "uint16".into(),
        DataType::UInt32 => "uint32".into(),
        DataType::UInt64 => "uint64".into(),
        DataType::Float16 => "float16".into(),
        DataType::Float32 => "float32".into(),
        DataType::Float64 => "float64".into(),
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "text".into(),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView => "blob".into(),
        DataType::FixedSizeBinary(n) => format!("blob[{n}]"),
        DataType::Date32 | DataType::Date64 => "date".into(),
        DataType::Timestamp(_, Some(_)) => "timestamptz".into(),
        DataType::Timestamp(_, None) => "timestamp".into(),
        DataType::Time32(_) | DataType::Time64(_) => "time".into(),
        DataType::Duration(_) => "duration".into(),
        DataType::Interval(_) => "interval".into(),
        DataType::Decimal128(p, s) | DataType::Decimal256(p, s) => format!("decimal({p},{s})"),
        DataType::FixedSizeList(item, n) => format!("{}[{n}]", type_label(item.data_type())),
        DataType::List(item) | DataType::LargeList(item) | DataType::ListView(item) => {
            format!("{}[]", type_label(item.data_type()))
        }
        DataType::Struct(fields) => format!("struct({})", fields.len()),
        DataType::Map(..) => "map".into(),
        DataType::Dictionary(_, value) => type_label(value),
        DataType::Null => "null".into(),
        other => other.to_string().to_lowercase(),
    }
}

/// Whether values of this type should be right-aligned (numbers).
pub fn is_numeric(data_type: &DataType) -> bool {
    data_type.is_numeric()
}

/// A result set prepared for display.
#[derive(Debug, Clone)]
pub struct ResultTable {
    pub result: Arc<ResultSet>,
    pub columns: Vec<ColumnMeta>,
    /// Display order → source row. `None` = natural order.
    pub order: Option<Arc<UInt32Array>>,
    pub sort: Option<(usize, bool)>,
}

/// Per-column display metadata.
#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    pub type_label: String,
    pub data_type: DataType,
    /// Suggested width in pixels.
    pub width: f32,
    pub numeric: bool,
}

/// Approximate advance of one monospace glyph at the grid font size
/// (13px × 0.6em, matching the grid's truncation).
pub const CHAR_WIDTH: f32 = 7.8;
/// Minimum / maximum automatic column width.
pub const MIN_COLUMN_WIDTH: f32 = 72.0;
pub const MAX_COLUMN_WIDTH: f32 = 340.0;

impl ResultTable {
    pub fn new(result: Arc<ResultSet>) -> Self {
        let batch = &result.batch;
        let sample = batch.num_rows().min(200);
        let columns = batch
            .schema()
            .fields()
            .iter()
            .zip(batch.columns())
            .map(|(field, column)| {
                let type_label = type_label(field.data_type());
                let header_chars = field.name().chars().count().max(type_label.len() + 3);
                let widest_value = CellFormatter::new(column.as_ref())
                    .map(|formatter| {
                        (0..sample)
                            .map(|row| formatter.format(row).chars().count())
                            .max()
                            .unwrap_or(0)
                    })
                    .unwrap_or(8);
                let chars = header_chars.max(widest_value) as f32;
                ColumnMeta {
                    name: field.name().clone(),
                    type_label,
                    data_type: field.data_type().clone(),
                    width: (chars * CHAR_WIDTH + 24.0).clamp(MIN_COLUMN_WIDTH, MAX_COLUMN_WIDTH),
                    numeric: is_numeric(field.data_type()),
                }
            })
            .collect();
        Self {
            result,
            columns,
            order: None,
            sort: None,
        }
    }

    pub fn batch(&self) -> &RecordBatch {
        &self.result.batch
    }

    pub fn row_count(&self) -> usize {
        self.result.batch.num_rows()
    }

    pub fn column(&self, index: usize) -> &ArrayRef {
        self.result.batch.column(index)
    }

    /// Maps a displayed row index to the row in the underlying batch.
    pub fn source_row(&self, display_row: usize) -> usize {
        match &self.order {
            Some(order) => order.value(display_row) as usize,
            None => display_row,
        }
    }

    /// Cycles sorting on `column`: ascending → descending → natural order.
    pub fn toggle_sort(&mut self, column: usize) {
        let next = match self.sort {
            Some((current, true)) if current == column => Some((column, false)),
            Some((current, false)) if current == column => None,
            _ => Some((column, true)),
        };
        self.apply_sort(next);
    }

    fn apply_sort(&mut self, sort: Option<(usize, bool)>) {
        self.sort = None;
        self.order = None;
        let Some((column, ascending)) = sort else {
            return;
        };
        let options = SortOptions {
            descending: !ascending,
            nulls_first: false,
        };
        // Some types (e.g. vectors) are not orderable; leave them unsorted.
        if let Ok(indices) = sort_to_indices(self.column(column).as_ref(), Some(options), None) {
            self.order = Some(Arc::new(indices));
            self.sort = Some((column, ascending));
        }
    }

    /// Formats a displayed cell.
    pub fn cell_text(&self, display_row: usize, column: usize) -> String {
        self.cell_text_at_source(self.source_row(display_row), column)
    }

    /// Formats a cell addressed by its row in the underlying batch.
    pub fn cell_text_at_source(&self, source_row: usize, column: usize) -> String {
        format_value(self.column(column).as_ref(), source_row)
    }

    /// Writes the (sorted) table as RFC 4180 CSV. Binary values are written
    /// as hex so the export stays lossless.
    pub fn write_csv(&self, mut out: impl Write) -> Result<()> {
        let header: Vec<String> = self.columns.iter().map(|c| csv_field(&c.name)).collect();
        writeln!(out, "{}", header.join(","))?;

        let options = format_options().with_null("");
        let formatters = self
            .result
            .batch
            .columns()
            .iter()
            .map(|column| ArrayFormatter::try_new(column.as_ref(), &options))
            .collect::<Result<Vec<_>, _>>()?;
        for display_row in 0..self.row_count() {
            let row = self.source_row(display_row);
            let fields: Vec<String> = formatters
                .iter()
                .map(|formatter| csv_field(&formatter.value(row).to_string()))
                .collect();
            writeln!(out, "{}", fields.join(","))?;
        }
        Ok(())
    }

    /// Saves the table as CSV at `path`.
    pub fn save_csv(&self, path: &Path) -> Result<()> {
        let file = std::fs::File::create(path)?;
        let mut writer = std::io::BufWriter::new(file);
        self.write_csv(&mut writer)?;
        writer.flush()?;
        Ok(())
    }
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// Formats a count with thousands separators: `12345` → `12,345`.
pub fn thousands(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Formats a byte count: `1536` → `1.5 KB`.
pub fn human_bytes(bytes: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Formats a duration compactly: `850µs`, `12.3ms`, `1.24s`.
pub fn human_duration(duration: std::time::Duration) -> String {
    let micros = duration.as_micros();
    if micros < 1_000 {
        format!("{micros}µs")
    } else if micros < 1_000_000 {
        format!("{:.1}ms", micros as f64 / 1_000.0)
    } else {
        format!("{:.2}s", duration.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lancedb::arrow::arrow_array::{Int32Array, StringArray};
    use lancedb::arrow::arrow_schema::{Field, Schema};

    fn table() -> ResultTable {
        let schema = Schema::new(vec![
            Field::new("n", DataType::Int32, true),
            Field::new("s", DataType::Utf8, true),
        ]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(Int32Array::from(vec![Some(3), None, Some(1)])),
                Arc::new(StringArray::from(vec!["c", "a,b", "say \"hi\""])),
            ],
        )
        .unwrap();
        ResultTable::new(Arc::new(ResultSet {
            batch,
            truncated: false,
        }))
    }

    #[test]
    fn sort_cycles_and_keeps_nulls_last() {
        let mut t = table();
        t.toggle_sort(0);
        assert_eq!(t.sort, Some((0, true)));
        assert_eq!(
            (0..3).map(|r| t.cell_text(r, 0)).collect::<Vec<_>>(),
            ["1", "3", "NULL"]
        );
        t.toggle_sort(0);
        assert_eq!(t.cell_text(0, 0), "3");
        t.toggle_sort(0);
        assert_eq!(t.sort, None);
        assert_eq!(t.cell_text(0, 0), "3");
    }

    #[test]
    fn csv_quotes_special_characters() {
        let mut out = Vec::new();
        table().write_csv(&mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "n,s\n3,c\n,\"a,b\"\n1,\"say \"\"hi\"\"\"\n"
        );
    }

    #[test]
    fn human_formatting() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(1234567), "1,234,567");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(
            human_duration(std::time::Duration::from_micros(1500)),
            "1.5ms"
        );
    }

    #[test]
    fn type_labels_are_short() {
        let item = Arc::new(Field::new("item", DataType::Float32, true));
        assert_eq!(type_label(&DataType::FixedSizeList(item, 8)), "float32[8]");
        assert_eq!(type_label(&DataType::Utf8View), "text");
    }
}

#[cfg(test)]
mod binary_tests {
    use super::*;
    use lancedb::arrow::arrow_array::{BinaryArray, LargeBinaryArray};

    #[test]
    fn binary_cells_are_described() {
        let array = BinaryArray::from(vec![Some(&b"%PDF-1.4"[..]), None, Some(&[1u8, 2][..])]);
        assert!(format_value(&array, 0).starts_with("PDF document"));
        assert_eq!(format_value(&array, 1), "NULL");
        assert_eq!(format_value(&array, 2), "0x0102 · 2 B");
        let large = LargeBinaryArray::from(vec![Some(&b"abc"[..])]);
        assert_eq!(binary_value(&large, 0), Some(&b"abc"[..]));
        assert_eq!(binary_value(&large, 5), None);
    }
}
