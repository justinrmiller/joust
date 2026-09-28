//! "Columns" tab: one profile card per result column.

use iced::widget::canvas::{self, Frame, Geometry, Path};
use iced::widget::{
    Space, canvas as canvas_widget, column, container, progress_bar, row, scrollable, text,
};
use iced::{Element, Fill, Font, Point, Rectangle, Renderer, Size, Theme, font, mouse};

use crate::app::Message;
use crate::profile::{ColumnProfile, Distinct, Histogram, Summary};
use crate::results::{human_bytes, thousands};
use crate::theme::{self, ThemeId};
use crate::ui::chart::plain_number;
use crate::ui::grid::truncate;

const CARD_WIDTH: f32 = 290.0;

/// Renders the column explorer.
pub fn view<'a>(profiles: &'a [ColumnProfile], theme_id: ThemeId) -> Element<'a, Message> {
    let cards = profiles.iter().map(|profile| card(profile, theme_id));
    scrollable(
        container(row(cards).spacing(12).wrap().vertical_spacing(12))
            .padding(16)
            .width(Fill),
    )
    .direction(crate::ui::thin_scrollbar())
    .height(Fill)
    .into()
}

fn card<'a>(profile: &'a ColumnProfile, theme_id: ThemeId) -> Element<'a, Message> {
    let bold = Font {
        weight: font::Weight::Semibold,
        ..Font::DEFAULT
    };
    let header = row![
        text(truncate(&profile.name, 26)).font(bold).size(14),
        Space::new().width(Fill),
        container(text(&profile.type_label).size(11).font(Font::MONOSPACE))
            .padding([2, 6])
            .style(theme::badge),
    ]
    .align_y(iced::Center);

    let null_pct = profile.null_fraction() * 100.0;
    let distinct = match profile.distinct {
        Distinct::Exact(count) => thousands(count),
        Distinct::MoreThan(cap) => format!("> {}", thousands(cap)),
        Distinct::NotComputed => "—".to_string(),
    };
    let stats = row![
        stat("rows", thousands(profile.rows)),
        stat("distinct", distinct),
        stat("null", format!("{null_pct:.1}%")),
    ]
    .spacing(16);

    let body: Element<'a, Message> = match &profile.summary {
        Summary::Numeric {
            min,
            max,
            mean,
            std_dev,
            histogram,
        } => column![
            sparkbars(histogram, theme_id),
            range_row(plain_number(*min), plain_number(*max)),
            row![
                stat("mean", plain_number(*mean)),
                stat("std dev", plain_number(*std_dev))
            ]
            .spacing(16),
        ]
        .spacing(6)
        .into(),
        Summary::Temporal {
            min,
            max,
            histogram,
        } => column![
            sparkbars(histogram, theme_id),
            range_row(min.clone(), max.clone()),
        ]
        .spacing(6)
        .into(),
        Summary::Vector {
            dimension,
            min_norm,
            max_norm,
            mean_norm,
            histogram,
        } => column![
            text(format!("L2 norm distribution · {dimension} dims"))
                .size(11)
                .style(|theme: &Theme| text::Style {
                    color: Some(theme::muted(theme))
                }),
            sparkbars(histogram, theme_id),
            range_row(plain_number(*min_norm), plain_number(*max_norm)),
            stat("mean norm", format!("{mean_norm:.4}")),
        ]
        .spacing(6)
        .into(),
        Summary::Text {
            min_len,
            max_len,
            mean_len,
            top,
        } => {
            let total = profile.rows.saturating_sub(profile.nulls).max(1);
            let bars = top.iter().map(|(value, count)| {
                frequency_bar(value, *count, *count as f32 / total as f32, theme_id)
            });
            column![
                column(bars).spacing(4),
                row![
                    stat("min len", thousands(*min_len)),
                    stat("max len", thousands(*max_len)),
                    stat("avg len", format!("{mean_len:.1}")),
                ]
                .spacing(16),
            ]
            .spacing(8)
            .into()
        }
        Summary::Boolean {
            true_count,
            false_count,
        } => {
            let total = (true_count + false_count).max(1) as f32;
            column![
                frequency_bar("true", *true_count, *true_count as f32 / total, theme_id),
                frequency_bar("false", *false_count, *false_count as f32 / total, theme_id),
            ]
            .spacing(4)
            .into()
        }
        Summary::Media {
            formats,
            min_bytes,
            max_bytes,
            mean_bytes,
            dimensions,
            durations,
            histogram,
        } => {
            let total = profile.rows.saturating_sub(profile.nulls).max(1);
            let bars = formats.iter().take(TOP_FORMATS).map(|(format, count)| {
                frequency_bar(format, *count, *count as f32 / total as f32, theme_id)
            });
            let mut body = column![
                column(bars).spacing(4),
                text("size distribution").size(10).style(muted_text),
                sparkbars(histogram, theme_id),
                range_row(human_bytes(*min_bytes), human_bytes(*max_bytes)),
                stat("mean size", human_bytes(*mean_bytes as usize)),
            ]
            .spacing(6);
            if let Some(((min_w, min_h), (max_w, max_h))) = dimensions {
                let range = if (min_w, min_h) == (max_w, max_h) {
                    format!("{min_w}×{min_h}")
                } else {
                    format!("{min_w}×{min_h} … {max_w}×{max_h}")
                };
                body = body.push(stat("frame size", range));
            }
            if let Some((shortest, longest)) = durations {
                let range = if (longest - shortest).abs() < 0.5 {
                    crate::av::format_duration(*shortest)
                } else {
                    format!(
                        "{} … {}",
                        crate::av::format_duration(*shortest),
                        crate::av::format_duration(*longest)
                    )
                };
                body = body.push(stat("duration", range));
            }
            body.into()
        }
        Summary::Other => text(if profile.nulls == profile.rows {
            "All values are NULL"
        } else {
            "No summary for this type"
        })
        .size(12)
        .style(|theme: &Theme| text::Style {
            color: Some(theme::muted(theme)),
        })
        .into(),
    };

    container(column![header, stats, body].spacing(10))
        .padding(14)
        .width(CARD_WIDTH)
        .style(theme::card)
        .into()
}

