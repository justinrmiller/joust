//! Result charts: bar, line, scatter and histogram, drawn on a canvas.
//!
//! Charts are single-series by design: one measure against one dimension,
//! drawn in the validated series colour, with hairline gridlines, clean
//! 1-2-5 ticks and a hover tooltip. Aggregate in SQL for anything richer.

use std::fmt;

use iced::alignment;
use iced::mouse::{self, Cursor};
use iced::widget::canvas::{self, Action, Event, Frame, Geometry, Path, Stroke, Text};
use iced::widget::text::Alignment;
use iced::{Color, Font, Pixels, Point, Rectangle, Renderer, Size, Theme, Vector, font};
use lancedb::arrow::arrow_array::cast::AsArray;
use lancedb::arrow::arrow_array::types::Int64Type;
use lancedb::arrow::arrow_array::{Array, Float64Array};
use lancedb::arrow::arrow_cast::cast;
use lancedb::arrow::arrow_schema::{DataType, TimeUnit};

use crate::app::Message;
use crate::results::{ResultTable, format_value, thousands};
use crate::theme::{self, ThemeId};
use crate::ui::grid::truncate;

/// Most bars a bar chart shows (aggregate in SQL for more).
pub const MAX_BARS: usize = 60;
/// Most points plotted by line and scatter charts.
pub const MAX_POINTS: usize = 5_000;
const HISTOGRAM_BINS: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChartKind {
    #[default]
    Bar,
    Line,
    Scatter,
    Histogram,
}

impl ChartKind {
    pub const ALL: [ChartKind; 4] = [
        ChartKind::Bar,
        ChartKind::Line,
        ChartKind::Scatter,
        ChartKind::Histogram,
    ];

    /// Whether the chart uses an X column.
    pub fn uses_x(self) -> bool {
        self != ChartKind::Histogram
    }
}

impl fmt::Display for ChartKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ChartKind::Bar => "Bar",
            ChartKind::Line => "Line",
            ChartKind::Scatter => "Scatter",
            ChartKind::Histogram => "Histogram",
        })
    }
}

/// A column choice in a chart picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnChoice {
    pub index: usize,
    pub name: String,
}

impl fmt::Display for ColumnChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

/// What to plot.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChartSpec {
    pub kind: ChartKind,
    pub x: Option<usize>,
    pub y: Option<usize>,
}

impl ChartSpec {
    /// Picks sensible defaults for a fresh result: the first text or temporal
    /// column as X and the first numeric column (other than X) as Y.
    pub fn suggest(table: &ResultTable) -> Self {
        let columns = &table.columns;
        let all_numeric: Vec<usize> = (0..columns.len()).filter(|&i| columns[i].numeric).collect();
        // Identifier columns are numeric but rarely the measure worth plotting.
        let measures: Vec<usize> = all_numeric
            .iter()
            .copied()
            .filter(|&i| !is_identifier(&columns[i].name))
            .collect();
        let numeric = if measures.is_empty() {
            all_numeric
        } else {
            measures
        };
        let temporal = (0..columns.len()).find(|&i| columns[i].data_type.is_temporal());
        let text = (0..columns.len()).find(|&i| is_categorical(&columns[i].data_type));

        let (kind, x) = match (temporal, text) {
            (Some(t), _) => (ChartKind::Line, Some(t)),
            (None, Some(c)) => (ChartKind::Bar, Some(c)),
            (None, None) if numeric.len() >= 2 => (ChartKind::Scatter, Some(numeric[0])),
            _ => (ChartKind::Histogram, None),
        };
        let y = numeric
            .iter()
            .copied()
            .find(|&i| Some(i) != x)
            .or(numeric.first().copied());
        Self { kind, x, y }
    }
}

/// `id`, `user_id`, `movieId`, ...
fn is_identifier(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "id" || lower.ends_with("_id") || (name.len() > 2 && name.ends_with("Id"))
}

fn is_categorical(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Utf8
            | DataType::LargeUtf8
            | DataType::Utf8View
            | DataType::Boolean
            | DataType::Dictionary(..)
    )
}

/// Plot-ready data derived from a [`ChartSpec`].
#[derive(Debug, Clone, PartialEq)]
pub struct ChartData {
    pub kind: ChartKind,
    pub x_label: String,
    pub y_label: String,
    pub x_axis: XAxis,
    /// `(x, y)` pairs; for bars and histograms `x` is the bar index.
    pub points: Vec<(f64, f64)>,
    /// Bar labels (bar charts) or bin edges (histograms).
    pub labels: Vec<String>,
    /// Note shown under the chart (e.g. truncation).
    pub note: Option<String>,
}

/// How X values are rendered on the axis.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum XAxis {
    Numeric,
    /// Microseconds since the epoch.
    Timestamp,
    Categorical,
}

