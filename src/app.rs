//! Application state, messages and update logic.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use iced::keyboard::{self, key};
use iced::widget::{pane_grid, text_editor};
use iced::{Subscription, Task, Theme, event, window};

use crate::db::functions::is_vector_type;
use crate::db::import::ImportSummary;
use crate::db::{Database, IndexKind, QueryOutcome, TableInfo, quote_ident};
use crate::media;
use crate::profile::{ColumnProfile, profile_batch};
use crate::results::ResultTable;
use crate::settings::{Settings, sample_database_path};
use crate::theme::ThemeId;
use crate::ui::chart::{self, ChartData, ChartKind, ChartSpec, ColumnChoice};
use crate::ui::gallery::{self, Gallery, MediaColumn, Source};

/// Text the editor starts with.
pub const WELCOME_SQL: &str = "\
-- Welcome to joust: SQL for LanceDB (DataFusion dialect).
-- Ctrl+Enter runs the editor, or only the selection if there is one.
-- Try vector_search('table', 'column', '[...]', k) and fts('table', '{...}').
SELECT table_name, table_type
FROM information_schema.tables
WHERE table_schema = 'main';
";

/// Example queries offered for the sample database.
pub const SAMPLE_EXAMPLES: &[(&str, &str)] = &[
    (
        "Movies per genre",
        "SELECT genre, count(*) AS movies, min(year) AS first, max(year) AS latest\nFROM movies\nGROUP BY genre\nORDER BY movies DESC;",
    ),
    (
        "Vector search: sci-fi horror",
        "-- The query vector leans on the sci-fi and horror axes of the synthetic embedding.\nSELECT title, year, director, _distance\nFROM vector_search('movies', 'vector', '[0.75, 0.1, 0, 0, 0, 0.55, 0, 0.35]', 8, 'cosine')\nORDER BY _distance;",
    ),
    (
        "Full-text search",
        "SELECT id, title, year, _score\nFROM fts('movies', '{\"match\": {\"column\": \"title\", \"terms\": \"die hard\"}}')\nORDER BY _score DESC;",
    ),
    (
        "Poster gallery (open the Media tab)",
        "-- Posters are procedurally generated PNGs stored in a binary column.\nSELECT title, year, genre, poster\nFROM movies\nORDER BY genre, year;",
    ),
    (
        "Media functions",
        "SELECT title,\n       media_type(poster) AS mime,\n       image_width(poster) AS width,\n       image_height(poster) AS height,\n       byte_length(poster) AS bytes\nFROM movies\nORDER BY bytes DESC\nLIMIT 10;",
    ),
    (
        "Daily events & revenue",
        "SELECT date_trunc('day', ts) AS day,\n       count(*) AS events,\n       round(sum(amount_usd), 2) AS revenue_usd\nFROM events\nGROUP BY 1\nORDER BY 1;",
    ),
    (
        "Latency percentiles by country",
        "SELECT country,\n       count(*) AS events,\n       round(avg(latency_ms), 1) AS avg_ms,\n       round(approx_percentile_cont(latency_ms, 0.95), 1) AS p95_ms\nFROM events\nGROUP BY country\nORDER BY p95_ms DESC;",
    ),
];

/// Runtime for all database work, separate from iced's executor so that
/// CPU-heavy queries cannot starve UI timers and subscriptions.
static DB_RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    // Leave a core for the UI thread so the window stays responsive while a
    // query saturates the others.
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(cores.saturating_sub(1).max(1))
        .thread_name("joust-db")
        .enable_all()
        .build()
        .expect("failed to start the database runtime")
});

/// Runs `work` on the database runtime. Returns a future for iced to await
/// and a handle that cancels the work itself (not just the wait).
fn spawn_db<T: Send + 'static>(
    work: impl Future<Output = anyhow::Result<T>> + Send + 'static,
) -> (
    impl Future<Output = Result<T, String>> + Send + 'static,
    tokio::task::AbortHandle,
) {
    let handle = DB_RUNTIME.spawn(work);
    let abort = handle.abort_handle();
    let result = async move {
        match handle.await {
            Ok(result) => result.map_err(|error| error_text(&error)),
            Err(error) if error.is_cancelled() => Err("cancelled".to_string()),
            Err(error) => Err(format!("database task failed: {error}")),
        }
    };
    (result, abort)
}

