//! Helpers shared by unit tests: result tables, synthetic input events,
//! encoded images, a throwaway sample database, and driving [`App`]'s
//! update loop without a window.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use futures::StreamExt;
use iced::keyboard::{self, Key, Modifiers, key};
use iced::mouse::{self, Cursor, ScrollDelta};
use iced::widget::canvas;
use iced::{Event, Point, Rectangle, Size, Task};
use iced_test::runtime::{Action, task::into_stream};
use lancedb::arrow::arrow_array::{
    ArrayRef, Float64Array, Int32Array, RecordBatch, StringArray, TimestampMicrosecondArray,
};
use lancedb::arrow::arrow_schema::{DataType, Field, Schema, TimeUnit};

use crate::app::{App, Message};
use crate::db::ResultSet;
use crate::results::ResultTable;
use crate::settings::Settings;
use crate::theme::ThemeId;

/// Wraps a batch as a displayable result.
pub fn table(batch: RecordBatch) -> ResultTable {
    ResultTable::new(Arc::new(ResultSet {
        batch,
        truncated: false,
    }))
}

/// `rows` rows of `(id int32, name text, value float64, ts timestamp)`.
pub fn numbers_batch(rows: usize) -> RecordBatch {
    let schema = Schema::new(vec![
        Field::new("id", DataType::Int32, false),
        Field::new("name", DataType::Utf8, false),
        Field::new("value", DataType::Float64, true),
        Field::new(
            "ts",
            DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".into())),
            false,
        ),
    ]);
    let columns: Vec<ArrayRef> = vec![
        Arc::new(Int32Array::from_iter_values(0..rows as i32)),
        Arc::new(StringArray::from_iter_values(
            (0..rows).map(|i| format!("row {i}")),
        )),
        Arc::new(Float64Array::from_iter(
            (0..rows).map(|i| (i % 7 != 3).then_some(i as f64 * 1.5)),
        )),
        Arc::new(
            TimestampMicrosecondArray::from_iter_values(
                (0..rows as i64).map(|i| 1_767_225_600_000_000 + i * 3_600_000_000),
            )
            .with_timezone("UTC"),
        ),
    ];
    RecordBatch::try_new(Arc::new(schema), columns).unwrap()
}

/// A result built from named columns.
pub fn columns_table(columns: Vec<(&str, ArrayRef)>) -> ResultTable {
    table(RecordBatch::try_from_iter(columns).unwrap())
}

/// A result of `rows` numeric rows.
pub fn numbers_table(rows: usize) -> ResultTable {
    table(numbers_batch(rows))
}

/// A solid-colour PNG.
pub fn png(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let image =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(width, height, image::Rgb(rgb)));
    crate::media::encode_png(&image).unwrap()
}

pub fn bounds(width: f32, height: f32) -> Rectangle {
    Rectangle::new(Point::ORIGIN, Size::new(width, height))
}

pub fn at(x: f32, y: f32) -> Cursor {
    Cursor::Available(Point::new(x, y))
}

pub fn wheel(x: f32, y: f32) -> Event {
    Event::Mouse(mouse::Event::WheelScrolled {
        delta: ScrollDelta::Lines { x, y },
    })
}

pub fn press() -> Event {
    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
}

pub fn release() -> Event {
    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
}

pub fn moved(x: f32, y: f32) -> Event {
    Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(x, y),
    })
}

/// A left click (press and release) at the current cursor.
pub fn click_events() -> [Event; 2] {
    [press(), release()]
}

pub fn modifiers(modifiers: Modifiers) -> Event {
    Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers))
}

pub fn key_press(key: Key, modifiers: Modifiers) -> Event {
    Event::Keyboard(keyboard::Event::KeyPressed {
        modified_key: key.clone(),
        key,
        physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat: false,
    })
}

pub fn named(named: key::Named) -> Key {
    Key::Named(named)
}

pub fn character(c: &str) -> Key {
    Key::Character(c.into())
}

/// The message a canvas [`canvas::Action`] publishes, if any.
pub fn published<M>(action: Option<canvas::Action<M>>) -> Option<M> {
    action.and_then(|action| action.into_inner().0)
}

/// Whether a canvas [`canvas::Action`] captured its event.
pub fn captured<M>(action: Option<canvas::Action<M>>) -> bool {
    action.is_some_and(|action| action.into_inner().2 == iced::event::Status::Captured)
}

/// Runtime for test-side database work; kept for the whole test process
/// because Lance may hold handles to the runtime that opened a table.
pub static RUNTIME: LazyLock<tokio::runtime::Runtime> =
    LazyLock::new(|| tokio::runtime::Runtime::new().unwrap());

/// Creates the sample database in a temporary directory; returns the
/// directory guard and the database path.
pub fn sample_path() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample.lancedb");
    RUNTIME
        .block_on(crate::db::sample::create_sample_database(&path))
        .unwrap();
    (dir, path)
}

/// An app with in-memory settings (never touches the user's files).
pub fn app() -> App {
    App::with_settings(Settings::default(), None).0
}

/// Runs `task` to completion and returns the messages it produced.
pub fn outputs(task: Task<Message>) -> Vec<Message> {
    let Some(stream) = into_stream(task) else {
        return Vec::new();
    };
    RUNTIME.block_on(
        stream
            .filter_map(|action| async move {
                match action {
                    Action::Output(message) => Some(message),
                    _ => None,
                }
            })
            .collect(),
    )
}

/// Runs `task` and feeds everything it produces back into `app` until no
/// work is left, like the iced runtime would.
pub fn settle(app: &mut App, task: Task<Message>) {
    let mut queue: VecDeque<Message> = outputs(task).into();
    let mut steps = 0;
    while let Some(message) = queue.pop_front() {
        steps += 1;
        assert!(steps < 200, "update loop does not settle");
        queue.extend(outputs(app.update(message)));
    }
}

/// Sends `message` to `app` and settles the resulting work.
pub fn send(app: &mut App, message: Message) {
    let task = app.update(message);
    settle(app, task);
}

/// An app with the database at `path` open.
pub fn opened(path: &Path) -> App {
    let (mut app, task) = App::with_settings(Settings::default(), Some(path.display().to_string()));
    assert!(
        app.busy
            .as_deref()
            .is_some_and(|b| b.starts_with("Opening"))
    );
    settle(&mut app, task);
    assert!(app.database.is_some(), "open failed: {:?}", app.error);
    app
}

/// Draws `program` on a headless canvas of `size` with `theme`, after
/// moving the cursor to `cursor` (so hover states render too). Returns the
/// messages the canvas published.
pub fn render_canvas<P>(
    program: P,
    size: (f32, f32),
    cursor: Option<Point>,
    theme: ThemeId,
) -> Vec<Message>
where
    P: canvas::Program<Message>,
{
    let mut ui = iced_test::Simulator::with_size(
        iced::Settings::default(),
        size,
        canvas(program).width(iced::Fill).height(iced::Fill),
    );
    if let Some(point) = cursor {
        ui.point_at(point);
        let _ = ui.simulate([moved(point.x, point.y)]);
    }
    ui.snapshot(&theme.to_theme()).expect("canvas renders");
    ui.into_messages().collect()
}
