//! Application state, messages and update logic.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use iced::keyboard::{self, key};
use iced::widget::{pane_grid, text_editor};
use iced::{Subscription, Task, Theme, event, window};

use crate::db::functions::is_vector_type;
use crate::db::import::ImportSummary;
use crate::db::remote::{
    ColumnStatus, FunctionDetail, FunctionInfo, JobAction, JobDetail, JobRow, RemoteDatabase,
    function_ref,
};
use crate::db::{Computed, Database, IndexKind, QueryOutcome, TableInfo, TableRef, quote_ident};
use crate::enterprise::dashboard::{self, Range, TILES, TileContent, TileId};
use crate::enterprise::dialog::{ConnectDialog, ConnectField, OpenTab, reconnect_credential};
use crate::enterprise::profiles::{self, AuthMethod, Profiles, ResolvedCredential};
use crate::enterprise::{Check, EnterpriseTarget};
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

/// What the editor shows after connecting to a server (replacing
/// [`WELCOME_SQL`], whose `information_schema` query is local-only).
pub const ENTERPRISE_WELCOME_SQL: &str = "\
-- Connected to LanceDB Enterprise: SQL runs on the server, one statement at a time.
-- Try SHOW TABLES, SHOW JOBS, SHOW FUNCTIONS or SELECT … ORDER BY distance(column, [...]).
SHOW TABLES;
";

/// Rows shown by a table preview.
const PREVIEW_ROWS: usize = 100;

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
        "Video trailers (open the Media tab)",
        "-- Clips are generated with ffmpeg: animated gradients in each genre's colours.\nSELECT title, genre, clip,\n       media_duration(clip) AS seconds,\n       media_codec(clip) AS codec,\n       media_width(clip) AS width,\n       media_height(clip) AS height\nFROM trailers\nORDER BY genre;",
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

/// Reads an environment variable (credentials use the `lancedb` CLI's).
fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// `server 1.15.0-beta.0`, plus what is unavailable.
pub fn server_summary(database: &RemoteDatabase) -> String {
    let capabilities = database.capabilities();
    let mut parts = vec![match &capabilities.server_version {
        Some(version) => format!("server {version}"),
        None => "server version unknown".to_string(),
    }];
    if !capabilities.sql() {
        parts.push("SQL unavailable: browsing only".to_string());
    }
    parts.join(" · ")
}

/// `CREATE OR REPLACE FUNCTION` for `detail`'s signature and source: the
/// starting point for a new version of the Function listed as `listed`
/// (`ns$name` keeps its namespace).
pub fn function_sql(listed: &str, detail: &FunctionDetail) -> String {
    format!(
        "-- Running this creates a new version of {name}.\n\
         CREATE OR REPLACE FUNCTION {name}({inputs}) RETURNS {output}\n\
         LANGUAGE python AS $$\n{source}\n$$;\n",
        name = function_ref(listed).sql_name(),
        inputs = detail.inputs.join(", "),
        output = detail.output,
        source = detail.source.as_deref().unwrap_or_default().trim_end(),
    )
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
    pub kind: Option<media::MediaKind>,
    /// The picture: an image, or a video's poster frame.
    pub handle: Option<iced::widget::image::Handle>,
    /// Evenly spaced video frames (needs ffmpeg), for the filmstrip and
    /// flip-book playback.
    pub frames: Vec<iced::widget::image::Handle>,
    /// Colour descriptor of the picture (for "Find similar").
    pub color: Option<[f32; media::COLOR_DIM]>,
    /// Audio/video metadata.
    pub info: Option<crate::av::AvInfo>,
}

impl Preview {
    /// Delay between flip-book frames: roughly real time, within limits.
    pub fn frame_interval(&self) -> Duration {
        let duration = self
            .info
            .as_ref()
            .and_then(|info| info.duration)
            .unwrap_or(4.0);
        let per_frame = duration / self.frames.len().max(1) as f64;
        Duration::from_secs_f64(per_frame.clamp(0.12, 0.6))
    }
}

/// Whether an example can run against `catalog` (the trailers table only
/// exists when ffmpeg was available to generate it).
pub fn example_available(sql: &str, catalog: &[TableInfo]) -> bool {
    !sql.contains("FROM trailers") || catalog.iter().any(|t| t.name == "trailers")
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
    /// A Function's details (servers).
    Function,
}

/// What fills the window below the top bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// The sidebar, the editor and the results.
    #[default]
    Workspace,
    /// A server's jobs.
    Jobs,
    /// Tiles over a server's system views.
    Dashboards,
}

/// Which jobs the Jobs screen lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JobFilter {
    #[default]
    All,
    Active,
    Failed,
    Finished,
}

impl JobFilter {
    pub const ALL: [Self; 4] = [Self::All, Self::Active, Self::Failed, Self::Finished];

    pub fn matches(self, job: &JobRow) -> bool {
        match self {
            Self::All => true,
            Self::Active => job.is_active(),
            Self::Failed => job.failed(),
            Self::Finished => !job.is_active() && !job.failed(),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Active => "Active",
            Self::Failed => "Failed",
            Self::Finished => "Finished",
        }
    }
}

/// An API key typed in the dialog this session, and the REST endpoint it was
/// typed for: reconnecting reuses it for that server only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionKey {
    pub endpoint: String,
    pub key: String,
}

impl SessionKey {
    /// The key, when `target` is the server it was typed for.
    pub fn for_target(&self, target: &EnterpriseTarget) -> Option<&str> {
        target
            .endpoints()
            .is_ok_and(|(rest, _)| rest == self.endpoint)
            .then_some(self.key.as_str())
    }
}

/// The time now, in Unix milliseconds.
pub fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

/// Most jobs the Jobs screen lists at once.
pub const MAX_LISTED_JOBS: usize = 300;
/// How often the Jobs screen refreshes while it shows.
const JOBS_REFRESH: Duration = Duration::from_secs(3);
/// How often the dashboard's timer re-checks table health: each check runs
/// two statements per table.
const HEALTH_REFRESH_MILLIS: i64 = 10 * 60 * 1000;
/// How often the Dashboards screen refreshes while it shows.
const DASHBOARD_REFRESH: Duration = Duration::from_secs(60);

/// The Jobs screen.
#[derive(Debug, Clone, Default)]
pub struct JobsView {
    /// `None` until the first load.
    pub jobs: Option<Result<Vec<JobRow>, String>>,
    pub loading: bool,
    pub filter: JobFilter,
    pub search: String,
    pub selected: Option<String>,
    pub detail: Option<Result<JobDetail, String>>,
    /// The job whose Cancel was clicked once and needs confirming.
    pub confirm_cancel: Option<String>,
    /// What the server said to the last pause, resume or cancel of the
    /// selected job.
    pub action: Option<Result<String, String>>,
}

impl JobsView {
    /// The jobs the filter and search let through, newest first.
    pub fn visible(&self) -> Vec<&JobRow> {
        let Some(Ok(jobs)) = &self.jobs else {
            return Vec::new();
        };
        let search = self.search.trim().to_lowercase();
        jobs.iter()
            .filter(|job| self.filter.matches(job))
            .filter(|job| {
                search.is_empty()
                    || [&job.job_id, &job.job_type, &job.table, &job.function]
                        .iter()
                        .any(|field| field.to_lowercase().contains(&search))
            })
            .take(MAX_LISTED_JOBS)
            .collect()
    }

    /// How many jobs each filter matches.
    pub fn count(&self, filter: JobFilter) -> usize {
        match &self.jobs {
            Some(Ok(jobs)) => jobs.iter().filter(|job| filter.matches(job)).count(),
            _ => 0,
        }
    }

    pub fn selected_job(&self) -> Option<&JobRow> {
        let id = self.selected.as_ref()?;
        match &self.jobs {
            Some(Ok(jobs)) => jobs.iter().find(|job| &job.job_id == id),
            _ => None,
        }
    }
}

/// The Dashboards screen.
#[derive(Debug, Clone, Default)]
pub struct DashboardView {
    pub range: Range,
    /// Loaded tiles; a tile keeps its last content while it reloads.
    pub tiles: BTreeMap<TileId, Result<TileContent, String>>,
    /// Bumped on every refresh, so late replies of an older one are dropped.
    pub generation: u64,
    /// When a tile last arrived (Unix milliseconds).
    pub loaded_at: Option<i64>,
    /// Tiles of the latest refresh still loading.
    pub pending: usize,
    /// When table health last arrived (Unix milliseconds).
    pub health_at: Option<i64>,
}

/// A Function shown in the results pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionView {
    pub name: String,
    pub version: String,
    /// `None` while it loads.
    pub detail: Option<Result<FunctionDetail, String>>,
}

/// Template for a new Function (`NewFunctionTemplate`).
pub const FUNCTION_TEMPLATE: &str = "\
-- A Python Function the server runs. Add dependencies with WITH (pip = ['numpy'])
-- after the closing $$, and compute a column with it:
--   ALTER TABLE t ADD COLUMN y AS my_function(x) WITH (version = '1');
CREATE FUNCTION my_function(x BIGINT NOT NULL) RETURNS BIGINT
LANGUAGE python AS $$
def my_function(x: int) -> int:
    return x + 1