/// Builds chart data from the current result.
pub fn build(table: &ResultTable, spec: &ChartSpec) -> Result<ChartData, String> {
    let y = spec.y.ok_or("Pick a numeric column for the Y axis")?;
    let y_meta = &table.columns[y];
    if !y_meta.numeric {
        return Err(format!("{} is not numeric", y_meta.name));
    }
    let y_values = to_f64(table, y)?;
    let rows = table.row_count();

    match spec.kind {
        ChartKind::Histogram => {
            let values: Vec<f64> = y_values
                .iter()
                .flatten()
                .copied()
                .filter(|v| v.is_finite())
                .collect();
            let histogram = crate::profile::Histogram::from_values(values.iter().copied())
                .ok_or("No numeric values to plot")?;
            let counts = rebin(&values, histogram.min, histogram.max, HISTOGRAM_BINS);
            let width = (histogram.max - histogram.min) / HISTOGRAM_BINS as f64;
            let years = looks_like_years(histogram.min, histogram.max);
            Ok(ChartData {
                kind: spec.kind,
                x_label: y_meta.name.clone(),
                y_label: "count".into(),
                x_axis: XAxis::Numeric,
                points: counts
                    .iter()
                    .enumerate()
                    .map(|(i, count)| (histogram.min + width * (i as f64 + 0.5), *count as f64))
                    .collect(),
                labels: (0..HISTOGRAM_BINS)
                    .map(|i| {
                        let lo = histogram.min + width * i as f64;
                        format!(
                            "{} – {}",
                            axis_label(lo, years),
                            axis_label(lo + width, years)
                        )
                    })
                    .collect(),
                note: None,
            })
        }
        ChartKind::Bar => {
            let x = spec.x.ok_or("Pick a column for the X axis")?;
            let shown = rows.min(MAX_BARS);
            let points = (0..shown)
                .filter_map(|row| {
                    y_values
                        .get(row)
                        .copied()
                        .flatten()
                        .map(|v| (row as f64, v))
                })
                .collect();
            let labels = (0..shown)
                .map(|row| format_value(table.column(x).as_ref(), table.source_row(row)))
                .collect();
            Ok(ChartData {
                kind: spec.kind,
                x_label: table.columns[x].name.clone(),
                y_label: y_meta.name.clone(),
                x_axis: XAxis::Categorical,
                points,
                labels,
                note: (rows > MAX_BARS).then(|| {
                    format!(
                        "Showing the first {MAX_BARS} of {} rows — aggregate with GROUP BY to chart everything",
                        thousands(rows)
                    )
                }),
            })
        }
        ChartKind::Line | ChartKind::Scatter => {
            let x = spec.x.ok_or("Pick a column for the X axis")?;
            let x_meta = &table.columns[x];
            let x_axis = if x_meta.data_type.is_temporal() {
                XAxis::Timestamp
            } else if x_meta.numeric {
                XAxis::Numeric
            } else {
                return Err(format!(
                    "{} is not numeric or temporal — use a bar chart for categories",
                    x_meta.name
                ));
            };
            let x_values = to_f64(table, x)?;
            let mut points: Vec<(f64, f64)> = x_values
                .iter()
                .zip(&y_values)
                .filter_map(|(x, y)| Some(((*x)?, (*y)?)))
                .filter(|(x, y)| x.is_finite() && y.is_finite())
                .collect();
            if points.is_empty() {
                return Err("No rows have both X and Y values".into());
            }
            let total = points.len();
            if spec.kind == ChartKind::Line {
                points.sort_by(|a, b| a.0.total_cmp(&b.0));
            }
            if total > MAX_POINTS {
                let stride = total.div_ceil(MAX_POINTS);
                points = points.into_iter().step_by(stride).collect();
            }
            Ok(ChartData {
                kind: spec.kind,
                x_label: x_meta.name.clone(),
                y_label: y_meta.name.clone(),
                x_axis,
                points,
                labels: Vec::new(),
                note: (total > MAX_POINTS).then(|| {
                    format!(
                        "Sampled {} of {} points",
                        thousands(MAX_POINTS),
                        thousands(total)
                    )
                }),
            })
        }
    }
}

/// Column values (in display order) as `f64`; temporal columns become µs.
fn to_f64(table: &ResultTable, column: usize) -> Result<Vec<Option<f64>>, String> {
    let array = table.column(column);
    let values: Vec<Option<f64>> = if array.data_type().is_temporal() {
        let micros = cast(array, &DataType::Timestamp(TimeUnit::Microsecond, None))
            .and_then(|a| cast(&a, &DataType::Int64))
            .map_err(|e| e.to_string())?;
        let micros = micros.as_primitive::<Int64Type>();
        micros.iter().map(|v| v.map(|v| v as f64)).collect()
    } else {
        let floats = cast(array, &DataType::Float64).map_err(|e| e.to_string())?;
        let floats = floats
            .as_any()
            .downcast_ref::<Float64Array>()
            .ok_or("could not read numbers")?;
        floats.iter().collect()
    };
    Ok((0..table.row_count())
        .map(|row| values[table.source_row(row)])
        .collect())
}

fn rebin(values: &[f64], min: f64, max: f64, bins: usize) -> Vec<usize> {
    let mut counts = vec![0; bins];
    let width = (max - min) / bins as f64;
    for value in values {
        let bin = if width > 0.0 {
            (((value - min) / width) as usize).min(bins - 1)
        } else {
            bins / 2
        };
        counts[bin] += 1;
    }
    counts
}

/// "Nice" tick values covering `[min, max]` with roughly `target` steps.
pub fn nice_ticks(min: f64, max: f64, target: usize) -> Vec<f64> {
    if !min.is_finite() || !max.is_finite() {
        return Vec::new();
    }
    let (min, max) = if (max - min).abs() < f64::EPSILON {
        (min - 1.0, max + 1.0)
    } else {
        (min, max)
    };
    let raw = (max - min) / target.max(1) as f64;
    let magnitude = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 2.5, 5.0, 10.0]
        .iter()
        .map(|m| m * magnitude)
        .find(|step| *step >= raw)
        .unwrap_or(10.0 * magnitude);
    let start = (min / step).floor() * step;
    let mut ticks = Vec::new();
    let mut value = start;
    while value <= max + step * 0.5 {
        ticks.push(if value.abs() < step * 1e-9 {
            0.0
        } else {
            value
        });
        value += step;
    }
    ticks
}

