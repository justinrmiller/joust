//! Query plan visualiser: draws the executed physical plan as a tree.
//!
//! Each operator is a card showing its name, parameters, rows produced and
//! compute time, with a bar proportional to its share of the slowest
//! operator's time. Edges are labelled with the rows flowing between
//! operators. Drag to pan, scroll to move, Ctrl+scroll to zoom, hover a card
//! for the full details.

use iced::alignment;
use iced::mouse::{self, Cursor, ScrollDelta};
use iced::widget::canvas::{self, Action, Event, Frame, Geometry, Path, Stroke, Text};
use iced::widget::text::Alignment;
use iced::{Font, Pixels, Point, Rectangle, Renderer, Size, Theme, Vector, font, keyboard};

use crate::app::Message;
use crate::db::plan::PlanNode;
use crate::results::{human_duration, thousands};
use crate::theme::{self, ThemeId};
use crate::ui::grid::truncate;

const NODE_WIDTH: f32 = 250.0;
const NODE_HEIGHT: f32 = 80.0;
const H_GAP: f32 = 28.0;
const V_GAP: f32 = 36.0;
const MARGIN: f32 = 24.0;

/// Canvas program drawing a [`PlanNode`] tree.
pub struct PlanDiagram<'a> {
    pub root: &'a PlanNode,
    pub theme: ThemeId,
    /// Changes with every new plan; resets pan and zoom.
    pub generation: u64,
}

#[derive(Debug)]
pub struct PlanState {
    generation: u64,
    offset: Vector,
    zoom: f32,
    drag: Option<(Point, Vector)>,
    hover: Option<usize>,
    modifiers: keyboard::Modifiers,
}

impl Default for PlanState {
    fn default() -> Self {
        Self {
            generation: 0,
            offset: Vector::new(MARGIN, MARGIN),
            zoom: 1.0,
            drag: None,
            hover: None,
            modifiers: keyboard::Modifiers::default(),
        }
    }
}

/// A node placed in diagram coordinates.
struct Placed<'a> {
    node: &'a PlanNode,
    position: Point,
    parent: Option<usize>,
}

/// Lays the tree out top-down: leaves take consecutive slots, parents are
/// centred over their children.
fn layout(root: &PlanNode) -> Vec<Placed<'_>> {
    fn place<'a>(
        node: &'a PlanNode,
        depth: usize,
        parent: Option<usize>,
        next_slot: &mut f32,
        out: &mut Vec<Placed<'a>>,
    ) -> f32 {
        let index = out.len();
        out.push(Placed {
            node,
            position: Point::ORIGIN,
            parent,
        });
        let center = if node.children.is_empty() {
            let x = *next_slot;
            *next_slot += NODE_WIDTH + H_GAP;
            x
        } else {
            let centers: Vec<f32> = node
                .children
                .iter()
                .map(|child| place(child, depth + 1, Some(index), next_slot, out))
                .collect();
            (centers[0] + centers[centers.len() - 1]) / 2.0
        };
        out[index].position = Point::new(center, depth as f32 * (NODE_HEIGHT + V_GAP));
        center
    }

    let mut placed = Vec::with_capacity(root.node_count());
    let mut next_slot = 0.0;
    place(root, 0, None, &mut next_slot, &mut placed);
    placed
}

impl PlanDiagram<'_> {
    fn node_at(&self, state: &PlanState, point: Point) -> Option<usize> {
        let diagram = Point::new(
            (point.x - state.offset.x) / state.zoom,
            (point.y - state.offset.y) / state.zoom,
        );
        layout(self.root).iter().position(|placed| {
            let rect = node_rect(placed.position);
            rect.contains(diagram)
        })
    }
}