/// Like [`spawn_db`] for work that is never cancelled.
fn db_task<T: Send + 'static>(
    work: impl Future<Output = anyhow::Result<T>> + Send + 'static,
    to_message: impl FnOnce(Result<T, String>) -> Message + Send + 'static,
) -> Task<Message> {
    Task::perform(spawn_db(work).0, to_message)
}

/// Formats an error with its causes, skipping causes whose text the message
/// already contains (DataFusion errors embed their sources).
pub fn error_text(error: &anyhow::Error) -> String {
    let mut text = error.to_string();
    for cause in error.chain().skip(1) {
        let cause = cause.to_string();
        if !text.contains(&cause) {
            text.push_str(": ");
            text.push_str(&cause);
        }
    }
    text
}

/// A decoded image for the selected cell.
#[derive(Debug, Clone)]
pub struct Preview {
    pub row: usize,
    pub column: usize,
    pub handle: iced::widget::image::Handle,
    /// Colour descriptor of the image (for "Find similar images").
    pub color: [f32; media::COLOR_DIM],
}

/// Which tab of the results pane is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResultTab {
    #[default]
    Rows,
    Columns,
    Media,
    Plan,
    Chart,
}

/// How the plan tab renders the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanMode {
    #[default]
    Diagram,
    Physical,
    Logical,
}

/// Maximum rows materialised per statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowLimit(pub usize);

impl RowLimit {
    pub const ALL: [RowLimit; 4] = [
        RowLimit(1_000),
        RowLimit(10_000),
        RowLimit(100_000),
        RowLimit(1_000_000),
    ];
}

impl std::fmt::Display for RowLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            n if n >= 1_000_000 => write!(f, "Limit {}M rows", n / 1_000_000),
            n => write!(f, "Limit {}k rows", n / 1_000),
        }
    }
}

/// The panes of the main layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    Sidebar,
    Editor,
    Results,
}

/// A query currently executing.
pub struct Running {
    pub id: u64,
    pub started: Instant,
    abort: tokio::task::AbortHandle,
}

#[derive(Debug, Clone)]
pub enum Message {
    // Connection
    PathChanged(String),
    OpenDatabase,
    OpenRecent(String),
    BrowseDatabase,
    DatabasePicked(Option<PathBuf>),
    OpenSample,
    SampleReady(Result<String, String>),
    DatabaseOpened(Result<(Database, Vec<TableInfo>), String>),
    RefreshCatalog,
    CatalogLoaded(Result<Vec<TableInfo>, String>),
    // Sidebar
    ToggleTable(String),
    PreviewTable(String),
    InsertText(String),
    CreateIndex(String, String, IndexKind),
    IndexCreated(Result<String, String>),
    LoadQuery(String),
    RunExample(String),
    // Editor & execution
    Edit(text_editor::Action),
    Run,
    Cancel,
    QueryFinished(u64, Result<Arc<QueryOutcome>, String>),
    ProfilesReady(u64, Arc<Vec<ColumnProfile>>),
    Tick,
    SetRowLimit(RowLimit),
    // Media
    ImportMedia,
    MediaFolderPicked(Option<PathBuf>),
    MediaImported(Result<ImportSummary, String>),
    GalleryReady(u64, Arc<Gallery>),
    SetGalleryColumn(ColumnChoice),
    PreviewReady(u64, Option<Arc<Preview>>),
    SaveCell,
    CellSaved(Result<Option<PathBuf>, String>),
    FindSimilarImages,
    // Results
    SelectTab(ResultTab),
    SortColumn(usize),
    SelectCell(usize, usize),
    CopyCell,
    FindSimilar,
    ExportCsv,
    CsvExported(Result<Option<PathBuf>, String>),
    SetPlanMode(PlanMode),
    SetChartKind(ChartKind),
    SetChartX(ColumnChoice),
    SetChartY(ColumnChoice),
    // Chrome
    PaneResized(pane_grid::ResizeEvent),
    SelectTheme(ThemeId),
    DismissMessages,
}