/// Compact tick label: `1,500`, `2.5k`, `1.2M`, `0.25`.
pub fn tick_label(value: f64) -> String {
    let abs = value.abs();
    if abs >= 1e9 {
        format!("{:.1}B", value / 1e9)
    } else if abs >= 1e6 {
        format!("{:.1}M", value / 1e6)
    } else if abs >= 1e4 {
        format!("{:.1}k", value / 1e3)
    } else if abs >= 100.0 || value.fract() == 0.0 {
        let rounded = value.round() as i64;
        let sign = if rounded < 0 { "-" } else { "" };
        format!("{sign}{}", thousands(rounded.unsigned_abs() as usize))
    } else if abs >= 1.0 {
        format!("{value:.1}")
    } else {
        let text = format!("{value:.3}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// A value as written in data (no thousands separators): `1941`, `0.125`.
pub fn plain_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        format!("{}", value as i64)
    } else if value.abs() >= 1e9 || (value != 0.0 && value.abs() < 1e-4) {
        format!("{value:.3e}")
    } else {
        let text = format!("{value:.4}");
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Whether integer values in `[min, max]` look like calendar years, which
/// read wrongly with thousands separators ("1,984").
pub fn looks_like_years(min: f64, max: f64) -> bool {
    min >= 1000.0 && max < 3000.0
}

/// Axis tick label, switching to plain numbers for year-like ranges.
fn axis_label(value: f64, years: bool) -> String {
    if years && value.fract() == 0.0 {
        plain_number(value)
    } else {
        tick_label(value)
    }
}

fn time_label(micros: f64, span_micros: f64) -> String {
    use lancedb::arrow::arrow_array::temporal_conversions::timestamp_us_to_datetime;
    let Some(datetime) = timestamp_us_to_datetime(micros as i64) else {
        return tick_label(micros);
    };
    const DAY: f64 = 86_400e6;
    if span_micros > 400.0 * DAY {
        datetime.format("%Y-%m").to_string()
    } else if span_micros > 2.0 * DAY {
        datetime.format("%b %d").to_string()
    } else {
        datetime.format("%H:%M").to_string()
    }
}

/// Full timestamp for tooltips; midnight values show just the date.
fn full_time_label(micros: f64) -> String {
    use lancedb::arrow::arrow_array::temporal_conversions::timestamp_us_to_datetime;
    match timestamp_us_to_datetime(micros as i64) {
        Some(datetime) => {
            let text = datetime.format("%Y-%m-%d %H:%M").to_string();
            text.strip_suffix(" 00:00")
                .map(str::to_string)
                .unwrap_or(text)
        }
        None => tick_label(micros),
    }
}

/// Canvas program rendering a [`ChartData`].
pub struct Chart<'a> {
    pub data: &'a ChartData,
    pub theme: ThemeId,
}

#[derive(Debug, Default)]
pub struct ChartState {
    hover: Option<Point>,
}

/// Plot area and scales for one frame.
struct Plot {
    area: Rectangle,
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
    y_ticks: Vec<f64>,
}

impl Plot {
    fn new(data: &ChartData, bounds: Size) -> Self {
        let area = Rectangle {
            x: 64.0,
            y: 16.0,
            width: (bounds.width - 64.0 - 24.0).max(10.0),
            height: (bounds.height - 16.0 - 56.0).max(10.0),
        };
        let (x_min, x_max) = match data.kind {
            ChartKind::Bar | ChartKind::Histogram => (-0.5, data.points.len().max(1) as f64 - 0.5),
            _ => min_max(data.points.iter().map(|p| p.0)),
        };
        let (mut y_min, mut y_max) = min_max(data.points.iter().map(|p| p.1));
        if matches!(data.kind, ChartKind::Bar | ChartKind::Histogram) {
            // Bars grow from a zero baseline.
            y_min = y_min.min(0.0);
            y_max = y_max.max(0.0);
        }
        let y_ticks = nice_ticks(y_min, y_max, 5);
        let y_min = y_ticks.first().copied().unwrap_or(y_min).min(y_min);
        let y_max = y_ticks.last().copied().unwrap_or(y_max).max(y_max);
        Self {
            area,
            x_min,
            x_max,
            y_min,
            y_max,
            y_ticks,
        }
    }

    fn x(&self, value: f64) -> f32 {
        let span = (self.x_max - self.x_min).max(f64::EPSILON);
        self.area.x + ((value - self.x_min) / span) as f32 * self.area.width
    }

    fn y(&self, value: f64) -> f32 {
        let span = (self.y_max - self.y_min).max(f64::EPSILON);
        self.area.y + self.area.height - ((value - self.y_min) / span) as f32 * self.area.height
    }

    fn band(&self, count: usize) -> f32 {
        self.area.width / count.max(1) as f32
    }
}

fn min_max(values: impl Iterator<Item = f64>) -> (f64, f64) {
    let (min, max) = values.fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
        (lo.min(v), hi.max(v))
    });
    if min.is_finite() {
        if min == max {
            (min - 1.0, max + 1.0)
        } else {
            (min, max)
        }
    } else {
        (0.0, 1.0)
    }
}

