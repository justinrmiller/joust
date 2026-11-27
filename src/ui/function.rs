//! The Function tab: one version of a LanceDB Enterprise Function.

use iced::widget::text::Span;
use iced::widget::{
    Space, button, center, column, container, rich_text, row, scrollable, span, text,
};
use iced::{Alignment, Element, Fill, Font, Length};

use super::{BOLD, faint, muted, section_label, thin_scrollbar, with_tooltip};
use crate::app::{App, FunctionView, Message};
use crate::db::remote::{FunctionDetail, FunctionInfo};
use crate::highlight::{color_of, python};
use crate::results::thousands;
use crate::theme;
use crate::theme::SyntaxColors;

/// Width of the versions and columns panel.
const SIDE_WIDTH: f32 = 340.0;
/// Characters of an image digest shown before eliding.
const DIGEST_CHARS: usize = 19;

pub fn view<'a>(app: &'a App, view: &'a FunctionView) -> Element<'a, Message> {
    let versions: Vec<&FunctionInfo> = app
        .functions
        .iter()
        .filter(|function| function.name == view.name)
        .collect();
    let info = versions
        .iter()
        .find(|function| function.version == view.version);
    let detail = match &view.detail {
        Some(Ok(detail)) => Some(detail),
        _ => None,
    };

    let mut header = row![
        text(view.name.as_str()).size(18).font(BOLD),
        container(text(format!("v{}", view.version)).size(11))
            .padding([1, 6])
            .style(theme::accent_badge),
    ]
    .spacing(10)
    .align_y(Alignment::Center);
    if detail.is_some_and(|detail| detail.disabled) {
        header = header.push(
            container(text("disabled").size(11))
                .padding([1, 6])
                .style(theme::badge),
        );
    }
    if let Some(created) = info.and_then(|info| info.created_at_millis) {
        header = header.push(muted(format!("created {}", utc_minute(created))).size(12));
    }
    let header = header
        .push(Space::new().width(Fill))
        .push(with_tooltip(
            button(text("New version…").size(12))
                .padding([5, 10])
                .on_press_maybe(detail.map(|_| Message::EditFunctionTemplate))
                .style(theme::primary_button),
            "Opens CREATE OR REPLACE FUNCTION with this source in the editor",
        ))
        .push(with_tooltip(
            button(text("Drop…").size(12))
                .padding([5, 10])
                .on_press(Message::DropFunctionTemplate)
                .style(theme::secondary_button),
            "Opens DROP FUNCTION for this version in the editor (it does not run)",
        ))
        .push(
            button(text("Close").size(12))
                .padding([5, 10])
                .on_press(Message::CloseFunction)
                .style(theme::secondary_button),
        );

    let body: Element<'a, Message> = match &view.detail {
        None => center(muted("Loading the Function…").size(13)).into(),
        Some(Err(error)) => container(text(error.as_str()).size(12).font(Font::MONOSPACE))
            .padding([8, 12])
            .width(Fill)
            .style(theme::error_banner)
            .into(),
        Some(Ok(detail)) => row![
            details(detail, app.theme_id().syntax()),
            container(side_panel(app, view, &versions)).width(SIDE_WIDTH),
        ]
        .spacing(16)
        .into(),
    };

    container(column![header, body].spacing(14))
        .padding(16)
        .width(Fill)
        .height(Fill)
        .into()
}

/// Runtime, packages, signature, image and source.
fn details(detail: &FunctionDetail, colors: SyntaxColors) -> Element<'_, Message> {
    let runtime = match (&detail.python_version, &detail.environment) {
        (Some(python), Some(environment)) => format!("Python {python} · {environment}"),
        (Some(python), None) => format!("Python {python}"),
        _ => "unknown".to_string(),
    };
    let packages = if detail.packages.is_empty() {
        "none".to_string()
    } else {
        detail.packages.join(", ")
    };
    let image = detail.image_digest.as_deref().map_or_else(
        || "not built".to_string(),
        |digest| {
            if digest.chars().count() > DIGEST_CHARS {
                format!("{}…", digest.chars().take(DIGEST_CHARS).collect::<String>())
            } else {
                digest.to_string()
            }
        },
    );
    let facts = row![
        fact("Runtime", runtime, 1),
        fact(
            "Signature",
            format!("({}) → {}", detail.inputs.join(", "), detail.output),
            2,
        ),
        fact("Packages", packages, 1),
        fact("Image", image, 1),
    ]
    .spacing(10);

    let source: Element<'_, Message> = match &detail.source {
        Some(source) => scrollable(
            container(highlighted_python(source, colors))
                .padding(12)
                .width(Fill),
        )
        .direction(thin_scrollbar())
        .height(Fill)
        .into(),
        None => center(muted("The server sent no source for this version.").size(12)).into(),
    };

    column![
        facts,
        section_label("Source"),
        container(source)
            .width(Fill)
            .height(Fill)
            .style(theme::card),
    ]
    .spacing(10)
    .width(Fill)
    .into()
}

