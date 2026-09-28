//! "Media" tab: a thumbnail gallery for result columns holding images, either
//! as bytes (binary columns) or as paths to local image files.

use std::collections::HashMap;

use iced::widget::{
    Space, button, center, column, container, image, pick_list, row, scrollable, text,
};
use iced::{Alignment, ContentFit, Element, Fill, Font, Theme};
use lancedb::arrow::arrow_array::Array;
use lancedb::arrow::arrow_schema::DataType;

use crate::app::{App, Message};
use crate::media::{self, MediaKind};
use crate::results::{ResultTable, binary_value, is_binary, thousands};
use crate::theme;
use crate::ui::chart::ColumnChoice;
use crate::ui::grid::truncate;

/// Thumbnails are built for at most this many rows.
pub const GALLERY_LIMIT: usize = 400;
/// Longest side of gallery thumbnails.
pub const THUMB_SIDE: u32 = 160;
/// Longest side of the inspector preview.
pub const PREVIEW_SIDE: u32 = 360;
const CARD_SIZE: f32 = 150.0;
/// Files read for path columns are capped at this size.
const MAX_PATH_BYTES: u64 = 64 * 1024 * 1024;

/// Where a media column's content lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Bytes,
    Path,
}

/// A result column that holds media.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaColumn {
    pub index: usize,
    pub name: String,
    pub source: Source,
    /// Whether (sampled) values include images, i.e. can be previewed.
    pub images: bool,
}

/// Finds media columns by sampling their first non-null values.
pub fn media_columns(table: &ResultTable) -> Vec<MediaColumn> {
    const SAMPLE: usize = 24;
    let mut found = Vec::new();
    for (index, meta) in table.columns.iter().enumerate() {
        let array = table.column(index);
        let values = (0..array.len())
            .filter(|&row| array.is_valid(row))
            .take(SAMPLE);
        if is_binary(&meta.data_type) {
            let formats: Vec<_> = values
                .filter_map(|row| binary_value(array.as_ref(), row).and_then(media::sniff))
                .collect();
            if !formats.is_empty() {
                found.push(MediaColumn {
                    index,
                    name: meta.name.clone(),
                    source: Source::Bytes,
                    images: formats.iter().any(|f| f.kind == MediaKind::Image),
                });
            }
        } else if matches!(
            meta.data_type,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
        ) {
            let sampled: Vec<usize> = values.collect();
            let images = sampled
                .iter()
                .filter(|&&row| media::image_path(&table.cell_text_at_source(row, index)).is_some())
                .count();
            if !sampled.is_empty() && images * 2 >= sampled.len() {
                found.push(MediaColumn {
                    index,
                    name: meta.name.clone(),
                    source: Source::Path,
                    images: true,
                });
            }
        }
    }
    found
}

/// Raw bytes of a media cell (reading the file for path columns).
pub fn cell_bytes(table: &ResultTable, column: &MediaColumn, source_row: usize) -> Option<Vec<u8>> {
    match column.source {
        Source::Bytes => {
            binary_value(table.column(column.index).as_ref(), source_row).map(<[u8]>::to_vec)
        }
        Source::Path => {
            let path = media::image_path(&table.cell_text_at_source(source_row, column.index))?;
            if std::fs::metadata(&path).ok()?.len() > MAX_PATH_BYTES {
                return None;
            }
            std::fs::read(path).ok()
        }
    }
}

/// Decodes `bytes` into an RGBA image handle no larger than `max_side`.
pub fn handle_for(bytes: &[u8], max_side: u32) -> Option<image::Handle> {
    let thumb = media::thumbnail(bytes, max_side)?;
    let (w, h) = thumb.dimensions();
    Some(image::Handle::from_rgba(w, h, thumb.into_raw()))
}

/// One gallery tile.
#[derive(Debug, Clone)]
pub struct Thumb {
    pub handle: image::Handle,
    /// e.g. `PNG 120×180 · 8.1 KB`
    pub label: String,
}

/// Thumbnails for one media column, keyed by source row.
#[derive(Debug, Clone)]
pub struct Gallery {
    pub column: usize,
    pub thumbs: HashMap<usize, Thumb>,
    /// Column used for captions.
    pub caption: Option<usize>,
    /// True when the result had more rows than [`GALLERY_LIMIT`].
    pub limited: bool,
}

/// Builds thumbnails (blocking; run off the UI thread).
pub fn build(table: &ResultTable, column: &MediaColumn) -> Gallery {
    let rows = table.row_count().min(GALLERY_LIMIT);
    let thumbs = (0..rows)
        .filter_map(|row| {
            let bytes = cell_bytes(table, column, row)?;
            let handle = handle_for(&bytes, THUMB_SIDE)?;
            Some((
                row,
                Thumb {
                    handle,
                    label: media::describe(&bytes),
                },
            ))
        })
        .collect();
    Gallery {
        column: column.index,
        thumbs,
        caption: caption_column(table, column.index),
        limited: table.row_count() > GALLERY_LIMIT,
    }
}

/// A text column that makes a good caption (title, name, …).
fn caption_column(table: &ResultTable, media_column: usize) -> Option<usize> {
    let text_columns: Vec<usize> = (0..table.columns.len())
        .filter(|&i| {
            i != media_column
                && matches!(
                    table.columns[i].data_type,
                    DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
                )
        })
        .collect();
    const PREFERRED: [&str; 6] = ["title", "name", "file_name", "caption", "label", "path"];
    PREFERRED
        .iter()
        .find_map(|name| {
            text_columns
                .iter()
                .copied()
                .find(|&i| table.columns[i].name.eq_ignore_ascii_case(name))
        })
        .or_else(|| text_columns.first().copied())
}

