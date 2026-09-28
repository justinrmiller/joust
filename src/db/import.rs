//! Importing a folder of media files (images, audio, video, PDFs) as a Lance
//! table, so they can be queried, browsed and searched like any other data.
//!
//! Each file becomes a row with its path, format, size, timestamps and bytes.
//! Images and videos also get their dimensions, a small PNG `thumbnail`
//! (cheap to browse; a poster frame for videos, which needs ffmpeg) and a
//! `color_vector` for `vector_search` by visual similarity. Videos and audio
//! get their duration and codec. Files over [`MAX_INLINE_BYTES`] are
//! imported by reference: everything but `data`, which stays NULL.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use lancedb::arrow::arrow_array::types::Float32Type;
use lancedb::arrow::arrow_array::{
    ArrayRef, FixedSizeListArray, Float64Array, Int32Array, Int64Array, LargeBinaryArray,
    RecordBatch, StringArray, TimestampMicrosecondArray,
};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, TimeUnit};

use crate::media::{self, COLOR_DIM, MediaKind};

/// Larger files are imported by reference (`data` is NULL, `path` remains).
pub const MAX_INLINE_BYTES: u64 = 256 * 1024 * 1024;
/// At most this many files are imported.
pub const MAX_FILES: usize = 5_000;
/// Stop copying originals once they add up to this much (later files are
/// imported by reference).
pub const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Longest side of stored thumbnails.
pub const THUMBNAIL_SIDE: u32 = 256;

/// Outcome of an import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    pub table: String,
    pub imported: usize,
    /// Media files left out (unreadable, or over [`MAX_FILES`]).
    pub skipped: usize,
    /// Files imported by reference because of their size.
    pub referenced: usize,
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
    duration: Option<f64>,
    codec: Option<String>,
    modified: Option<i64>,
    thumbnail: Option<Vec<u8>>,
    color: Option<[f32; COLOR_DIM]>,
    data: Option<Vec<u8>>,
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
    use std::io::Read;

    let metadata = std::fs::metadata(path).ok()?;
    let mut head = Vec::with_capacity(64);
    std::fs::File::open(path)
        .ok()?
        .take(64)
        .read_to_end(&mut head)
        .ok()?;
    let format = media::sniff(&head).or_else(|| media::from_extension(path))?;
    let data = if metadata.len() <= MAX_INLINE_BYTES {
        Some(std::fs::read(path).ok()?)
    } else {
        None
    };
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .and_then(|elapsed| i64::try_from(elapsed.as_micros()).ok());

    let av_info = matches!(format.kind, MediaKind::Video | MediaKind::Audio)
        .then(|| crate::av::info_from_file(path))
        .flatten();
    // The picture: the image itself or a video's poster frame.
    let picture = match format.kind {
        MediaKind::Image => match &data {
            Some(bytes) => media::decode(bytes),
            None => image::ImageReader::open(path)
                .ok()
                .and_then(|reader| reader.with_guessed_format().ok())
                .and_then(|reader| reader.decode().ok()),
        },
        MediaKind::Video if crate::ffmpeg::available() => {
            let time = crate::ffmpeg::poster_time(av_info.as_ref().and_then(|i| i.duration));
            crate::ffmpeg::frame_at(path, time, THUMBNAIL_SIDE)
        }
        _ => None,
    };
    let (width, height) = match (&picture, &av_info) {
        (Some(image), _) if format.kind == MediaKind::Image => {
            (Some(image.width()), Some(image.height()))
        }
        (_, Some(info)) => (info.width, info.height),
        _ => (None, None),
    };

    Some(Row {
        path: path.to_string_lossy().into_owned(),
        file_name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        kind: format.kind.label(),
        mime: format.mime,
        size: metadata.len() as i64,
        width: width.and_then(|w| i32::try_from(w).ok()),
        height: height.and_then(|h| i32::try_from(h).ok()),
        duration: av_info.as_ref().and_then(|info| info.duration),
        codec: av_info
            .as_ref()
            .and_then(|info| info.codec().map(str::to_string)),
        modified,
        thumbnail: picture
            .as_ref()
            .and_then(|image| media::thumbnail_png(image, THUMBNAIL_SIDE)),
        color: picture.as_ref().map(media::color_vector),
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
        Field::new("duration_s", DataType::Float64, true),
        Field::new("codec", DataType::Utf8, true),
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
        Field::new("data", DataType::LargeBinary, true),
    ])
}

/// Scans `folder` and builds the table contents. Returns the batch, the number
/// of media files skipped and the number imported by reference.
pub fn build_media_batch(folder: &Path) -> Result<(RecordBatch, usize, usize)> {
    let paths = scan(folder)?;
    let mut skipped = paths.len().saturating_sub(MAX_FILES);
    let paths = &paths[..paths.len().min(MAX_FILES)];

    let mut rows = Vec::with_capacity(paths.len());
    let mut total = 0u64;
    let mut referenced = 0;
    for row in read_rows(paths) {
        let Some(mut row) = row else {
            skipped += 1;
            continue;
        };
        // Past the total budget, keep further files by reference.
        if let Some(data) = &row.data {
            if total + data.len() as u64 > MAX_TOTAL_BYTES {
                row.data = None;
            } else {
                total += data.len() as u64;
            }
        }
        if row.data.is_none() {
            referenced += 1;
        }
        rows.push(row);
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
        Arc::new(rows.iter().map(|r| r.duration).collect::<Float64Array>()),
        Arc::new(
            rows.iter()
                .map(|r| r.codec.as_deref())
                .collect::<StringArray>(),
        ),
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
        Arc::new(
            rows.iter()
                .map(|r| r.data.as_deref())
                .collect::<LargeBinaryArray>(),
        ),
    ];
    let batch = RecordBatch::try_new(Arc::new(media_schema()), columns)?;
    Ok((batch, skipped, referenced))
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

        let (batch, skipped, referenced) = build_media_batch(dir.path()).unwrap();
        assert_eq!((batch.num_rows(), skipped, referenced), (2, 0, 0));
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
            .column(10)
            .as_any()
            .downcast_ref::<LargeBinaryArray>()
            .unwrap();
        assert_eq!(media::image_dimensions(thumbs.value(1)), Some((256, 128)));
        assert!(batch.column(11).is_null(0));
    }
}