/// Formats listed on media cards.
const TOP_FORMATS: usize = 5;

fn muted_text(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(theme::muted(theme)),
    }
}

fn stat<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    column![
        text(label).size(10).style(|theme: &Theme| text::Style {
            color: Some(theme::muted(theme))
        }),
        text(value).size(13).font(Font::MONOSPACE),
    ]
    .spacing(1)
    .into()
}

fn range_row<'a>(min: String, max: String) -> Element<'a, Message> {
    let faint = |theme: &Theme| text::Style {
        color: Some(theme::muted(theme)),
    };
    row![
        text(min).size(11).font(Font::MONOSPACE).style(faint),
        Space::new().width(Fill),
        text(max).size(11).font(Font::MONOSPACE).style(faint),
    ]
    .into()
}

/// Horizontal share bar for a frequent value.
fn frequency_bar<'a>(
    value: &str,
    count: usize,
    share: f32,
    theme_id: ThemeId,
) -> Element<'a, Message> {
    let label = row![
        text(truncate(value, 28)).size(12),
        Space::new().width(Fill),
        text(thousands(count)).size(12).font(Font::MONOSPACE),
    ];
    let bar = progress_bar(0.0..=1.0, share)
        .girth(4)
        .style(move |theme: &Theme| progress_bar::Style {
            background: theme::divider(theme).into(),
            bar: theme_id.series().into(),
            border: iced::Border {
                radius: 2.0.into(),
                ..iced::Border::default()
            },
        });
    column![label, bar].spacing(3).into()
}

/// A tiny histogram.
fn sparkbars<'a>(histogram: &'a Histogram, theme_id: ThemeId) -> Element<'a, Message> {
    canvas_widget(Sparkbars {
        histogram,
        theme_id,
    })
    .width(Fill)
    .height(56)
    .into()
}

struct Sparkbars<'a> {
    histogram: &'a Histogram,
    theme_id: ThemeId,
}

impl canvas::Program<Message> for Sparkbars<'_> {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let counts = &self.histogram.counts;
        let max = self.histogram.max_count().max(1) as f32;
        let band = bounds.width / counts.len().max(1) as f32;
        let baseline = bounds.height - 1.0;
        for (i, count) in counts.iter().enumerate() {
            if *count == 0 {
                continue;
            }
            let height = ((*count as f32 / max) * (bounds.height - 2.0)).max(1.5);
            frame.fill(
                &Path::rectangle(
                    Point::new(i as f32 * band + 1.0, baseline - height),
                    Size::new((band - 2.0).max(1.0), height),
                ),
                self.theme_id.series(),
            );
        }
        frame.fill_rectangle(
            Point::new(0.0, baseline),
            Size::new(bounds.width, 1.0),
            theme::divider(theme),
        );
        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use iced_test::Simulator;

    use super::*;

    fn profile(name: &str, distinct: Distinct, nulls: usize, summary: Summary) -> ColumnProfile {
        ColumnProfile {
            name: name.into(),
            type_label: "blob".into(),
            rows: 10,
            nulls,
            distinct,
            summary,
        }
    }

    fn media(dimensions: Option<((u32, u32), (u32, u32))>, durations: (f64, f64)) -> Summary {
        Summary::Media {
            formats: vec![("MP4 video".into(), 8), ("PNG image".into(), 2)],
            min_bytes: 1_000,
            max_bytes: 90_000,
            mean_bytes: 20_000.0,
            dimensions,
            durations: Some(durations),
            histogram: Histogram::from_values([1_000.0, 90_000.0].into_iter()).unwrap(),
        }
    }

    #[test]
    fn cards_describe_every_summary_kind() {
        let profiles = vec![
            profile(
                "clips",
                Distinct::NotComputed,
                0,
                media(Some(((64, 36), (128, 72))), (2.0, 4.0)),
            ),
            profile(
                "stills",
                Distinct::MoreThan(crate::profile::DISTINCT_CAP),
                0,
                media(Some(((64, 36), (64, 36))), (3.0, 3.2)),
            ),
            profile("empty", Distinct::Exact(0), 10, Summary::Other),
            profile("tags", Distinct::Exact(3), 2, Summary::Other),
        ];
        let mut ui = Simulator::with_size(
            iced::Settings::default(),
            (1200.0, 700.0),
            view(&profiles, ThemeId::JoustDark),
        );
        for label in [
            "64×36 … 128×72",
            "0:02 … 0:04",
            "64×36",
            "0:03",
            "> 100,000",
            "—",
            "All values are NULL",
            "No summary for this type",
            "20.0%",
        ] {
            assert!(ui.find(label).is_ok(), "{label}");
        }
        ui.snapshot(&ThemeId::JoustDark.to_theme()).unwrap();
    }

    #[test]
    fn real_profiles_render_in_light_and_dark_themes() {
        let table = crate::test_support::numbers_table(30);
        let profiles = crate::profile::profile_batch(table.batch());
        for theme in [ThemeId::JoustLight, ThemeId::CatppuccinMocha] {
            let mut ui = Simulator::with_size(
                iced::Settings::default(),
                (1200.0, 700.0),
                view(&profiles, theme),
            );
            assert!(ui.find("value").is_ok());
            ui.snapshot(&theme.to_theme()).unwrap();
        }
    }
}