/// The whole application.
pub struct App {
    pub settings: Settings,
    pub theme: Theme,

    pub path_input: String,
    pub database: Option<Database>,
    pub catalog: Vec<TableInfo>,
    pub expanded: HashSet<String>,
    pub busy: Option<String>,

    pub editor: text_editor::Content,

    pub running: Option<Running>,
    next_id: u64,
    pub outcome: Option<Arc<QueryOutcome>>,
    pub table: Option<ResultTable>,
    /// Bumped on every new result; canvases reset their view state on change.
    pub generation: u64,
    pub profiles: Option<Arc<Vec<ColumnProfile>>>,
    pub error: Option<String>,
    pub notice: Option<String>,

    pub tab: ResultTab,
    pub selected: Option<(usize, usize)>,
    pub plan_mode: PlanMode,
    pub chart_spec: ChartSpec,
    pub chart: Option<Result<ChartData, String>>,

    pub panes: pane_grid::State<PaneKind>,

    /// Media columns of the current result.
    pub media_columns: Vec<MediaColumn>,
    pub gallery: Option<Arc<Gallery>>,
    pub preview: Option<Arc<Preview>>,
}

impl App {
    /// Creates the app, optionally opening `initial` right away.
    pub fn new(initial: Option<String>) -> (Self, Task<Message>) {
        let settings = Settings::load();
        let panes = pane_grid::State::with_configuration(pane_grid::Configuration::Split {
            axis: pane_grid::Axis::Vertical,
            ratio: 0.21,
            a: Box::new(pane_grid::Configuration::Pane(PaneKind::Sidebar)),
            b: Box::new(pane_grid::Configuration::Split {
                axis: pane_grid::Axis::Horizontal,
                ratio: 0.36,
                a: Box::new(pane_grid::Configuration::Pane(PaneKind::Editor)),
                b: Box::new(pane_grid::Configuration::Pane(PaneKind::Results)),
            }),
        });

        let mut app = Self {
            theme: settings.theme.to_theme(),
            path_input: initial.clone().unwrap_or_default(),
            settings,
            database: None,
            catalog: Vec::new(),
            expanded: HashSet::new(),
            busy: None,
            editor: text_editor::Content::with_text(WELCOME_SQL),
            running: None,
            next_id: 0,
            outcome: None,
            table: None,
            generation: 0,
            profiles: None,
            error: None,
            notice: None,
            tab: ResultTab::default(),
            selected: None,
            plan_mode: PlanMode::default(),
            chart_spec: ChartSpec::default(),
            chart: None,
            panes,
            media_columns: Vec::new(),
            gallery: None,
            preview: None,
        };

        let task = match initial {
            Some(path) => app.open(path),
            None => Task::none(),
        };
        (app, task)
    }

    pub fn title(&self) -> String {
        match &self.database {
            Some(db) => format!("joust — {}", db.uri()),
            None => "joust".to_string(),
        }
    }

    pub fn theme(&self) -> Theme {
        self.theme.clone()
    }

    pub fn theme_id(&self) -> ThemeId {
        self.settings.theme
    }

