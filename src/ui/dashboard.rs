//! The Dashboards screen: headline numbers, stacked bars and tables over a
//! LanceDB Enterprise server's system views, jobs and tables.

use iced::widget::{Space, button, canvas, center, column, container, row, scrollable, text};
use iced::{Alignment, Element, Fill, Font, Length, Theme};

use super::function::utc_minute;
use super::series_chart::{StackedBars, series_color};
use super::{BOLD, faint, muted, thin_scrollbar, with_tooltip};
use crate::app::{App, Message};
use crate::db::TableKind;
use crate::db::remote::MAX_HEALTH_TABLES;
use crate::enterprise::dashboard::{
    Bars, Range, TILES, Tile, TileContent, TileId, TileView, tile_error, tile_sql,
};
use crate::results::ResultTable;
use crate::theme;

/// Heights of a headline tile and of a chart or table tile.
const KPI_HEIGHT: f32 = 112.0;
const PANEL_HEIGHT: f32 = 320.0;
/// Rows a table tile shows.
const TABLE_ROWS: usize = 20;

pub fn view(app: &App) -> Element<'_, Message> {
    let dashboard = &app.dashboard;
    let mut ranges = row![].spacing(2);
    for range in Range::ALL {
        ranges = ranges.push(
            button(text(range.to_string()).size(12))
                .padding([4, 10])
                .on_press(Message::SetDashboardRange(range))
                .style(theme::list_item(dashboard.range == range)),
        );
    }
    let updated = dashboard.loaded_at.map_or_else(
        || "Loading…".to_string(),
        |at| format!("Updated {} · refreshes every minute", utc_minute(at)),
    );
    let header = row![
        text("Dashboards").size(18).font(BOLD),
        Space::new().width(8),
        ranges,
        Space::new().width(Fill),
        faint(updated).size(11),
        button(text("Refresh").size(12))
            .padding([5, 12])
            .on_press(Message::RefreshDashboard)
            .style(theme::secondary_button),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let dark = app.theme_id().is_dark();
    let (kpis, panels): (Vec<&Tile>, Vec<&Tile>) =
        TILES.iter().partition(|tile| tile.view == TileView::Kpi);
    let mut kpi_row = row![].spacing(12);
    for tile in kpis {
        kpi_row = kpi_row.push(tile_card(app, tile, KPI_HEIGHT, dark));
    }
    let mut grid = column![header, kpi_row].spacing(12);
    for pair in panels.chunks(2) {
        let mut line = row![].spacing(12);
        for tile in pair {
            line = line.push(tile_card(app, tile, PANEL_HEIGHT, dark));
        }
        grid = grid.push(line);
    }

    // Tiles show their own errors; workspace errors stay on the SQL screen.
    scrollable(container(grid).padding(16).width(Fill))
        .direction(thin_scrollbar())
        .height(Fill)
        .into()
}

/// A tile: its title, what it shows, and where the data came from.
fn tile_card<'a>(app: &'a App, tile: &'a Tile, height: f32, dark: bool) -> Element<'a, Message> {
    let mut head = row![
        text(tile.title).size(13).font(BOLD),
        Space::new().width(Fill)
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let open: Option<(Message, &str)> = if tile_sql(tile.id, Range::default(), 0, "").is_some() {
        Some((
            Message::OpenTileQuery(tile.id),
            "Runs this tile's SQL in the editor, to see every row or change the query",
        ))
    } else if tile.id == TileId::TableHealth {
        // No single query covers every table: open the first one listed
        // (the one most in need of attention).
        match app.dashboard.tiles.get(&tile.id) {
            Some(Ok(TileContent::Table(table))) if table.row_count() > 0 => Some((
                Message::OpenTableStats(table.cell_text(0, 0)),
                "Runs SHOW STATS for the first table listed; click a row for another table",
            )),
            _ => None,
        }
    } else {
        None
    };
    if let Some((message, tip)) = open {
        head = head.push(with_tooltip(
            button(text("Open in editor").size(11))
                .padding([2, 8])
                .on_press(message)
                .style(theme::ghost_button),
            tip,
        ));
    }

    let body: Element<'a, Message> = match app.dashboard.tiles.get(&tile.id) {
        None => center(faint("Loading…").size(12)).into(),
        // The server's message; the whole error on hover.
        Some(Err(error)) => with_tooltip(
            container({
                let message = text(tile_error(error))
                    .size(11)
                    .font(Font::MONOSPACE)
                    .wrapping(text::Wrapping::WordOrGlyph);
                // Headline tiles have room for the message alone.
                if tile.view == TileView::Kpi {
                    column![message]
                } else {
                    column![
                        text("The server could not run this tile's query").size(12),
                        message
                    ]
                    .spacing(2)
                }
            })
            .padding([6, 10])
            .width(Fill)
            .style(theme::error_banner),
            error,
        ),
        Some(Ok(TileContent::Empty)) => center(muted(tile.empty).size(12)).padding([0, 12]).into(),
        Some(Ok(TileContent::Kpi { value, note })) => column![
            text(value.as_str()).size(26).font(BOLD),
            muted(note.as_str()).size(12),
        ]
        .spacing(2)
        .into(),
        Some(Ok(TileContent::Bars(bars))) => column![
            legend(bars, dark),
            canvas(StackedBars {
                bars,
                theme: app.theme_id(),
            })
            .width(Fill)
            .height(Fill),
        ]
        .spacing(8)
        .into(),
        Some(Ok(TileContent::Table(table))) if tile.id == TileId::TableHealth => {
            let tables = app
                .catalog
                .iter()
                .filter(|table| table.kind != TableKind::View)
                .count();
            let mut note = "Click a table to run SHOW STATS for it.".to_string();
            if tables > MAX_HEALTH_TABLES {
                note = format!("Checked the first {MAX_HEALTH_TABLES} of {tables} tables. {note}");
            }
            small_table(table, Rows::Tables(note))
        }
        Some(Ok(TileContent::Table(table))) => small_table(table, Rows::First(TABLE_ROWS)),
    };

    container(
        column![
            head,
            container(body).width(Fill).height(Fill),
            faint(tile.source).size(10).font(Font::MONOSPACE),
        ]
        .spacing(8),
    )
    .padding(12)
    .width(Fill)
    .height(height)
    .style(theme::card)
    .into()
}

