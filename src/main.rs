//! joust — a desktop SQL workbench for LanceDB.

mod app;
mod db;
mod highlight;
mod media;
mod profile;
mod results;
mod settings;
mod theme;
mod ui;

use app::App;

fn main() -> iced::Result {
    // `joust [path]` opens a database on start-up.
    let initial = std::env::args().nth(1);

    iced::application(move || App::new(initial.clone()), App::update, ui::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        .window_size((1440.0, 900.0))
        .antialiasing(true)
        .run()
}