    pub fn row_limit(&self) -> RowLimit {
        RowLimit(self.settings.row_limit)
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let keys = event::listen_with(shortcut);
        if self.running.is_some() {
            Subscription::batch([
                keys,
                iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick),
            ])
        } else {
            keys
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::PathChanged(path) => {
                self.path_input = path;
                Task::none()
            }
            Message::OpenDatabase => {
                let path = self.path_input.trim().to_string();
                if path.is_empty() {
                    self.error = Some("Enter the path of a LanceDB database directory".into());
                    return Task::none();
                }
                self.open(path)
            }
            Message::OpenRecent(path) => {
                self.path_input = path.clone();
                self.open(path)
            }
            Message::BrowseDatabase => Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .set_title("Open a LanceDB database directory")
                        .pick_folder()
                        .await
                        .map(|handle| handle.path().to_path_buf())
                },
                Message::DatabasePicked,
            ),
            Message::DatabasePicked(Some(path)) => {
                let path = path.to_string_lossy().to_string();
                self.path_input = path.clone();
                self.open(path)
            }
            Message::DatabasePicked(None) => Task::none(),
            Message::OpenSample => {
                self.busy = Some("Creating the sample database…".into());
                self.error = None;
                let path = sample_database_path();
                db_task(
                    async move {
                        crate::db::sample::create_sample_database(&path).await?;
                        Ok(path.to_string_lossy().to_string())
                    },
                    Message::SampleReady,
                )
            }
            Message::SampleReady(Ok(path)) => {
                self.path_input = path.clone();
                if self.editor.text().trim() == WELCOME_SQL.trim() {
                    self.editor = text_editor::Content::with_text(SAMPLE_EXAMPLES[0].1);
                }
                self.open(path)
            }
            Message::SampleReady(Err(error)) | Message::DatabaseOpened(Err(error)) => {
                self.busy = None;
                self.error = Some(error);
                Task::none()
            }
            Message::DatabaseOpened(Ok((database, catalog))) => {
                self.busy = None;
                self.settings.remember_database(database.uri());
                self.settings.save();
                self.notice = Some(format!(
                    "Opened {} ({} table{})",
                    database.uri(),
                    catalog.len(),
                    if catalog.len() == 1 { "" } else { "s" }
                ));
                self.error = None;
                self.expanded = catalog.iter().take(3).map(|t| t.name.clone()).collect();
                self.catalog = catalog;
                self.database = Some(database);
                Task::none()
            }
            Message::RefreshCatalog => self.refresh_catalog(),
            Message::CatalogLoaded(Ok(catalog)) => {
                self.busy = None;
                self.catalog = catalog;
                Task::none()
            }
            Message::CatalogLoaded(Err(error)) => {
                self.busy = None;
                self.error = Some(error);
                Task::none()
            }
            Message::ToggleTable(name) => {
                if !self.expanded.remove(&name) {
                    self.expanded.insert(name);
                }
                Task::none()
            }
            Message::PreviewTable(name) => {
                self.editor = text_editor::Content::with_text(&format!(
                    "SELECT *\nFROM {}\nLIMIT 100;",
                    quote_ident(&name)
                ));
                self.run()
            }
            Message::InsertText(text) => {
                self.editor
                    .perform(text_editor::Action::Edit(text_editor::Edit::Paste(
                        Arc::new(text),
                    )));
                Task::none()
            }
            Message::CreateIndex(table, column, kind) => {
                let Some(database) = self.database.clone() else {
                    return Task::none();
                };
                self.busy = Some(format!("Indexing {table}.{column}…"));
                db_task(
                    async move { database.create_index(&table, &column, kind).await },
                    Message::IndexCreated,
                )
            }
            Message::IndexCreated(result) => {
                self.busy = None;
                match result {
                    Ok(note) => self.notice = Some(note),
                    Err(error) => self.error = Some(error),
                }
                self.refresh_catalog()
            }
            Message::LoadQuery(sql) => {
                self.editor = text_editor::Content::with_text(&sql);
                Task::none()
            }
            Message::RunExample(sql) => {
                self.editor = text_editor::Content::with_text(&sql);
                self.run()
            }
            Message::Edit(action) => {
                self.editor.perform(action);
                Task::none()
            }
            Message::Run => self.run(),
            Message::Cancel => {
                if let Some(running) = self.running.take() {
                    running.abort.abort();
                    self.notice = Some("Query cancelled".into());
                }
                Task::none()
            }
            Message::QueryFinished(id, result) => {
                if self.running.as_ref().is_none_or(|running| running.id != id) {
                    return Task::none();
                }
                self.running = None;
                match result {
                    Ok(outcome) => self.show_outcome(outcome),
                    Err(error) => {
                        self.error = Some(error);
                        Task::none()
                    }
                }
            }
            Message::ProfilesReady(generation, profiles) => {
                if generation == self.generation {
                    self.profiles = Some(profiles);
                }
                Task::none()
            }
            Message::Tick => Task::none(),
            Message::SetRowLimit(limit) => {
                self.settings.row_limit = limit.0;
                self.settings.save();
                Task::none()
            }
            Message::SelectTab(tab) => {
                self.tab = tab;
                Task::none()
            }
            Message::SortColumn(column) => {
                if let Some(table) = &mut self.table {
                    table.toggle_sort(column);
                    self.selected = None;
                    self.rebuild_chart();
                }
                Task::none()
            }
            Message::SelectCell(row, column) => {
                self.selected = Some((row, column));
                self.load_preview(row, column)
            }
            Message::PreviewReady(generation, preview) => {
                if generation == self.generation
                    && let Some(preview) = preview
                    && self.selected == Some((preview.row, preview.column))
                {
                    self.preview = Some(preview);
                }
                Task::none()
            }
            Message::GalleryReady(generation, gallery) => {
                if generation == self.generation {
                    self.gallery = Some(gallery);
                }
                Task::none()
            }
            Message::SetGalleryColumn(choice) => {
                self.gallery = None;
                self.build_gallery(choice.index)
            }
            Message::SaveCell => self.save_cell(),
            Message::CellSaved(Ok(Some(path))) => {
                self.notice = Some(format!("Saved {}", path.display()));
                Task::none()
            }
            Message::CellSaved(Ok(None)) => Task::none(),
            Message::CellSaved(Err(error)) => {
                self.error = Some(format!("Save failed: {error}"));
                Task::none()
            }
            Message::FindSimilarImages => self.find_similar_images(),
            Message::ImportMedia => Task::perform(
                async {
                    rfd::AsyncFileDialog::new()
                        .set_title("Import a folder of media files")
                        .pick_folder()
                        .await
                        .map(|handle| handle.path().to_path_buf())
                },
                Message::MediaFolderPicked,
            ),
            Message::MediaFolderPicked(None) => Task::none(),
            Message::MediaFolderPicked(Some(folder)) => {
                let Some(database) = self.database.clone() else {
                    return Task::none();
                };
                self.busy = Some(format!("Importing {}…", folder.display()));
                self.error = None;
                db_task(
                    async move { database.import_media_folder(folder).await },
                    Message::MediaImported,
                )
            }
            Message::MediaImported(Ok(summary)) => {
                self.busy = None;
                self.notice = Some(format!(
                    "Imported {} file{} into {}{}",
                    summary.imported,
                    if summary.imported == 1 { "" } else { "s" },
                    summary.table,
                    if summary.skipped > 0 {
                        format!(" ({} skipped: too large or unreadable)", summary.skipped)
                    } else {
                        String::new()
                    }
                ));
                self.expanded.insert(summary.table.clone());
                self.editor = text_editor::Content::with_text(&format!(
                    "SELECT file_name, kind, width, height, size_bytes, modified, thumbnail\nFROM {}\nORDER BY file_name\nLIMIT 200;",
                    quote_ident(&summary.table)
                ));
                self.tab = ResultTab::Media;
                Task::batch([self.refresh_catalog(), self.run()])
            }
            Message::MediaImported(Err(error)) => {
                self.busy = None;
                self.error = Some(error);
                Task::none()
            }
            Message::CopyCell => match self.selected_value() {
                Some(value) => {
                    self.notice = Some("Copied value to the clipboard".into());
                    iced::clipboard::write(value)
                }
                None => Task::none(),
            },
            Message::FindSimilar => self.find_similar(),
            Message::ExportCsv => {
                let Some(table) = self.table.clone() else {
                    return Task::none();
                };
                Task::perform(
                    async move {
                        let Some(handle) = rfd::AsyncFileDialog::new()
                            .set_title("Export results as CSV")
                            .set_file_name("results.csv")
                            .add_filter("CSV", &["csv"])
                            .save_file()
                            .await
                        else {
                            return Ok(None);
                        };
                        let path = handle.path().to_path_buf();
                        tokio::task::spawn_blocking(move || {
                            table.save_csv(&path).map(|()| Some(path))
                        })
                        .await
                        .map_err(|e| e.to_string())?
                        .map_err(|e| error_text(&e))
                    },
                    Message::CsvExported,
                )
            }
            Message::CsvExported(Ok(Some(path))) => {
                self.notice = Some(format!("Exported results to {}", path.display()));
                Task::none()
            }
            Message::CsvExported(Ok(None)) => Task::none(),
            Message::CsvExported(Err(error)) => {
                self.error = Some(format!("Export failed: {error}"));
                Task::none()
            }
            Message::SetPlanMode(mode) => {
                self.plan_mode = mode;
                Task::none()
            }
            Message::SetChartKind(kind) => {
                self.chart_spec.kind = kind;
                self.rebuild_chart();
                Task::none()
            }
            Message::SetChartX(choice) => {
                self.chart_spec.x = Some(choice.index);
                self.rebuild_chart();
                Task::none()
            }
            Message::SetChartY(choice) => {
                self.chart_spec.y = Some(choice.index);
                self.rebuild_chart();
                Task::none()
            }
            Message::PaneResized(pane_grid::ResizeEvent { split, ratio }) => {
                self.panes.resize(split, ratio);
                Task::none()
            }
            Message::SelectTheme(theme) => {
                self.settings.theme = theme;
                self.theme = theme.to_theme();
                self.settings.save();
                Task::none()
            }
            Message::DismissMessages => {
                self.error = None;
                self.notice = None;
                Task::none()
            }
        }
    }

    fn open(&mut self, path: String) -> Task<Message> {
        self.busy = Some(format!("Opening {path}…"));
        self.error = None;
        db_task(
            async move {
                let database = Database::open(&path).await?;
                let catalog = database.sync_catalog().await?;
                Ok((database, catalog))
            },
            Message::DatabaseOpened,
        )
    }

    fn refresh_catalog(&mut self) -> Task<Message> {
        let Some(database) = self.database.clone() else {
            return Task::none();
        };
        db_task(
            async move { database.sync_catalog().await },
            Message::CatalogLoaded,
        )
    }

    /// SQL to run: the selection if there is one, otherwise the whole editor.
    fn sql_to_run(&self) -> String {
        self.editor
            .selection()
            .filter(|selection| !selection.trim().is_empty())
            .unwrap_or_else(|| self.editor.text())
    }

    fn run(&mut self) -> Task<Message> {
        let Some(database) = self.database.clone() else {
            self.error = Some("Open a database first (or create the sample database)".into());
            return Task::none();
        };
        if let Some(running) = self.running.take() {
            running.abort.abort();
        }
        let sql = self.sql_to_run();
        self.settings.remember_query(&sql);
        self.settings.save();
        self.error = None;
        self.notice = None;

        self.next_id += 1;
        let id = self.next_id;
        let limit = self.settings.row_limit;
        let (result, abort) =
            spawn_db(async move { database.run_sql(&sql, limit).await.map(Arc::new) });
        self.running = Some(Running {
            id,
            started: Instant::now(),
            abort,
        });
        Task::perform(result, move |result| Message::QueryFinished(id, result))
    }

    fn show_outcome(&mut self, outcome: Arc<QueryOutcome>) -> Task<Message> {
        self.generation += 1;
        self.selected = None;
        self.profiles = None;
        self.preview = None;
        self.gallery = None;
        self.media_columns = Vec::new();
        if !outcome.notes.is_empty() {
            self.notice = Some(outcome.notes.join(" · "));
        }
        let catalog_task = if outcome.catalog_changed {
            self.refresh_catalog()
        } else {
            Task::none()
        };

        self.table = outcome
            .result
            .as_ref()
            .map(|result| ResultTable::new(Arc::new(result.clone())));
        self.outcome = Some(outcome);

        let Some(table) = &self.table else {
            self.chart = None;
            return catalog_task;
        };
        self.chart_spec = ChartSpec::suggest(table);
        let batch = table.batch().clone();
        let media_columns = gallery::media_columns(table);
        self.rebuild_chart();

        self.media_columns = media_columns;
        let gallery_task = match self.media_columns.iter().find(|c| c.images) {
            Some(column) => self.build_gallery(column.index),
            None => {
                if self.tab == ResultTab::Media {
                    self.tab = ResultTab::Rows;
                }
                Task::none()
            }
        };

        let generation = self.generation;
        let profile_task = Task::perform(
            async move {
                tokio::task::spawn_blocking(move || profile_batch(&batch))
                    .await
                    .unwrap_or_default()
            },
            move |profiles| Message::ProfilesReady(generation, Arc::new(profiles)),
        );
        Task::batch([catalog_task, profile_task, gallery_task])
    }

    fn rebuild_chart(&mut self) {
        self.chart = self
            .table
            .as_ref()
            .map(|table| chart::build(table, &self.chart_spec));
    }

    /// Formatted value of the selected cell.
    pub fn selected_value(&self) -> Option<String> {
        let (row, column) = self.selected?;
        let table = self.table.as_ref()?;
        (row < table.row_count() && column < table.columns.len())
            .then(|| table.cell_text(row, column))
    }

    /// Whether the current result has an image column to show in the gallery.
    pub fn has_gallery(&self) -> bool {
        self.media_columns.iter().any(|c| c.images)
    }

    fn build_gallery(&mut self, column: usize) -> Task<Message> {
        let (Some(table), Some(media_column)) = (
            self.table.clone(),
            self.media_columns
                .iter()
                .find(|c| c.index == column)
                .cloned(),
        ) else {
            return Task::none();
        };
        let generation = self.generation;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || gallery::build(&table, &media_column))
                    .await
                    .ok()
            },
            move |gallery| match gallery {
                Some(gallery) => Message::GalleryReady(generation, Arc::new(gallery)),
                None => Message::Tick,
            },
        )
    }

    /// Decodes the selected cell's image (if it is one) off the UI thread.
    fn load_preview(&mut self, row: usize, column: usize) -> Task<Message> {
        let (Some(table), Some(media_column)) = (
            self.table.clone(),
            self.media_columns
                .iter()
                .find(|c| c.index == column && c.images)
                .cloned(),
        ) else {
            return Task::none();
        };
        let generation = self.generation;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let bytes = gallery::cell_bytes(&table, &media_column, table.source_row(row))?;
                    let image = media::decode(&bytes)?;
                    Some(Arc::new(Preview {
                        row,
                        column,
                        handle: gallery::handle_for(&bytes, gallery::PREVIEW_SIDE)?,
                        color: media::color_vector(&image),
                    }))
                })
                .await
                .ok()
                .flatten()
            },
            move |preview| Message::PreviewReady(generation, preview),
        )
    }

    /// `(table, colour-vector column)` to search with the selected image.
    pub fn similar_image_target(&self) -> Option<(String, String)> {
        let (row, column) = self.selected?;
        self.preview
            .as_ref()
            .filter(|p| p.row == row && p.column == column)?;
        let name = &self.table.as_ref()?.columns.get(column)?.name;
        let color_type = |data_type: &lancedb::arrow::arrow_schema::DataType| {
            matches!(data_type, lancedb::arrow::arrow_schema::DataType::FixedSizeList(item, n)
                if *n as usize == media::COLOR_DIM
                    && item.data_type() == &lancedb::arrow::arrow_schema::DataType::Float32)
        };
        // A table that has this image column and a colour vector next to it.
        self.catalog.iter().find_map(|table| {
            table.columns.iter().find(|c| &c.name == name)?;
            let vectors: Vec<_> = table
                .columns
                .iter()
                .filter(|c| color_type(&c.data_type))
                .collect();
            vectors
                .iter()
                .find(|c| c.name.contains("color"))
                .or(vectors.first())
                .map(|c| (table.name.clone(), c.name.clone()))
        })
    }

    fn find_similar_images(&mut self) -> Task<Message> {
        let (Some((table, column)), Some(preview)) =
            (self.similar_image_target(), self.preview.clone())
        else {
            return Task::none();
        };
        let vector: Vec<String> = preview.color.iter().map(|v| format!("{v:.5}")).collect();
        self.editor = text_editor::Content::with_text(&format!(
            "-- Images whose colours are closest to the selected image\nSELECT *\nFROM vector_search('{}', '{}', '[{}]', 12)\nORDER BY _distance;",
            table.replace('\'', "''"),
            column.replace('\'', "''"),
            vector.join(", ")
        ));
        self.tab = ResultTab::Media;
        self.run()
    }

    fn save_cell(&mut self) -> Task<Message> {
        let (Some((row, column)), Some(table)) = (self.selected, self.table.clone()) else {
            return Task::none();
        };
        let Some(media_column) = self
            .media_columns
            .iter()
            .find(|c| c.index == column && c.source == Source::Bytes)
            .cloned()
        else {
            return Task::none();
        };
        let Some(bytes) = gallery::cell_bytes(&table, &media_column, table.source_row(row)) else {
            return Task::none();
        };
        let extension = media::sniff(&bytes).map_or("bin", |format| format.extension);
        let file_name = format!("{}-row{}.{extension}", media_column.name, row + 1);
        Task::perform(
            async move {
                let Some(handle) = rfd::AsyncFileDialog::new()
                    .set_title("Save value as")
                    .set_file_name(&file_name)
                    .save_file()
                    .await
                else {
                    return Ok(None);
                };
                let path = handle.path().to_path_buf();
                tokio::fs::write(&path, bytes)
                    .await
                    .map(|()| Some(path))
                    .map_err(|e| e.to_string())
            },
            Message::CellSaved,
        )
    }

    /// `(table, column)` a selected vector cell can be searched against.
    pub fn similar_target(&self) -> Option<(String, String)> {
        let (_, column) = self.selected?;
        let meta = self.table.as_ref()?.columns.get(column)?;
        if !is_vector_type(&meta.data_type) {
            return None;
        }
        self.catalog.iter().find_map(|table| {
            table
                .columns
                .iter()
                .find(|c| c.name == meta.name && c.data_type == meta.data_type)
                .map(|c| (table.name.clone(), c.name.clone()))
        })
    }

    fn find_similar(&mut self) -> Task<Message> {
        let (Some((table, column)), Some(vector)) = (self.similar_target(), self.selected_value())
        else {
            return Task::none();
        };
        self.editor = text_editor::Content::with_text(&format!(
            "-- Nearest neighbours of the selected {column} value\nSELECT *\nFROM vector_search('{}', '{}', '{vector}', 10)\nORDER BY _distance;",
            table.replace('\'', "''"),
            column.replace('\'', "''"),
        ));
        self.tab = ResultTab::Rows;
        self.run()
    }

    /// Elapsed time of the running query.
    pub fn running_for(&self) -> Option<Duration> {
        self.running
            .as_ref()
            .map(|running| running.started.elapsed())
    }
}

/// Global keyboard shortcuts (for events no widget captured).
fn shortcut(event: iced::Event, _status: event::Status, _window: window::Id) -> Option<Message> {
    let iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event else {
        return None;
    };
    match key.as_ref() {
        keyboard::Key::Named(key::Named::Enter) if modifiers.command() => Some(Message::Run),
        keyboard::Key::Named(key::Named::F5) => Some(Message::Run),
        keyboard::Key::Named(key::Named::Escape) => Some(Message::Cancel),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_text_skips_repeated_causes() {
        let inner = anyhow::anyhow!("No field named nope");
        let outer = inner.context("Schema error: No field named nope");
        assert_eq!(error_text(&outer), "Schema error: No field named nope");

        let distinct = anyhow::anyhow!("permission denied").context("could not open /data");
        assert_eq!(
            error_text(&distinct),
            "could not open /data: permission denied"
        );
    }

    #[test]
    fn database_runtime_reports_cancellation() {
        let (result, abort) = spawn_db(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        });
        abort.abort();
        let result = futures::executor::block_on(result);
        assert_eq!(result, Err("cancelled".to_string()));
    }
}