/// Every series with its colour and its total over the range, so identity
/// never rests on colour alone.
fn legend(bars: &Bars, dark: bool) -> Element<'_, Message> {
    let mut items = row![].spacing(14);
    for (index, name) in bars.series.iter().enumerate() {
        let color = series_color(index, dark);
        let total: f64 = bars.values[index].iter().sum();
        items = items.push(
            row![
                container(Space::new().width(10).height(10)).style(move |_: &Theme| {
                    container::Style {
                        background: Some(color.into()),
                        border: iced::Border {
                            radius: 2.0.into(),
                            ..iced::Border::default()
                        },
                        ..container::Style::default()
                    }
                }),
                text(name.as_str()).size(11),
                faint(format!("{} {}", super::chart::tick_label(total), bars.unit)).size(11),
            ]
            .spacing(5)
            .align_y(Alignment::Center),
        );
    }
    items.wrap().vertical_spacing(4).into()
}

/// Which rows a table tile shows.
enum Rows {
    /// The first n; the rest are a query away (Open in editor).
    First(usize),
    /// All of them, each opening its table's statistics (table health),
    /// with a note under them.
    Tables(String),
}

/// A table tile's rows, in columns sized by their content.
fn small_table(table: &ResultTable, rows_shown: Rows) -> Element<'_, Message> {
    // Each column's share of the width follows the longest text it shows.
    let shown = match &rows_shown {
        Rows::First(limit) => table.row_count().min(*limit),
        Rows::Tables(_) => table.row_count(),
    };
    let portions: Vec<u16> = table
        .columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            (0..shown)
                .map(|row| table.cell_text(row, index).chars().count())
                .chain([column.name.chars().count()])
                .max()
                .unwrap_or(1)
                // Long names (tables, paths) share the width rather than
                // take it; every column gets room around its text.
                .clamp(4, 24) as u16
                + 3
        })
        .collect();
    let cell = |content: String, portion: u16, numeric: bool, header: bool| {
        let label = if header {
            muted(content).size(11).font(BOLD)
        } else {
            text(content).size(12)
        };
        container(label.wrapping(text::Wrapping::None))
            .width(Length::FillPortion(portion))
            .align_x(if numeric {
                iced::alignment::Horizontal::Right
            } else {
                iced::alignment::Horizontal::Left
            })
            .clip(true)
    };

    let mut head = row![].spacing(10);
    for (column, portion) in table.columns.iter().zip(&portions) {
        head = head.push(cell(column.name.clone(), *portion, column.numeric, true));
    }
    let mut rows = column![].spacing(4);
    for row_index in 0..shown {
        let mut line = row![].spacing(10);
        for (index, (column, portion)) in table.columns.iter().zip(&portions).enumerate() {
            line = line.push(cell(
                table.cell_text(row_index, index),
                *portion,
                column.numeric,
                false,
            ));
        }
        rows = match &rows_shown {
            Rows::First(_) => rows.push(line),
            Rows::Tables(_) => rows.push(
                button(line)
                    .width(Fill)
                    .padding([1, 0])
                    .on_press(Message::OpenTableStats(table.cell_text(row_index, 0)))
                    .style(theme::list_item(false)),
            ),
        };
    }
    match &rows_shown {
        Rows::First(_) if table.row_count() > shown => {
            rows = rows.push(
                faint(format!(
                    "{} more rows. Open in editor to see them all.",
                    table.row_count() - shown
                ))
                .size(11),
            );
        }
        Rows::Tables(note) => rows = rows.push(faint(note.clone()).size(11)),
        Rows::First(_) => {}
    }
    column![
        container(head).padding(iced::Padding::ZERO.right(12)),
        super::hairline(),
        scrollable(rows.padding(iced::Padding::ZERO.right(12)))
            .direction(thin_scrollbar())
            .height(Fill),
    ]
    .spacing(6)
    .into()
}
