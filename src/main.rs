//! joust — a desktop SQL workbench for LanceDB.

mod app;
mod av;
mod db;
mod ffmpeg;
mod highlight;
mod media;
mod profile;
mod results;
mod settings;
#[cfg(test)]
mod test_support;
mod theme;
mod ui;

use app::App;

fn main() -> iced::Result {
    // `joust [path]` opens a database on start-up.
    let initial = std::env::args().nth(1);

    // `joust --version` prints the version without opening a window, so a
    // release binary can be checked from a script (and by the release build).
    if matches!(initial.as_deref(), Some("--version" | "-V")) {
        println!("joust {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    iced::application(move || App::new(initial.clone()), App::update, ui::view)
        .title(App::title)
        .theme(App::theme)
        .subscription(App::subscription)
        .window_size((1440.0, 900.0))
        .antialiasing(true)
        .run()
}