$$;
";

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
    /// Sample database path and number of generated trailer videos.
    SampleReady(Result<(String, usize), String>),
    DatabaseOpened(Result<(Database, Vec<TableInfo>), String>),
    RefreshCatalog,
    CatalogLoaded(Result<Vec<TableInfo>, String>),
    // LanceDB Enterprise
    /// Opens the Open dialog (a local folder or a server).
    OpenConnectDialog,
    SelectOpenTab(OpenTab),
    CloseConnectDialog,
    ConnectProfile(String),
    ConnectEdit(ConnectField, String),
    ConnectAuth(AuthMethod),
    TestConnection,
    ConnectionTested(Result<Vec<Check>, String>),
    ConnectEnterprise,
    OpenRecentEnterprise(EnterpriseTarget),
    ShowScreen(Screen),
    RefreshJobs,
    /// Jobs loaded for the database opened `opened`-th (see [`App::opened`]).
    JobsLoaded(u64, Result<Vec<JobRow>, String>),
    SetJobFilter(JobFilter),
    SearchJobs(String),
    SelectJob(String),
    JobDetailLoaded(String, Result<JobDetail, String>),
    RunJobAction(String, JobAction),
    JobActionDone(String, Result<String, String>),
    CloseJob,
    /// Shows a job (by id) on the Jobs screen.
    OpenJob(String),
    RefreshDashboard,
    /// The Dashboards screen's timer: a refresh unless one is still loading.
    DashboardTick,
    SetDashboardRange(Range),
    /// Runs a tile's SQL (for the current range) in the editor.
    OpenTileQuery(TileId),
    /// Runs `SHOW STATS FOR TABLE` for a table (by [`TableInfo::key`]) in
    /// the editor.
    OpenTableStats(String),
    TileLoaded(u64, TileId, Result<TileContent, String>),
    SwitchDatabase(String),
    /// On a server, also run `EXPLAIN ANALYZE` to fill the plan view.
    SetCollectPlan(bool),
    FunctionsLoaded(u64, Result<Vec<FunctionInfo>, String>),
    /// Shows a Function version (name, version) in the Function tab.
    SelectFunction(String, String),
    FunctionDescribed(String, String, Result<FunctionDetail, String>),
    CloseFunction,
    /// Freshness of a table's Function columns, by [`TableInfo::key`].
    ColumnStatusLoaded(u64, String, Result<Vec<ColumnStatus>, String>),
    RefreshColumn(TableRef, String),
    ShowColumnErrors(TableRef),
    ComputedColumnTemplate(TableRef),
    NewFunctionTemplate,
    EditFunctionTemplate,
    DropFunctionTemplate,
    RefreshMaterializedView(TableRef),
    // Sidebar
    /// Expands or collapses a catalog entry, by [`TableInfo::key`].
    ToggleTable(String),
    PreviewTable(TableRef),
    InsertText(String),
    CreateIndex(TableRef, String, IndexKind),
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
    /// Import summary and the refreshed catalog (which includes the new
    /// table, so the follow-up query can run).
    MediaImported(Result<(ImportSummary, Vec<TableInfo>), String>),
    GalleryReady(u64, Arc<Gallery>),
    SetGalleryColumn(ColumnChoice),
    PreviewReady(u64, Option<Arc<Preview>>),
    SaveCell,
    CellSaved(Result<Option<PathBuf>, String>),
    OpenExternally,
    OpenedExternally(Result<(), String>),
    TogglePlayback,
    NextFrame,
    ShowFrame(usize),
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
    /// Expanded catalog entries, by [`TableInfo::key`].
    pub expanded: HashSet<String>,
    pub busy: Option<String>,

    /// The "Connect to LanceDB Enterprise" dialog, while open.
    pub connect: Option<ConnectDialog>,
    /// The `lancedb` CLI's profiles file (`None` in tests: built-in only).
    config_path: Option<PathBuf>,
    /// An API key typed this session, for switching databases on the same
    /// server. Never saved.
    session_api_key: Option<SessionKey>,
    /// On a server: also run `EXPLAIN ANALYZE` for the plan view.
    pub collect_plan: bool,
    /// Every Function version on the server (`SHOW FUNCTIONS HISTORY`).
    pub functions: Vec<FunctionInfo>,
    /// The Function in the Function tab.
    pub function: Option<FunctionView>,
    /// Freshness of Function columns, by [`TableInfo::key`].
    pub column_status: HashMap<String, Result<Vec<ColumnStatus>, String>>,
    pub screen: Screen,
    pub jobs: JobsView,
    pub dashboard: DashboardView,
    /// How many databases have been opened; replies loaded for an earlier
    /// one carry an older count and are dropped.
    pub opened: u64,

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
    /// Filmstrip frame shown in the preview, and whether it is flipping.
    pub frame: usize,
    pub playing: bool,
    /// Extra note to show once the next database finishes opening.
    open_note: Option<String>,
}

impl App {
    /// Creates the app, optionally opening `initial` right away.
    pub fn new(initial: Option<String>) -> (Self, Task<Message>) {
        let (mut app, task) = Self::with_settings(Settings::load(), initial);
        app.config_path = profiles::config_path();
        (app, task)
    }

    /// Creates the app with explicit settings (tests pass in-memory ones).
    pub fn with_settings(settings: Settings, initial: Option<String>) -> (Self, Task<Message>) {
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
            connect: None,
            config_path: None,
            session_api_key: None,
            collect_plan: false,
            functions: Vec::new(),
            function: None,
            column_status: HashMap::new(),
            screen: Screen::default(),
            jobs: JobsView::default(),
            dashboard: DashboardView::default(),
            opened: 0,
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
            frame: 0,
            playing: false,
            open_note: None,
        };