/// The Media tab.
pub fn view(app: &App) -> Element<'_, Message> {
    let (Some(table), Some(gallery)) = (&app.table, &app.gallery) else {
        return center(
            text("Building thumbnails…")
                .size(14)
                .style(|theme: &Theme| text::Style {
                    color: Some(theme::muted(theme)),
                }),
        )
        .into();
    };

    let image_columns: Vec<ColumnChoice> = app
        .media_columns
        .iter()
        .filter(|c| c.images)
        .map(|c| ColumnChoice {
            index: c.index,
            name: c.name.clone(),
        })
        .collect();
    let selected_choice = image_columns
        .iter()
        .find(|c| c.index == gallery.column)
        .cloned();
    let note = format!(
        "{} image{}{}",
        thousands(gallery.thumbs.len()),
        if gallery.thumbs.len() == 1 { "" } else { "s" },
        if gallery.limited {
            format!(
                " · thumbnails for the first {} rows",
                thousands(GALLERY_LIMIT)
            )
        } else {
            String::new()
        }
    );
    let toolbar = container(
        row![
            text("Column").size(12).style(muted),
            pick_list(image_columns, selected_choice, Message::SetGalleryColumn)
                .text_size(12)
                .padding([4, 8])
                .style(theme::picker),
            Space::new().width(Fill),
            text(note).size(12).style(muted),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([6, 12])
    .width(Fill);

    let selected_row = app
        .selected
        .filter(|(_, column)| *column == gallery.column)
        .map(|(row, _)| row);
    let cards = (0..table.row_count())
        .filter_map(|display_row| {
            let source = table.source_row(display_row);
            gallery
                .thumbs
                .get(&source)
                .map(|thumb| (display_row, source, thumb))
        })
        .take(GALLERY_LIMIT)
        .map(|(display_row, source, thumb)| {
            let caption = match gallery.caption {
                Some(column) => table.cell_text_at_source(source, column),
                None => format!("row {}", thousands(display_row + 1)),
            };
            let card = column![
                container(
                    image(thumb.handle.clone())
                        .content_fit(ContentFit::Contain)
                        .width(CARD_SIZE)
                        .height(CARD_SIZE),
                )
                .center_x(CARD_SIZE)
                .center_y(CARD_SIZE)
                .style(theme::badge),
                text(truncate(&caption, 22)).size(12),
                text(truncate(&thumb.label, 26))
                    .size(10)
                    .font(Font::MONOSPACE)
                    .style(muted),
            ]
            .spacing(4)
            .width(CARD_SIZE);
            button(card)
                .padding(6)
                .on_press(Message::SelectCell(display_row, gallery.column))
                .style(theme::list_item(selected_row == Some(display_row)))
                .into()
        });

    let grid = scrollable(
        container(row(cards).spacing(8).wrap().vertical_spacing(8))
            .padding(12)
            .width(Fill),
    )
    .direction(crate::ui::thin_scrollbar())
    .height(Fill);

    let mut layout = column![toolbar, crate::ui::hairline(), grid];
    if selected_row.is_some()
        && let Some(inspector) = crate::ui::inspector(app)
    {
        layout = layout.push(crate::ui::hairline()).push(inspector);
    }
    layout.into()
}

fn muted(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::muted(theme)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use lancedb::arrow::arrow_array::{BinaryArray, RecordBatch, StringArray};
    use lancedb::arrow::arrow_schema::{Field, Schema};

    use crate::db::ResultSet;

    fn png() -> Vec<u8> {
        media::encode_png(&::image::DynamicImage::new_rgb8(30, 20)).unwrap()
    }

    #[test]
    fn detects_media_columns_and_builds_thumbnails() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.png");
        std::fs::write(&file, png()).unwrap();
        let bytes = png();
        let schema = Schema::new(vec![
            Field::new("title", DataType::Utf8, true),
            Field::new("img", DataType::Binary, true),
            Field::new("path", DataType::Utf8, true),
            Field::new("raw", DataType::Binary, true),
        ]);
        let batch = RecordBatch::try_new(
            Arc::new(schema),
            vec![
                Arc::new(StringArray::from(vec!["first", "second"])),
                Arc::new(BinaryArray::from(vec![Some(bytes.as_slice()), None])),
                Arc::new(StringArray::from(vec![
                    file.to_str().unwrap(),
                    file.to_str().unwrap(),
                ])),
                Arc::new(BinaryArray::from(vec![Some(&[1u8, 2, 3][..]), None])),
            ],
        )
        .unwrap();
        let table = ResultTable::new(Arc::new(ResultSet {
            batch,
            truncated: false,
        }));

        let columns = media_columns(&table);
        assert_eq!(
            columns
                .iter()
                .map(|c| (c.name.as_str(), c.source))
                .collect::<Vec<_>>(),
            [("img", Source::Bytes), ("path", Source::Path)]
        );

        let gallery = build(&table, &columns[0]);
        assert_eq!(gallery.thumbs.len(), 1);
        assert_eq!(gallery.caption, Some(0));
        assert!(gallery.thumbs[&0].label.starts_with("PNG 30×20"));

        let from_paths = build(&table, &columns[1]);
        assert_eq!(from_paths.thumbs.len(), 2);
    }
}