impl Chart<'_> {
    /// Index of the point/bar under the cursor.
    fn hovered(&self, plot: &Plot, cursor: Point) -> Option<usize> {
        if !plot.area.contains(cursor) || self.data.points.is_empty() {
            return None;
        }
        match self.data.kind {
            ChartKind::Bar | ChartKind::Histogram => {
                let index = ((cursor.x - plot.area.x) / plot.band(self.data.points.len())) as usize;
                (index < self.data.points.len()).then_some(index)
            }
            ChartKind::Line => self
                .data
                .points
                .iter()
                .enumerate()
                .min_by(|a, b| {
                    (plot.x(a.1.0) - cursor.x)
                        .abs()
                        .total_cmp(&(plot.x(b.1.0) - cursor.x).abs())
                })
                .map(|(i, _)| i),
            ChartKind::Scatter => self
                .data
                .points
                .iter()
                .enumerate()
                .map(|(i, p)| (i, Point::new(plot.x(p.0), plot.y(p.1)).distance(cursor)))
                .filter(|(_, distance)| *distance < 12.0)
                .min_by(|a, b| a.1.total_cmp(&b.1))
                .map(|(i, _)| i),
        }
    }

    fn x_text(&self, value: f64, plot: &Plot) -> String {
        match self.data.x_axis {
            XAxis::Timestamp => time_label(value, plot.x_max - plot.x_min),
            _ => axis_label(value, looks_like_years(plot.x_min, plot.x_max)),
        }
    }
}