/// `source` as Python, coloured like SQL in the editor.
fn highlighted_python(source: &str, colors: SyntaxColors) -> Element<'_, Message> {
    let mut spans: Vec<Span<'_, (), Font>> = Vec::new();
    let mut open = None;
    for (index, line) in source.lines().enumerate() {
        if index > 0 {
            spans.push(span("\n"));
        }
        let (tokens, still_open) = python::tokenize_line(line, open);
        open = still_open;
        let mut at = 0;
        for (range, token) in tokens {
            if range.start > at {
                spans.push(span(&line[at..range.start]));
            }
            spans.push(span(&line[range.clone()]).color(color_of(token, &colors)));
            at = range.end;
        }
        if at < line.len() {
            spans.push(span(&line[at..]));
        }
    }
    rich_text(spans).size(12).font(Font::MONOSPACE).into()
}

/// A labelled card; `portion` is its share of the row's width. Long values
/// (Arrow types such as `fixed_size_list<float32, 384>`) break anywhere.
fn fact<'a>(label: &'a str, value: String, portion: u16) -> Element<'a, Message> {
    container(
        column![
            section_label(label),
            text(value)
                .size(12)
                .font(Font::MONOSPACE)
                .wrapping(text::Wrapping::WordOrGlyph),
        ]
        .spacing(4),
    )
    .padding([8, 10])
    .width(Length::FillPortion(portion))
    .style(theme::card)
    .into()
}

/// Every version, and the columns this Function computes.
fn side_panel<'a>(
    app: &'a App,
    view: &'a FunctionView,
    versions: &[&'a FunctionInfo],
) -> Element<'a, Message> {
    let mut panel = column![section_label("Versions")].spacing(4);
    for version in versions {
        let current = version.version == view.version;
        panel = panel.push(
            button(
                row![
                    text(format!("v{}", version.version)).size(12).font(BOLD),
                    faint(version.signature.as_str())
                        .size(11)
                        .font(Font::MONOSPACE)
                        .width(Fill),
                    faint(
                        version
                            .created_at_millis
                            .map(utc_minute)
                            .unwrap_or_default()
                    )
                    .size(11)
                    .wrapping(text::Wrapping::None),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .padding([4, 8])
            .on_press(Message::SelectFunction(
                version.name.clone(),
                version.version.clone(),
            ))
            .style(theme::list_item(current)),
        );
    }

    panel = panel.push(Space::new().height(8));
    panel = panel.push(section_label("Columns computed by it"));
    let mut found = false;
    for (table, statuses) in &app.column_status {
        let Ok(statuses) = statuses else {
            continue;
        };
        for status in statuses
            .iter()
            .filter(|status| status.listed_function() == view.name)
        {
            found = true;
            panel = panel.push(
                column![
                    row![
                        text(format!("{table}.{}", status.column))
                            .size(12)
                            .font(Font::MONOSPACE),
                        Space::new().width(Fill),
                        faint(format!("v{}", status.function_version)).size(11),
                    ]
                    .align_y(Alignment::Center),
                    faint(freshness(
                        status.fresh,
                        status.stale,
                        status.failed,
                        status.never_computed
                    ))
                    .size(11),
                ]
                .spacing(2),
            );
        }
    }
    if !found {
        let checked = app.column_status.values().any(Result::is_ok);
        panel = panel.push(
            faint(if checked {
                "None of the expanded tables has a column computed by it."
            } else {
                "Expand a table in the sidebar to see the Function columns it has."
            })
            .size(11),
        );
    }
    scrollable(panel).direction(thin_scrollbar()).into()
}

/// `50 fresh · 2 stale · 0 failed` (plus never-computed rows, when any).
pub fn freshness(fresh: u64, stale: u64, failed: u64, never: u64) -> String {
    let mut text = format!(
        "{} fresh · {} stale · {} failed",
        thousands(fresh as usize),
        thousands(stale as usize),
        thousands(failed as usize)
    );
    if never > 0 {
        text.push_str(&format!(" · {} never computed", thousands(never as usize)));
    }
    text
}

/// `2026-11-24 13:22 UTC` for a Unix time in milliseconds.
pub fn utc_minute(millis: i64) -> String {
    let (year, month, day, minute) = crate::enterprise::dashboard::civil(millis);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02} UTC",
        minute / 60,
        minute % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_dates_and_freshness() {
        assert_eq!(utc_minute(0), "1970-01-01 00:00 UTC");
        assert_eq!(utc_minute(1_795_554_564_300), "2026-11-24 21:09 UTC");
        assert_eq!(utc_minute(951_782_400_000), "2000-02-29 00:00 UTC");
        assert_eq!(utc_minute(-60_000), "1969-12-31 23:59 UTC");
        assert_eq!(freshness(50, 2, 0, 0), "50 fresh · 2 stale · 0 failed");
        assert_eq!(
            freshness(1_000, 0, 1, 3),
            "1,000 fresh · 0 stale · 1 failed · 3 never computed"
        );
    }
}