/// Initial view: fit the whole tree when possible (never below 85% zoom so
/// text stays legible), centred horizontally, anchored at the top.
fn fit(root: &PlanNode, bounds: Size) -> (Vector, f32) {
    let placed = layout(root);
    let width = placed.iter().map(|p| p.position.x).fold(0.0, f32::max) + NODE_WIDTH;
    let height = placed.iter().map(|p| p.position.y).fold(0.0, f32::max) + NODE_HEIGHT;
    let zoom = ((bounds.width - 2.0 * MARGIN) / width)
        .min((bounds.height - 2.0 * MARGIN) / height)
        .clamp(0.85, 1.0);
    let x = ((bounds.width - width * zoom) / 2.0).max(MARGIN);
    (Vector::new(x, MARGIN), zoom)
}

fn node_rect(position: Point) -> Rectangle {
    Rectangle::new(
        Point::new(position.x, position.y),
        Size::new(NODE_WIDTH, NODE_HEIGHT),
    )
}

impl canvas::Program<Message> for PlanDiagram<'_> {
    type State = PlanState;

    fn update(
        &self,
        state: &mut PlanState,
        event: &Event,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> Option<Action<Message>> {
        if state.generation != self.generation {
            let (offset, zoom) = fit(self.root, bounds.size());
            *state = PlanState {
                generation: self.generation,
                offset,
                zoom,
                ..PlanState::default()
            };
        }
        let position = cursor.position_in(bounds);
        match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers;
                None
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let point = position?;
                let (dx, dy) = match delta {
                    ScrollDelta::Lines { x, y } => (x * 40.0, y * 40.0),
                    ScrollDelta::Pixels { x, y } => (*x, *y),
                };
                if state.modifiers.command() {
                    // Zoom around the cursor.
                    let old = state.zoom;
                    state.zoom = (state.zoom * (1.0 + dy / 400.0)).clamp(0.3, 2.5);
                    let factor = state.zoom / old;
                    state.offset = Vector::new(
                        point.x - (point.x - state.offset.x) * factor,
                        point.y - (point.y - state.offset.y) * factor,
                    );
                } else if state.modifiers.shift() {
                    state.offset += Vector::new(dy, dx);
                } else {
                    state.offset += Vector::new(dx, dy);
                }
                Some(Action::request_redraw().and_capture())
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                let point = position?;
                state.drag = Some((point, state.offset));
                Some(Action::capture())
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.drag.take().map(|_| Action::capture())
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                if let (Some((origin, offset)), Some(point)) = (state.drag, cursor.position()) {
                    let point = Point::new(point.x - bounds.x, point.y - bounds.y);
                    state.offset = offset + (point - origin);
                    return Some(Action::request_redraw().and_capture());
                }
                let hover = position.and_then(|point| self.node_at(state, point));
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
        state: &PlanState,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> Vec<Geometry> {
        let fresh = state.generation == self.generation;
        let (offset, zoom, hover) = if fresh {
            (state.offset, state.zoom, state.hover)
        } else {
            let (offset, zoom) = fit(self.root, bounds.size());
            (offset, zoom, None)
        };

        let palette = theme.extended_palette();
        let background = palette.background.base.color;
        let text_color = palette.background.base.text;
        let muted = theme::muted(theme);
        let divider = theme::divider(theme);
        let chrome = theme::chrome(theme);
        let series = self.theme.series();
        let accent = palette.primary.base.color;

        let mut frame = Frame::new(renderer, bounds.size());
        frame.fill_rectangle(Point::ORIGIN, bounds.size(), background);

        let placed = layout(self.root);
        let max_elapsed = self.root.max_elapsed().as_secs_f64();
        let bold = Font {
            weight: font::Weight::Semibold,
            ..Font::DEFAULT
        };

        frame.with_save(|frame| {
            frame.translate(offset);
            frame.scale(zoom);

            // Edges (child top → parent bottom) with row counts.
            for placed_node in &placed {
                let Some(parent) = placed_node.parent else {
                    continue;
                };
                let parent_pos = placed[parent].position;
                let from = Point::new(
                    placed_node.position.x + NODE_WIDTH / 2.0,
                    placed_node.position.y,
                );
                let to = Point::new(parent_pos.x + NODE_WIDTH / 2.0, parent_pos.y + NODE_HEIGHT);
                let mid_y = (from.y + to.y) / 2.0;
                let path = Path::new(|builder| {
                    builder.move_to(from);
                    builder.bezier_curve_to(Point::new(from.x, mid_y), Point::new(to.x, mid_y), to);
                });
                frame.stroke(
                    &path,
                    Stroke::default()
                        .with_width(1.5)
                        .with_color(theme::faint(theme)),
                );
                if let Some(rows) = placed_node.node.output_rows {
                    frame.fill_text(Text {
                        content: format!("{} rows", thousands(rows)),
                        position: Point::new((from.x + to.x) / 2.0 + 6.0, mid_y),
                        color: muted,
                        size: Pixels(11.0),
                        align_y: alignment::Vertical::Center,
                        ..Text::default()
                    });
                }
            }

            // Operator cards.
            for (index, placed_node) in placed.iter().enumerate() {
                let node = placed_node.node;
                let rect = node_rect(placed_node.position);
                let hovered = hover == Some(index);
                let card = Path::rounded_rectangle(rect.position(), rect.size(), 8.0.into());
                frame.fill(&card, if hovered { theme::hover(theme) } else { chrome });
                frame.stroke(
                    &card,
                    Stroke::default()
                        .with_width(if hovered { 2.0 } else { 1.0 })
                        .with_color(if hovered { accent } else { divider }),
                );

                let left = rect.x + 12.0;
                frame.fill_text(Text {
                    content: truncate(&node.name, 30),
                    position: Point::new(left, rect.y + 16.0),
                    color: text_color,
                    size: Pixels(13.0),
                    font: bold,
                    align_y: alignment::Vertical::Center,
                    ..Text::default()
                });
                frame.fill_text(Text {
                    content: truncate(&node.detail, 36),
                    position: Point::new(left, rect.y + 35.0),
                    color: muted,
                    size: Pixels(11.0),
                    font: Font::MONOSPACE,
                    align_y: alignment::Vertical::Center,
                    ..Text::default()
                });

                let rows = node.output_rows.map_or("—".to_string(), |rows| {
                    format!("{} rows", thousands(rows))
                });
                let time = node.elapsed.map_or("—".to_string(), human_duration);
                frame.fill_text(Text {
                    content: rows,
                    position: Point::new(left, rect.y + 56.0),
                    color: text_color,
                    size: Pixels(12.0),
                    align_y: alignment::Vertical::Center,
                    ..Text::default()
                });
                frame.fill_text(Text {
                    content: time,
                    position: Point::new(rect.x + NODE_WIDTH - 12.0, rect.y + 56.0),
                    color: text_color,
                    size: Pixels(12.0),
                    align_x: Alignment::Right,
                    align_y: alignment::Vertical::Center,
                    ..Text::default()
                });

                // Share of the slowest operator's compute time.
                let track = Rectangle::new(
                    Point::new(left, rect.y + NODE_HEIGHT - 14.0),
                    Size::new(NODE_WIDTH - 24.0, 4.0),
                );
                frame.fill(
                    &Path::rounded_rectangle(track.position(), track.size(), 2.0.into()),
                    divider,
                );
                let share = match (node.elapsed, max_elapsed > 0.0) {
                    (Some(elapsed), true) => (elapsed.as_secs_f64() / max_elapsed) as f32,
                    _ => 0.0,
                };
                if share > 0.0 {
                    frame.fill(
                        &Path::rounded_rectangle(
                            track.position(),
                            Size::new((track.width * share).max(4.0), track.height),
                            2.0.into(),
                        ),
                        series,
                    );
                }
            }
        });

        // Hover details, drawn in screen space near the cursor.
        if let (Some(index), Some(cursor)) = (hover, cursor.position_in(bounds)) {
            let node = placed[index].node;
            let mut lines = vec![node.name.clone()];
            lines.extend(wrap(&node.detail, 64));
            lines.extend(
                node.metrics
                    .iter()
                    .map(|(name, value)| format!("{name}: {value}")),
            );
            let lines: Vec<String> = lines.into_iter().take(18).collect();
            let height = 16.0 * lines.len() as f32 + 16.0;
            let width = lines
                .iter()
                .map(|line| line.chars().count())
                .max()
                .unwrap_or(10) as f32
                * 6.9
                + 20.0;
            let mut origin = Point::new(cursor.x + 16.0, cursor.y + 16.0);
            if origin.x + width > bounds.width {
                origin.x = (cursor.x - width - 8.0).max(4.0);
            }
            if origin.y + height > bounds.height {
                origin.y = (bounds.height - height - 4.0).max(4.0);
            }
            let tip = Path::rounded_rectangle(origin, Size::new(width, height), 6.0.into());
            frame.fill(&tip, text_color);
            for (i, line) in lines.iter().enumerate() {
                frame.fill_text(Text {
                    content: line.clone(),
                    position: Point::new(origin.x + 10.0, origin.y + 8.0 + 16.0 * i as f32),
                    color: background,
                    size: Pixels(11.5),
                    font: if i == 0 { bold } else { Font::MONOSPACE },
                    ..Text::default()
                });
            }
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &PlanState,
        bounds: Rectangle,
        cursor: Cursor,
    ) -> mouse::Interaction {
        if state.drag.is_some() {
            mouse::Interaction::Grabbing
        } else if cursor.is_over(bounds) {
            mouse::Interaction::Grab
        } else {
            mouse::Interaction::default()
        }
    }
}

/// Soft-wraps `text` at commas/spaces so lines stay under `width` characters.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_inclusive([' ', ',']) {
        if current.chars().count() + word.chars().count() > width && !current.is_empty() {
            lines.push(current.trim_end().to_string());
            current.clear();
        }
        current.push_str(word);
    }
    if !current.trim().is_empty() {
        lines.push(current.trim_end().to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use iced::widget::canvas::Program;
    use std::time::Duration;

    use iced::keyboard::Modifiers;

    use super::*;
    use crate::test_support::{
        at, bounds, captured, modifiers, moved, press, published, release, render_canvas, wheel,
    };

    fn node(name: &str, children: Vec<PlanNode>) -> PlanNode {
        PlanNode {
            name: name.into(),
            detail: String::new(),
            output_rows: None,
            elapsed: None,
            metrics: Vec::new(),
            children,
        }
    }

    /// A small executed-looking plan: a projection over a filtered scan.
    fn plan() -> PlanNode {
        let scan = PlanNode {
            name: "LanceScan".into(),
            detail: "uri=/data/movies.lance, projection=[id, title, year], row_id=false, ordered=true, range=None".into(),
            output_rows: Some(64),
            elapsed: Some(Duration::from_micros(900)),
            metrics: vec![("output_rows".into(), "64".into()), ("elapsed_compute".into(), "900µs".into())],
            children: Vec::new(),
        };
        let filter = PlanNode {
            name: "FilterExec".into(),
            detail: "year@2 > 1980".into(),
            output_rows: Some(40),
            elapsed: Some(Duration::from_micros(120)),
            metrics: Vec::new(),
            children: vec![scan.clone(), scan],
        };
        PlanNode {
            name: "ProjectionExec".into(),
            detail: "expr=[title@1 as title]".into(),
            output_rows: Some(40),
            elapsed: None,
            metrics: Vec::new(),
            children: vec![filter],
        }
    }

    /// Screen position of `node`'s card centre for a given view.
    fn card_center(state: &PlanState, position: Point) -> Point {
        Point::new(
            state.offset.x + (position.x + NODE_WIDTH / 2.0) * state.zoom,
            state.offset.y + (position.y + NODE_HEIGHT / 2.0) * state.zoom,
        )
    }

    #[test]
    fn new_plans_are_fitted_to_the_view() {
        let root = plan();
        // Fits in a large view at full zoom, centred.
        let (offset, zoom) = fit(&root, Size::new(2000.0, 1000.0));
        assert_eq!(zoom, 1.0);
        assert!(offset.x > MARGIN);
        // Never shrinks below 85%.
        let (offset, zoom) = fit(&root, Size::new(200.0, 100.0));
        assert_eq!(zoom, 0.85);
        assert_eq!(offset, Vector::new(MARGIN, MARGIN));
        assert_eq!(root.node_count(), 4);
    }

    #[test]
    fn pans_zooms_drags_and_hovers() {
        let root = plan();
        let diagram = PlanDiagram {
            root: &root,
            theme: ThemeId::Nord,
            generation: 3,
        };
        let bounds = bounds(900.0, 500.0);
        let mut state = PlanState::default();

        // The first event of a new plan resets the view.
        assert!(
            diagram
                .update(
                    &mut state,
                    &modifiers(Modifiers::empty()),
                    bounds,
                    at(5.0, 5.0)
                )
                .is_none()
        );
        let (offset, zoom) = fit(&root, bounds.size());
        assert_eq!(
            (state.generation, state.offset, state.zoom),
            (3, offset, zoom)
        );

        // Scrolling pans (lines and pixels); outside the canvas does nothing.
        assert!(
            diagram
                .update(
                    &mut state,
                    &wheel(0.0, -1.0),
                    bounds,
                    mouse::Cursor::Unavailable
                )
                .is_none()
        );
        assert!(captured(diagram.update(
            &mut state,
            &wheel(0.0, -1.0),
            bounds,
            at(50.0, 50.0)
        )));
        assert_eq!(state.offset, offset + Vector::new(0.0, -40.0));
        let pixels = Event::Mouse(mouse::Event::WheelScrolled {
            delta: ScrollDelta::Pixels { x: 5.0, y: 0.0 },
        });
        assert!(captured(diagram.update(
            &mut state,
            &pixels,
            bounds,
            at(50.0, 50.0)
        )));
        assert_eq!(state.offset, offset + Vector::new(5.0, -40.0));

        // Shift swaps the axes.
        let _ = diagram.update(
            &mut state,
            &modifiers(Modifiers::SHIFT),
            bounds,
            at(50.0, 50.0),
        );
        let before = state.offset;
        let _ = diagram.update(&mut state, &wheel(0.0, -1.0), bounds, at(50.0, 50.0));
        assert_eq!(state.offset, before + Vector::new(-40.0, 0.0));

        // Ctrl zooms around the cursor, within limits.
        let _ = diagram.update(
            &mut state,
            &modifiers(Modifiers::COMMAND),
            bounds,
            at(50.0, 50.0),
        );
        let cursor = Point::new(300.0, 200.0);
        let anchor = |state: &PlanState| {
            (
                (cursor.x - state.offset.x) / state.zoom,
                (cursor.y - state.offset.y) / state.zoom,
            )
        };
        let (ax, ay) = anchor(&state);
        let _ = diagram.update(&mut state, &wheel(0.0, 2.0), bounds, at(cursor.x, cursor.y));
        assert!(state.zoom > zoom);
        let (bx, by) = anchor(&state);
        assert!(
            (ax - bx).abs() < 0.01 && (ay - by).abs() < 0.01,
            "cursor point stays put"
        );
        for _ in 0..20 {
            let _ = diagram.update(
                &mut state,
                &wheel(0.0, 50.0),
                bounds,
                at(cursor.x, cursor.y),
            );
        }
        assert_eq!(state.zoom, 2.5);
        for _ in 0..20 {
            let _ = diagram.update(
                &mut state,
                &wheel(0.0, -50.0),
                bounds,
                at(cursor.x, cursor.y),
            );
        }
        assert_eq!(state.zoom, 0.3);
        let _ = diagram.update(
            &mut state,
            &modifiers(Modifiers::empty()),
            bounds,
            at(5.0, 5.0),
        );

        // Dragging moves the diagram with the cursor.
        assert_eq!(
            diagram.mouse_interaction(&state, bounds, at(10.0, 10.0)),
            mouse::Interaction::Grab
        );
        assert_eq!(
            diagram.mouse_interaction(&state, bounds, at(-10.0, 10.0)),
            mouse::Interaction::default()
        );
        assert!(
            diagram
                .update(&mut state, &press(), bounds, at(-10.0, 10.0))
                .is_none()
        );
        assert!(captured(diagram.update(
            &mut state,
            &press(),
            bounds,
            at(100.0, 100.0)
        )));
        assert_eq!(
            diagram.mouse_interaction(&state, bounds, at(100.0, 100.0)),
            mouse::Interaction::Grabbing
        );
        let before = state.offset;
        let action = diagram.update(&mut state, &moved(160.0, 130.0), bounds, at(160.0, 130.0));
        assert!(captured(action));
        assert_eq!(state.offset, before + Vector::new(60.0, 30.0));
        assert!(captured(diagram.update(
            &mut state,
            &release(),
            bounds,
            at(160.0, 130.0)
        )));
        assert!(
            diagram
                .update(&mut state, &release(), bounds, at(160.0, 130.0))
                .is_none()
        );

        // Hovering a card highlights it (redraw only, not captured).
        let placed = layout(&root);
        let over = card_center(&state, placed[1].position);
        let action = diagram.update(
            &mut state,
            &moved(over.x, over.y),
            bounds,
            at(over.x, over.y),
        );
        assert!(action.is_some() && published(action).is_none());
        assert_eq!(state.hover, Some(1));
        assert!(
            diagram
                .update(
                    &mut state,
                    &moved(over.x, over.y),
                    bounds,
                    at(over.x, over.y)
                )
                .is_none()
        );
        let _ = diagram.update(&mut state, &moved(1.0, 499.0), bounds, at(1.0, 499.0));
        assert_eq!(state.hover, None);
        assert!(
            diagram
                .update(
                    &mut state,
                    &Event::Mouse(mouse::Event::CursorEntered),
                    bounds,
                    at(1.0, 1.0)
                )
                .is_none()
        );
    }

    #[test]
    fn draws_cards_edges_and_tooltips() {
        let root = plan();
        let size = (900.0, 420.0);
        let (offset, zoom) = fit(&root, Size::new(size.0, size.1));
        let state = PlanState {
            generation: 1,
            offset,
            zoom,
            ..PlanState::default()
        };
        let placed = layout(&root);
        // No cursor; the root; a leaf near the bottom-right (tooltip flips).
        let cursors = [
            None,
            Some(card_center(&state, placed[0].position)),
            Some(card_center(&state, placed[3].position)),
        ];
        for (theme, cursor) in [
            ThemeId::JoustLight,
            ThemeId::TokyoNight,
            ThemeId::SolarizedLight,
        ]
        .into_iter()
        .zip(cursors)
        {
            let diagram = PlanDiagram {
                root: &root,
                theme,
                generation: 1,
            };
            assert!(render_canvas(diagram, size, cursor, theme).is_empty());
        }
    }

    #[test]
    fn parents_are_centred_over_children() {
        let tree = node(
            "root",
            vec![node("a", vec![]), node("b", vec![node("c", vec![])])],
        );
        let placed = layout(&tree);
        assert_eq!(placed.len(), 4);
        let x = |name: &str| {
            placed
                .iter()
                .find(|p| p.node.name == name)
                .unwrap()
                .position
        };
        assert_eq!(x("a").x, 0.0);
        assert_eq!(x("b").x, NODE_WIDTH + H_GAP);
        assert_eq!(x("c").x, x("b").x);
        assert_eq!(x("root").x, (x("a").x + x("b").x) / 2.0);
        assert!(x("c").y > x("b").y && x("b").y > x("root").y);
    }

    #[test]
    fn wraps_long_details() {
        let lines = wrap("expr=[a@0 as a, b@1 as b, c@2 as c]", 16);
        assert!(lines.iter().all(|line| line.chars().count() <= 16));
        assert_eq!(
            lines.concat().replace(' ', ""),
            "expr=[a@0asa,b@1asb,c@2asc]"
        );
    }
}