impl canvas::Program<Message> for Chart<'_> {
    type State = ChartState;

    fn update(
        &self,
        state: &mut ChartState,
        event: &Event,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> Option<Action<Message>> {
        match event {
            Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft) => {
                let hover = cursor.position_in(bounds);
                if hover != state.hover {
                    state.hover = hover;
                    return Some(Action::request_redraw());
                }
                None
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &ChartState,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: Cursor,
    ) -> Vec<Geometry> {
        let palette = theme.extended_palette();
        let background = palette.background.base.color;
        let text_color = palette.background.base.text;
        let muted = theme::muted(theme);
        let grid = theme::divider(theme);
        let series = self.theme.series();
        let data = self.data;

        let mut frame = Frame::new(renderer, bounds.size());
        frame.fill_rectangle(Point::ORIGIN, bounds.size(), background);
        let plot = Plot::new(data, bounds.size());
        let hairline = Stroke::default().with_width(1.0).with_color(grid);

        // Horizontal gridlines + Y ticks.
        for tick in &plot.y_ticks {
            let y = plot.y(*tick).round() + 0.5;
            frame.stroke(
                &Path::line(
                    Point::new(plot.area.x, y),
                    Point::new(plot.area.x + plot.area.width, y),
                ),
                hairline,
            );
            frame.fill_text(Text {
                content: tick_label(*tick),
                position: Point::new(plot.area.x - 8.0, y),
                color: muted,
                size: Pixels(11.0),
                align_x: Alignment::Right,
                align_y: alignment::Vertical::Center,
                ..Text::default()
            });
        }

        // X ticks.
        let x_tick_y = plot.area.y + plot.area.height + 14.0;
        match data.kind {
            ChartKind::Bar | ChartKind::Histogram => {
                let count = data.points.len().max(1);
                let band = plot.band(count);
                let every = ((70.0 / band).ceil() as usize).max(1);
                for i in (0..count).step_by(every) {
                    let label = match data.kind {
                        ChartKind::Bar => data.labels.get(i).cloned().unwrap_or_default(),
                        _ => {
                            let first = data.points.first().map_or(0.0, |p| p.0);
                            let last = data.points.last().map_or(0.0, |p| p.0);
                            axis_label(
                                data.points.get(i).map_or(0.0, |p| p.0),
                                looks_like_years(first, last),
                            )
                        }
                    };
                    frame.fill_text(Text {
                        content: truncate(&label, ((band * every as f32) / 6.5) as usize),
                        position: Point::new(plot.area.x + band * (i as f32 + 0.5), x_tick_y),
                        color: muted,
                        size: Pixels(11.0),
                        align_x: Alignment::Center,
                        align_y: alignment::Vertical::Center,
                        ..Text::default()
                    });
                }
            }
            ChartKind::Line | ChartKind::Scatter => {
                let ticks = if data.x_axis == XAxis::Timestamp {
                    let steps = 6;
                    (0..=steps)
                        .map(|i| plot.x_min + (plot.x_max - plot.x_min) * i as f64 / steps as f64)
                        .collect()
                } else {
                    nice_ticks(plot.x_min, plot.x_max, 6)
                };
                for tick in ticks
                    .into_iter()
                    .filter(|t| *t >= plot.x_min && *t <= plot.x_max)
                {
                    frame.fill_text(Text {
                        content: self.x_text(tick, &plot),
                        position: Point::new(plot.x(tick), x_tick_y),
                        color: muted,
                        size: Pixels(11.0),
                        align_x: Alignment::Center,
                        align_y: alignment::Vertical::Center,
                        ..Text::default()
                    });
                }
            }
        }

        // Axis titles.
        frame.fill_text(Text {
            content: data.x_label.clone(),
            position: Point::new(plot.area.x + plot.area.width / 2.0, bounds.height - 14.0),
            color: text_color,
            size: Pixels(12.0),
            align_x: Alignment::Center,
            align_y: alignment::Vertical::Center,
            ..Text::default()
        });
        frame.with_save(|frame| {
            frame.translate(Vector::new(14.0, plot.area.y + plot.area.height / 2.0));
            frame.rotate(-std::f32::consts::FRAC_PI_2);
            frame.fill_text(Text {
                content: data.y_label.clone(),
                position: Point::ORIGIN,
                color: text_color,
                size: Pixels(12.0),
                align_x: Alignment::Center,
                align_y: alignment::Vertical::Center,
                ..Text::default()
            });
        });

        let hovered = state.hover.and_then(|cursor| self.hovered(&plot, cursor));
        let baseline = plot.y(0.0_f64.clamp(plot.y_min, plot.y_max));

        match data.kind {
            ChartKind::Bar | ChartKind::Histogram => {
                let band = plot.band(data.points.len());
                // Histograms touch (2px surface gap); bars leave air, ≤ 24px thick.
                let width = if data.kind == ChartKind::Histogram {
                    (band - 2.0).max(1.0)
                } else {
                    (band * 0.7).clamp(2.0, 24.0)
                };
                for (i, (_, value)) in data.points.iter().enumerate() {
                    let top = plot.y(*value);
                    let x = plot.area.x + band * (i as f32 + 0.5) - width / 2.0;
                    let (y, height) = if top <= baseline {
                        (top, baseline - top)
                    } else {
                        (baseline, top - baseline)
                    };
                    let color = if hovered.is_some() && hovered != Some(i) {
                        Color { a: 0.55, ..series }
                    } else {
                        series
                    };
                    frame.fill(&bar_path(x, y, width, height, top <= baseline), color);
                }
            }
            ChartKind::Line => {
                if data.points.len() > 1 {
                    let line = Path::new(|builder| {
                        for (i, (x, y)) in data.points.iter().enumerate() {
                            let point = Point::new(plot.x(*x), plot.y(*y));
                            if i == 0 {
                                builder.move_to(point);
                            } else {
                                builder.line_to(point);
                            }
                        }
                    });
                    frame.stroke(
                        &line,
                        Stroke::default()
                            .with_width(2.0)
                            .with_color(series)
                            .with_line_join(canvas::LineJoin::Round)
                            .with_line_cap(canvas::LineCap::Round),
                    );
                }
                if data.points.len() <= 60 || data.points.len() == 1 {
                    for (x, y) in &data.points {
                        dot(
                            &mut frame,
                            Point::new(plot.x(*x), plot.y(*y)),
                            series,
                            background,
                        );
                    }
                }
            }
            ChartKind::Scatter => {
                let radius = if data.points.len() > 1_000 { 2.5 } else { 4.0 };
                let fill = if data.points.len() > 1_000 {
                    Color { a: 0.55, ..series }
                } else {
                    series
                };
                for (x, y) in &data.points {
                    let center = Point::new(plot.x(*x), plot.y(*y));
                    if data.points.len() <= 1_000 {
                        frame.fill(&Path::circle(center, radius + 2.0), background);
                    }
                    frame.fill(&Path::circle(center, radius), fill);
                }
            }
        }

        // Baseline axis.
        frame.stroke(
            &Path::line(
                Point::new(plot.area.x, baseline.round() + 0.5),
                Point::new(plot.area.x + plot.area.width, baseline.round() + 0.5),
            ),
            Stroke::default()
                .with_width(1.0)
                .with_color(theme::faint(theme)),
        );

        // Hover layer: crosshair / emphasis + tooltip.
        if let Some(index) = hovered {
            let (x, y) = data.points[index];
            let (anchor, lines) = match data.kind {
                ChartKind::Bar => (
                    Point::new(
                        plot.area.x + plot.band(data.points.len()) * (index as f32 + 0.5),
                        plot.y(y),
                    ),
                    vec![
                        data.labels.get(index).cloned().unwrap_or_default(),
                        format!("{}: {}", data.y_label, full_number(y)),
                    ],
                ),
                ChartKind::Histogram => (
                    Point::new(
                        plot.area.x + plot.band(data.points.len()) * (index as f32 + 0.5),
                        plot.y(y),
                    ),
                    vec![
                        format!(
                            "{} {}",
                            data.x_label,
                            data.labels.get(index).cloned().unwrap_or_default()
                        ),
                        format!("{} rows", thousands(y as usize)),
                    ],
                ),
                ChartKind::Line | ChartKind::Scatter => {
                    let point = Point::new(plot.x(x), plot.y(y));
                    if data.kind == ChartKind::Line {
                        frame.stroke(
                            &Path::line(
                                Point::new(point.x, plot.area.y),
                                Point::new(point.x, plot.area.y + plot.area.height),
                            ),
                            Stroke::default()
                                .with_width(1.0)
                                .with_color(theme::faint(theme)),
                        );
                    }
                    dot(&mut frame, point, series, background);
                    let x_text = match data.x_axis {
                        XAxis::Timestamp => full_time_label(x),
                        _ if looks_like_years(plot.x_min, plot.x_max) => plain_number(x),
                        _ => full_number(x),
                    };
                    (
                        point,
                        vec![
                            format!("{}: {x_text}", data.x_label),
                            format!("{}: {}", data.y_label, full_number(y)),
                        ],
                    )
                }
            };
            tooltip(
                &mut frame,
                bounds.size(),
                anchor,
                &lines,
                text_color,
                background,
            );
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        _state: &ChartState,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> mouse::Interaction {
        if cursor.is_over(bounds) {
            mouse::Interaction::Crosshair
        } else {
            mouse::Interaction::default()
        }
    }
}

/// A bar with a 4px rounded data end and a square baseline end.
fn bar_path(x: f32, y: f32, width: f32, height: f32, grows_up: bool) -> Path {
    let r = 4.0_f32.min(width / 2.0).min(height);
    Path::new(|b| {
        if grows_up {
            b.move_to(Point::new(x, y + height));
            b.line_to(Point::new(x, y + r));
            b.arc_to(Point::new(x, y), Point::new(x + r, y), r);
            b.line_to(Point::new(x + width - r, y));
            b.arc_to(Point::new(x + width, y), Point::new(x + width, y + r), r);
            b.line_to(Point::new(x + width, y + height));
        } else {
            b.move_to(Point::new(x, y));
            b.line_to(Point::new(x, y + height - r));
            b.arc_to(Point::new(x, y + height), Point::new(x + r, y + height), r);
            b.line_to(Point::new(x + width - r, y + height));
            b.arc_to(
                Point::new(x + width, y + height),
                Point::new(x + width, y + height - r),
                r,
            );
            b.line_to(Point::new(x + width, y));
        }
        b.close();
    })
}

/// An 8px marker with a 2px surface ring.
fn dot(frame: &mut Frame, center: Point, color: Color, surface: Color) {
    frame.fill(&Path::circle(center, 6.0), surface);
    frame.fill(&Path::circle(center, 4.0), color);
}

fn tooltip(
    frame: &mut Frame,
    bounds: Size,
    anchor: Point,
    lines: &[String],
    ink: Color,
    paper: Color,
) {
    let width = lines.iter().map(|l| l.chars().count()).max().unwrap_or(4) as f32 * 7.0 + 20.0;
    let height = lines.len() as f32 * 17.0 + 12.0;
    let mut origin = Point::new(anchor.x + 12.0, anchor.y - height - 8.0);
    if origin.x + width > bounds.width {
        origin.x = anchor.x - width - 12.0;
    }
    origin.x = origin.x.max(4.0);
    if origin.y < 4.0 {
        origin.y = anchor.y + 12.0;
    }
    frame.fill(
        &Path::rounded_rectangle(origin, Size::new(width, height), 6.0.into()),
        ink,
    );
    for (i, line) in lines.iter().enumerate() {
        frame.fill_text(Text {
            content: line.clone(),
            position: Point::new(origin.x + 10.0, origin.y + 6.0 + 17.0 * i as f32),
            color: paper,
            size: Pixels(12.0),
            font: if i == 0 {
                Font {
                    weight: font::Weight::Semibold,
                    ..Font::DEFAULT
                }
            } else {
                Font::DEFAULT
            },
            ..Text::default()
        });
    }
}

fn full_number(value: f64) -> String {
    if value.fract() == 0.0 && value.abs() < 1e15 {
        let sign = if value < 0.0 { "-" } else { "" };
        format!("{sign}{}", thousands(value.abs() as usize))
    } else {
        format!("{value:.4}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use iced::widget::canvas::Program;
    use std::sync::Arc;

    use lancedb::arrow::arrow_array::{ArrayRef, Float64Array, Int64Array, StringArray};

    use super::*;
    use crate::test_support::{
        at, bounds, columns_table, moved, numbers_table, render_canvas, wheel,
    };

    const SIZE: (f32, f32) = (640.0, 360.0);

    fn spec(kind: ChartKind, x: Option<usize>, y: Option<usize>) -> ChartSpec {
        ChartSpec { kind, x, y }
    }

    /// `(label text, value float)` rows.
    fn labelled(rows: usize) -> ResultTable {
        columns_table(vec![
            (
                "label",
                Arc::new(StringArray::from_iter_values(
                    (0..rows).map(|i| format!("item {i}")),
                )) as ArrayRef,
            ),
            (
                "value",
                Arc::new(Float64Array::from_iter_values(
                    (0..rows).map(|i| (i as f64 - 3.0) * 2.5),
                )),
            ),
        ])
    }

    #[test]
    fn suggestions_follow_the_result_shape() {
        // Temporal X → line; the identifier column is not the measure.
        let events = numbers_table(20);
        assert_eq!(
            ChartSpec::suggest(&events),
            spec(ChartKind::Line, Some(3), Some(2))
        );
        // Text X → bars.
        assert_eq!(
            ChartSpec::suggest(&labelled(5)),
            spec(ChartKind::Bar, Some(0), Some(1))
        );
        // Two numbers → scatter; one → histogram; only identifiers → still used.
        let pair = columns_table(vec![
            ("a", Arc::new(Int64Array::from(vec![1, 2, 3])) as ArrayRef),
            ("b", Arc::new(Float64Array::from(vec![1.0, 4.0, 9.0]))),
        ]);
        assert_eq!(
            ChartSpec::suggest(&pair),
            spec(ChartKind::Scatter, Some(0), Some(1))
        );
        let single = columns_table(vec![(
            "id",
            Arc::new(Int64Array::from(vec![1, 2, 3])) as ArrayRef,
        )]);
        assert_eq!(
            ChartSpec::suggest(&single),
            spec(ChartKind::Histogram, None, Some(0))
        );
        let text = columns_table(vec![(
            "t",
            Arc::new(StringArray::from(vec!["x"])) as ArrayRef,
        )]);
        assert_eq!(
            ChartSpec::suggest(&text),
            spec(ChartKind::Bar, Some(0), None)
        );

        assert!(!ChartKind::Histogram.uses_x() && ChartKind::Scatter.uses_x());
        let names: Vec<String> = ChartKind::ALL.iter().map(ToString::to_string).collect();
        assert_eq!(names, ["Bar", "Line", "Scatter", "Histogram"]);
        let choice = ColumnChoice {
            index: 1,
            name: "value".into(),
        };
        assert_eq!(choice.to_string(), "value");
    }

    #[test]
    fn builds_each_chart_kind() {
        let bars = build(&labelled(80), &spec(ChartKind::Bar, Some(0), Some(1))).unwrap();
        assert_eq!(bars.points.len(), MAX_BARS);
        assert_eq!(bars.labels[2], "item 2");
        assert_eq!(bars.x_axis, XAxis::Categorical);
        assert!(bars.note.as_deref().unwrap().contains("first 60 of 80"));

        // Nulls (every 7th value) are skipped; sorting is respected.
        let mut events = numbers_table(50);
        events.toggle_sort(2);
        let histogram = build(&events, &spec(ChartKind::Histogram, None, Some(2))).unwrap();
        assert_eq!(histogram.points.len(), HISTOGRAM_BINS);
        let counted: f64 = histogram.points.iter().map(|p| p.1).sum();
        assert_eq!(counted, 43.0);
        assert_eq!(histogram.labels.len(), HISTOGRAM_BINS);
        assert_eq!(histogram.y_label, "count");

        let line = build(&events, &spec(ChartKind::Line, Some(3), Some(2))).unwrap();
        assert_eq!(line.x_axis, XAxis::Timestamp);
        assert_eq!(line.points.len(), 43);
        assert!(
            line.points.windows(2).all(|w| w[0].0 <= w[1].0),
            "sorted by X"
        );

        let scatter = build(&events, &spec(ChartKind::Scatter, Some(0), Some(2))).unwrap();
        assert_eq!(scatter.x_axis, XAxis::Numeric);
        assert!(scatter.note.is_none());

        let many = numbers_table(MAX_POINTS * 2 + 10);
        let sampled = build(&many, &spec(ChartKind::Scatter, Some(0), Some(0))).unwrap();
        assert!(sampled.points.len() <= MAX_POINTS);
        assert!(
            sampled
                .note
                .as_deref()
                .unwrap()
                .starts_with("Sampled 5,000")
        );
    }

    #[test]
    fn explains_charts_it_cannot_draw() {
        let table = labelled(4);
        let error = |kind, x, y| build(&table, &spec(kind, x, y)).unwrap_err();
        assert!(error(ChartKind::Bar, Some(0), None).starts_with("Pick a numeric column"));
        assert_eq!(
            error(ChartKind::Bar, Some(1), Some(0)),
            "label is not numeric"
        );
        assert!(error(ChartKind::Bar, None, Some(1)).starts_with("Pick a column"));
        assert!(error(ChartKind::Line, None, Some(1)).starts_with("Pick a column"));
        assert!(error(ChartKind::Line, Some(0), Some(1)).contains("use a bar chart"));

        let empty = columns_table(vec![
            (
                "a",
                Arc::new(Float64Array::from(vec![None, Some(1.0)])) as ArrayRef,
            ),
            ("b", Arc::new(Float64Array::from(vec![Some(2.0), None]))),
            ("c", Arc::new(Float64Array::from(vec![None::<f64>, None]))),
        ]);
        let error = |kind, x, y| build(&empty, &spec(kind, x, y)).unwrap_err();
        assert_eq!(
            error(ChartKind::Scatter, Some(0), Some(1)),
            "No rows have both X and Y values"
        );
        assert_eq!(
            error(ChartKind::Histogram, None, Some(2)),
            "No numeric values to plot"
        );
    }

    #[test]
    fn time_axis_labels_scale_with_the_span() {
        const DAY: f64 = 86_400e6;
        let start = 1_767_225_600_000_000.0; // 2026-01-01
        assert_eq!(time_label(start, 800.0 * DAY), "2026-01");
        assert_eq!(time_label(start, 10.0 * DAY), "Jan 01");
        assert_eq!(time_label(start + 3_600e6, DAY), "01:00");
        assert_eq!(time_label(f64::MAX, DAY), tick_label(f64::MAX));
        assert_eq!(full_time_label(f64::MAX), tick_label(f64::MAX));
        assert!(nice_ticks(f64::NAN, 1.0, 5).is_empty());
        assert_eq!(tick_label(3_000_000_000.0), "3.0B");
        assert_eq!(tick_label(2.5), "2.5");
        assert_eq!(plain_number(0.00001), "1.000e-5");
        assert_eq!(rebin(&[1.0, 1.0], 1.0, 1.0, 4), vec![0, 0, 2, 0]);
        assert_eq!(min_max(std::iter::empty()), (0.0, 1.0));
        assert_eq!(min_max([2.0, 2.0].into_iter()), (1.0, 3.0));
    }

    #[test]
    fn hover_tracks_the_cursor() {
        let data = build(&labelled(6), &spec(ChartKind::Bar, Some(0), Some(1))).unwrap();
        let chart = Chart {
            data: &data,
            theme: ThemeId::Nord,
        };
        let bounds = bounds(SIZE.0, SIZE.1);
        let mut state = ChartState::default();
        let action = chart.update(&mut state, &moved(100.0, 100.0), bounds, at(100.0, 100.0));
        assert!(action.is_some());
        assert_eq!(state.hover, Some(Point::new(100.0, 100.0)));
        assert!(
            chart
                .update(&mut state, &moved(100.0, 100.0), bounds, at(100.0, 100.0))
                .is_none()
        );
        let left = Event::Mouse(mouse::Event::CursorLeft);
        assert!(
            chart
                .update(&mut state, &left, bounds, Cursor::Unavailable)
                .is_some()
        );
        assert_eq!(state.hover, None);
        assert!(
            chart
                .update(&mut state, &wheel(0.0, 1.0), bounds, at(1.0, 1.0))
                .is_none()
        );
    }

    #[test]
    fn finds_the_hovered_point() {
        let size = Size::new(SIZE.0, SIZE.1);
        let table = numbers_table(10);
        for kind in ChartKind::ALL {
            let data = build(&table, &spec(kind, Some(0), Some(0))).unwrap();
            let chart = Chart {
                data: &data,
                theme: ThemeId::JoustLight,
            };
            let plot = Plot::new(&data, size);
            assert_eq!(chart.hovered(&plot, Point::new(1.0, 1.0)), None, "{kind}");
            let (x, y) = data.points[data.points.len() / 2];
            let target = match kind {
                ChartKind::Bar | ChartKind::Histogram => {
                    Point::new(plot.x(x.round()), plot.area.center_y())
                }
                _ => Point::new(plot.x(x), plot.y(y)),
            };
            assert!(chart.hovered(&plot, target).is_some(), "{kind}");
        }
    }

    #[test]
    fn every_kind_draws_with_a_tooltip() {
        let events = numbers_table(40);
        let charts = [
            (labelled(12), spec(ChartKind::Bar, Some(0), Some(1))),
            (numbers_table(40), spec(ChartKind::Line, Some(3), Some(2))),
            (
                numbers_table(40),
                spec(ChartKind::Scatter, Some(0), Some(2)),
            ),
            (events, spec(ChartKind::Histogram, None, Some(2))),
        ];
        let themes = [ThemeId::JoustLight, ThemeId::JoustDark, ThemeId::Dracula];
        for (i, (table, spec)) in charts.iter().enumerate() {
            let data = build(table, spec).unwrap();
            let plot = Plot::new(&data, Size::new(SIZE.0, SIZE.1));
            // Hover the middle point; and one near the right edge so the
            // tooltip flips to the cursor's left.
            let (x, y) = data.points[data.points.len() / 2];
            let middle = match spec.kind {
                ChartKind::Bar | ChartKind::Histogram => {
                    Point::new(plot.x(x.round()), plot.area.center_y())
                }
                _ => Point::new(plot.x(x), plot.y(y)),
            };
            let (x, y) = *data.points.last().unwrap();
            let edge = match spec.kind {
                ChartKind::Bar | ChartKind::Histogram => {
                    Point::new(plot.x((data.points.len() - 1) as f64), plot.area.y + 4.0)
                }
                _ => Point::new(
                    plot.x(x) - 2.0,
                    plot.y(y)
                        .clamp(plot.area.y + 1.0, plot.area.y + plot.area.height - 1.0),
                ),
            };
            for cursor in [None, Some(middle), Some(edge)] {
                let chart = Chart {
                    data: &data,
                    theme: themes[i % themes.len()],
                };
                assert!(render_canvas(chart, SIZE, cursor, themes[i % themes.len()]).is_empty());
            }
        }
    }

    #[test]
    fn nice_ticks_use_round_steps() {
        assert_eq!(
            nice_ticks(0.0, 100.0, 5),
            vec![0.0, 20.0, 40.0, 60.0, 80.0, 100.0]
        );
        assert_eq!(nice_ticks(3.0, 3.0, 4), vec![2.0, 2.5, 3.0, 3.5, 4.0]);
        let ticks = nice_ticks(-7.0, 13.0, 4);
        assert_eq!(ticks.first(), Some(&-10.0));
        assert!(ticks.contains(&0.0));
    }

    #[test]
    fn tick_labels_are_compact() {
        assert_eq!(tick_label(0.0), "0");
        assert_eq!(tick_label(1500.0), "1,500");
        assert_eq!(tick_label(25_000.0), "25.0k");
        assert_eq!(tick_label(2_500_000.0), "2.5M");
        assert_eq!(tick_label(0.25), "0.25");
        assert_eq!(tick_label(-300.0), "-300");
    }

    #[test]
    fn plain_numbers_have_no_separators() {
        assert_eq!(plain_number(1941.0), "1941");
        assert_eq!(plain_number(1976.88888), "1976.8889");
        assert_eq!(plain_number(-0.5), "-0.5");
        assert!(looks_like_years(1941.0, 2024.0));
        assert!(!looks_like_years(0.0, 2024.0));
        assert_eq!(axis_label(1990.0, true), "1990");
        assert_eq!(axis_label(1990.0, false), "1,990");
    }

    #[test]
    fn identifiers_are_not_default_measures() {
        assert!(is_identifier("id"));
        assert!(is_identifier("user_id"));
        assert!(is_identifier("movieId"));
        assert!(!is_identifier("paid"));
        assert!(!is_identifier("width"));
        // 2026-01-01T00:00:00Z and 2026-01-01T13:30:00Z.
        assert_eq!(full_time_label(1_767_225_600_000_000.0), "2026-01-01");
        assert_eq!(full_time_label(1_767_274_200_000_000.0), "2026-01-01 13:30");
    }

    #[test]
    fn full_numbers() {
        assert_eq!(full_number(1234.0), "1,234");
        assert_eq!(full_number(-5.0), "-5");
        assert_eq!(full_number(0.125), "0.125");
    }
}
