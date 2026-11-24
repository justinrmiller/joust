//! Stacked bars for dashboard tiles: one bar per day (or category), split by
//! series.
//!
//! Series take the categorical palette's slots in a fixed order (validated
//! for adjacent pairs, which is what a stack puts side by side), with steps
//! chosen separately for light and dark surfaces. Every mark gets a 2px
//! surface gap and a rounded top; hovering a bar shows its breakdown. The
//! legend and the tile's table view sit outside the canvas, so identity never
//! rests on colour alone.

use iced::mouse::{self, Cursor};
use iced::widget::canvas::{self, Action, Event, Frame, Geometry, Path, Text};
use iced::{Color, Font, Pixels, Point, Rectangle, Renderer, Size, Theme, color};

use super::chart::{bar_path, nice_ticks, tick_label};
use crate::app::Message;
use crate::enterprise::dashboard::Bars;
use crate::theme::{self, ThemeId};

/// The categorical palette's first six slots, for light surfaces.
pub const LIGHT: [Color; 6] = [
    color!(0x2a78d6),
    color!(0xeb6834),
    color!(0x1baf7a),
    color!(0xeda100),
    color!(0xe87ba4),
    color!(0x008300),
];

/// The same hues stepped for dark surfaces.
pub const DARK: [Color; 6] = [
    color!(0x3987e5),
    color!(0xd95926),
    color!(0x199e70),
    color!(0xc98500),
    color!(0xd55181),
    color!(0x0ca30c),
];

/// The colour of series `index` (in the order [`Bars`] lists them).
pub fn series_color(index: usize, dark: bool) -> Color {
    let palette = if dark { &DARK } else { &LIGHT };
    palette[index % palette.len()]
}

const AXIS_WIDTH: f32 = 44.0;
const LABEL_HEIGHT: f32 = 18.0;
const TOP_PAD: f32 = 8.0;
/// Space between stacked segments, and the bar width's share of its slot.
const GAP: f32 = 2.0;
const BAR_SHARE: f32 = 0.62;
const MAX_BAR_WIDTH: f32 = 56.0;
const TEXT_SIZE: f32 = 10.0;
/// Rough width of one label character, to thin out crowded axis labels.
const CHAR_WIDTH: f32 = 6.0;

pub struct StackedBars<'a> {
    pub bars: &'a Bars,
    pub theme: ThemeId,
}

/// The bar under the cursor.
#[derive(Debug, Default)]
pub struct State {
    hovered: Option<usize>,
}

/// Where the bars go inside `bounds`.
struct Plot {
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    slot: f32,
    max: f64,
    ticks: Vec<f64>,
}

impl StackedBars<'_> {
    fn plot(&self, size: Size) -> Plot {
        let max_total = self
            .bars
            .totals()
            .into_iter()
            .fold(0.0_f64, f64::max)
            .max(f64::MIN_POSITIVE);
        // Bars start at zero: an all-zero chart gets no negative tick.
        let ticks: Vec<f64> = nice_ticks(0.0, max_total, 4)
            .into_iter()
            .filter(|tick| *tick >= 0.0)
            .collect();
        let max = ticks.last().copied().unwrap_or(max_total).max(max_total);
        let width = (size.width - AXIS_WIDTH).max(1.0);
        Plot {
            left: AXIS_WIDTH,
            top: TOP_PAD,
            width,
            height: (size.height - LABEL_HEIGHT - TOP_PAD).max(1.0),
            slot: width / self.bars.x.len().max(1) as f32,
            max,
            ticks,
        }
    }

    /// The bar whose slot contains `point` (relative to the canvas).
    fn bar_at(&self, plot: &Plot, point: Point) -> Option<usize> {
        if point.x < plot.left || point.y < plot.top || point.y > plot.top + plot.height {
            return None;
        }
        let index = ((point.x - plot.left) / plot.slot) as usize;
        (index < self.bars.x.len()).then_some(index)
    }
}

