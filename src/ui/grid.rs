//! Virtualised result grid drawn on a canvas.
//!
//! Only visible cells are formatted and drawn, so result sets with hundreds of
//! thousands of rows scroll smoothly. Supports sorting (header click), column
//! resizing (drag a header edge), cell selection (click / arrow keys) and
//! draggable scrollbars.

use iced::alignment;
use iced::keyboard::{self, key};
use iced::mouse::{self, Cursor, ScrollDelta};
use iced::widget::canvas::{self, Action, Event, Frame, Geometry, Path, Stroke, Text};
use iced::widget::text::Alignment;
use iced::{Font, Pixels, Point, Rectangle, Renderer, Size, Theme, Vector, font};

use crate::app::Message;
use crate::results::{ResultTable, thousands};
use crate::theme;

pub const ROW_HEIGHT: f32 = 26.0;
pub const HEADER_HEIGHT: f32 = 44.0;
const FONT_SIZE: f32 = 13.0;
const CHAR_WIDTH: f32 = FONT_SIZE * 0.6;
const CELL_PADDING: f32 = 10.0;
const SCROLLBAR: f32 = 10.0;
const RESIZE_GRIP: f32 = 5.0;
const MIN_WIDTH: f32 = 48.0;

/// Canvas program rendering a [`ResultTable`].
pub struct Grid<'a> {
    pub table: &'a ResultTable,
    /// Selected `(display_row, column)`.
    pub selected: Option<(usize, usize)>,
    /// Changes whenever a new result arrives; resets scroll and widths.
    pub generation: u64,
}

/// Interaction state kept by the canvas between frames.
#[derive(Debug, Default)]
pub struct GridState {
    generation: u64,
    scroll: Vector,
    widths: Vec<f32>,
    hover: Option<Hit>,
    drag: Option<Drag>,
    focused: bool,
    modifiers: keyboard::Modifiers,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Hit {
    Cell(usize, usize),
    Header(usize),
    Resize(usize),
    RowNumber(usize),
    VerticalBar,
    HorizontalBar,
}

#[derive(Debug, Clone, Copy)]
enum Drag {
    Resize {
        column: usize,
        origin: f32,
        width: f32,
    },
    VerticalBar {
        origin: f32,
        scroll: f32,
    },
    HorizontalBar {
        origin: f32,
        scroll: f32,
    },
}

/// Pre-computed geometry for one frame.
struct Layout {
    bounds: Size,
    gutter: f32,
    widths: Vec<f32>,
    content: Size,
    body: Rectangle,
    show_vbar: bool,
    show_hbar: bool,
}

impl Layout {
    fn new(table: &ResultTable, widths: &[f32], bounds: Size) -> Self {
        let digits = thousands(table.row_count().max(1)).len() as f32;
        let gutter = (digits * CHAR_WIDTH + 2.0 * CELL_PADDING).max(44.0);
        let widths: Vec<f32> = table
            .columns
            .iter()
            .enumerate()
            .map(|(i, column)| widths.get(i).copied().unwrap_or(column.width))
            .collect();
        let content = Size::new(widths.iter().sum(), table.row_count() as f32 * ROW_HEIGHT);

        // Scrollbars take space only when needed (and may need each other).
        let avail_w = bounds.width - gutter;
        let avail_h = bounds.height - HEADER_HEIGHT;
        let mut show_hbar = content.width > avail_w;
        let show_vbar = content.height > avail_h - if show_hbar { SCROLLBAR } else { 0.0 };
        if show_vbar && !show_hbar {
            show_hbar = content.width > avail_w - SCROLLBAR;
        }

        let body = Rectangle {
            x: gutter,
            y: HEADER_HEIGHT,
            width: (avail_w - if show_vbar { SCROLLBAR } else { 0.0 }).max(0.0),
            height: (avail_h - if show_hbar { SCROLLBAR } else { 0.0 }).max(0.0),
        };
        Self {
            bounds,
            gutter,
            widths,
            content,
            body,
            show_vbar,
            show_hbar,
        }
    }

    fn max_scroll(&self) -> Vector {
        Vector::new(
            (self.content.width - self.body.width).max(0.0),
            (self.content.height - self.body.height).max(0.0),
        )
    }

    fn clamp(&self, scroll: Vector) -> Vector {
        let max = self.max_scroll();
        Vector::new(scroll.x.clamp(0.0, max.x), scroll.y.clamp(0.0, max.y))
    }

    /// Left edge of `column` in content coordinates.
    fn column_x(&self, column: usize) -> f32 {
        self.widths[..column].iter().sum()
    }

    fn column_at(&self, content_x: f32) -> Option<usize> {
        let mut x = 0.0;
        for (i, width) in self.widths.iter().enumerate() {
            if content_x >= x && content_x < x + width {
                return Some(i);
            }
            x += width;
        }
        None
    }

    fn vbar_thumb(&self, scroll: Vector) -> Rectangle {
        let track = self.body.height;
        let length = (track * track / self.content.height.max(1.0)).clamp(24.0, track);
        let max = self.max_scroll().y.max(1.0);
        Rectangle {
            x: self.bounds.width - SCROLLBAR + 2.0,
            y: self.body.y + (track - length) * (scroll.y / max),
            width: SCROLLBAR - 4.0,
            height: length,
        }
    }

    fn hbar_thumb(&self, scroll: Vector) -> Rectangle {
        let track = self.body.width;
        let length = (track * track / self.content.width.max(1.0)).clamp(24.0, track);
        let max = self.max_scroll().x.max(1.0);
        Rectangle {
            x: self.body.x + (track - length) * (scroll.x / max),
            y: self.bounds.height - SCROLLBAR + 2.0,
            width: length,
            height: SCROLLBAR - 4.0,
        }
    }

