//! Importing a folder of media files (images, audio, video, PDFs) as a Lance
//! table, so they can be queried, browsed and searched like any other data.
//!
//! Each file becomes a row with its path, format, size, timestamps and bytes.
//! Images also get their dimensions, a small PNG `thumbnail` (cheap to
//! browse) and a `color_vector` for `vector_search` by visual similarity.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use lancedb::arrow::arrow_array::types::Float32Type;
use lancedb::arrow::arrow_array::{
    ArrayRef, FixedSizeListArray, Int32Array, Int64Array, LargeBinaryArray, RecordBatch,
    StringArray, TimestampMicrosecondArray,
};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, TimeUnit};

use crate::media::{self, COLOR_DIM, MediaKind};

/// Files larger than this are skipped.
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// At most this many files are imported.
pub const MAX_FILES: usize = 5_000;
/// Stop once the originals add up to this much.
pub const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Longest side of stored thumbnails.
pub const THUMBNAIL_SIDE: u32 = 256;

/// Outcome of an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    pub table: String,
    pub imported: usize,
    /// Media files left out (too large, unreadable, or over the limits).
    pub skipped: usize,
}

/// One scanned file, ready to become a row.
struct Row {
    path: String,
    file_name: String,
    kind: &'static str,
    mime: &'static str,
    size: i64,
    width: Option<i32>,
    height: Option<i32>,
    modified: Option<i64>,
    thumbnail: Option<Vec<u8>>,
    color: Option<[f32; COLOR_DIM]>,
    data: Vec<u8>,
}

/// A table name derived from the folder name that doesn't clash with `existing`.
pub fn table_name_for(folder: &Path, existing: &[String]) -> String {
    let stem = folder
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("media");
    let mut base: String = stem
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    if base.is_empty() {
        base = "media".into();
    }
    if base.starts_with(|c: char| c.is_ascii_digit()) {
        base = format!("m_{base}");
    }
    if !existing.contains(&base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}_{n}"))
        .find(|name| !existing.contains(name))
        .expect("an unused suffix exists")
}

/// Media files under `folder` (recursive, hidden entries skipped), sorted.
fn scan(folder: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![folder.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries =
            std::fs::read_dir(&dir).with_context(|| format!("could not read {}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let hidden = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if hidden {
                continue;
            }
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(path),
                Ok(kind) if kind.is_file() && media::from_extension(&path).is_some() => {
                    found.push(path)
                }
                _ => {}
            }
        }
    }
    found.sort();
    Ok(found)
}

fn read_row(path: &Path) -> Option<Row> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    let format = media::sniff(&data).or_else(|| media::from_extension(path))?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|elapsed| i64::try_from(elapsed.as_micros()).ok());

    let decoded = (format.kind == MediaKind::Image)
        .then(|| media::decode(&data))
        .flatten();
    let (width, height) = match &decoded {
        Some(image) => (
            i32::try_from(image.width()).ok(),
            i32::try_from(image.height()).ok(),
        ),
        None => (None, None),
    };

    Some(Row {
        path: path.to_string_lossy().into_owned(),
        file_name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        kind: format.kind.label(),
        mime: format.mime,
        size: data.len() as i64,
        width,
        height,
        modified,
        thumbnail: decoded
            .as_ref()
            .and_then(|image| media::thumbnail_png(image, THUMBNAIL_SIDE)),
        color: decoded.as_ref().map(media::color_vector),
        data,
    })
}

/// Reads and decodes files on all cores, preserving order.
fn read_rows(paths: &[PathBuf]) -> Vec<Option<Row>> {
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get());
    let chunk = paths.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|chunk| scope.spawn(move || chunk.iter().map(|p| read_row(p)).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap_or_default())
            .collect()
    })
}