impl canvas::Program<Message> for StackedBars<'_> {
    type State = State;

    fn update(
        &self,
        state: &mut State,
        event: &Event,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> Option<Action<Message>> {
        if !matches!(event, Event::Mouse(mouse::Event::CursorMoved { .. }))
            && !matches!(event, Event::Mouse(mouse::Event::CursorLeft))
        {
            return None;
        }
        let plot = self.plot(bounds.size());
        let hovered = cursor
            .position_in(bounds)
            .and_then(|point| self.bar_at(&plot, point));
        (hovered != state.hovered).then(|| {
            state.hovered = hovered;
            Action::request_redraw()
        })
    }

    fn draw(
        &self,
        state: &State,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: Cursor,
    ) -> Vec<Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let plot = self.plot(bounds.size());
        let dark = self.theme.is_dark();
        let surface = theme.extended_palette().background.base.color;
        let ink = theme::muted(theme);
        let label = |frame: &mut Frame, content: String, position: Point, align_right: bool| {
            frame.fill_text(Text {
                content,
                position,
                color: ink,
                size: Pixels(TEXT_SIZE),
                font: Font::DEFAULT,
                align_x: if align_right {
                    iced::widget::text::Alignment::Right
                } else {
                    iced::widget::text::Alignment::Center
                },
                align_y: iced::alignment::Vertical::Center,
                ..Text::default()
            });
        };
        let y_of = |value: f64| plot.top + plot.height - (value / plot.max) as f32 * plot.height;

        // Recessive gridlines and tick labels.
        for tick in &plot.ticks {
            let y = y_of(*tick);
            frame.fill_rectangle(
                Point::new(plot.left, y),
                Size::new(plot.width, 1.0),
                theme::divider(theme),
            );
            label(
                &mut frame,
                tick_label(*tick),
                Point::new(plot.left - 6.0, y),
                true,
            );
        }

        if let Some(index) = state.hovered {
            frame.fill_rectangle(
                Point::new(plot.left + plot.slot * index as f32, plot.top),
                Size::new(plot.slot, plot.height),
                theme::hover(theme),
            );
        }

        let bar_width = (plot.slot * BAR_SHARE).clamp(2.0, MAX_BAR_WIDTH);
        let every = label_stride(&self.bars.x, plot.slot);
        for (column, x_label) in self.bars.x.iter().enumerate() {
            let center = plot.left + plot.slot * (column as f32 + 0.5);
            let x = center - bar_width / 2.0;
            let mut base = 0.0;
            let top_series = (0..self.bars.series.len())
                .rev()
                .find(|row| self.bars.values[*row][column] > 0.0);
            for (row, values) in self.bars.values.iter().enumerate() {
                let value = values[column];
                if value <= 0.0 {
                    continue;
                }
                let bottom = y_of(base);
                base += value;
                let top = y_of(base);
                // A 2px surface gap above every segment but the top one.
                let gap = if Some(row) == top_series { 0.0 } else { GAP };
                let height = (bottom - top - gap).max(1.0);
                let color = series_color(row, dark);
                if Some(row) == top_series {
                    frame.fill(&bar_path(x, top, bar_width, height, true), color);
                } else {
                    frame.fill(
                        &Path::rectangle(Point::new(x, top + gap), Size::new(bar_width, height)),
                        color,
                    );
                }
            }
            if column % every == 0 {
                label(
                    &mut frame,
                    x_label.clone(),
                    Point::new(center, plot.top + plot.height + LABEL_HEIGHT / 2.0 + 2.0),
                    false,
                );
            }
        }

        if let Some(index) = state.hovered {
            tooltip(
                &mut frame,
                self.bars,
                index,
                &plot,
                bounds.size(),
                theme,
                surface,
                dark,
            );
        }
        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &State,
        _bounds: Rectangle,
        _cursor: Cursor,
    ) -> mouse::Interaction {
        if state.hovered.is_some() {
            mouse::Interaction::Crosshair
        } else {
            mouse::Interaction::default()
        }
    }
}

/// Label every `n`th bar so labels do not collide.
fn label_stride(labels: &[String], slot: f32) -> usize {
    let widest = labels.iter().map(|l| l.chars().count()).max().unwrap_or(1) as f32;
    let needed = widest * CHAR_WIDTH + 8.0;
    ((needed / slot.max(1.0)).ceil() as usize).max(1)
}