    fn hit(&self, point: Point, scroll: Vector, rows: usize) -> Option<Hit> {
        if self.show_vbar && point.x >= self.bounds.width - SCROLLBAR && point.y >= self.body.y {
            return Some(Hit::VerticalBar);
        }
        if self.show_hbar && point.y >= self.bounds.height - SCROLLBAR && point.x >= self.body.x {
            return Some(Hit::HorizontalBar);
        }
        let content_x = point.x - self.gutter + scroll.x;
        if point.y < HEADER_HEIGHT {
            if point.x < self.gutter {
                return None;
            }
            // Grab the right edge of a column to resize it.
            let mut edge = 0.0;
            for (i, width) in self.widths.iter().enumerate() {
                edge += width;
                if (content_x - edge).abs() <= RESIZE_GRIP {
                    return Some(Hit::Resize(i));
                }
            }
            return self.column_at(content_x).map(Hit::Header);
        }
        let row = ((point.y - HEADER_HEIGHT + scroll.y) / ROW_HEIGHT) as usize;
        if row >= rows || point.y > self.body.y + self.body.height {
            return None;
        }
        if point.x < self.gutter {
            return Some(Hit::RowNumber(row));
        }
        self.column_at(content_x)
            .map(|column| Hit::Cell(row, column))
    }
}

impl Grid<'_> {
    fn layout(&self, state: &GridState, bounds: Size) -> Layout {
        let widths = if state.generation == self.generation {
            state.widths.as_slice()
        } else {
            &[]
        };
        Layout::new(self.table, widths, bounds)
    }

    /// Scroll offset that keeps `(row, column)` visible.
    fn reveal(&self, layout: &Layout, scroll: Vector, row: usize, column: usize) -> Vector {
        let mut scroll = scroll;
        let top = row as f32 * ROW_HEIGHT;
        if top < scroll.y {
            scroll.y = top;
        } else if top + ROW_HEIGHT > scroll.y + layout.body.height {
            scroll.y = top + ROW_HEIGHT - layout.body.height;
        }
        let left = layout.column_x(column);
        let right = left + layout.widths[column];
        if left < scroll.x {
            scroll.x = left;
        } else if right > scroll.x + layout.body.width {
            scroll.x = (right - layout.body.width).min(left);
        }
        layout.clamp(scroll)
    }

    fn handle_key(
        &self,
        state: &mut GridState,
        layout: &Layout,
        key: &keyboard::Key,
        modifiers: keyboard::Modifiers,
    ) -> Option<Action<Message>> {
        let rows = self.table.row_count();
        let columns = self.table.columns.len();
        if rows == 0 || columns == 0 {
            return None;
        }
        if let keyboard::Key::Character(c) = key
            && c.as_str() == "c"
            && modifiers.command()
        {
            return Some(Action::publish(Message::CopyCell).and_capture());
        }

        let (row, column) = self.selected.unwrap_or((0, 0));
        let page = ((layout.body.height / ROW_HEIGHT) as usize).max(1);
        let (row, column) = match key.as_ref() {
            keyboard::Key::Named(key::Named::ArrowUp) => (row.saturating_sub(1), column),
            keyboard::Key::Named(key::Named::ArrowDown) => ((row + 1).min(rows - 1), column),
            keyboard::Key::Named(key::Named::ArrowLeft) => (row, column.saturating_sub(1)),
            keyboard::Key::Named(key::Named::ArrowRight) => (row, (column + 1).min(columns - 1)),
            keyboard::Key::Named(key::Named::PageUp) => (row.saturating_sub(page), column),
            keyboard::Key::Named(key::Named::PageDown) => ((row + page).min(rows - 1), column),
            keyboard::Key::Named(key::Named::Home) if modifiers.command() => (0, column),
            keyboard::Key::Named(key::Named::End) if modifiers.command() => (rows - 1, column),
            keyboard::Key::Named(key::Named::Home) => (row, 0),
            keyboard::Key::Named(key::Named::End) => (row, columns - 1),
            _ => return None,
        };
        state.scroll = self.reveal(layout, state.scroll, row, column);
        Some(Action::publish(Message::SelectCell(row, column)).and_capture())
    }
}