/// Schema of an imported media table.
pub fn media_schema() -> Schema {
    let item = Arc::new(Field::new("item", DataType::Float32, true));
    Schema::new(vec![
        Field::new("path", DataType::Utf8, false),
        Field::new("file_name", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, false),
        Field::new("mime", DataType::Utf8, false),
        Field::new("size_bytes", DataType::Int64, false),
        Field::new("width", DataType::Int32, true),
        Field::new("height", DataType::Int32, true),
        Field::new(
            "modified",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            true,
        ),
        Field::new("thumbnail", DataType::LargeBinary, true),
        Field::new(
            "color_vector",
            DataType::FixedSizeList(item, COLOR_DIM as i32),
            true,
        ),
        Field::new("data", DataType::LargeBinary, false),
    ])
}

/// Scans `folder` and builds the table contents. Returns the batch and the
/// number of media files that were skipped.
pub fn build_media_batch(folder: &Path) -> Result<(RecordBatch, usize)> {
    let paths = scan(folder)?;
    let mut skipped = paths.len().saturating_sub(MAX_FILES);
    let paths = &paths[..paths.len().min(MAX_FILES)];

    let mut rows = Vec::with_capacity(paths.len());
    let mut total = 0u64;
    for row in read_rows(paths) {
        match row {
            Some(row) if total + row.size as u64 <= MAX_TOTAL_BYTES => {
                total += row.size as u64;
                rows.push(row);
            }
            _ => skipped += 1,
        }
    }

    let columns: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from_iter_values(rows.iter().map(|r| &r.path))),
        Arc::new(StringArray::from_iter_values(
            rows.iter().map(|r| &r.file_name),
        )),
        Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.kind))),
        Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.mime))),
        Arc::new(Int64Array::from_iter_values(rows.iter().map(|r| r.size))),
        Arc::new(rows.iter().map(|r| r.width).collect::<Int32Array>()),
        Arc::new(rows.iter().map(|r| r.height).collect::<Int32Array>()),
        Arc::new(
            rows.iter()
                .map(|r| r.modified)
                .collect::<TimestampMicrosecondArray>()
                .with_timezone("UTC"),
        ),
        Arc::new(
            rows.iter()
                .map(|r| r.thumbnail.as_deref())
                .collect::<LargeBinaryArray>(),
        ),
        Arc::new(
            FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
                rows.iter().map(|r| {
                    r.color
                        .map(|c| c.iter().map(|v| Some(*v)).collect::<Vec<_>>())
                }),
                COLOR_DIM as i32,
            ),
        ),
        Arc::new(LargeBinaryArray::from_iter_values(
            rows.iter().map(|r| r.data.as_slice()),
        )),
    ];
    let batch = RecordBatch::try_new(Arc::new(media_schema()), columns)?;
    Ok((batch, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage};
    use lancedb::arrow::arrow_array::Array;

    #[test]
    fn table_names_are_sanitised_and_unique() {
        let existing = vec![
            "vacation_photos".to_string(),
            "vacation_photos_2".to_string(),
        ];
        assert_eq!(
            table_name_for(Path::new("/x/Vacation Photos!"), &existing),
            "vacation_photos_3"
        );
        assert_eq!(
            table_name_for(Path::new("/x/2024 trip"), &[]),
            "m_2024_trip"
        );
        assert_eq!(table_name_for(Path::new("/"), &[]), "media");
    }

    #[test]
    fn builds_rows_for_media_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let red = DynamicImage::ImageRgb8(RgbImage::from_pixel(300, 150, Rgb([200, 20, 20])));
        red.save(dir.path().join("red.png")).unwrap();
        std::fs::create_dir(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/paper.pdf"), b"%PDF-1.7 fake").unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"ignored").unwrap();
        std::fs::write(dir.path().join(".hidden.png"), b"ignored").unwrap();

        let (batch, skipped) = build_media_batch(dir.path()).unwrap();
        assert_eq!((batch.num_rows(), skipped), (2, 0));
        let kinds = batch
            .column(2)
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(kinds.value(0), "document");
        assert_eq!(kinds.value(1), "image");
        let widths = batch
            .column(5)
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert!(widths.is_null(0));
        assert_eq!(widths.value(1), 300);
        let thumbs = batch
            .column(8)
            .as_any()
            .downcast_ref::<LargeBinaryArray>()
            .unwrap();
        assert_eq!(media::image_dimensions(thumbs.value(1)), Some((256, 128)));
        assert!(batch.column(9).is_null(0));
    }
}