        let task = match initial {
            Some(path) => app.open(path),
            None => Task::none(),
        };
        (app, task)
    }

    pub fn title(&self) -> String {
        match &self.database {
            Some(db) => format!("joust — {}", db.label()),
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
        let mut subscriptions = vec![event::listen_with(shortcut)];
        if self.running.is_some() {
            subscriptions
                .push(iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick));
        }
        if self.playing
            && let Some(preview) = &self.preview
        {
            subscriptions
                .push(iced::time::every(preview.frame_interval()).map(|_| Message::NextFrame));
        }
        if self.remote().is_some() {
            match self.screen {
                Screen::Jobs => subscriptions
                    .push(iced::time::every(JOBS_REFRESH).map(|_| Message::RefreshJobs)),
                Screen::Dashboards => subscriptions
                    .push(iced::time::every(DASHBOARD_REFRESH).map(|_| Message::DashboardTick)),
                Screen::Workspace => {}
            }
        }
        Subscription::batch(subscriptions)
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::PathChanged(path) => {
                self.path_input = path;
                Task::none()
            }
            Message::OpenDatabase => {
                if self.busy.is_some() {
                    return Task::none();
                }
                let path = self.path_input.trim().to_string();
                if path.is_empty() {
                    self.fail("Enter the path of a LanceDB database directory".into());
                    return Task::none();
                }
                self.open(path)
            }
            Message::OpenRecent(_) if self.busy.is_some() => Task::none(),
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
                        let trailers = crate::db::sample::create_sample_database(&path).await?;
                        Ok((path.to_string_lossy().to_string(), trailers))
                    },
                    Message::SampleReady,
                )
            }
            Message::SampleReady(Ok((path, trailers))) => {
                self.path_input = path.clone();
                self.open_note = (trailers == 0).then(|| {
                    "ffmpeg was not found, so the sample's video trailers table was skipped".into()
                });
                if self.editor.text().trim() == WELCOME_SQL.trim() {
                    self.editor = text_editor::Content::with_text(SAMPLE_EXAMPLES[0].1);
                }
                self.open(path)
            }
            Message::SampleReady(Err(error)) | Message::DatabaseOpened(Err(error)) => {
                self.busy = None;
                self.fail(error);
                Task::none()
            }
            Message::DatabaseOpened(Ok((database, catalog))) => {
                self.busy = None;
                self.connect = None;
                let server_note = match &database {
                    Database::Local(_) => {
                        self.settings.remember_database(database.uri());
                        None
                    }
                    Database::Remote(remote) => {
                        self.settings.remember_enterprise(remote.target());
                        if self.editor.text().trim() == WELCOME_SQL.trim() {
                            self.editor = text_editor::Content::with_text(ENTERPRISE_WELCOME_SQL);
                        }
                        Some(server_summary(remote))
                    }
                };
                self.settings.save();
                self.notice = Some(format!(
                    "Opened {} ({} table{}){}",
                    database.label(),
                    catalog.len(),
                    if catalog.len() == 1 { "" } else { "s" },
                    server_note
                        .into_iter()
                        .chain(self.open_note.take())
                        .map(|note| format!(" · {note}"))
                        .collect::<String>(),
                ));
                self.error = None;
                self.expanded = catalog.iter().take(3).map(TableInfo::key).collect();
                self.catalog = catalog;
                self.database = Some(database);
                self.functions.clear();
                self.function = None;
                self.column_status.clear();
                if self.tab == ResultTab::Function {
                    self.tab = ResultTab::Rows;
                }
                self.opened += 1;
                self.jobs = JobsView::default();
                // The generation keeps counting, so tiles still loading for
                // the previous database are dropped when they arrive.
                self.dashboard = DashboardView {
                    generation: self.dashboard.generation + 1,
                    ..DashboardView::default()
                };
                if self.remote().is_none() {
                    self.screen = Screen::Workspace;
                }
                let refresh = match self.screen {
                    Screen::Jobs => self.refresh_jobs(),
                    Screen::Dashboards => self.refresh_dashboard(true),
                    Screen::Workspace => Task::none(),
                };
                Task::batch([self.load_server_extras(), refresh])
            }
            Message::OpenConnectDialog => {
                let mut dialog = ConnectDialog::new(self.config_path.as_deref());
                // Start where the open database is.
                if self.remote().is_some() {
                    dialog.tab = OpenTab::Enterprise;
                }
                self.connect = Some(dialog);
                Task::none()
            }
            Message::SelectOpenTab(tab) => {
                if let Some(dialog) = &mut self.connect {
                    dialog.tab = tab;
                    dialog.error = None;
                }
                Task::none()
            }
            Message::CloseConnectDialog => {
                self.connect = None;
                Task::none()
            }
            Message::ConnectProfile(name) => {
                if let Some(dialog) = &mut self.connect {
                    dialog.select(&name);
                }
                Task::none()
            }
            Message::ConnectEdit(field, value) => {
                if let Some(dialog) = &mut self.connect {
                    dialog.edit(field, value);
                }
                Task::none()
            }
            Message::ConnectAuth(method) => {
                if let Some(dialog) = &mut self.connect {
                    dialog.set_auth(method);
                }
                Task::none()
            }
            Message::TestConnection => {
                let Some(dialog) = &mut self.connect else {
                    return Task::none();
                };
                match dialog.resolve(env_var) {
                    Err(error) => {
                        dialog.error = Some(error_text(&error));
                        Task::none()
                    }
                    Ok((target, credential)) => {
                        dialog.working = Some("Testing…");
                        dialog.checks = None;
                        dialog.error = None;
                        db_task(
                            async move {
                                let database = RemoteDatabase::connect(target, credential).await?;
                                Ok(database.checks())
                            },
                            Message::ConnectionTested,
                        )
                    }
                }
            }
            Message::ConnectionTested(result) => {
                if let Some(dialog) = &mut self.connect {
                    dialog.working = None;
                    match result {
                        Ok(checks) => dialog.checks = Some(checks),
                        Err(error) => dialog.error = Some(error),
                    }
                }
                Task::none()
            }
            Message::ConnectEnterprise => {
                let Some(dialog) = &mut self.connect else {
                    return Task::none();
                };
                match dialog.resolve(env_var) {
                    Err(error) => {
                        dialog.error = Some(error_text(&error));
                        Task::none()
                    }
                    Ok((target, credential)) => {
                        dialog.working = Some("Connecting…");
                        dialog.error = None;
                        self.session_api_key = (target.auth == AuthMethod::ApiKey)
                            .then(|| target.endpoints().ok())
                            .flatten()
                            .map(|(endpoint, _)| SessionKey {
                                endpoint,
                                key: dialog.api_key.trim().to_string(),
                            });
                        self.connect_to(target, credential)
                    }
                }
            }
            Message::OpenRecentEnterprise(target) => {
                if self.busy.is_some() {
                    return Task::none();
                }
                self.reconnect(target)
            }
            Message::ShowScreen(screen) => {
                if screen != Screen::Workspace && self.remote().is_none() {
                    return Task::none();
                }
                self.screen = screen;
                match screen {
                    Screen::Jobs => Task::batch([self.refresh_jobs(), self.load_job_detail()]),
                    Screen::Dashboards if self.dashboard.tiles.is_empty() => {
                        self.refresh_dashboard(true)
                    }
                    _ => Task::none(),
                }
            }
            Message::RefreshJobs => {
                let detail = self
                    .jobs
                    .selected_job()
                    .filter(|job| job.is_active())
                    .map(|_| self.load_job_detail());
                Task::batch([self.refresh_jobs(), detail.unwrap_or_else(Task::none)])
            }
            Message::JobsLoaded(opened, result) => {
                if opened == self.opened {
                    self.jobs.loading = false;
                    self.jobs.jobs = Some(result);
                }
                Task::none()
            }
            Message::SetJobFilter(filter) => {
                self.jobs.filter = filter;
                Task::none()
            }
            Message::SearchJobs(text) => {
                self.jobs.search = text;
                Task::none()
            }
            Message::SelectJob(job_id) => {
                self.jobs.selected = Some(job_id);
                self.jobs.detail = None;
                self.jobs.confirm_cancel = None;
                self.jobs.action = None;
                self.load_job_detail()
            }
            Message::JobDetailLoaded(job_id, result) => {
                if self.jobs.selected.as_ref() == Some(&job_id) {
                    self.jobs.detail = Some(result);
                }
                Task::none()
            }
            Message::RunJobAction(job_id, action) => {
                let Some(remote) = self.remote().cloned() else {
                    return Task::none();
                };
                if action == JobAction::Cancel && self.jobs.confirm_cancel.as_ref() != Some(&job_id)
                {
                    // Cancelling throws work away: ask for a second click.
                    self.jobs.confirm_cancel = Some(job_id);
                    return Task::none();
                }
                self.jobs.confirm_cancel = None;
                let id = job_id.clone();
                Task::perform(
                    spawn_db(async move { remote.job_action(&id, action).await }).0,
                    move |result| Message::JobActionDone(job_id.clone(), result),
                )
            }
            Message::JobActionDone(job_id, result) => {
                if self.jobs.selected.as_ref() == Some(&job_id) {
                    self.jobs.action = Some(result);
                }
                Task::batch([self.refresh_jobs(), self.load_job_detail()])
            }
            Message::OpenJob(job_id) => {
                if self.remote().is_none() {
                    return Task::none();
                }
                self.screen = Screen::Jobs;
                self.jobs.filter = JobFilter::All;
                self.jobs.search.clear();
                self.jobs.selected = Some(job_id);
                self.jobs.detail = None;
                self.jobs.confirm_cancel = None;
                self.jobs.action = None;
                Task::batch([self.refresh_jobs(), self.load_job_detail()])
            }
            Message::CloseJob => {
                self.jobs.selected = None;
                self.jobs.detail = None;
                self.jobs.confirm_cancel = None;
                self.jobs.action = None;
                Task::none()
            }
            Message::RefreshDashboard => self.refresh_dashboard(true),
            Message::DashboardTick => {
                if self.dashboard.pending > 0 {
                    return Task::none();
                }
                self.refresh_dashboard(false)
            }
            Message::OpenTableStats(key) => {
                let Some(table) = self.catalog.iter().find(|table| table.key() == key) else {
                    return Task::none();
                };
                let sql = format!("SHOW STATS FOR TABLE {};", table.sql_name());
                self.screen = Screen::Workspace;
                self.editor = text_editor::Content::with_text(&sql);
                self.run()
            }
            Message::OpenTileQuery(tile) => {
                let database = self
                    .remote()
                    .map(|remote| remote.target().database.clone())
                    .unwrap_or_default();
                let Some(sql) =
                    dashboard::tile_sql(tile, self.dashboard.range, now_millis(), &database)
                else {
                    return Task::none();
                };
                self.screen = Screen::Workspace;
                self.editor = text_editor::Content::with_text(&sql);
                self.run()
            }
            Message::SetDashboardRange(range) => {
                self.dashboard.range = range;
                self.refresh_dashboard(true)
            }
            Message::TileLoaded(generation, tile, result) => {
                if generation == self.dashboard.generation {
                    if tile == TileId::TableHealth && result.is_ok() {
                        self.dashboard.health_at = Some(now_millis());
                    }
                    self.dashboard.tiles.insert(tile, result);
                    self.dashboard.loaded_at = Some(now_millis());
                    self.dashboard.pending = self.dashboard.pending.saturating_sub(1);
                }
                Task::none()
            }
            Message::SwitchDatabase(name) => {
                // One connection at a time: the last one to finish would win.
                if self.busy.is_some() {
                    return Task::none();
                }
                let Some(remote) = self.database.as_ref().and_then(Database::remote) else {
                    return Task::none();
                };
                if remote.target().database == name {
                    return Task::none();
                }
                let target = remote.target().with_database(&name);
                self.reconnect(target)
            }
            Message::RefreshCatalog => self.refresh_catalog(),
            Message::CatalogLoaded(Ok(catalog)) => {
                self.busy = None;
                self.catalog = catalog;
                self.column_status.clear();
                self.load_server_extras()
            }
            Message::CatalogLoaded(Err(error)) => {
                self.busy = None;
                self.error = Some(error);
                Task::none()
            }
            Message::ToggleTable(key) => {
                if self.expanded.remove(&key) {
                    return Task::none();
                }
                self.expanded.insert(key.clone());
                if self.column_status.contains_key(&key) {
                    Task::none()
                } else {
                    self.load_column_status(&key)
                }
            }
            Message::SetCollectPlan(on) => {
                self.collect_plan = on;
                Task::none()
            }
            Message::FunctionsLoaded(opened, _) if opened != self.opened => Task::none(),
            Message::FunctionsLoaded(_, Ok(functions)) => {
                self.functions = functions;
                Task::none()
            }
            Message::FunctionsLoaded(_, Err(error)) => {
                self.error = Some(format!("Could not list Functions: {error}"));
                Task::none()
            }
            Message::SelectFunction(name, version) => {
                let Some(remote) = self.database.as_ref().and_then(Database::remote).cloned()
                else {
                    return Task::none();
                };
                self.function = Some(FunctionView {
                    name: name.clone(),
                    version: version.clone(),
                    detail: None,
                });
                self.tab = ResultTab::Function;
                Task::perform(
                    spawn_db({
                        let (name, version) = (name.clone(), version.clone());
                        async move { remote.describe_function(&name, &version).await }
                    })
                    .0,
                    move |result| Message::FunctionDescribed(name.clone(), version.clone(), result),
                )
            }
            Message::FunctionDescribed(name, version, result) => {
                if let Some(view) = &mut self.function
                    && view.name == name
                    && view.version == version
                {
                    view.detail = Some(result);
                }
                Task::none()
            }
            Message::CloseFunction => {
                self.function = None;
                if self.tab == ResultTab::Function {
                    self.tab = ResultTab::Rows;
                }
                Task::none()
            }
            Message::ColumnStatusLoaded(opened, key, result) => {
                if opened == self.opened {
                    self.column_status.insert(key, result);
                }
                Task::none()
            }
            Message::RefreshColumn(table, column) => {
                self.editor = text_editor::Content::with_text(&format!(
                    "REFRESH COLUMN {} ON {};",
                    quote_ident(&column),
                    table.sql_name()
                ));
                self.run()
            }
            Message::ShowColumnErrors(table) => {
                self.editor = text_editor::Content::with_text(&format!(
                    "SHOW ERRORS ON {};",
                    table.sql_name()
                ));
                self.run()
            }
            Message::RefreshMaterializedView(table) => {
                self.editor = text_editor::Content::with_text(&format!(
                    "REFRESH MATERIALIZED VIEW {};",
                    table.sql_name()
                ));
                self.run()
            }
            Message::ComputedColumnTemplate(table) => {
                let name = table.sql_name();
                self.editor = text_editor::Content::with_text(&format!(
                    "-- Add a computed column to {}: a SQL expression over other columns…\n\
                     ALTER TABLE {name} ADD COLUMN new_column AS <expression>;\n\
                     -- …or a Function version (see FUNCTIONS in the sidebar):\n\
                     -- ALTER TABLE {name} ADD COLUMN new_column AS my_function(column) WITH (version = '1');\n\
                     -- Then compute it: REFRESH COLUMN new_column ON {name};\n",
                    table.key()
                ));
                self.notice = Some("Edit the template, then run it".into());
                Task::none()
            }
            Message::NewFunctionTemplate => {
                self.editor = text_editor::Content::with_text(FUNCTION_TEMPLATE);
                self.notice = Some("Edit the template, then run it to create the Function".into());
                Task::none()
            }
            Message::EditFunctionTemplate => {
                let Some(Some(Ok(detail))) = self.function.as_ref().map(|view| &view.detail) else {
                    return Task::none();
                };
                let Some(view) = &self.function else {
                    return Task::none();
                };
                self.editor = text_editor::Content::with_text(&function_sql(&view.name, detail));
                self.notice = Some(
                    "Running this creates a new version; columns keep the version they use".into(),
                );
                Task::none()
            }
            Message::DropFunctionTemplate => {
                let Some(view) = &self.function else {
                    return Task::none();
                };
                self.editor = text_editor::Content::with_text(&format!(
                    "-- Drops this version. Columns computed by it stop refreshing.\n\
                     DROP FUNCTION {} WITH (version = '{}');\n",
                    function_ref(&view.name).sql_name(),
                    view.version.replace('\'', "''")
                ));
                self.notice = Some("Review, then run it to drop the version".into());
                Task::none()
            }
            Message::PreviewTable(table) => {
                if let Some(database) = self.database.clone()
                    && !database.sql_available()
                {
                    // No SQL on this server: read the rows over REST instead.
                    self.error = None;
                    self.notice = None;
                    return self.start_query(async move {
                        database.preview(&table, PREVIEW_ROWS).await.map(Arc::new)
                    });
                }
                self.editor = text_editor::Content::with_text(&format!(
                    "SELECT *\nFROM {}\nLIMIT {PREVIEW_ROWS};",
                    table.sql_name()
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
                self.busy = Some(format!("Indexing {}.{column}…", table.key()));
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
                    if let Some(remote) = self.database.as_ref().and_then(Database::remote).cloned()
                    {
                        // Also stop work the server would otherwise finish
                        // (queries it runs as jobs keep going on their own).
                        return Task::perform(
                            spawn_db(async move { remote.cancel().await }).0,
                            |_| Message::Tick,
                        );
                    }
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
                if self.selected != Some((row, column)) {
                    self.playing = false;
                    self.frame = 0;
                }
                self.selected = Some((row, column));
                self.load_preview(row, column)
            }
            Message::PreviewReady(generation, preview) => {
                if generation == self.generation
                    && let Some(preview) = preview
                    && self.selected == Some((preview.row, preview.column))
                {
                    self.frame = 0;
                    self.playing = false;
                    self.preview = Some(preview);
                }
                Task::none()
            }
            Message::TogglePlayback => {
                self.playing = !self.playing && self.current_frames() > 1;
                Task::none()
            }
            Message::NextFrame => {
                let frames = self.current_frames();
                if frames > 0 {
                    self.frame = (self.frame + 1) % frames;
                }
                Task::none()
            }
            Message::ShowFrame(frame) => {
                self.playing = false;
                self.frame = frame.min(self.current_frames().saturating_sub(1));
                Task::none()
            }
            Message::OpenExternally => self.open_externally(),
            Message::OpenedExternally(Ok(())) => Task::none(),
            Message::OpenedExternally(Err(error)) => {
                self.error = Some(format!("Could not open the file: {error}"));
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
                    async move {
                        let summary = database.import_media_folder(folder).await?;
                        let catalog = database.sync_catalog().await?;
                        Ok((summary, catalog))
                    },
                    Message::MediaImported,
                )
            }
            Message::MediaImported(Ok((summary, catalog))) => {
                self.busy = None;
                self.catalog = catalog;
                self.expanded.insert(summary.table.clone());
                self.editor = text_editor::Content::with_text(&format!(
                    "SELECT file_name, kind, width, height, duration_s, codec, size_bytes, modified, thumbnail, path\nFROM {}\nORDER BY file_name\nLIMIT 200;",
                    quote_ident(&summary.table)
                ));
                self.tab = ResultTab::Media;
                // Running clears messages, so post the summary afterwards.
                let task = self.run();
                self.notice = Some(format!(
                    "Imported {} file{} into {}{}",
                    summary.imported,
                    if summary.imported == 1 { "" } else { "s" },
                    summary.table,
                    [
                        (summary.skipped > 0)
                            .then(|| format!("{} unreadable or over the limit", summary.skipped)),
                        (summary.referenced > 0).then(|| {
                            format!("{} large file(s) kept by path only", summary.referenced)
                        }),
                    ]
                    .into_iter()
                    .flatten()
                    .map(|note| format!(" · {note}"))
                    .collect::<String>()
                ));
                task
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

    /// Shows why opening failed: in the Open dialog when it is up (it
    /// started the attempt), else in the window.
    fn fail(&mut self, error: String) {
        match &mut self.connect {
            Some(dialog) => {
                dialog.working = None;
                dialog.error = Some(error);
            }
            None => self.error = Some(error),
        }
    }

    /// The open server, if any.
    pub fn remote(&self) -> Option<&RemoteDatabase> {
        self.database.as_ref().and_then(Database::remote)
    }

    /// Reloads the jobs list (unless a load is already running).
    fn refresh_jobs(&mut self) -> Task<Message> {
        let Some(remote) = self.remote().cloned() else {
            return Task::none();
        };
        if self.jobs.loading {
            return Task::none();
        }
        self.jobs.loading = true;
        let opened = self.opened;
        db_task(async move { remote.jobs().await }, move |result| {
            Message::JobsLoaded(opened, result)
        })
    }

    /// Loads the selected job's detail.
    fn load_job_detail(&self) -> Task<Message> {
        let (Some(remote), Some(job_id)) = (self.remote().cloned(), self.jobs.selected.clone())
        else {
            return Task::none();
        };
        let id = job_id.clone();
        Task::perform(
            spawn_db(async move { remote.job_detail(&id).await }).0,
            move |result| Message::JobDetailLoaded(job_id.clone(), result),
        )
    }

    /// Reloads the dashboard's tiles; tiles keep their last content until
    /// the new one arrives. Unless `full`, table health is skipped while it
    /// is less than [`HEALTH_REFRESH_MILLIS`] old.
    fn refresh_dashboard(&mut self, full: bool) -> Task<Message> {
        let Some(remote) = self.remote().cloned() else {
            return Task::none();
        };
        self.dashboard.generation += 1;
        let generation = self.dashboard.generation;
        let (range, now) = (self.dashboard.range, now_millis());
        let database = remote.target().database.clone();
        let health_fresh = !full
            && self.dashboard.tiles.contains_key(&TileId::TableHealth)
            && self
                .dashboard
                .health_at
                .is_some_and(|at| now - at < HEALTH_REFRESH_MILLIS);
        let tables: Vec<TableRef> = self
            .catalog
            .iter()
            .filter(|table| table.kind != crate::db::TableKind::View)
            .map(TableInfo::table_ref)
            .collect();
        self.dashboard
            .tiles
            .insert(TileId::Tables, Ok(dashboard::tables_kpi(&self.catalog)));
        let ids: Vec<TileId> = TILES
            .iter()
            .map(|tile| tile.id)
            .filter(|id| *id != TileId::Tables)
            .filter(|id| !(health_fresh && *id == TileId::TableHealth))
            .collect();
        self.dashboard.pending = ids.len();
        let tasks = ids.into_iter().map(|id| {
            let (remote, tables, database) = (remote.clone(), tables.clone(), database.clone());
            Task::perform(
                spawn_db(async move {
                    match id {
                        TileId::ActiveJobs => Ok(dashboard::active_jobs_kpi(&remote.jobs().await?)),
                        TileId::TableHealth => Ok(dashboard::health_table(
                            &remote.table_health(&tables).await?,
                        )),
                        _ => {
                            let sql =
                                dashboard::tile_sql(id, range, now, &database).expect("a SQL tile");
                            let batches = remote.sql_batches(&sql).await?;
                            Ok(dashboard::content(id.tile(), &batches, range, now))
                        }
                    }
                })
                .0,
                move |result| Message::TileLoaded(generation, id, result),
            )
        });
        Task::batch(tasks.collect::<Vec<_>>())
    }

    /// After connecting to a server or refreshing its catalog: load its
    /// Functions, and the column status of expanded tables.
    fn load_server_extras(&mut self) -> Task<Message> {
        let Some(remote) = self.database.as_ref().and_then(Database::remote).cloned() else {
            self.functions.clear();
            return Task::none();
        };
        let opened = self.opened;
        let functions = db_task(async move { remote.functions().await }, move |result| {
            Message::FunctionsLoaded(opened, result)
        });
        let keys: Vec<String> = self.expanded.iter().cloned().collect();
        let statuses = keys.iter().map(|key| self.load_column_status(key));
        Task::batch(
            std::iter::once(functions)
                .chain(statuses)
                .collect::<Vec<_>>(),
        )
    }

    /// Loads the freshness of `key`'s Function columns, if it has any.
    fn load_column_status(&self, key: &str) -> Task<Message> {
        let Some(remote) = self.database.as_ref().and_then(Database::remote).cloned() else {
            return Task::none();
        };
        let Some(table) = self.catalog.iter().find(|table| {
            table.key() == key
                && table
                    .columns
                    .iter()
                    .any(|column| column.computed == Some(Computed::Function))
        }) else {
            return Task::none();
        };
        let table = table.table_ref();
        let key = key.to_string();
        let opened = self.opened;
        Task::perform(
            spawn_db(async move { remote.column_status(&table).await }).0,
            move |result| Message::ColumnStatusLoaded(opened, key.clone(), result),
        )
    }

    /// Rows a server query has sent so far (`None` locally).
    pub fn rows_received(&self) -> Option<usize> {
        self.database
            .as_ref()
            .and_then(Database::remote)
            .map(RemoteDatabase::rows_received)
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
        let sql = self.sql_to_run();
        self.settings.remember_query(&sql);
        self.settings.save();
        self.error = None;
        self.notice = None;
        let limit = self.settings.row_limit;
        let collect_plan = self.collect_plan;
        self.start_query(async move { database.run(&sql, limit, collect_plan).await.map(Arc::new) })
    }

    /// Starts `work` as the running query, cancelling any other.
    fn start_query(
        &mut self,
        work: impl Future<Output = anyhow::Result<Arc<QueryOutcome>>> + Send + 'static,
    ) -> Task<Message> {
        if let Some(running) = self.running.take() {
            running.abort.abort();
        }
        self.next_id += 1;
        let id = self.next_id;
        let (result, abort) = spawn_db(work);
        self.running = Some(Running {
            id,
            started: Instant::now(),
            abort,
        });
        Task::perform(result, move |result| Message::QueryFinished(id, result))
    }

    /// Connects to a LanceDB Enterprise server and loads its catalog.
    fn connect_to(
        &mut self,
        target: EnterpriseTarget,
        credential: ResolvedCredential,
    ) -> Task<Message> {
        self.busy = Some(format!("Connecting to {}…", target.label()));
        self.error = None;
        db_task(
            async move {
                let database = Database::connect(target, credential).await?;
                let catalog = database.sync_catalog().await?;
                Ok((database, catalog))
            },
            Message::DatabaseOpened,
        )
    }

    /// Connects to `target` without the dialog, or opens the dialog on it
    /// when a credential has to be entered.
    fn reconnect(&mut self, target: EnterpriseTarget) -> Task<Message> {
        let profiles = self
            .config_path
            .as_deref()
            .and_then(|path| Profiles::load(path).ok())
            .unwrap_or_default();
        let key = self
            .session_api_key
            .as_ref()
            .and_then(|session| session.for_target(&target));
        match reconnect_credential(&target, &profiles, key, env_var) {
            Ok(credential) => self.connect_to(target, credential),
            Err(error) => {
                self.connect = Some(ConnectDialog::for_target(
                    self.config_path.as_deref(),
                    &target,
                    error_text(&error),
                ));
                Task::none()
            }
        }
    }

    /// SQL for the `k` rows of `table` whose `column` is nearest `vector`
    /// (`[…]` text): `vector_search(…)` locally, `distance(…)` on a server.
    fn nearest_sql(
        &self,
        comment: &str,
        table: &str,
        column: &str,
        vector: &str,
        k: usize,
    ) -> String {
        if self.database.as_ref().and_then(Database::remote).is_some() {
            let source = self
                .catalog
                .iter()
                .find(|entry| entry.name == table)
                .map_or_else(|| quote_ident(table), TableInfo::sql_name);
            return format!(
                "-- {comment}\nSELECT *, distance({}, {vector}) AS _distance\nFROM {source}\nORDER BY _distance\nLIMIT {k};",
                quote_ident(column)
            );
        }
        format!(
            "-- {comment}\nSELECT *\nFROM vector_search('{}', '{}', '{vector}', {k})\nORDER BY _distance;",
            table.replace('\'', "''"),
            column.replace('\'', "''"),
        )
    }

    fn show_outcome(&mut self, outcome: Arc<QueryOutcome>) -> Task<Message> {
        self.generation += 1;
        self.selected = None;
        self.profiles = None;
        self.preview = None;
        self.playing = false;
        self.frame = 0;
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
        let gallery_task = match self.media_columns.iter().find(|c| c.visual) {
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
        self.media_columns.iter().any(|c| c.visual)
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

    /// Frames available for flip-book playback of the current preview.
    fn current_frames(&self) -> usize {
        self.preview
            .as_ref()
            .map_or(0, |preview| preview.frames.len())
    }

    /// Loads the selected cell's picture, filmstrip and metadata off the UI
    /// thread (any media column; audio gets metadata only).
    fn load_preview(&mut self, row: usize, column: usize) -> Task<Message> {
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
                tokio::task::spawn_blocking(move || {
                    let value = gallery::cell_value(&table, &media_column, table.source_row(row))?;
                    let still = value.still(gallery::PREVIEW_SIDE);
                    let frames = value
                        .filmstrip(gallery::FILMSTRIP_FRAMES, gallery::FILMSTRIP_SIDE)
                        .iter()
                        .map(gallery::handle_of)
                        .collect();
                    Some(Arc::new(Preview {
                        row,
                        column,
                        kind: value.kind(),
                        color: still.as_ref().map(media::color_vector),
                        handle: still.as_ref().map(gallery::handle_of),
                        frames,
                        info: value.av_info(),
                    }))
                })
                .await
                .ok()
                .flatten()
            },
            move |preview| Message::PreviewReady(generation, preview),
        )
    }

    /// `(table, colour-vector column)` to search with the selected image or
    /// video (its poster frame's colours).
    pub fn similar_image_target(&self) -> Option<(String, String)> {
        let (row, column) = self.selected?;
        self.preview
            .as_ref()
            .filter(|p| p.row == row && p.column == column && p.color.is_some())?;
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
        let Some(color) = preview.color else {
            return Task::none();
        };
        let vector: Vec<String> = color.iter().map(|v| format!("{v:.5}")).collect();
        let sql = self.nearest_sql(
            "Rows whose picture colours are closest to the selection",
            &table,
            &column,
            &format!("[{}]", vector.join(", ")),
            12,
        );
        self.editor = text_editor::Content::with_text(&sql);
        self.tab = ResultTab::Media;
        self.run()
    }

    /// Opens the selected media value in the system's default application
    /// (a video player for videos). Bytes are written to a temp file first.
    fn open_externally(&mut self) -> Task<Message> {
        let (Some((row, column)), Some(table)) = (self.selected, self.table.clone()) else {
            return Task::none();
        };
        let Some(media_column) = self
            .media_columns
            .iter()
            .find(|c| c.index == column)
            .cloned()
        else {
            return Task::none();
        };
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let value = gallery::cell_value(&table, &media_column, table.source_row(row))
                        .ok_or("nothing to open")?;
                    let path = match value {
                        media::MediaValue::File(path) => path,
                        media::MediaValue::Bytes(bytes) => {
                            let extension = media::sniff(&bytes).map_or("bin", |f| f.extension);
                            // Kept after joust exits: the player may still be using it.
                            let file = tempfile::Builder::new()
                                .prefix("joust-")
                                .suffix(&format!(".{extension}"))
                                .tempfile()
                                .map_err(|e| e.to_string())?;
                            std::fs::write(file.path(), &bytes).map_err(|e| e.to_string())?;
                            file.keep().map_err(|e| e.to_string())?.1
                        }
                    };
                    open_with_system(&path)
                })
                .await
                .map_err(|e| e.to_string())?
            },
            Message::OpenedExternally,
        )
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
        let Some(media::MediaValue::Bytes(bytes)) =
            gallery::cell_value(&table, &media_column, table.source_row(row))
        else {
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
        let sql = self.nearest_sql(
            &format!("Nearest neighbours of the selected {column} value"),
            &table,
            &column,
            &vector,
            10,
        );
        self.editor = text_editor::Content::with_text(&sql);
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

/// Opens `path` with the platform's default application.
fn open_with_system(path: &std::path::Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let mut command = std::process::Command::new("xdg-open");
    command
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{e}; the file is at {}", path.display()))
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
    use crate::test_support::{app, opened, outputs, sample_path, send};

    fn run(app: &mut App, sql: &str) {
        send(app, Message::LoadQuery(sql.to_string()));
        send(app, Message::Run);
        assert!(app.running.is_none());
    }

    fn column_of(app: &App, name: &str) -> usize {
        let table = app.table.as_ref().expect("a result table");
        table.columns.iter().position(|c| c.name == name).unwrap()
    }

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

        let (result, _) = spawn_db(async { anyhow::Ok(7) });
        assert_eq!(futures::executor::block_on(result), Ok(7));
    }

    #[test]
    fn small_helpers() {
        assert_eq!(RowLimit(1_000).to_string(), "Limit 1k rows");
        assert_eq!(RowLimit(100_000).to_string(), "Limit 100k rows");
        assert_eq!(RowLimit(1_000_000).to_string(), "Limit 1M rows");

        let trailers = TableInfo {
            namespace: Vec::new(),
            name: "trailers".into(),
            kind: crate::db::TableKind::Table,
            columns: Vec::new(),
            num_rows: 0,
            version: 1,
            size_bytes: 0,
            indices: Vec::new(),
        };
        assert!(example_available("SELECT 1", &[]));
        assert!(!example_available("SELECT * FROM trailers", &[]));
        assert!(example_available("SELECT * FROM trailers", &[trailers]));

        let mut preview = Preview {
            row: 0,
            column: 0,
            kind: None,
            handle: None,
            frames: Vec::new(),
            color: None,
            info: None,
        };
        // No duration: 4 s spread over (at least) one frame, capped.
        assert_eq!(preview.frame_interval(), Duration::from_secs_f64(0.6));
        preview.frames = vec![iced::widget::image::Handle::from_rgba(1, 1, vec![0; 4]); 8];
        preview.info = Some(crate::av::AvInfo {
            duration: Some(0.4),
            ..crate::av::AvInfo::default()
        });
        assert_eq!(preview.frame_interval(), Duration::from_secs_f64(0.12));
    }

    #[test]
    fn keyboard_shortcuts() {
        use crate::test_support::{character, key_press, named};
        use iced::keyboard::Modifiers;
        let press = |key, modifiers| {
            shortcut(
                key_press(key, modifiers),
                event::Status::Ignored,
                window::Id::unique(),
            )
        };
        assert!(matches!(
            press(named(key::Named::Enter), Modifiers::COMMAND),
            Some(Message::Run)
        ));
        assert!(press(named(key::Named::Enter), Modifiers::empty()).is_none());
        assert!(matches!(
            press(named(key::Named::F5), Modifiers::empty()),
            Some(Message::Run)
        ));
        assert!(matches!(
            press(named(key::Named::Escape), Modifiers::empty()),
            Some(Message::Cancel)
        ));
        assert!(press(character("a"), Modifiers::empty()).is_none());
        assert!(
            shortcut(
                iced::Event::Mouse(iced::mouse::Event::CursorLeft),
                event::Status::Ignored,
                window::Id::unique()
            )
            .is_none()
        );
    }

    #[test]
    fn without_a_database() {
        let mut app = app();
        assert_eq!(app.title(), "joust");
        assert_eq!(app.row_limit(), RowLimit(100_000));
        assert_eq!(app.editor.text().trim(), WELCOME_SQL.trim());

        // Nothing to run or open yet.
        send(&mut app, Message::Run);
        assert!(app.error.as_deref().unwrap().starts_with("Open a database"));
        // Nothing opens while something else is opening.
        app.busy = Some("Opening /data…".into());
        send(&mut app, Message::PathChanged("/data/other".into()));
        send(&mut app, Message::OpenDatabase);
        assert_eq!(app.busy.as_deref(), Some("Opening /data…"));
        assert!(
            app.error.as_deref().unwrap().starts_with("Open a database"),
            "no new error either"
        );
        app.busy = None;
        send(&mut app, Message::PathChanged("   ".into()));
        send(&mut app, Message::OpenDatabase);
        assert!(app.error.as_deref().unwrap().starts_with("Enter the path"));
        send(&mut app, Message::DismissMessages);
        assert!(app.error.is_none() && app.notice.is_none());

        // Actions that need a database, a result or a selection do nothing.
        for message in [
            Message::DatabasePicked(None),
            Message::MediaFolderPicked(None),
            Message::MediaFolderPicked(Some("/tmp".into())),
            Message::CreateIndex("t".into(), "c".into(), IndexKind::Auto),
            Message::RefreshCatalog,
            Message::CopyCell,
            Message::FindSimilar,
            Message::FindSimilarImages,
            Message::SaveCell,
            Message::OpenExternally,
            Message::ExportCsv,
            Message::SortColumn(0),
            Message::SetGalleryColumn(ColumnChoice {
                index: 0,
                name: "x".into(),
            }),
            Message::SelectCell(0, 0),
            Message::Cancel,
            Message::Tick,
            Message::OpenedExternally(Ok(())),
            Message::CsvExported(Ok(None)),
            Message::CellSaved(Ok(None)),
        ] {
            let task = app.update(message.clone());
            assert_eq!(task.units(), 0, "{message:?} started work");
        }
        assert!(app.error.is_none(), "{:?}", app.error);
        assert!(app.selected_value().is_none());
        assert!(app.running_for().is_none());

        // Results of background work are reported.
        send(&mut app, Message::SampleReady(Err("disk full".into())));
        assert_eq!(app.error.as_deref(), Some("disk full"));
        send(&mut app, Message::CatalogLoaded(Err("gone".into())));
        assert_eq!(app.error.as_deref(), Some("gone"));
        send(&mut app, Message::IndexCreated(Err("bad column".into())));
        assert_eq!(app.error.as_deref(), Some("bad column"));
        send(&mut app, Message::IndexCreated(Ok("Indexed t.c".into())));
        assert_eq!(app.notice.as_deref(), Some("Indexed t.c"));
        send(&mut app, Message::MediaImported(Err("no files".into())));
        assert_eq!(app.error.as_deref(), Some("no files"));
        send(&mut app, Message::OpenedExternally(Err("no player".into())));
        assert!(app.error.as_deref().unwrap().contains("no player"));
        send(&mut app, Message::CsvExported(Err("read-only".into())));
        assert!(app.error.as_deref().unwrap().starts_with("Export failed"));
        send(
            &mut app,
            Message::CsvExported(Ok(Some("/x/results.csv".into()))),
        );
        assert!(app.notice.as_deref().unwrap().contains("results.csv"));
        send(&mut app, Message::CellSaved(Err("read-only".into())));
        assert!(app.error.as_deref().unwrap().starts_with("Save failed"));
        send(
            &mut app,
            Message::CellSaved(Ok(Some("/x/poster.png".into()))),
        );
        assert!(app.notice.as_deref().unwrap().contains("poster.png"));
        send(&mut app, Message::CatalogLoaded(Ok(Vec::new())));
        assert!(app.busy.is_none());

        // Preferences.
        send(&mut app, Message::SetRowLimit(RowLimit(1_000)));
        assert_eq!(app.row_limit(), RowLimit(1_000));
        send(&mut app, Message::SelectTheme(ThemeId::Nord));
        assert_eq!(app.theme_id(), ThemeId::Nord);
        assert_eq!(app.theme(), ThemeId::Nord.to_theme());
        let split = *app.panes.layout().splits().next().unwrap();
        send(
            &mut app,
            Message::PaneResized(pane_grid::ResizeEvent { split, ratio: 0.3 }),
        );
        send(&mut app, Message::SelectTab(ResultTab::Chart));
        assert_eq!(app.tab, ResultTab::Chart);
        send(&mut app, Message::SetPlanMode(PlanMode::Logical));
        assert_eq!(app.plan_mode, PlanMode::Logical);

        // Editor text.
        send(&mut app, Message::LoadQuery("SELECT 1".into()));
        send(&mut app, Message::Edit(text_editor::Action::SelectAll));
        assert_eq!(app.sql_to_run(), "SELECT 1");
        send(&mut app, Message::InsertText("SELECT 2".into()));
        assert_eq!(app.editor.text().trim(), "SELECT 2");
        send(&mut app, Message::ToggleTable("movies".into()));
        assert!(app.expanded.contains("movies"));
        send(&mut app, Message::ToggleTable("movies".into()));
        assert!(app.expanded.is_empty());

        // Playback controls need frames.
        send(&mut app, Message::TogglePlayback);
        assert!(!app.playing);
        send(&mut app, Message::NextFrame);
        send(&mut app, Message::ShowFrame(3));
        assert_eq!(app.frame, 0);
    }

    #[test]
    fn opening_a_missing_database_reports_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-directory");
        std::fs::write(&file, "x").unwrap();
        let mut app = app();
        send(&mut app, Message::OpenRecent(file.display().to_string()));
        assert!(app.database.is_none());
        assert!(app.busy.is_none());
        assert!(app.error.is_some());
    }

    #[test]
    fn querying_the_sample_database() {
        let (_dir, path) = sample_path();
        let mut app = opened(&path);
        let tables = app.catalog.len();
        assert!(app.title().contains(&path.display().to_string()));
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains(&format!("({tables} tables)"))
        );
        assert_eq!(app.expanded.len(), tables.min(3));
        assert_eq!(app.settings.recent_databases, [path.display().to_string()]);

        // The welcome query lists the tables; profiles and a chart follow.
        send(&mut app, Message::Run);
        assert_eq!(app.generation, 1);
        assert_eq!(app.table.as_ref().unwrap().row_count(), tables);
        assert_eq!(app.profiles.as_ref().unwrap().len(), 2);
        assert!(app.chart.is_some());
        assert_eq!(app.settings.history[0], WELCOME_SQL.trim());
        assert!(!app.has_gallery());

        // Aggregates: sorting, selection, copying, charts.
        run(
            &mut app,
            "SELECT genre, count(*) AS movies, avg(year) AS year FROM movies GROUP BY genre",
        );
        assert!(app.error.is_none(), "{:?}", app.error);
        send(&mut app, Message::SortColumn(1));
        send(&mut app, Message::SelectCell(0, 0));
        assert_eq!(app.selected, Some((0, 0)));
        assert!(app.selected_value().is_some());
        assert!(app.similar_target().is_none(), "text is not a vector");
        let task = app.update(Message::CopyCell);
        assert!(task.units() > 0, "writes the clipboard");
        assert!(app.notice.as_deref().unwrap().starts_with("Copied"));
        send(&mut app, Message::SortColumn(1));
        assert!(app.selected.is_none(), "sorting clears the selection");
        for kind in ChartKind::ALL {
            send(&mut app, Message::SetChartKind(kind));
            assert!(app.chart.is_some());
        }
        send(
            &mut app,
            Message::SetChartX(ColumnChoice {
                index: 0,
                name: "genre".into(),
            }),
        );
        send(
            &mut app,
            Message::SetChartY(ColumnChoice {
                index: 2,
                name: "year".into(),
            }),
        );
        assert_eq!((app.chart_spec.x, app.chart_spec.y), (Some(0), Some(2)));

        // Errors are shown and clear on the next run.
        run(&mut app, "SELECT nope FROM movies");
        assert!(app.error.as_deref().unwrap().contains("nope"));
        run(&mut app, "SELECT 1 AS one");
        assert!(app.error.is_none());

        // DDL notes and catalog refreshes.
        run(
            &mut app,
            "CREATE TABLE picks AS SELECT id, title FROM movies LIMIT 3",
        );
        assert!(app.notice.as_deref().unwrap().contains("(3 rows)"));
        assert!(app.table.is_none(), "DDL returns no rows");
        assert!(app.catalog.iter().any(|t| t.name == "picks"));
        send(&mut app, Message::RefreshCatalog);
        assert!(app.catalog.iter().any(|t| t.name == "picks"));

        // Build indexes from the sidebar.
        send(
            &mut app,
            Message::CreateIndex("picks".into(), "id".into(), IndexKind::Auto),
        );
        assert_eq!(app.notice.as_deref(), Some("Indexed picks.id"));
        let picks = app.catalog.iter().find(|t| t.name == "picks").unwrap();
        assert_eq!(picks.indices.len(), 1);
        send(
            &mut app,
            Message::CreateIndex("picks".into(), "title".into(), IndexKind::FullText),
        );
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .starts_with("Built full-text")
        );
        send(
            &mut app,
            Message::CreateIndex("nope".into(), "id".into(), IndexKind::Auto),
        );
        assert!(app.error.as_deref().unwrap().contains("not open"));

        // Previews and examples.
        send(&mut app, Message::PreviewTable("picks".into()));
        assert!(app.editor.text().starts_with("SELECT *\nFROM picks"));
        assert_eq!(app.table.as_ref().unwrap().row_count(), 3);
        send(&mut app, Message::RunExample(SAMPLE_EXAMPLES[0].1.into()));
        assert!(app.table.as_ref().unwrap().row_count() > 1);

        // Stale results are ignored.
        let generation = app.generation;
        send(
            &mut app,
            Message::ProfilesReady(generation + 5, Arc::new(Vec::new())),
        );
        assert!(app.profiles.as_ref().is_some_and(|p| !p.is_empty()));
        send(&mut app, Message::QueryFinished(999, Err("late".into())));
        assert!(app.error.is_none());
    }

    #[test]
    fn running_queries_can_be_cancelled_or_superseded() {
        let (_dir, path) = sample_path();
        let mut app = opened(&path);
        send(
            &mut app,
            Message::LoadQuery("SELECT count(*) FROM events".into()),
        );
        let first = app.update(Message::Run);
        assert!(app.running.is_some());
        assert!(app.running_for().is_some());
        let _ = app.subscription();
        // Running again supersedes the first query; its result (cancelled or
        // not, depending on timing) must not end the new run.
        let second = app.update(Message::Run);
        let id = app.running.as_ref().unwrap().id;
        let stale = outputs(first);
        assert!(matches!(&stale[..], [Message::QueryFinished(i, _)] if *i == id - 1));
        for message in stale {
            let _ = app.update(message);
        }
        assert!(app.running.is_some());
        assert!(app.outcome.is_none());

        // After a cancel, the query's late result is ignored.
        send(&mut app, Message::Cancel);
        assert!(app.running.is_none());
        assert_eq!(app.notice.as_deref(), Some("Query cancelled"));
        let late = outputs(second);
        assert!(matches!(&late[..], [Message::QueryFinished(i, _)] if *i == id));
        for message in late {
            let _ = app.update(message);
        }
        assert!(app.outcome.is_none(), "cancelled query produced a result");
    }

    #[test]
    fn media_results_gallery_and_previews() {
        let (dir, path) = sample_path();
        let mut app = opened(&path);
        run(
            &mut app,
            "SELECT id, title, vector, poster FROM movies ORDER BY id",
        );
        assert!(app.has_gallery());
        assert_eq!(app.media_columns.len(), 1);
        let gallery = app.gallery.clone().expect("gallery built");
        assert_eq!(gallery.thumbs.len(), 64);

        // A vector cell offers "find similar".
        let vector = column_of(&app, "vector");
        send(&mut app, Message::SelectCell(0, vector));
        assert_eq!(
            app.similar_target(),
            Some(("movies".to_string(), "vector".to_string()))
        );
        assert!(app.preview.is_none(), "vectors have no preview");
        send(&mut app, Message::FindSimilar);
        assert!(
            app.editor
                .text()
                .contains("vector_search('movies', 'vector'")
        );
        assert_eq!(app.tab, ResultTab::Rows);
        assert_eq!(app.table.as_ref().unwrap().row_count(), 10);

        // An image cell loads a preview and offers a colour search.
        run(&mut app, "SELECT id, title, poster FROM movies ORDER BY id");
        let poster = column_of(&app, "poster");
        send(&mut app, Message::SelectCell(2, poster));
        let preview = app.preview.clone().expect("preview loaded");
        assert_eq!((preview.row, preview.column), (2, poster));
        assert_eq!(preview.kind, Some(media::MediaKind::Image));
        assert!(preview.handle.is_some() && preview.color.is_some());
        assert!(preview.frames.is_empty());
        assert_eq!(
            app.similar_image_target(),
            Some(("movies".to_string(), "poster_colors".to_string()))
        );
        // Saving and opening need a file dialog / a player; only check that
        // they start work (the tasks are not run).
        assert!(app.update(Message::SaveCell).units() > 0);
        assert!(app.update(Message::OpenExternally).units() > 0);
        assert!(app.update(Message::ExportCsv).units() > 0);
        send(
            &mut app,
            Message::SetGalleryColumn(ColumnChoice {
                index: poster,
                name: "poster".into(),
            }),
        );
        assert!(app.gallery.is_some());

        // A preview for a cell that is no longer selected is dropped.
        let stale = Arc::new(Preview {
            row: 9,
            ..(*preview).clone()
        });
        let generation = app.generation;
        send(&mut app, Message::PreviewReady(generation, Some(stale)));
        assert_eq!(app.preview.as_ref().unwrap().row, 2);
        send(&mut app, Message::GalleryReady(generation + 1, gallery));

        send(&mut app, Message::FindSimilarImages);
        assert!(
            app.editor
                .text()
                .contains("vector_search('movies', 'poster_colors'")
        );
        assert_eq!(app.tab, ResultTab::Media);
        assert_eq!(app.table.as_ref().unwrap().row_count(), 12);

        // Results without media fall back from the media tab.
        run(&mut app, "SELECT 1 AS one");
        assert_eq!(app.tab, ResultTab::Rows);

        // Importing a folder creates a table and shows it in the media tab.
        let folder = dir.path().join("Shots");
        std::fs::create_dir(&folder).unwrap();
        std::fs::write(
            folder.join("a.png"),
            crate::test_support::png(8, 8, [9, 9, 9]),
        )
        .unwrap();
        std::fs::write(
            folder.join("b.png"),
            crate::test_support::png(4, 4, [200, 9, 9]),
        )
        .unwrap();
        send(&mut app, Message::MediaFolderPicked(Some(folder)));
        assert!(app.error.is_none(), "{:?}", app.error);
        assert_eq!(app.notice.as_deref(), Some("Imported 2 files into shots"));
        assert!(app.catalog.iter().any(|t| t.name == "shots"));
        assert_eq!(app.tab, ResultTab::Media);
        assert_eq!(app.table.as_ref().unwrap().row_count(), 2);

        let catalog = app.catalog.clone();
        send(
            &mut app,
            Message::MediaImported(Ok((
                ImportSummary {
                    table: "shots".into(),
                    imported: 1,
                    skipped: 2,
                    referenced: 1,
                },
                catalog,
            ))),
        );
        let notice = app.notice.clone().unwrap();
        assert!(notice.contains("1 file into shots"), "{notice}");
        assert!(notice.contains("2 unreadable") && notice.contains("1 large file"));
    }

    #[test]
    fn video_previews_play_as_a_flip_book() {
        if !crate::ffmpeg::available() {
            eprintln!("skipping: ffmpeg not found");
            return;
        }
        let (_dir, path) = sample_path();
        let mut app = opened(&path);
        run(&mut app, "SELECT title, clip FROM trailers ORDER BY id");
        assert!(app.has_gallery());
        let clip = column_of(&app, "clip");
        send(&mut app, Message::SelectCell(0, clip));
        let preview = app.preview.clone().expect("preview loaded");
        assert_eq!(preview.kind, Some(media::MediaKind::Video));
        assert!(preview.frames.len() > 1);
        assert!(preview.info.as_ref().and_then(|i| i.duration).is_some());
        assert_eq!(
            app.similar_image_target(),
            Some(("trailers".to_string(), "clip_colors".to_string()))
        );

        send(&mut app, Message::TogglePlayback);
        assert!(app.playing);
        let _ = app.subscription();
        send(&mut app, Message::NextFrame);
        assert_eq!(app.frame, 1);
        send(&mut app, Message::ShowFrame(999));
        assert!(!app.playing);
        assert_eq!(app.frame, preview.frames.len() - 1);
        send(&mut app, Message::NextFrame);
        assert_eq!(app.frame, 0, "wraps around");

        // Selecting another cell stops playback.
        send(&mut app, Message::TogglePlayback);
        send(&mut app, Message::SelectCell(1, clip));
        assert!(!app.playing);
    }

    #[test]
    fn typed_and_picked_paths_open() {
        let (_dir, path) = sample_path();
        let mut app = app();
        send(
            &mut app,
            Message::PathChanged(format!("  {}  ", path.display())),
        );
        send(&mut app, Message::OpenDatabase);
        assert!(app.database.is_some(), "{:?}", app.error);
        let mut app = crate::test_support::app();
        send(&mut app, Message::DatabasePicked(Some(path.clone())));
        assert_eq!(app.path_input, path.display().to_string());
        assert!(app.database.is_some(), "{:?}", app.error);

        // Media actions on columns that can't be saved or searched.
        run(
            &mut app,
            "SELECT title, poster, arrow_cast(title, 'Binary') AS raw FROM movies",
        );
        let poster = column_of(&app, "poster");
        let raw = column_of(&app, "raw");
        send(&mut app, Message::SelectCell(0, raw));
        assert_eq!(app.update(Message::OpenExternally).units(), 0, "not media");
        assert_eq!(app.update(Message::SaveCell).units(), 0, "not media");
        // No preview loaded yet: nothing to search with.
        send(&mut app, Message::SelectCell(0, poster));
        app.preview = None;
        assert_eq!(app.update(Message::FindSimilarImages).units(), 0);
    }

    #[test]
    fn sample_ready_opens_the_sample_with_a_note() {
        let (_dir, path) = sample_path();
        let mut app = app();
        send(
            &mut app,
            Message::SampleReady(Ok((path.display().to_string(), 0))),
        );
        assert!(app.database.is_some());
        assert_eq!(app.editor.text().trim(), SAMPLE_EXAMPLES[0].1.trim());
        assert!(
            app.notice
                .as_deref()
                .unwrap()
                .contains("ffmpeg was not found")
        );

        // A later open keeps the user's SQL and has no note.
        send(&mut app, Message::LoadQuery("SELECT 42".into()));
        send(
            &mut app,
            Message::SampleReady(Ok((path.display().to_string(), 3))),
        );
        assert_eq!(app.editor.text().trim(), "SELECT 42");
        assert!(!app.notice.as_deref().unwrap().contains("ffmpeg"));
    }

    fn dialog(app: &App) -> &ConnectDialog {
        app.connect.as_ref().expect("the connect dialog is open")
    }

    #[test]
    fn connect_dialog_edits_and_validates() {
        let mut app = app();
        send(&mut app, Message::OpenConnectDialog);
        assert_eq!(dialog(&app).selected.as_deref(), Some("local"));
        assert_eq!(dialog(&app).rest_url, "localhost:10024");

        send(
            &mut app,
            Message::ConnectEdit(ConnectField::Database, " ".into()),
        );
        send(&mut app, Message::TestConnection);
        assert!(dialog(&app).error.as_deref().unwrap().contains("database"));
        assert!(dialog(&app).working.is_none());
        send(&mut app, Message::ConnectEnterprise);
        assert!(
            dialog(&app).error.is_some(),
            "nothing starts without a database"
        );

        send(
            &mut app,
            Message::ConnectEdit(ConnectField::Database, "d".into()),
        );
        assert!(dialog(&app).error.is_none(), "editing clears the error");
        send(&mut app, Message::ConnectAuth(AuthMethod::ApiKey));
        send(&mut app, Message::TestConnection);
        assert!(dialog(&app).error.as_deref().unwrap().contains("API key"));
        send(&mut app, Message::ConnectProfile("local".into()));
        assert_eq!(dialog(&app).auth, AuthMethod::Automatic);

        send(&mut app, Message::CloseConnectDialog);
        assert!(app.connect.is_none());
        // Without a dialog these do nothing.
        for message in [
            Message::ConnectProfile("local".into()),
            Message::ConnectEdit(ConnectField::RestUrl, "x".into()),
            Message::ConnectAuth(AuthMethod::TokenFile),
            Message::TestConnection,
            Message::ConnectEnterprise,
            Message::ConnectionTested(Ok(Vec::new())),
            Message::SwitchDatabase("other".into()),
        ] {
            send(&mut app, message);
        }
        assert!(app.connect.is_none() && app.database.is_none());
    }

    #[test]
    fn unreachable_servers_report_in_the_dialog() {
        let mut app = app();
        send(&mut app, Message::OpenConnectDialog);
        send(
            &mut app,
            Message::ConnectEdit(ConnectField::RestUrl, "http://127.0.0.1:9".into()),
        );
        send(
            &mut app,
            Message::ConnectEdit(ConnectField::Database, "d".into()),
        );

        let task = app.update(Message::TestConnection);
        assert_eq!(dialog(&app).working, Some("Testing…"));
        let messages = outputs(task);
        assert!(matches!(&messages[..], [Message::ConnectionTested(Err(_))]));
        for message in messages {
            send(&mut app, message);
        }
        assert!(dialog(&app).working.is_none());
        assert!(
            dialog(&app)
                .error
                .as_deref()
                .unwrap()
                .contains("could not list tables")
        );
        send(
            &mut app,
            Message::ConnectionTested(Ok(vec![Check::ok("REST API", "ok")])),
        );
        assert_eq!(dialog(&app).checks.as_ref().unwrap().len(), 1);

        send(&mut app, Message::ConnectEnterprise);
        assert!(app.database.is_none());
        assert!(app.busy.is_none());
        assert!(
            dialog(&app)
                .error
                .as_deref()
                .unwrap()
                .contains("could not list tables")
        );
        assert!(
            app.error.is_none(),
            "the dialog shows the error, not the banner"
        );
    }

    #[test]
    fn recent_connections_needing_a_key_reopen_the_dialog() {
        let mut app = app();
        let target = EnterpriseTarget {
            rest_url: "http://127.0.0.1:9".into(),
            database: "d".into(),
            auth: AuthMethod::ApiKey,
            ..EnterpriseTarget::default()
        };
        send(&mut app, Message::OpenRecentEnterprise(target.clone()));
        assert_eq!(dialog(&app).database, "d");
        assert!(dialog(&app).error.as_deref().unwrap().contains("API key"));

        // A key typed for another server is not sent to this one.
        app.connect = None;
        app.session_api_key = Some(SessionKey {
            endpoint: "http://other.example.com:10024".into(),
            key: "sk_other".into(),
        });
        send(&mut app, Message::OpenRecentEnterprise(target.clone()));
        assert!(dialog(&app).error.as_deref().unwrap().contains("API key"));

        // Nothing reconnects while something else is opening.
        app.connect = None;
        app.busy = Some("Opening /data…".into());
        send(&mut app, Message::OpenRecentEnterprise(target.clone()));
        send(&mut app, Message::OpenRecent("/data/other".into()));
        assert!(app.connect.is_none() && app.error.is_none());
        app.busy = None;

        // With a key typed earlier this session for this server, it
        // reconnects straight away.
        app.session_api_key = Some(SessionKey {
            endpoint: "http://127.0.0.1:9".into(),
            key: "sk_session".into(),
        });
        send(&mut app, Message::OpenRecentEnterprise(target));
        assert!(app.connect.is_none());
        assert!(
            app.error
                .as_deref()
                .unwrap()
                .contains("could not list tables")
        );
    }

    #[test]
    fn nearest_neighbour_sql_is_local_without_a_server() {
        let app = app();
        assert_eq!(
            app.nearest_sql("note", "t", "v", "[1, 2]", 5),
            "-- note\nSELECT *\nFROM vector_search('t', 'v', '[1, 2]', 5)\nORDER BY _distance;"
        );
    }

    /// Against a running server (see `db::remote::tests::against_a_running_server`).
    #[test]
    fn works_against_a_running_server() {
        let Some((mut app, _server)) = crate::test_support::enterprise_app() else {
            return;
        };
        assert_eq!(app.editor.text().trim(), ENTERPRISE_WELCOME_SQL.trim());
        assert!(
            app.notice.as_deref().unwrap().contains("server "),
            "{:?}",
            app.notice
        );
        assert!(app.title().contains(" · "));
        let target = app.settings.recent_enterprise[0].clone();
        assert_eq!(
            app.session_api_key
                .as_ref()
                .and_then(|session| session.for_target(&target)),
            Some(profiles::LOCAL_API_KEY)
        );

        send(&mut app, Message::Run);
        assert!(app.error.is_none(), "{:?}", app.error);
        assert!(
            app.table.as_ref().unwrap().row_count() > 0,
            "SHOW TABLES lists tables"
        );

        let table = app
            .catalog
            .iter()
            .find(|t| t.kind == crate::db::TableKind::Table)
            .expect("the server has a table")
            .clone();
        send(&mut app, Message::PreviewTable(table.table_ref()));
        assert!(app.error.is_none(), "{:?}", app.error);
        assert!(app.editor.text().contains(&table.sql_name()));

        let sql = app.nearest_sql("note", &table.name, "vec", "[1, 2]", 3);
        assert!(sql.contains("distance(vec, [1, 2])") && sql.contains(&table.sql_name()));

        // Same database: nothing to do. A recent connection reuses the
        // session key.
        send(&mut app, Message::SwitchDatabase(target.database.clone()));
        send(&mut app, Message::OpenRecentEnterprise(target.clone()));
        assert!(
            app.database
                .as_ref()
                .is_some_and(|db| db.remote().is_some())
        );
        assert!(
            app.connect.is_none(),
            "{:?}",
            app.connect.as_ref().map(|d| &d.error)
        );

        // Testing from the dialog fills in the checks.
        send(&mut app, Message::OpenConnectDialog);
        send(
            &mut app,
            Message::ConnectEdit(ConnectField::RestUrl, target.rest_url.clone()),
        );
        send(
            &mut app,
            Message::ConnectEdit(ConnectField::Database, target.database.clone()),
        );
        send(&mut app, Message::TestConnection);
        let checks = dialog(&app).checks.clone().expect("checks after a test");
        assert!(checks[0].ok, "{checks:?}");
    }

    fn sample_detail(version: &str) -> FunctionDetail {
        FunctionDetail {
            name: "f".into(),
            version: version.into(),
            source: Some("def f(x):\n    return x\n".into()),
            inputs: vec!["x int64 NOT NULL".into()],
            output: "int64".into(),
            ..FunctionDetail::default()
        }
    }

    #[test]
    fn server_actions_write_sql_into_the_editor() {
        let mut app = app();
        let table = TableRef {
            namespace: vec!["team".into()],
            name: "t".into(),
        };
        send(
            &mut app,
            Message::RefreshColumn(table.clone(), "My Col".into()),
        );
        assert_eq!(
            app.editor.text().trim(),
            "REFRESH COLUMN \"My Col\" ON team.t;"
        );
        assert!(app.error.as_deref().unwrap().contains("Open a database"));
        send(&mut app, Message::ShowColumnErrors(table.clone()));
        assert_eq!(app.editor.text().trim(), "SHOW ERRORS ON team.t;");
        send(&mut app, Message::RefreshMaterializedView(table.clone()));
        assert_eq!(
            app.editor.text().trim(),
            "REFRESH MATERIALIZED VIEW team.t;"
        );
        send(&mut app, Message::ComputedColumnTemplate(table));
        assert!(
            app.editor
                .text()
                .contains("ALTER TABLE team.t ADD COLUMN new_column AS")
        );
        assert!(app.notice.as_deref().unwrap().contains("template"));
        send(&mut app, Message::NewFunctionTemplate);
        assert_eq!(app.editor.text(), FUNCTION_TEMPLATE);

        send(&mut app, Message::SetCollectPlan(true));
        assert!(app.collect_plan);
        assert_eq!(app.rows_received(), None, "no server");
        // Server-only actions do nothing without a server.
        send(&mut app, Message::SelectFunction("f".into(), "1".into()));
        assert!(app.function.is_none());
        send(&mut app, Message::EditFunctionTemplate);
        send(&mut app, Message::DropFunctionTemplate);
        assert_eq!(app.editor.text(), FUNCTION_TEMPLATE);
    }

    #[test]
    fn function_tab_lifecycle() {
        let mut app = app();
        app.function = Some(FunctionView {
            name: "f".into(),
            version: "2".into(),
            detail: None,
        });
        app.tab = ResultTab::Function;
        send(&mut app, Message::EditFunctionTemplate);
        assert_eq!(
            app.editor.text().trim(),
            WELCOME_SQL.trim(),
            "nothing until it loads"
        );

        // A reply for another version is stale and ignored.
        send(
            &mut app,
            Message::FunctionDescribed("f".into(), "1".into(), Ok(sample_detail("1"))),
        );
        assert!(app.function.as_ref().unwrap().detail.is_none());
        send(
            &mut app,
            Message::FunctionDescribed("f".into(), "2".into(), Ok(sample_detail("2"))),
        );
        send(&mut app, Message::EditFunctionTemplate);
        let sql = app.editor.text();
        assert!(sql.contains("CREATE OR REPLACE FUNCTION f(x int64 NOT NULL) RETURNS int64"));
        assert!(sql.contains("$$\ndef f(x):\n    return x\n$$;"), "{sql}");
        send(&mut app, Message::DropFunctionTemplate);
        assert!(
            app.editor
                .text()
                .contains("DROP FUNCTION f WITH (version = '2');")
        );

        send(&mut app, Message::CloseFunction);
        assert!(app.function.is_none());
        assert_eq!(app.tab, ResultTab::Rows);

        let opened = app.opened;
        send(
            &mut app,
            Message::ColumnStatusLoaded(opened, "t".into(), Ok(Vec::new())),
        );
        assert!(app.column_status.contains_key("t"));
        send(
            &mut app,
            Message::ColumnStatusLoaded(opened + 1, "late".into(), Ok(Vec::new())),
        );
        assert!(
            !app.column_status.contains_key("late"),
            "for another database"
        );
        let function = FunctionInfo {
            name: "f".into(),
            version: "2".into(),
            ..FunctionInfo::default()
        };
        send(
            &mut app,
            Message::FunctionsLoaded(opened + 1, Ok(vec![function.clone()])),
        );
        assert!(app.functions.is_empty(), "for another database");
        send(
            &mut app,
            Message::FunctionsLoaded(opened, Ok(vec![function])),
        );
        assert_eq!(app.functions.len(), 1);
        send(
            &mut app,
            Message::FunctionsLoaded(opened, Err("denied".into())),
        );
        assert!(
            app.error
                .as_deref()
                .unwrap()
                .contains("Could not list Functions")
        );
    }

    #[test]
    fn namespaced_function_templates_keep_the_namespace() {
        let mut app = app();
        app.function = Some(FunctionView {
            name: "models$clip".into(),
            version: "2".into(),
            detail: Some(Ok(sample_detail("2"))),
        });
        send(&mut app, Message::EditFunctionTemplate);
        assert!(
            app.editor
                .text()
                .contains("CREATE OR REPLACE FUNCTION models.clip(x int64 NOT NULL)"),
            "{}",
            app.editor.text()
        );
        send(&mut app, Message::DropFunctionTemplate);
        assert!(
            app.editor
                .text()
                .contains("DROP FUNCTION models.clip WITH (version = '2');")
        );
    }

    #[test]
    fn expanding_local_tables_loads_no_status() {
        let (_dir, path) = sample_path();
        let mut app = opened(&path);
        let key = app.catalog[0].key();
        app.expanded.remove(&key);
        send(&mut app, Message::ToggleTable(key.clone()));
        assert!(app.expanded.contains(&key));
        assert!(app.column_status.is_empty());
        send(&mut app, Message::ToggleTable(key.clone()));
        assert!(!app.expanded.contains(&key));
    }

    #[test]
    fn jobs_and_dashboard_messages_without_a_server() {
        use crate::db::remote::JobRow;

        let mut app = app();
        let job = |id: &str, state: &str| JobRow {
            job_id: id.into(),
            job_type: "reindex_fts".into(),
            table: "images".into(),
            state: state.into(),
            ..JobRow::default()
        };
        let opened = app.opened;
        send(
            &mut app,
            Message::JobsLoaded(opened + 1, Ok(vec![job("j1-old", "running")])),
        );
        assert!(app.jobs.jobs.is_none(), "for another database");
        send(
            &mut app,
            Message::JobsLoaded(
                opened,
                Ok(vec![
                    job("j1-a", "running"),
                    job("j1-b", "failed"),
                    job("j1-c", "succeeded"),
                    job("j1-d", "timed_out"),
                ]),
            ),
        );
        assert!(!app.jobs.loading);
        let counts: Vec<usize> = JobFilter::ALL.iter().map(|f| app.jobs.count(*f)).collect();
        assert_eq!(counts, [4, 1, 2, 1], "a timed-out job failed");
        assert_eq!(
            JobFilter::ALL.map(JobFilter::label),
            ["All", "Active", "Failed", "Finished"]
        );
        send(&mut app, Message::SetJobFilter(JobFilter::Failed));
        assert_eq!(app.jobs.visible().len(), 2, "failed and timed out");
        send(&mut app, Message::SetJobFilter(JobFilter::All));
        send(&mut app, Message::SearchJobs("J1-C".into()));
        assert_eq!(app.jobs.visible()[0].job_id, "j1-c", "search ignores case");
        send(&mut app, Message::SearchJobs(String::new()));

        send(&mut app, Message::SelectJob("j1-a".into()));
        assert_eq!(app.jobs.selected_job().unwrap().state, "running");
        send(
            &mut app,
            Message::JobDetailLoaded("j1-b".into(), Err("late reply".into())),
        );
        assert!(
            app.jobs.detail.is_none(),
            "replies for another job are dropped"
        );
        send(
            &mut app,
            Message::JobDetailLoaded("j1-a".into(), Err("gone".into())),
        );
        assert!(app.jobs.detail.is_some());
        // Actions need a server.
        send(
            &mut app,
            Message::RunJobAction("j1-a".into(), JobAction::Cancel),
        );
        assert!(app.jobs.confirm_cancel.is_none());
        send(
            &mut app,
            Message::JobActionDone("j1-a".into(), Ok("Cancel requested".into())),
        );
        assert_eq!(app.jobs.action, Some(Ok("Cancel requested".into())));
        send(
            &mut app,
            Message::JobActionDone("j1-b".into(), Err("not allowed".into())),
        );
        assert_eq!(
            app.jobs.action,
            Some(Ok("Cancel requested".into())),
            "replies about another job are dropped"
        );
        send(
            &mut app,
            Message::JobActionDone("j1-a".into(), Err("not allowed".into())),
        );
        assert_eq!(app.jobs.action, Some(Err("not allowed".into())));
        assert!(app.error.is_none(), "job errors stay on the Jobs screen");
        send(&mut app, Message::CloseJob);
        assert!(app.jobs.selected.is_none() && app.jobs.detail.is_none());
        send(&mut app, Message::OpenJob("j1-a".into()));
        assert_eq!(app.screen, Screen::Workspace, "jobs live on servers");
        send(&mut app, Message::RefreshJobs);
        assert!(!app.jobs.loading);

        // Dashboard tiles: only replies of the latest refresh count.
        send(&mut app, Message::SetDashboardRange(Range::Day));
        assert_eq!(app.dashboard.range, Range::Day);
        let generation = app.dashboard.generation;
        send(
            &mut app,
            Message::TileLoaded(generation + 1, TileId::Jobs, Ok(TileContent::Empty)),
        );
        assert!(app.dashboard.tiles.is_empty());
        send(
            &mut app,
            Message::TileLoaded(generation, TileId::Jobs, Ok(TileContent::Empty)),
        );
        assert!(app.dashboard.tiles.contains_key(&TileId::Jobs));
        assert!(app.dashboard.loaded_at.is_some());
        send(&mut app, Message::RefreshDashboard);

        // A tile's SQL opens in the editor (and runs; here, with no database).
        let before = app.editor.text();
        send(&mut app, Message::OpenTileQuery(TileId::Tables));
        assert_eq!(app.editor.text(), before, "the Tables tile has no SQL");
        app.screen = Screen::Dashboards;
        send(&mut app, Message::OpenTileQuery(TileId::FailedJobs));
        assert_eq!(app.screen, Screen::Workspace);
        assert!(app.editor.text().contains("system.public.job_metering"));
        // A table health row runs SHOW STATS for its table.
        let before = app.editor.text();
        send(&mut app, Message::OpenTableStats("gone".into()));
        assert_eq!(app.editor.text(), before, "not in the catalog");
        app.catalog.push(TableInfo {
            namespace: vec!["team".into()],
            name: "docs".into(),
            kind: crate::db::TableKind::Table,
            columns: Vec::new(),
            num_rows: 0,
            version: 1,
            size_bytes: 0,
            indices: Vec::new(),
        });
        app.screen = Screen::Dashboards;
        send(&mut app, Message::OpenTableStats("team.docs".into()));
        assert_eq!(app.screen, Screen::Workspace);
        assert_eq!(app.editor.text().trim(), "SHOW STATS FOR TABLE team.docs;");
        assert!(now_millis() > 1_700_000_000_000);
    }
}