impl canvas::Program<Message> for Grid<'_> {
    type State = GridState;

    fn update(
        &self,
        state: &mut GridState,
        event: &Event,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> Option<Action<Message>> {
        if state.generation != self.generation {
            *state = GridState {
                generation: self.generation,
                ..GridState::default()
            };
        }
        let layout = self.layout(state, bounds.size());
        state.scroll = layout.clamp(state.scroll);
        let position = cursor.position_in(bounds);
        let rows = self.table.row_count();

        match event {
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                position?;
                let (mut dx, mut dy) = match delta {
                    ScrollDelta::Lines { x, y } => (-x * ROW_HEIGHT * 3.0, -y * ROW_HEIGHT * 3.0),
                    ScrollDelta::Pixels { x, y } => (-x, -y),
                };
                if dx == 0.0 && state.modifiers.shift() {
                    std::mem::swap(&mut dx, &mut dy);
                }
                state.scroll = layout.clamp(state.scroll + Vector::new(dx, dy));
                Some(Action::request_redraw().and_capture())
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                // While dragging, track the cursor even outside the canvas.
                let absolute = cursor
                    .position()
                    .map(|p| Point::new(p.x - bounds.x, p.y - bounds.y));
                if let (Some(drag), Some(point)) = (state.drag, absolute) {
                    match drag {
                        Drag::Resize {
                            column,
                            origin,
                            width,
                        } => {
                            let widths = if state.widths.len() == layout.widths.len() {
                                &mut state.widths
                            } else {
                                state.widths = layout.widths.clone();
                                &mut state.widths
                            };
                            widths[column] = (width + point.x - origin).max(MIN_WIDTH);
                        }
                        Drag::VerticalBar { origin, scroll } => {
                            let track = layout.body.height - layout.vbar_thumb(state.scroll).height;
                            let ratio = layout.max_scroll().y / track.max(1.0);
                            state.scroll.y = scroll + (point.y - origin) * ratio;
                        }
                        Drag::HorizontalBar { origin, scroll } => {
                            let track = layout.body.width - layout.hbar_thumb(state.scroll).width;
                            let ratio = layout.max_scroll().x / track.max(1.0);
                            state.scroll.x = scroll + (point.x - origin) * ratio;
                        }
                    }
                    state.scroll = layout.clamp(state.scroll);
                    return Some(Action::request_redraw().and_capture());
                }
                let hover = position.and_then(|p| layout.hit(p, state.scroll, rows));
                if hover != state.hover {
                    state.hover = hover;
                    return Some(Action::request_redraw());
                }
                None
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let Some(point) = position else {
                    state.focused = false;
                    return None;
                };
                state.focused = true;
                match layout.hit(point, state.scroll, rows) {
                    Some(Hit::Resize(column)) => {
                        state.drag = Some(Drag::Resize {
                            column,
                            origin: point.x,
                            width: layout.widths[column],
                        });
                        Some(Action::capture())
                    }
                    Some(Hit::Header(column)) => {
                        Some(Action::publish(Message::SortColumn(column)).and_capture())
                    }
                    Some(Hit::Cell(row, column)) => {
                        Some(Action::publish(Message::SelectCell(row, column)).and_capture())
                    }
                    Some(Hit::RowNumber(row)) => Some(
                        Action::publish(Message::SelectCell(row, self.selected.map_or(0, |s| s.1)))
                            .and_capture(),
                    ),
                    Some(Hit::VerticalBar) => {
                        let thumb = layout.vbar_thumb(state.scroll);
                        if point.y < thumb.y || point.y > thumb.y + thumb.height {
                            // Clicking the track pages towards the click.
                            let direction = if point.y < thumb.y { -1.0 } else { 1.0 };
                            state.scroll.y += direction * layout.body.height;
                            state.scroll = layout.clamp(state.scroll);
                        }
                        state.drag = Some(Drag::VerticalBar {
                            origin: point.y,
                            scroll: state.scroll.y,
                        });
                        Some(Action::request_redraw().and_capture())
                    }
                    Some(Hit::HorizontalBar) => {
                        let thumb = layout.hbar_thumb(state.scroll);
                        if point.x < thumb.x || point.x > thumb.x + thumb.width {
                            let direction = if point.x < thumb.x { -1.0 } else { 1.0 };
                            state.scroll.x += direction * layout.body.width;
                            state.scroll = layout.clamp(state.scroll);
                        }
                        state.drag = Some(Drag::HorizontalBar {
                            origin: point.x,
                            scroll: state.scroll.x,
                        });
                        Some(Action::request_redraw().and_capture())
                    }
                    None => Some(Action::capture()),
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => state
                .drag
                .take()
                .map(|_| Action::request_redraw().and_capture()),
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers;
                None
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if state.focused =>
            {
                self.handle_key(state, &layout, key, *modifiers)
            }
            _ => None,
        }
    }

    fn draw(
        &self,
        state: &GridState,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: Cursor,
    ) -> Vec<Geometry> {
        let layout = self.layout(state, bounds.size());
        let scroll = layout.clamp(if state.generation == self.generation {
            state.scroll
        } else {
            Vector::ZERO
        });
        let hover = (state.generation == self.generation)
            .then_some(state.hover)
            .flatten();

        let palette = theme.extended_palette();
        let background = palette.background.base.color;
        let text_color = palette.background.base.text;
        let muted = theme::muted(theme);
        let faint = theme::faint(theme);
        let divider = theme::divider(theme);
        let chrome = theme::chrome(theme);
        let hover_color = theme::hover(theme);
        let selection = theme::selection(theme);
        let accent = palette.primary.base.color;

        let mut frame = Frame::new(renderer, bounds.size());
        frame.fill_rectangle(Point::ORIGIN, bounds.size(), background);

        let rows = self.table.row_count();
        let first_row = (scroll.y / ROW_HEIGHT) as usize;
        let last_row = (((scroll.y + layout.body.height) / ROW_HEIGHT).ceil() as usize).min(rows);

        // Visible columns: (index, screen x, width).
        let mut visible_columns = Vec::new();
        let mut x = layout.gutter - scroll.x;
        for (i, width) in layout.widths.iter().enumerate() {
            if x + width > layout.gutter && x < layout.body.x + layout.body.width {
                visible_columns.push((i, x, *width));
            }
            x += width;
        }
        let content_right = (x).min(layout.body.x + layout.body.width);

        let row_y = |row: usize| HEADER_HEIGHT + row as f32 * ROW_HEIGHT - scroll.y;
        let hovered_row = match hover {
            Some(Hit::Cell(row, _)) | Some(Hit::RowNumber(row)) => Some(row),
            _ => None,
        };
        let selected_row = self.selected.map(|(row, _)| row);

        // Fills and lines are clipped by hand: iced_wgpu emits `with_clip`
        // meshes beneath the parent frame's own geometry, so only text is
        // drawn inside clipped sub-frames.
        let rows_area = layout.body_with_gutter();

        // Row backgrounds.
        for row in first_row..last_row {
            let fill = if Some(row) == selected_row {
                Some(selection)
            } else if Some(row) == hovered_row {
                Some(hover_color)
            } else if row % 2 == 1 {
                Some(theme::mix(background, chrome, 0.5))
            } else {
                None
            };
            if let Some(fill) = fill {
                let rect = Rectangle::new(
                    Point::new(0.0, row_y(row)),
                    Size::new(content_right, ROW_HEIGHT),
                );
                fill_clipped(&mut frame, rect, rows_area, fill);
            }
        }

        // Cells, clipped column by column.
        let mut formatters = Vec::with_capacity(visible_columns.len());
        for (column, _, _) in &visible_columns {
            formatters.push(crate::results::CellFormatter::new(
                self.table.column(*column).as_ref(),
            ));
        }
        for ((column, x, width), formatter) in visible_columns.iter().zip(&formatters) {
            let meta = &self.table.columns[*column];
            let clip = Rectangle {
                x: x.max(layout.gutter),
                y: layout.body.y,
                width: (x + width).min(layout.body.x + layout.body.width) - x.max(layout.gutter),
                height: layout.body.height,
            };
            let max_chars = ((width - 2.0 * CELL_PADDING) / CHAR_WIDTH).max(1.0) as usize;
            frame.with_clip(clip, |frame| {
                for row in first_row..last_row {
                    let source = self.table.source_row(row);
                    let (content, is_null) = match formatter {
                        Some(formatter) => {
                            let array = self.table.column(*column);
                            let content = formatter.format(source);
                            (content, array.is_null(source))
                        }
                        None => ("?".to_string(), false),
                    };
                    let content = truncate(&content, max_chars);
                    let y = row_y(row) + ROW_HEIGHT / 2.0;
                    let (position, align_x) = if meta.numeric {
                        (Point::new(x + width - CELL_PADDING, y), Alignment::Right)
                    } else {
                        (Point::new(x + CELL_PADDING, y), Alignment::Left)
                    };
                    frame.fill_text(Text {
                        content,
                        position,
                        color: if is_null { faint } else { text_color },
                        size: Pixels(FONT_SIZE),
                        font: Font::MONOSPACE,
                        align_x,
                        align_y: alignment::Vertical::Center,
                        ..Text::default()
                    });
                }
            });
        }

        // Horizontal row lines and vertical column lines (1px fills).
        let hairline = Stroke::default().with_width(1.0).with_color(divider);
        for row in first_row..=last_row {
            let rect = Rectangle::new(
                Point::new(0.0, row_y(row).round()),
                Size::new(content_right, 1.0),
            );
            fill_clipped(&mut frame, rect, rows_area, divider);
        }
        let rows_bottom = row_y(rows).min(layout.body.y + layout.body.height);
        for (_, x, width) in &visible_columns {
            let rect = Rectangle::new(
                Point::new((x + width).round(), layout.body.y),
                Size::new(1.0, (rows_bottom - layout.body.y).max(0.0)),
            );
            fill_clipped(&mut frame, rect, layout.body, divider);
        }

        // Selected cell outline (four 2px edges).
        if let Some((row, column)) = self.selected
            && let Some((_, x, width)) = visible_columns.iter().find(|(c, _, _)| *c == column)
        {
            let (x, y, w, h) = (*x, row_y(row), *width, ROW_HEIGHT);
            for edge in [
                Rectangle::new(Point::new(x, y), Size::new(w, 2.0)),
                Rectangle::new(Point::new(x, y + h - 2.0), Size::new(w, 2.0)),
                Rectangle::new(Point::new(x, y), Size::new(2.0, h)),
                Rectangle::new(Point::new(x + w - 2.0, y), Size::new(2.0, h)),
            ] {
                fill_clipped(&mut frame, edge, layout.body, accent);
            }
        }

        // Row-number gutter.
        frame.fill_rectangle(
            Point::new(0.0, HEADER_HEIGHT),
            Size::new(layout.gutter, layout.body.height),
            chrome,
        );
        frame.with_clip(
            Rectangle::new(
                Point::new(0.0, HEADER_HEIGHT),
                Size::new(layout.gutter, layout.body.height),
            ),
            |frame| {
                for row in first_row..last_row {
                    frame.fill_text(Text {
                        content: thousands(row + 1),
                        position: Point::new(
                            layout.gutter - CELL_PADDING,
                            row_y(row) + ROW_HEIGHT / 2.0,
                        ),
                        color: if Some(row) == selected_row {
                            text_color
                        } else {
                            faint
                        },
                        size: Pixels(FONT_SIZE - 1.0),
                        font: Font::MONOSPACE,
                        align_x: Alignment::Right,
                        align_y: alignment::Vertical::Center,
                        ..Text::default()
                    });
                }
            },
        );
        frame.stroke(
            &Path::line(
                Point::new(layout.gutter - 0.5, 0.0),
                Point::new(layout.gutter - 0.5, layout.body.y + layout.body.height),
            ),
            hairline,
        );

        // Header.
        frame.fill_rectangle(
            Point::ORIGIN,
            Size::new(bounds.width, HEADER_HEIGHT),
            chrome,
        );
        let bold = Font {
            weight: font::Weight::Semibold,
            ..Font::DEFAULT
        };
        for (column, x, width) in &visible_columns {
            let meta = &self.table.columns[*column];
            let clip = Rectangle {
                x: x.max(layout.gutter),
                y: 0.0,
                width: ((x + width).min(layout.body.x + layout.body.width) - x.max(layout.gutter))
                    .max(0.0),
                height: HEADER_HEIGHT,
            };
            let hovered = matches!(hover, Some(Hit::Header(c)) if c == *column);
            let sort = self
                .table
                .sort
                .filter(|(c, _)| c == column)
                .map(|(_, asc)| asc);
            if hovered {
                let rect = Rectangle::new(Point::new(*x, 0.0), Size::new(*width, HEADER_HEIGHT));
                fill_clipped(&mut frame, rect, clip, hover_color);
            }
            frame.with_clip(clip, |frame| {
                let indicator = match sort {
                    Some(true) => " ▲",
                    Some(false) => " ▼",
                    None => "",
                };
                let max_chars = ((width - 2.0 * CELL_PADDING) / CHAR_WIDTH).max(1.0) as usize;
                frame.fill_text(Text {
                    content: truncate(&format!("{}{indicator}", meta.name), max_chars),
                    position: Point::new(x + CELL_PADDING, 14.0),
                    color: text_color,
                    size: Pixels(FONT_SIZE),
                    font: bold,
                    align_y: alignment::Vertical::Center,
                    ..Text::default()
                });
                frame.fill_text(Text {
                    content: truncate(&meta.type_label, max_chars + 2),
                    position: Point::new(x + CELL_PADDING, 31.0),
                    color: muted,
                    size: Pixels(11.0),
                    font: Font::MONOSPACE,
                    align_y: alignment::Vertical::Center,
                    ..Text::default()
                });
            });
            let edge = (x + width).round() + 0.5;
            if edge > layout.gutter {
                let resizing = matches!(hover, Some(Hit::Resize(c)) if c == *column)
                    || matches!(state.drag, Some(Drag::Resize { column: c, .. }) if c == *column);
                frame.stroke(
                    &Path::line(Point::new(edge, 6.0), Point::new(edge, HEADER_HEIGHT - 6.0)),
                    Stroke::default()
                        .with_width(if resizing { 2.0 } else { 1.0 })
                        .with_color(if resizing { accent } else { divider }),
                );
            }
        }
        frame.fill_text(Text {
            content: "#".into(),
            position: Point::new(layout.gutter - CELL_PADDING, HEADER_HEIGHT / 2.0),
            color: faint,
            size: Pixels(FONT_SIZE - 1.0),
            font: Font::MONOSPACE,
            align_x: Alignment::Right,
            align_y: alignment::Vertical::Center,
            ..Text::default()
        });
        frame.stroke(
            &Path::line(
                Point::new(0.0, HEADER_HEIGHT - 0.5),
                Point::new(bounds.width, HEADER_HEIGHT - 0.5),
            ),
            hairline,
        );

        // Scrollbars.
        let thumb_color = theme::mix(background, text_color, 0.28);
        let thumb_active = theme::mix(background, text_color, 0.45);
        if layout.show_vbar {
            let thumb = layout.vbar_thumb(scroll);
            let active = matches!(hover, Some(Hit::VerticalBar))
                || matches!(state.drag, Some(Drag::VerticalBar { .. }));
            frame.fill(
                &Path::rounded_rectangle(thumb.position(), thumb.size(), 3.0.into()),
                if active { thumb_active } else { thumb_color },
            );
        }
        if layout.show_hbar {
            let thumb = layout.hbar_thumb(scroll);
            let active = matches!(hover, Some(Hit::HorizontalBar))
                || matches!(state.drag, Some(Drag::HorizontalBar { .. }));
            frame.fill(
                &Path::rounded_rectangle(thumb.position(), thumb.size(), 3.0.into()),
                if active { thumb_active } else { thumb_color },
            );
        }

        if rows == 0 {
            frame.fill_text(Text {
                content: "No rows".into(),
                position: Point::new(bounds.width / 2.0, HEADER_HEIGHT + 40.0),
                color: muted,
                size: Pixels(14.0),
                align_x: Alignment::Center,
                align_y: alignment::Vertical::Center,
                ..Text::default()
            });
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &GridState,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> mouse::Interaction {
        match state.drag {
            Some(Drag::Resize { .. }) => return mouse::Interaction::ResizingHorizontally,
            Some(_) => return mouse::Interaction::Grabbing,
            None => {}
        }
        if !cursor.is_over(bounds) {
            return mouse::Interaction::default();
        }
        match state.hover {
            Some(Hit::Resize(_)) => mouse::Interaction::ResizingHorizontally,
            Some(Hit::Header(_)) => mouse::Interaction::Pointer,
            Some(Hit::Cell(..)) | Some(Hit::RowNumber(_)) => mouse::Interaction::Cell,
            Some(Hit::VerticalBar) | Some(Hit::HorizontalBar) => mouse::Interaction::Grab,
            None => mouse::Interaction::default(),
        }
    }
}

/// Fills the part of `rect` that lies inside `clip`.
fn fill_clipped(frame: &mut Frame, rect: Rectangle, clip: Rectangle, color: iced::Color) {
    if let Some(visible) = rect.intersection(&clip) {
        frame.fill_rectangle(visible.position(), visible.size(), color);
    }
}

impl Layout {
    /// Body plus the row-number gutter (for full-width row decorations).
    fn body_with_gutter(&self) -> Rectangle {
        Rectangle {
            x: 0.0,
            y: self.body.y,
            width: self.body.x + self.body.width,
            height: self.body.height,
        }
    }
}

/// Truncates to `max_chars` characters, adding an ellipsis when shortened.
/// Newlines are shown as `↵` so rows stay single-line.
pub fn truncate(value: &str, max_chars: usize) -> String {
    let single_line = || value.chars().map(|c| if c == '\n' { '↵' } else { c });
    if value.chars().count() <= max_chars {
        single_line().collect()
    } else {
        let mut out: String = single_line().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::*;
    use crate::theme::ThemeId;
    use iced::widget::canvas::Program;

    const SIZE: (f32, f32) = (420.0, 240.0);

    fn setup(rows: usize) -> (ResultTable, GridState) {
        (numbers_table(rows), GridState::default())
    }

    fn layout(table: &ResultTable) -> Layout {
        Layout::new(table, &[], Size::new(SIZE.0, SIZE.1))
    }

    /// Screen point in the middle of `(row, column)` with no scrolling.
    fn cell_point(layout: &Layout, row: usize, column: usize) -> (f32, f32) {
        (
            layout.gutter + layout.column_x(column) + layout.widths[column] / 2.0,
            HEADER_HEIGHT + row as f32 * ROW_HEIGHT + ROW_HEIGHT / 2.0,
        )
    }

    #[test]
    fn draws_hover_sorting_resizing_and_empty_results() {
        let size = (640.0, 300.0);
        let mut table = numbers_table(60);
        let layout = Layout::new(&table, &[], Size::new(size.0, size.1));
        let (x, y) = cell_point(&layout, 2, 1);
        // Unsorted, no cursor (first draw uses default widths); hovering a
        // cell; then sorted both ways while hovering a header.
        for (sort_clicks, cursor) in [
            (0, None),
            (0, Some(Point::new(x, y))),
            (1, Some(Point::new(x, 10.0))),
            (1, Some(Point::new(5.0, y))),
        ] {
            for _ in 0..sort_clicks {
                table.toggle_sort(1);
            }
            let grid = Grid {
                table: &table,
                selected: Some((4, 2)),
                generation: 1,
            };
            assert!(render_canvas(grid, size, cursor, ThemeId::JoustLight).is_empty());
        }

        // Mid-resize: the dragged column's edge is highlighted.
        let edge = layout.gutter + layout.widths[0];
        let grid = Grid {
            table: &table,
            selected: None,
            generation: 1,
        };
        let mut ui = iced_test::Simulator::with_size(
            iced::Settings::default(),
            size,
            iced::widget::canvas(grid)
                .width(iced::Fill)
                .height(iced::Fill),
        );
        ui.point_at(Point::new(edge, 10.0));
        let _ = ui.simulate([moved(edge, 10.0), press(), moved(edge + 30.0, 10.0)]);
        ui.point_at(Point::new(edge + 30.0, 10.0));
        ui.snapshot(&ThemeId::JoustDark.to_theme()).unwrap();

        let empty = numbers_table(0);
        let grid = Grid {
            table: &empty,
            selected: None,
            generation: 1,
        };
        assert!(render_canvas(grid, size, None, ThemeId::Nord).is_empty());
    }

    #[test]
    fn remaining_keys_scrolls_and_cursors() {
        let wide = table_with_many_columns();
        let b = bounds(SIZE.0, SIZE.1);
        let none = keyboard::Modifiers::empty();

        // Home after scrolling right brings the first column back into view.
        let mut state = GridState {
            focused: true,
            generation: 1,
            ..GridState::default()
        };
        let grid = Grid {
            table: &wide,
            selected: Some((3, 15)),
            generation: 1,
        };
        let _ = grid.update(
            &mut state,
            &key_press(named(key::Named::End), none),
            b,
            at(50.0, 50.0),
        );
        assert!(state.scroll.x > 0.0);
        let grid = Grid {
            table: &wide,
            selected: Some((3, 19)),
            generation: 1,
        };
        let _ = grid.update(
            &mut state,
            &key_press(named(key::Named::Home), none),
            b,
            at(50.0, 50.0),
        );
        assert_eq!(state.scroll.x, 0.0);

        // Page up, pixel scrolling.
        let table = numbers_table(100);
        let mut state = GridState {
            focused: true,
            generation: 1,
            ..GridState::default()
        };
        let grid = Grid {
            table: &table,
            selected: Some((50, 0)),
            generation: 1,
        };
        let action = grid.update(
            &mut state,
            &key_press(named(key::Named::PageUp), none),
            b,
            at(50.0, 50.0),
        );
        assert!(matches!(published(action), Some(Message::SelectCell(row, 0)) if row < 50));
        let pixels = Event::Mouse(mouse::Event::WheelScrolled {
            delta: ScrollDelta::Pixels { x: 0.0, y: -30.0 },
        });
        let before = state.scroll.y;
        assert!(captured(grid.update(
            &mut state,
            &pixels,
            b,
            at(100.0, 100.0)
        )));
        assert_eq!(state.scroll.y, before + 30.0);

        // Keys do nothing on an empty result.
        let empty = numbers_table(0);
        let grid = Grid {
            table: &empty,
            selected: None,
            generation: 1,
        };
        let mut state = GridState {
            focused: true,
            generation: 1,
            ..GridState::default()
        };
        assert!(
            grid.update(
                &mut state,
                &key_press(named(key::Named::ArrowDown), none),
                b,
                at(5.0, 5.0)
            )
            .is_none()
        );

        // Cursor shapes for scrollbars, empty space and an active resize.
        let grid = Grid {
            table: &table,
            selected: None,
            generation: 1,
        };
        let mut state = GridState {
            generation: 1,
            ..GridState::default()
        };
        state.hover = Some(Hit::VerticalBar);
        assert_eq!(
            grid.mouse_interaction(&state, b, at(10.0, 10.0)),
            mouse::Interaction::Grab
        );
        state.hover = None;
        assert_eq!(
            grid.mouse_interaction(&state, b, at(10.0, 10.0)),
            mouse::Interaction::default()
        );
        state.drag = Some(Drag::Resize {
            column: 0,
            origin: 0.0,
            width: 100.0,
        });
        assert_eq!(
            grid.mouse_interaction(&state, b, at(10.0, 10.0)),
            mouse::Interaction::ResizingHorizontally
        );
    }

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("a\nb", 5), "a↵b");
        assert_eq!(truncate("ñandú", 3), "ña…");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn layout_sizes_body_and_scrollbars() {
        let (table, _) = setup(100);
        let layout = layout(&table);
        assert!(layout.show_vbar, "100 rows overflow 240px");
        assert_eq!(layout.content.height, 100.0 * ROW_HEIGHT);
        assert_eq!(
            layout.max_scroll().y,
            layout.content.height - layout.body.height
        );
        let clamped = layout.clamp(Vector::new(-50.0, 1e9));
        assert_eq!(clamped, Vector::new(0.0, layout.max_scroll().y));
        assert_eq!(layout.column_at(-1.0), None);
        assert_eq!(layout.column_at(0.0), Some(0));

        let small = Layout::new(&numbers_table(2), &[], Size::new(2_000.0, 600.0));
        assert!(!small.show_vbar && !small.show_hbar);
        assert_eq!(small.max_scroll(), Vector::ZERO);
    }

    #[test]
    fn hit_testing() {
        let (table, _) = setup(10);
        let layout = layout(&table);
        let (x, y) = cell_point(&layout, 2, 1);
        assert_eq!(
            layout.hit(Point::new(x, y), Vector::ZERO, 10),
            Some(Hit::Cell(2, 1))
        );
        assert_eq!(
            layout.hit(Point::new(x, 10.0), Vector::ZERO, 10),
            Some(Hit::Header(1))
        );
        assert_eq!(
            layout.hit(Point::new(5.0, y), Vector::ZERO, 10),
            Some(Hit::RowNumber(2))
        );
        let edge = layout.gutter + layout.widths[0];
        assert_eq!(
            layout.hit(Point::new(edge, 10.0), Vector::ZERO, 10),
            Some(Hit::Resize(0))
        );
        assert_eq!(
            layout.hit(Point::new(5.0, 10.0), Vector::ZERO, 10),
            None,
            "corner"
        );
        // Past the last row.
        assert_eq!(layout.hit(Point::new(x, 200.0), Vector::ZERO, 3), None);
    }

    #[test]
    fn wheel_scrolls_inside_bounds_only() {
        let (table, mut state) = setup(100);
        let grid = Grid {
            table: &table,
            selected: None,
            generation: 1,
        };
        let action = grid.update(
            &mut state,
            &wheel(0.0, -1.0),
            bounds(SIZE.0, SIZE.1),
            at(100.0, 100.0),
        );
        assert!(captured(action));
        assert_eq!(state.scroll.y, 3.0 * ROW_HEIGHT);

        let outside = grid.update(
            &mut state,
            &wheel(0.0, -1.0),
            bounds(SIZE.0, SIZE.1),
            at(900.0, 900.0),
        );
        assert!(outside.is_none());
        assert_eq!(state.scroll.y, 3.0 * ROW_HEIGHT);

        // Shift turns vertical wheel movement horizontal.
        let wide = table_with_many_columns();
        let grid = Grid {
            table: &wide,
            selected: None,
            generation: 2,
        };
        let mut state = GridState::default();
        grid.update(
            &mut state,
            &modifiers(keyboard::Modifiers::SHIFT),
            bounds(SIZE.0, SIZE.1),
            at(100.0, 100.0),
        );
        grid.update(
            &mut state,
            &wheel(0.0, -1.0),
            bounds(SIZE.0, SIZE.1),
            at(100.0, 100.0),
        );
        assert!(state.scroll.x > 0.0);
        assert_eq!(state.scroll.y, 0.0);
    }

    fn table_with_many_columns() -> ResultTable {
        use lancedb::arrow::arrow_array::{ArrayRef, Int32Array, RecordBatch};
        use lancedb::arrow::arrow_schema::{Field, Schema};
        let fields: Vec<Field> = (0..20)
            .map(|i| {
                Field::new(
                    format!("a_rather_long_column_{i}"),
                    lancedb::arrow::arrow_schema::DataType::Int32,
                    false,
                )
            })
            .collect();
        let columns: Vec<ArrayRef> = (0..20)
            .map(|_| std::sync::Arc::new(Int32Array::from_iter_values(0..50)) as ArrayRef)
            .collect();
        table(RecordBatch::try_new(std::sync::Arc::new(Schema::new(fields)), columns).unwrap())
    }

    #[test]
    fn clicks_sort_select_and_focus() {
        let (table, mut state) = setup(10);
        let grid = Grid {
            table: &table,
            selected: None,
            generation: 1,
        };
        let layout = layout(&table);
        let b = bounds(SIZE.0, SIZE.1);

        let (x, y) = cell_point(&layout, 3, 2);
        let action = grid.update(&mut state, &press(), b, at(x, y));
        assert!(matches!(published(action), Some(Message::SelectCell(3, 2))));
        assert!(state.focused);

        let action = grid.update(&mut state, &press(), b, at(x, 10.0));
        assert!(matches!(published(action), Some(Message::SortColumn(2))));

        let action = grid.update(&mut state, &press(), b, at(5.0, y));
        assert!(matches!(published(action), Some(Message::SelectCell(3, 0))));

        // A click outside the grid drops keyboard focus.
        assert!(
            grid.update(&mut state, &press(), b, at(900.0, 900.0))
                .is_none()
        );
        assert!(!state.focused);
    }

    #[test]
    fn dragging_a_header_edge_resizes_the_column() {
        let (table, mut state) = setup(10);
        let grid = Grid {
            table: &table,
            selected: None,
            generation: 1,
        };
        let layout = layout(&table);
        let b = bounds(SIZE.0, SIZE.1);
        let edge = layout.gutter + layout.widths[0];

        grid.update(&mut state, &moved(edge, 10.0), b, at(edge, 10.0));
        assert_eq!(state.hover, Some(Hit::Resize(0)));
        assert_eq!(
            grid.mouse_interaction(&state, b, at(edge, 10.0)),
            mouse::Interaction::ResizingHorizontally
        );

        grid.update(&mut state, &press(), b, at(edge, 10.0));
        assert!(matches!(state.drag, Some(Drag::Resize { column: 0, .. })));
        grid.update(
            &mut state,
            &moved(edge + 40.0, 10.0),
            b,
            at(edge + 40.0, 10.0),
        );
        assert!((state.widths[0] - (layout.widths[0] + 40.0)).abs() < 1e-3);
        // Never narrower than the minimum.
        grid.update(&mut state, &moved(-500.0, 10.0), b, at(-500.0, 10.0));
        assert_eq!(state.widths[0], MIN_WIDTH);
        assert!(captured(grid.update(
            &mut state,
            &release(),
            b,
            at(0.0, 0.0)
        )));
        assert!(state.drag.is_none());
    }

    #[test]
    fn scrollbar_track_pages_and_thumb_drags() {
        let (table, mut state) = setup(200);
        let grid = Grid {
            table: &table,
            selected: None,
            generation: 1,
        };
        let layout = layout(&table);
        let b = bounds(SIZE.0, SIZE.1);
        let bar_x = SIZE.0 - SCROLLBAR / 2.0;

        // Click the track below the thumb: one page down.
        grid.update(&mut state, &press(), b, at(bar_x, SIZE.1 - 20.0));
        assert_eq!(state.scroll.y, layout.body.height);
        grid.update(&mut state, &release(), b, at(bar_x, SIZE.1 - 20.0));

        // Drag the thumb to the bottom.
        let thumb = layout.vbar_thumb(state.scroll);
        let grab = thumb.y + thumb.height / 2.0;
        grid.update(&mut state, &press(), b, at(bar_x, grab));
        assert_eq!(
            grid.mouse_interaction(&state, b, at(bar_x, grab)),
            mouse::Interaction::Grabbing
        );
        grid.update(&mut state, &moved(bar_x, 10_000.0), b, at(bar_x, 10_000.0));
        assert_eq!(state.scroll.y, layout.max_scroll().y);
        grid.update(&mut state, &release(), b, at(bar_x, 10_000.0));

        // Page up again from the bottom.
        grid.update(&mut state, &press(), b, at(bar_x, HEADER_HEIGHT + 2.0));
        assert_eq!(state.scroll.y, layout.max_scroll().y - layout.body.height);
    }

    #[test]
    fn horizontal_scrollbar() {
        let wide = table_with_many_columns();
        let grid = Grid {
            table: &wide,
            selected: None,
            generation: 1,
        };
        let mut state = GridState::default();
        let layout = Layout::new(&wide, &[], Size::new(SIZE.0, SIZE.1));
        assert!(layout.show_hbar);
        let b = bounds(SIZE.0, SIZE.1);
        let bar_y = SIZE.1 - SCROLLBAR / 2.0;
        grid.update(&mut state, &press(), b, at(SIZE.0 - 30.0, bar_y));
        assert_eq!(state.scroll.x, layout.body.width);
        grid.update(&mut state, &release(), b, at(0.0, 0.0));
        let thumb = layout.hbar_thumb(state.scroll);
        grid.update(&mut state, &press(), b, at(thumb.x + 2.0, bar_y));
        grid.update(
            &mut state,
            &moved(-10_000.0, bar_y),
            b,
            at(-10_000.0, bar_y),
        );
        assert_eq!(state.scroll.x, 0.0);
    }

    #[test]
    fn keyboard_navigation_and_copy() {
        let (table, mut state) = setup(100);
        let b = bounds(SIZE.0, SIZE.1);
        let key = |grid: &Grid, state: &mut GridState, k: keyboard::Key, m: keyboard::Modifiers| {
            published(grid.update(state, &key_press(k, m), b, at(50.0, 50.0)))
        };
        let none = keyboard::Modifiers::empty();

        // Unfocused: keys are ignored.
        let grid = Grid {
            table: &table,
            selected: Some((5, 1)),
            generation: 1,
        };
        assert!(key(&grid, &mut state, named(key::Named::ArrowDown), none).is_none());
        state.focused = true;

        let cases = [
            (named(key::Named::ArrowDown), none, (6, 1)),
            (named(key::Named::ArrowUp), none, (4, 1)),
            (named(key::Named::ArrowLeft), none, (5, 0)),
            (named(key::Named::ArrowRight), none, (5, 2)),
            (named(key::Named::Home), none, (5, 0)),
            (named(key::Named::End), none, (5, 3)),
            (named(key::Named::Home), keyboard::Modifiers::CTRL, (0, 1)),
            (named(key::Named::End), keyboard::Modifiers::CTRL, (99, 1)),
        ];
        for (k, m, expected) in cases {
            match key(&grid, &mut state, k.clone(), m) {
                Some(Message::SelectCell(row, column)) => {
                    assert_eq!((row, column), expected, "{k:?}")
                }
                other => panic!("{k:?} gave {other:?}"),
            }
        }
        let Some(Message::SelectCell(row, _)) =
            key(&grid, &mut state, named(key::Named::PageDown), none)
        else {
            panic!("page down");
        };
        assert!(row > 6);
        // Moving to the last row scrolls it into view.
        key(
            &grid,
            &mut state,
            named(key::Named::End),
            keyboard::Modifiers::CTRL,
        );
        assert_eq!(state.scroll.y, layout(&table).max_scroll().y);

        assert!(matches!(
            key(&grid, &mut state, character("c"), keyboard::Modifiers::CTRL),
            Some(Message::CopyCell)
        ));
        assert!(key(&grid, &mut state, character("x"), none).is_none());

        let empty = numbers_table(0);
        let grid = Grid {
            table: &empty,
            selected: None,
            generation: 3,
        };
        let mut state = GridState {
            focused: true,
            ..GridState::default()
        };
        assert!(key(&grid, &mut state, named(key::Named::ArrowDown), none).is_none());
    }

    #[test]
    fn hover_and_cursor_shapes() {
        let (table, mut state) = setup(10);
        let grid = Grid {
            table: &table,
            selected: None,
            generation: 1,
        };
        let layout = layout(&table);
        let b = bounds(SIZE.0, SIZE.1);
        let (x, y) = cell_point(&layout, 1, 1);
        assert!(
            grid.update(&mut state, &moved(x, y), b, at(x, y)).is_some(),
            "redraw on hover change"
        );
        assert!(
            grid.update(&mut state, &moved(x, y), b, at(x, y)).is_none(),
            "no change, no redraw"
        );
        assert_eq!(
            grid.mouse_interaction(&state, b, at(x, y)),
            mouse::Interaction::Cell
        );
        grid.update(&mut state, &moved(x, 10.0), b, at(x, 10.0));
        assert_eq!(
            grid.mouse_interaction(&state, b, at(x, 10.0)),
            mouse::Interaction::Pointer
        );
        assert_eq!(
            grid.mouse_interaction(&state, b, at(900.0, 900.0)),
            mouse::Interaction::default()
        );
    }

    #[test]
    fn a_new_result_resets_scroll_and_widths() {
        let (table, mut state) = setup(100);
        let grid = Grid {
            table: &table,
            selected: None,
            generation: 1,
        };
        let b = bounds(SIZE.0, SIZE.1);
        grid.update(&mut state, &wheel(0.0, -2.0), b, at(100.0, 100.0));
        state.widths = vec![500.0; 4];
        assert!(state.scroll.y > 0.0);

        let next = Grid {
            table: &table,
            selected: None,
            generation: 2,
        };
        next.update(&mut state, &moved(0.0, 0.0), b, at(0.0, 0.0));
        assert_eq!(state.scroll, Vector::ZERO);
        assert!(state.widths.is_empty());
        assert_eq!(state.generation, 2);
    }
}