/// The hovered bar's breakdown, beside the bar.
#[allow(clippy::too_many_arguments)]
fn tooltip(
    frame: &mut Frame,
    bars: &Bars,
    index: usize,
    plot: &Plot,
    size: Size,
    theme: &Theme,
    surface: Color,
    dark: bool,
) {
    let rows: Vec<(usize, f64)> = bars
        .values
        .iter()
        .enumerate()
        .map(|(row, values)| (row, values[index]))
        .filter(|(_, value)| *value > 0.0)
        .rev()
        .collect();
    let total: f64 = rows.iter().map(|(_, value)| value).sum();
    let line = 15.0;
    let width = 190.0;
    let height = line * (rows.len() as f32 + 2.0) + 8.0;
    let anchor = plot.left + plot.slot * (index as f32 + 1.0) + 6.0;
    let x = if anchor + width > size.width {
        (plot.left + plot.slot * index as f32 - width - 6.0).max(0.0)
    } else {
        anchor
    };
    let y = plot.top;
    let background = theme.extended_palette().background.base.text;
    frame.fill(
        &Path::rounded_rectangle(Point::new(x, y), Size::new(width, height), 4.0.into()),
        background,
    );
    let text = |frame: &mut Frame, content: String, at: Point, right: bool| {
        frame.fill_text(Text {
            content,
            position: at,
            color: surface,
            size: Pixels(11.0),
            align_x: if right {
                iced::widget::text::Alignment::Right
            } else {
                iced::widget::text::Alignment::Left
            },
            ..Text::default()
        });
    };
    text(
        frame,
        format!("{} · {} {}", bars.x[index], tick_label(total), bars.unit),
        Point::new(x + 8.0, y + 6.0),
        false,
    );
    for (i, (row, value)) in rows.iter().enumerate() {
        let top = y + 6.0 + line * (i as f32 + 1.0) + 2.0;
        frame.fill(
            &Path::rounded_rectangle(
                Point::new(x + 8.0, top + 2.0),
                Size::new(8.0, 8.0),
                2.0.into(),
            ),
            series_color(*row, dark),
        );
        text(
            frame,
            bars.series[*row].clone(),
            Point::new(x + 22.0, top),
            false,
        );
        text(
            frame,
            tick_label(*value),
            Point::new(x + width - 8.0, top),
            true,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{moved, render_canvas};

    fn bars() -> Bars {
        Bars {
            x: vec!["Nov 22".into(), "Nov 23".into(), "Nov 24".into()],
            series: vec!["sql".into(), "refresh".into()],
            values: vec![vec![1.0, 0.0, 2.5], vec![0.5, 0.0, 4.0]],
            unit: "CPU-h",
        }
    }

    #[test]
    fn palettes_follow_the_validated_order() {
        assert_eq!(series_color(0, false), LIGHT[0]);
        assert_eq!(series_color(1, true), DARK[1]);
        assert_eq!(
            series_color(6, false),
            LIGHT[0],
            "wraps rather than inventing hues"
        );
        assert_eq!(label_stride(&["Nov 24".into()], 100.0), 1);
        assert!(label_stride(&["Nov 24".into()], 10.0) > 1);
    }

    #[test]
    fn draws_and_hovers_in_both_modes() {
        let bars = bars();
        for theme in [ThemeId::JoustLight, ThemeId::TokyoNight] {
            let chart = StackedBars { bars: &bars, theme };
            // No hover, then hovering the last bar (tooltip flips left).
            render_canvas(
                StackedBars { bars: &bars, theme },
                (480.0, 220.0),
                None,
                theme,
            );
            render_canvas(chart, (480.0, 220.0), Some(Point::new(470.0, 100.0)), theme);
        }
        let chart = StackedBars {
            bars: &bars,
            theme: ThemeId::JoustLight,
        };
        let plot = chart.plot(Size::new(480.0, 220.0));
        assert_eq!(chart.bar_at(&plot, Point::new(50.0, 100.0)), Some(0));
        assert_eq!(
            chart.bar_at(&plot, Point::new(10.0, 100.0)),
            None,
            "on the axis"
        );
        assert_eq!(
            chart.bar_at(&plot, Point::new(100.0, 219.0)),
            None,
            "on the labels"
        );

        let mut state = State::default();
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(480.0, 220.0));
        let cursor = Cursor::Available(Point::new(470.0, 100.0));
        assert!(
            canvas::Program::update(&chart, &mut state, &moved(470.0, 100.0), bounds, cursor)
                .is_some()
        );
        assert_eq!(state.hovered, Some(2));
        assert!(
            canvas::Program::update(&chart, &mut state, &moved(470.0, 100.0), bounds, cursor)
                .is_none()
        );
        assert_eq!(
            canvas::Program::mouse_interaction(&chart, &state, bounds, cursor),
            mouse::Interaction::Crosshair
        );
        let away = Event::Mouse(mouse::Event::CursorLeft);
        assert!(
            canvas::Program::update(&chart, &mut state, &away, bounds, Cursor::Unavailable)
                .is_some()
        );
        assert_eq!(state.hovered, None);
        let key = crate::test_support::key_press(
            crate::test_support::named(iced::keyboard::key::Named::Enter),
            iced::keyboard::Modifiers::empty(),
        );
        assert!(canvas::Program::update(&chart, &mut state, &key, bounds, cursor).is_none());
        assert_eq!(
            canvas::Program::mouse_interaction(&chart, &state, bounds, cursor),
            mouse::Interaction::default()
        );
    }
}
