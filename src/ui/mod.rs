//! View code: layout of the main window and its panes.

pub mod chart;
pub mod columns;
pub mod connect;
pub mod dashboard;
pub mod function;
pub mod gallery;
pub mod grid;
pub mod jobs;
pub mod plan_view;
pub mod series_chart;

use iced::widget::{
    Space, button, canvas as canvas_widget, center, checkbox, column, container, hover, pane_grid,
    pick_list, row, scrollable, stack, text, text_editor, tooltip,
};
use iced::{Alignment, Element, Fill, Font, Length, Shrink, Theme, font, keyboard};

use crate::app::{
    App, JobFilter, Message, PaneKind, PlanMode, ResultTab, RowLimit, SAMPLE_EXAMPLES, Screen,
};
use std::collections::HashSet;

use crate::db::remote::ColumnStatus;
use crate::db::{Computed, Database, IndexKind, TableInfo, TableKind, quote_ident};
use crate::highlight::{self, SqlHighlighter};
use crate::media::MediaKind;
use crate::results::{human_bytes, human_duration, thousands, type_label};
use crate::theme::{self, ThemeId};
use chart::{ChartKind, ColumnChoice};

const BOLD: Font = Font {
    weight: font::Weight::Semibold,
    ..Font::DEFAULT
};

/// Root view.
pub fn view(app: &App) -> Element<'_, Message> {
    let body = match app.screen {
        Screen::Workspace => workspace(app),
        Screen::Jobs => jobs::view(app),
        Screen::Dashboards => dashboard::view(app),
    };

    let screen = container(column![
        top_bar(app),
        hairline(),
        body,
        hairline(),
        status_bar(app)
    ])
    .width(Fill)
    .height(Fill)
    .style(theme::app);

    match &app.connect {
        Some(dialog) => stack![screen, connect::overlay(app, dialog)].into(),
        None => screen.into(),
    }
}

/// The sidebar, the editor and the results, in resizable panes.
fn workspace(app: &App) -> Element<'_, Message> {
    let panes = pane_grid(&app.panes, |_pane, kind, _maximized| {
        let body = match kind {
            PaneKind::Sidebar => sidebar(app),
            PaneKind::Editor => editor_pane(app),
            PaneKind::Results => results_pane(app),
        };
        pane_grid::Content::new(container(body).width(Fill).height(Fill).style(theme::app))
    })
    .on_resize(8, Message::PaneResized)
    .spacing(1)
    .style(|theme: &Theme| {
        let accent = theme.extended_palette().primary.base.color;
        pane_grid::Style {
            hovered_region: pane_grid::Highlight {
                background: theme::hover(theme).into(),
                border: iced::Border::default(),
            },
            picked_split: pane_grid::Line {
                color: accent,
                width: 2.0,
            },
            hovered_split: pane_grid::Line {
                color: accent,
                width: 2.0,
            },
        }
    });

    container(panes)
        .width(Fill)
        .height(Fill)
        .style(|theme: &Theme| container::Style {
            // The 1px pane spacing shows this colour as dividers.
            background: Some(theme::divider(theme).into()),
            ..container::Style::default()
        })
        .into()
}

pub(crate) fn hairline<'a>() -> Element<'a, Message> {
    container(Space::new().width(Fill).height(1))
        .style(theme::rule)
        .into()
}

fn muted<'a>(content: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(content).style(|theme: &Theme| text::Style {
        color: Some(theme::muted(theme)),
    })
}

fn faint<'a>(content: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(content).style(|theme: &Theme| text::Style {
        color: Some(theme::faint(theme)),
    })
}

fn section_label<'a>(label: &'a str) -> Element<'a, Message> {
    muted(label.to_uppercase()).size(11).font(BOLD).into()
}

fn with_tooltip<'a>(
    content: impl Into<Element<'a, Message>>,
    tip: &'a str,
) -> Element<'a, Message> {
    tooltip(
        content,
        container(text(tip).size(12))
            .padding([4, 8])
            .style(theme::tooltip),
        tooltip::Position::Bottom,
    )
    .gap(4)
    .into()
}

// ---------------------------------------------------------------------------
// Top bar & status bar
// ---------------------------------------------------------------------------

fn top_bar(app: &App) -> Element<'_, Message> {
    let wordmark = text("joust").size(18).font(BOLD);
    let busy = app.busy.is_some();
    let open = with_tooltip(
        button(text("Open…").size(13))
            .padding([6, 12])
            .on_press_maybe((!busy).then_some(Message::OpenConnectDialog))
            .style(theme::secondary_button),
        "Open a LanceDB directory, or connect to a LanceDB Enterprise server",
    );
    let import = with_tooltip(
        button(text("Import media…").size(13))
            .padding([6, 12])
            .on_press_maybe((!busy && app.database.is_some()).then_some(Message::ImportMedia))
            .style(theme::secondary_button),
        "Load a folder of images, audio, video or PDFs into a new table",
    );
    let theme_picker = pick_list(ThemeId::ALL, Some(app.theme_id()), Message::SelectTheme)
        .text_size(13)
        .padding([6, 10])
        .style(theme::picker);

    container(
        row![
            wordmark,
            Space::new().width(12),
            screen_switch(app),
            database_picker(app),
            Space::new().width(Fill),
            open,
            import,
            Space::new().width(8),
            muted("Theme").size(12),
            theme_picker,
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .padding([8, 14])
    .width(Fill)
    .style(theme::top_bar)
    .into()
}

/// Width of the top bar's database picker.
const DATABASE_PICKER_WIDTH: f32 = 200.0;

/// On a server, the database the SQL editor, Jobs and Dashboards use;
/// picking another reconnects.
fn database_picker(app: &App) -> Element<'_, Message> {
    let Some(remote) = app.remote() else {
        return Space::new().into();
    };
    let databases = &remote.capabilities().databases;
    let current = remote.target().database.clone();
    let picker: Element<'_, Message> = if databases.is_empty() {
        text(current).size(13).into()
    } else {
        pick_list(databases.clone(), Some(current), Message::SwitchDatabase)
            .text_size(13)
            .padding([5, 10])
            .width(DATABASE_PICKER_WIDTH)
            .style(theme::picker)
            .into()
    };
    row![muted("Database").size(12), picker]
        .spacing(6)
        .align_y(Alignment::Center)
        .into()
}

/// SQL, Jobs and Dashboards, on a server.
fn screen_switch(app: &App) -> Element<'_, Message> {
    if app.remote().is_none() {
        return Space::new().into();
    }
    let active = app.jobs.count(JobFilter::Active);
    let jobs = if active > 0 {
        format!("Jobs · {active} active")
    } else {
        "Jobs".to_string()
    };
    let tab = |label: String, screen: Screen| {
        button(text(label).size(13))
            .padding([5, 10])
            .on_press(Message::ShowScreen(screen))
            .style(theme::list_item(app.screen == screen))
    };
    row![
        tab("SQL".to_string(), Screen::Workspace),
        tab(jobs, Screen::Jobs),
        tab("Dashboards".to_string(), Screen::Dashboards),
        Space::new().width(8),
    ]
    .spacing(2)
    .align_y(Alignment::Center)
    .into()
}

fn status_bar(app: &App) -> Element<'_, Message> {
    let connection: Element<'_, Message> = match (&app.database, &app.busy) {
        (_, Some(busy)) => row![status_dot(DotKind::Busy), text(busy.clone()).size(12)]
            .spacing(6)
            .align_y(Alignment::Center)
            .into(),
        (Some(db), None) => row![
            status_dot(DotKind::Ok),
            text(db.label()).size(12),
            faint(format!(
                "· {} table{}",
                app.catalog.len(),
                if app.catalog.len() == 1 { "" } else { "s" }
            ))
            .size(12),
        ]
        .spacing(6)
        .align_y(Alignment::Center)
        .into(),
        (None, None) => row![status_dot(DotKind::Idle), muted("Not connected").size(12)]
            .spacing(6)
            .align_y(Alignment::Center)
            .into(),
    };

    let query: Element<'_, Message> = if let Some(elapsed) = app.running_for() {
        let rows = app
            .rows_received()
            .filter(|rows| *rows > 0)
            .map_or_else(String::new, |rows| format!(" · {} rows", thousands(rows)));
        text(format!("Running… {}{rows}", human_duration(elapsed)))
            .size(12)
            .into()
    } else if let Some(outcome) = &app.outcome {
        let rows = app.table.as_ref().map_or(0, |t| t.row_count());
        muted(format!(
            "{} statement{} · {} rows · {}",
            outcome.statements,
            if outcome.statements == 1 { "" } else { "s" },
            thousands(rows),
            human_duration(outcome.elapsed)
        ))
        .size(12)
        .into()
    } else {
        Space::new().into()
    };

    let engine = match app.database.as_ref().and_then(Database::remote) {
        Some(remote) => {
            let capabilities = remote.capabilities();
            format!(
                "{} · LanceDB Enterprise {}",
                if capabilities.sql() {
                    "Server-side SQL"
                } else {
                    "Browse only (SQL unreachable)"
                },
                capabilities
                    .server_version
                    .as_deref()
                    .unwrap_or("(version unknown)")
            )
        }
        None => format!("DataFusion SQL · LanceDB {LANCEDB_VERSION}"),
    };

    container(
        row![
            connection,
            Space::new().width(Fill),
            query,
            faint(engine).size(12),
        ]
        .spacing(16)
        .align_y(Alignment::Center),
    )
    .padding([5, 14])
    .width(Fill)
    .style(theme::chrome_panel)
    .into()
}

/// LanceDB version joust was built against (for the status bar).
const LANCEDB_VERSION: &str = "0.41";

#[derive(Clone, Copy)]
enum DotKind {
    Ok,
    Busy,
    Idle,
}

fn status_dot<'a>(kind: DotKind) -> Element<'a, Message> {
    container(Space::new().width(8).height(8))
        .style(move |theme: &Theme| {
            let palette = theme.extended_palette();
            let color = match kind {
                DotKind::Ok => palette.success.base.color,
                DotKind::Busy => palette.warning.base.color,
                DotKind::Idle => theme::faint(theme),
            };
            container::Style {
                background: Some(color.into()),
                border: iced::Border {
                    radius: 4.0.into(),
                    ..iced::Border::default()
                },
                ..container::Style::default()
            }
        })
        .into()
}

// ---------------------------------------------------------------------------
// Sidebar
// ---------------------------------------------------------------------------

fn sidebar(app: &App) -> Element<'_, Message> {
    let mut content = column![].spacing(4).padding(12);

    let refresh = with_tooltip(
        button(text("↻").size(14))
            .padding([0, 6])
            .on_press_maybe(app.database.as_ref().map(|_| Message::RefreshCatalog))
            .style(theme::ghost_button),
        "Refresh tables",
    );
    let remote = app.database.as_ref().and_then(Database::remote);
    let header =
        row![section_label("Tables"), Space::new().width(Fill), refresh].align_y(Alignment::Center);
    content = content.push(header);

    if app.database.is_none() {
        content = content.push(Space::new().height(4));
        content = content.push(muted("No database open.").size(13));
        content = content.push(recent_connections(app));
    } else if app.catalog.is_empty() {
        content = content.push(muted("This database has no tables yet.").size(13));
    }

    // Server catalogs are grouped by namespace.
    let mut namespace = None;
    for table in &app.catalog {
        if remote.is_some() && namespace != Some(&table.namespace) {
            namespace = Some(&table.namespace);
            content = content.push(namespace_header(&table.namespace));
        }
        content = content.push(table_entry(app, table));
    }

    if let Some(remote) = remote
        && remote.capabilities().functions == Some(true)
    {
        content = content.push(functions_section(app));
    }

    if !app.settings.history.is_empty() {
        content = content.push(Space::new().height(12));
        content = content.push(section_label("History"));
        for sql in app.settings.history.iter().take(15) {
            let line = sql
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with("--"))
                .unwrap_or(sql);
            content = content.push(
                button(
                    faint(grid::truncate(line, 44))
                        .size(12)
                        .font(Font::MONOSPACE),
                )
                .width(Fill)
                .padding([3, 6])
                .on_press(Message::LoadQuery(sql.clone()))
                .style(theme::list_item(false)),
            );
        }
    }

    container(scrollable(content).direction(thin_scrollbar()).height(Fill))
        .width(Fill)
        .height(Fill)
        .style(theme::chrome_panel)
        .into()
}

/// Recent local databases and Enterprise connections.
fn recent_connections(app: &App) -> Element<'_, Message> {
    let mut list = column![].spacing(4);
    if !app.settings.recent_databases.is_empty() {
        list = list.push(Space::new().height(8));
        list = list.push(section_label("Recent"));
        for recent in &app.settings.recent_databases {
            list = list.push(
                button(
                    text(grid::truncate(recent, 40))
                        .size(12)
                        .font(Font::MONOSPACE),
                )
                .width(Fill)
                .padding([4, 6])
                .on_press(Message::OpenRecent(recent.clone()))
                .style(theme::list_item(false)),
            );
        }
    }
    if !app.settings.recent_enterprise.is_empty() {
        list = list.push(Space::new().height(8));
        list = list.push(section_label("Recent Enterprise"));
        for target in &app.settings.recent_enterprise {
            list = list.push(
                button(
                    row![
                        text(grid::truncate(&target.label(), 34)).size(12),
                        Space::new().width(Fill),
                        container(text("Enterprise").size(10))
                            .padding([1, 5])
                            .style(theme::badge),
                    ]
                    .align_y(Alignment::Center),
                )
                .width(Fill)
                .padding([4, 6])
                .on_press(Message::OpenRecentEnterprise(target.clone()))
                .style(theme::list_item(false)),
            );
        }
    }
    list.into()
}

/// The heading above a namespace's tables in a server catalog.
fn namespace_header(namespace: &[String]) -> Element<'_, Message> {
    let name = if namespace.is_empty() {
        "public".to_string()
    } else {
        crate::enterprise::format_namespace(namespace)
    };
    row![text(name).size(12).font(BOLD), faint("namespace").size(10),]
        .spacing(6)
        .align_y(Alignment::Center)
        .padding(iced::Padding {
            top: 6.0,
            ..iced::Padding::ZERO
        })
        .into()
}

fn table_entry<'a>(app: &'a App, table: &'a TableInfo) -> Element<'a, Message> {
    let expanded = app.expanded.contains(&table.key());
    let caret = if expanded { "▾" } else { "▸" };
    let trailing: Element<'a, Message> = match table.kind {
        TableKind::Table => faint(thousands(table.num_rows))
            .size(11)
            .font(Font::MONOSPACE)
            .into(),
        TableKind::View => container(text("view").size(10))
            .padding([1, 5])
            .style(theme::badge)
            .into(),
        TableKind::MaterializedView => with_tooltip(
            container(text("mv").size(10))
                .padding([1, 5])
                .style(theme::accent_badge),
            "Materialized view: a table kept up to date by refreshing its query",
        ),
    };
    let title = button(
        row![
            faint(caret).size(12).width(12),
            text(&table.name).size(13).font(BOLD),
            Space::new().width(Fill),
            trailing,
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .width(Fill)
    .padding([5, 6])
    .on_press(Message::ToggleTable(table.key()))
    .style(theme::list_item(false));

    if !expanded {
        return title.into();
    }

    let mut details = column![].spacing(1).padding(iced::Padding {
        left: 18.0,
        ..iced::Padding::ZERO
    });

    let meta = match table.kind {
        TableKind::View => "view · a stored query".to_string(),
        _ => format!(
            "v{} · {} · {} index{}",
            table.version,
            human_bytes(table.size_bytes),
            table.indices.len(),
            if table.indices.len() == 1 { "" } else { "es" }
        ),
    };
    let mut actions = row![faint(meta).size(11), Space::new().width(Fill)]
        .align_y(Alignment::Center)
        .spacing(6);
    let server_sql = app
        .database
        .as_ref()
        .and_then(Database::remote)
        .is_some_and(|remote| remote.capabilities().sql());
    if server_sql && table.kind == TableKind::MaterializedView {
        actions = actions.push(with_tooltip(
            button(text("Refresh").size(11))
                .padding([2, 8])
                .on_press(Message::RefreshMaterializedView(table.table_ref()))
                .style(theme::secondary_button),
            "Runs REFRESH MATERIALIZED VIEW (a server job)",
        ));
    }
    if server_sql && table.kind != TableKind::View {
        actions = actions.push(with_tooltip(
            button(text("+ Column").size(11))
                .padding([2, 8])
                .on_press(Message::ComputedColumnTemplate(table.table_ref()))
                .style(theme::secondary_button),
            "Add a computed column (opens a template in the editor)",
        ));
    }
    actions = actions.push(
        button(text("Preview").size(11))
            .padding([2, 8])
            .on_press(Message::PreviewTable(table.table_ref()))
            .style(theme::secondary_button),
    );
    details = details.push(actions);

    for column in &table.columns {
        let index = table
            .indices
            .iter()
            .find(|index| index.columns.contains(&column.name));
        let type_badge = container(
            container(
                text(type_label(&column.data_type))
                    .size(10)
                    .font(Font::MONOSPACE),
            )
            .padding([1, 5])
            .style(if column.is_vector() {
                theme::accent_badge
            } else {
                theme::badge
            }),
        )
        .width(TYPE_SLOT)
        .align_x(Alignment::End);

        let name = button(text(&column.name).size(12).font(Font::MONOSPACE))
            .padding([2, 4])
            .on_press(Message::InsertText(quote_ident(&column.name)))
            .style(theme::ghost_button);

        let mut base = row![name, Space::new().width(Fill)]
            .spacing(4)
            .align_y(Alignment::Center);
        if let Some(index) = index {
            base = base.push(with_tooltip(
                container(text(index.kind.clone()).size(10))
                    .padding([1, 5])
                    .style(theme::accent_badge),
                "Indexed",
            ));
        }
        match &column.computed {
            Some(Computed::Function) => {
                base = base.push(with_tooltip(
                    container(text("ƒ").size(10))
                        .padding([1, 5])
                        .style(theme::accent_badge),
                    "Computed by a Function",
                ));
            }
            Some(Computed::Expression(expression)) => {
                base = base.push(with_tooltip(
                    container(text("=").size(10))
                        .padding([1, 5])
                        .style(theme::badge),
                    expression.as_str(),
                ));
            }
            None => {}
        }
        let base = base.push(type_badge);

        // Un-indexed columns reveal an "add index" action on hover.
        let entry: Element<'a, Message> = if index.is_some() {
            base.into()
        } else {
            let (label, kind, tip) = if column.is_text() {
                (
                    "+ FTS",
                    IndexKind::FullText,
                    "Build a full-text index (enables fts())",
                )
            } else if column.is_vector() {
                (
                    "+ index",
                    IndexKind::Auto,
                    "Build a vector index (IVF-PQ needs ≥ 256 rows)",
                )
            } else {
                ("+ index", IndexKind::Auto, "Build a scalar (B-tree) index")
            };
            let action = with_tooltip(
                button(text(label).size(10))
                    .padding([1, 6])
                    .on_press(Message::CreateIndex(
                        table.table_ref(),
                        column.name.clone(),
                        kind,
                    ))
                    .style(theme::secondary_button),
                tip,
            );
            // The action takes the type badge's slot (on the panel's own
            // background), so the index and computed-column badges to its
            // left stay visible and keep their tooltips.
            hover(
                base,
                row![
                    Space::new().width(Fill),
                    container(action)
                        .width(TYPE_SLOT)
                        .height(Fill)
                        .align_x(Alignment::End)
                        .align_y(Alignment::Center)
                        .style(theme::chrome_panel),
                ]
                .height(Fill),
            )
        };
        details = details.push(entry);
        if column.computed == Some(Computed::Function)
            && let Some(status) = app.column_status.get(&table.key())
        {
            details = details.push(column_freshness(table, &column.name, status));
        }
    }

    column![title, details].spacing(2).into()
}

/// The freshness of a Function column, with Refresh and Errors actions.
fn column_freshness<'a>(
    table: &'a TableInfo,
    column: &'a str,
    status: &'a Result<Vec<ColumnStatus>, String>,
) -> Element<'a, Message> {
    let line: Element<'a, Message> = match status {
        Ok(statuses) => match statuses.iter().find(|status| status.column == column) {
            Some(status) => row![
                faint(function::freshness(
                    status.fresh,
                    status.stale,
                    status.failed,
                    status.never_computed
                ))
                .size(10),
                Space::new().width(Fill),
                with_tooltip(
                    button(text("Refresh").size(10))
                        .padding([1, 6])
                        .on_press(Message::RefreshColumn(
                            table.table_ref(),
                            column.to_string()
                        ))
                        .style(theme::secondary_button),
                    "Runs REFRESH COLUMN (a server job)",
                ),
                with_tooltip(
                    button(text("Errors").size(10))
                        .padding([1, 6])
                        .on_press(Message::ShowColumnErrors(table.table_ref()))
                        .style(theme::secondary_button),
                    "Runs SHOW ERRORS: rows the Function failed on",
                ),
            ]
            .spacing(4)
            .align_y(Alignment::Center)
            .into(),
            None => faint("No refresh status yet").size(10).into(),
        },
        Err(error) => with_tooltip(faint("Status unavailable").size(10), error.as_str()),
    };
    container(line)
        .padding(iced::Padding {
            left: 12.0,
            bottom: 3.0,
            ..iced::Padding::ZERO
        })
        .into()
}

/// The server's Functions, newest version of each.
fn functions_section(app: &App) -> Element<'_, Message> {
    let mut list = column![
        Space::new().height(12),
        row![
            section_label("Functions"),
            Space::new().width(Fill),
            with_tooltip(
                button(text("+ New").size(11))
                    .padding([1, 8])
                    .on_press(Message::NewFunctionTemplate)
                    .style(theme::secondary_button),
                "Opens a CREATE FUNCTION template in the editor",
            ),
        ]
        .align_y(Alignment::Center),
    ]
    .spacing(2);
    let mut seen = HashSet::new();
    // Sorted by name, newest version first: the first of each name.
    for function in app
        .functions
        .iter()
        .filter(|function| seen.insert(function.name.as_str()))
    {
        let selected = app
            .function
            .as_ref()
            .is_some_and(|view| view.name == function.name);
        let meta = if function.python_version.is_empty() {
            format!("v{}", function.version)
        } else {
            format!("v{} · py{}", function.version, function.python_version)
        };
        list = list.push(
            button(
                row![
                    faint("ƒ").size(12).font(Font::MONOSPACE),
                    text(function.name.as_str()).size(12).font(Font::MONOSPACE),
                    Space::new().width(Fill),
                    faint(meta).size(11),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .padding([3, 6])
            .on_press(Message::SelectFunction(
                function.name.clone(),
                function.version.clone(),
            ))
            .style(theme::list_item(selected)),
        );
    }
    if app.functions.is_empty() {
        list = list.push(faint("No Functions yet.").size(12));
    }
    list.into()
}

/// Width reserved for column type badges in the sidebar.
const TYPE_SLOT: f32 = 92.0;

pub(crate) fn thin_scrollbar() -> scrollable::Direction {
    scrollable::Direction::Vertical(scrollable::Scrollbar::new().width(6).scroller_width(6))
}

// ---------------------------------------------------------------------------
// Editor pane
// ---------------------------------------------------------------------------

fn editor_pane(app: &App) -> Element<'_, Message> {
    let run_or_cancel: Element<'_, Message> = if app.running.is_some() {
        button(text("■  Cancel").size(13).font(BOLD))
            .padding([6, 14])
            .on_press(Message::Cancel)
            .style(theme::secondary_button)
            .into()
    } else {
        with_tooltip(
            button(text("▶  Run").size(13).font(BOLD))
                .padding([6, 14])
                .on_press_maybe(app.database.as_ref().map(|_| Message::Run))
                .style(theme::primary_button),
            "Run the editor, or just the selection (Ctrl+Enter)",
        )
    };

    let hint = if app.editor.selection().is_some_and(|s| !s.trim().is_empty()) {
        "Runs the selection"
    } else {
        "Ctrl+Enter to run"
    };

    let server: Element<'_, Message> = match app.database.as_ref().and_then(Database::remote) {
        Some(remote) if remote.capabilities().sql() => row![
            with_tooltip(
                container(text("Runs on the server").size(11))
                    .padding([2, 8])
                    .style(theme::accent_badge),
                "SQL runs on the LanceDB Enterprise server, one statement per request",
            ),
            with_tooltip(
                checkbox(app.collect_plan)
                    .label("Collect plan")
                    .on_toggle(Message::SetCollectPlan)
                    .size(14)
                    .text_size(12),
                "Also run EXPLAIN ANALYZE for the Plan tab (executes the query again)",
            ),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into(),
        Some(_) => with_tooltip(
            container(text("SQL unavailable").size(11))
                .padding([2, 8])
                .style(theme::badge),
            "The server's SQL endpoint is unreachable; browsing and previews still work",
        ),
        None => Space::new().into(),
    };

    let toolbar = row![
        run_or_cancel,
        faint(hint).size(12),
        server,
        Space::new().width(Fill),
        pick_list(RowLimit::ALL, Some(app.row_limit()), Message::SetRowLimit)
            .text_size(12)
            .padding([4, 8])
            .style(theme::picker),
    ]
    .spacing(12)
    .align_y(Alignment::Center);

    let running = app.running.is_some();
    let editor = text_editor(&app.editor)
        .placeholder("SELECT * FROM my_table LIMIT 10;")
        .on_action(Message::Edit)
        .font(Font::MONOSPACE)
        .size(14)
        .padding(12)
        .height(Fill)
        .highlight_with::<SqlHighlighter>(app.theme_id(), highlight::to_format)
        .key_binding(move |press| {
            let is_enter = matches!(
                press.key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::Enter)
            );
            let is_escape = matches!(
                press.key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::Escape)
            );
            if is_enter && press.modifiers.command() {
                Some(text_editor::Binding::Custom(Message::Run))
            } else if is_escape && running {
                // Esc cancels a running query even while typing.
                Some(text_editor::Binding::Custom(Message::Cancel))
            } else {
                text_editor::Binding::from_key_press(press)
            }
        })
        .style(theme::editor);

    column![
        container(toolbar)
            .padding([8, 12])
            .width(Fill)
            .style(theme::chrome_panel),
        hairline(),
        container(editor).width(Fill).height(Fill).style(theme::app),
    ]
    .into()
}

// ---------------------------------------------------------------------------
// Results pane
// ---------------------------------------------------------------------------

fn results_pane(app: &App) -> Element<'_, Message> {
    let mut tabs = row![
        tab_button("Results", ResultTab::Rows, app.tab),
        tab_button("Columns", ResultTab::Columns, app.tab),
    ]
    .spacing(2);
    if app.has_gallery() {
        tabs = tabs.push(tab_button("Media", ResultTab::Media, app.tab));
    }
    let mut tabs = tabs
        .push(tab_button("Plan", ResultTab::Plan, app.tab))
        .push(tab_button("Chart", ResultTab::Chart, app.tab));
    if let Some(function) = &app.function {
        tabs = tabs.push(tab_button(
            format!("ƒ {}", function.name),
            ResultTab::Function,
            app.tab,
        ));
    }

    let summary: Element<'_, Message> = match (&app.table, &app.outcome) {
        (Some(table), Some(outcome)) => {
            let mut parts = row![
                text(format!("{} rows", thousands(table.row_count())))
                    .size(12)
                    .font(BOLD),
                muted(format!("{} columns", table.columns.len())).size(12),
                muted(human_duration(outcome.elapsed)).size(12),
            ]
            .spacing(10)
            .align_y(Alignment::Center);
            if table.result.truncated {
                parts = parts.push(with_tooltip(
                    container(text("truncated").size(11))
                        .padding([1, 6])
                        .style(theme::accent_badge),
                    "More rows were available than the row limit",
                ));
            }
            if let Some(next) = &outcome.continuation {
                parts = parts.push(with_tooltip(
                    button(text("Next page").size(12))
                        .padding([4, 10])
                        .on_press(Message::RunExample(next.clone()))
                        .style(theme::secondary_button),
                    "The server returned one page; run the statement for the next",
                ));
            }
            if let Some(job_id) = job_in_result(app, table) {
                parts = parts.push(with_tooltip(
                    button(text("Open job").size(12))
                        .padding([4, 10])
                        .on_press(Message::OpenJob(job_id))
                        .style(theme::secondary_button),
                    "Shows this job's progress, timeline and events on the Jobs screen",
                ));
            }
            parts = parts.push(
                button(text("Export CSV").size(12))
                    .padding([4, 10])
                    .on_press(Message::ExportCsv)
                    .style(theme::secondary_button),
            );
            parts.into()
        }
        (None, Some(outcome)) => muted(format!(
            "{} statement{} · {}",
            outcome.statements,
            if outcome.statements == 1 { "" } else { "s" },
            human_duration(outcome.elapsed)
        ))
        .size(12)
        .into(),
        _ => Space::new().into(),
    };

    let header = container(
        row![tabs, Space::new().width(Fill), summary]
            .spacing(12)
            .align_y(Alignment::Center),
    )
    .padding([6, 12])
    .width(Fill)
    .style(theme::chrome_panel);

    let body: Element<'_, Message> = if app.tab == ResultTab::Function
        && let Some(view) = &app.function
    {
        function::view(app, view)
    } else if app.database.is_none() {
        welcome(app)
    } else if app.outcome.is_none() && app.running.is_none() {
        examples(app)
    } else {
        match app.tab {
            ResultTab::Rows => rows_tab(app),
            ResultTab::Columns => columns_tab(app),
            ResultTab::Media if app.has_gallery() => media_tab(app),
            ResultTab::Media => rows_tab(app),
            ResultTab::Plan => plan_tab(app),
            ResultTab::Chart => chart_tab(app),
            ResultTab::Function => rows_tab(app),
        }
    };

    let mut layout = column![header, hairline()];
    if let Some(banner) = banners(app) {
        layout = layout.push(banner);
    }
    layout.push(body).into()
}

/// On a server, the job a result points at: the selected row's `job_id`,
/// or the only row's (`OPTIMIZE`, `CREATE INDEX` and refreshes return one).
fn job_in_result(app: &App, table: &crate::results::ResultTable) -> Option<String> {
    app.remote()?;
    let column = table
        .columns
        .iter()
        .position(|column| column.name == "job_id")?;
    let row = match app.selected {
        Some((row, _)) => row,
        None if table.row_count() == 1 => 0,
        None => return None,
    };
    Some(table.cell_text(row, column)).filter(|id| !id.is_empty() && id != "null")
}

fn tab_button<'a>(
    label: impl text::IntoFragment<'a>,
    tab: ResultTab,
    current: ResultTab,
) -> Element<'a, Message> {
    let active = tab == current;
    column![
        button(
            text(label)
                .size(13)
                .font(if active { BOLD } else { Font::DEFAULT })
        )
        .padding([5, 12])
        .on_press(Message::SelectTab(tab))
        .style(theme::tab(active)),
        container(Space::new().width(Fill).height(2))
            .width(Fill)
            .style(theme::tab_indicator(active)),
    ]
    .width(Shrink)
    .into()
}

fn banners(app: &App) -> Option<Element<'_, Message>> {
    let dismiss = || {
        button(text("✕").size(12))
            .padding([0, 6])
            .on_press(Message::DismissMessages)
            .style(theme::ghost_button)
    };
    let banner = if let Some(error) = &app.error {
        container(
            row![
                text(error.clone())
                    .size(13)
                    .font(Font::MONOSPACE)
                    .width(Fill),
                dismiss()
            ]
            .spacing(8),
        )
        .padding([8, 12])
        .width(Fill)
        .style(theme::error_banner)
    } else if let Some(notice) = &app.notice {
        container(
            row![text(notice.clone()).size(13).width(Fill), dismiss()]
                .spacing(8)
                .align_y(Alignment::Center),
        )
        .padding([6, 12])
        .width(Fill)
        .style(theme::info_banner)
    } else {
        return None;
    };
    Some(container(banner).padding([8, 12]).into())
}

fn welcome(app: &App) -> Element<'_, Message> {
    let mut card = column![
        text("Open a LanceDB database").size(22).font(BOLD),
        muted("Browse for a LanceDB directory, use Open… for a path or a LanceDB Enterprise server, or start with the sample database (movies with vector embeddings and poster images, 50k analytics events, and short video trailers when ffmpeg is installed).").size(14),
        row![
            button(text("Create sample database").size(14).font(BOLD))
                .padding([8, 16])
                .on_press_maybe(app.busy.is_none().then_some(Message::OpenSample))
                .style(theme::primary_button),
            button(text("Browse…").size(14))
                .padding([8, 16])
                .on_press_maybe(app.busy.is_none().then_some(Message::BrowseDatabase))
                .style(theme::secondary_button),
            button(text("Open…").size(14))
                .padding([8, 16])
                .on_press_maybe(app.busy.is_none().then_some(Message::OpenConnectDialog))
                .style(theme::secondary_button),
        ]
        .spacing(10),
    ]
    .spacing(14)
    .max_width(560);

    if !app.settings.recent_databases.is_empty() {
        card = card.push(section_label("Recent databases"));
        for recent in app.settings.recent_databases.iter().take(5) {
            card = card.push(
                button(text(recent).size(13).font(Font::MONOSPACE))
                    .padding([4, 8])
                    .on_press(Message::OpenRecent(recent.clone()))
                    .style(theme::ghost_button),
            );
        }
    }

    center(container(card).padding(28).style(theme::raised_card))
        .padding(24)
        .into()
}

fn examples(app: &App) -> Element<'_, Message> {
    let has_sample = ["movies", "events"]
        .iter()
        .all(|name| app.catalog.iter().any(|t| t.name == *name));

    let mut list = column![
        text("Ready").size(20).font(BOLD),
        muted("Write SQL above and press Ctrl+Enter. Click a table in the sidebar to expand it, a column name to insert it, or Preview to peek at rows.").size(14),
    ]
    .spacing(10)
    .max_width(640);

    if has_sample {
        list = list.push(Space::new().height(6));
        list = list.push(section_label("Try an example"));
        for (title, sql) in SAMPLE_EXAMPLES
            .iter()
            .filter(|(_, sql)| crate::app::example_available(sql, &app.catalog))
        {
            list = list.push(
                button(
                    column![
                        text(*title).size(14).font(BOLD),
                        faint(sql.lines().find(|l| !l.starts_with("--")).unwrap_or(sql))
                            .size(12)
                            .font(Font::MONOSPACE),
                    ]
                    .spacing(2),
                )
                .width(Fill)
                .padding([8, 12])
                .on_press(Message::RunExample((*sql).to_string()))
                .style(theme::secondary_button),
            );
        }
    } else if let Some(first) = app.catalog.first() {
        list = list.push(
            button(text(format!("Preview {}", first.name)).size(14))
                .padding([8, 14])
                .on_press(Message::PreviewTable(first.table_ref()))
                .style(theme::primary_button),
        );
    }

    scrollable(container(list).padding(28).width(Fill))
        .direction(thin_scrollbar())
        .height(Fill)
        .into()
}

fn running_placeholder(app: &App) -> Option<Element<'_, Message>> {
    let elapsed = app.running_for()?;
    Some(
        center(
            column![
                text("Running query…").size(16).font(BOLD),
                muted(human_duration(elapsed)).size(13),
                button(text("Cancel").size(13))
                    .padding([6, 14])
                    .on_press(Message::Cancel)
                    .style(theme::secondary_button),
            ]
            .spacing(8)
            .align_x(Alignment::Center),
        )
        .into(),
    )
}

fn rows_tab(app: &App) -> Element<'_, Message> {
    if let Some(placeholder) = running_placeholder(app) {
        return placeholder;
    }
    let Some(table) = &app.table else {
        return center(muted("The last statement returned no result set.").size(14)).into();
    };

    let grid = canvas_widget(grid::Grid {
        table,
        selected: app.selected,
        generation: app.generation,
    })
    .width(Fill)
    .height(Fill);

    let mut layout = column![grid];
    if let Some(inspector) = inspector(app) {
        layout = layout.push(hairline()).push(inspector);
    }
    layout.into()
}

/// Side of the image preview box in the inspector.
const PREVIEW_BOX: f32 = 140.0;

/// Details of the selected cell: full value, image preview, and actions.
pub(crate) fn inspector(app: &App) -> Option<Element<'_, Message>> {
    let (row, column) = app.selected?;
    let table = app.table.as_ref()?;
    let value = app.selected_value()?;
    let meta = &table.columns[column];
    let media = app.media_columns.iter().find(|c| c.index == column);
    let preview = app
        .preview
        .as_ref()
        .filter(|preview| preview.row == row && preview.column == column);
    let is_video = preview.is_some_and(|p| p.kind == Some(MediaKind::Video));

    let mut actions = row![
        button(text("Copy").size(12))
            .padding([4, 10])
            .on_press(Message::CopyCell)
            .style(theme::secondary_button),
    ]
    .spacing(6);
    if preview.is_some_and(|p| p.frames.len() > 1) {
        actions = actions.push(with_tooltip(
            button(
                text(if app.playing {
                    "❚❚ Pause"
                } else {
                    "▶ Play"
                })
                .size(12),
            )
            .padding([4, 10])
            .on_press(Message::TogglePlayback)
            .style(theme::secondary_button),
            "Flip through the filmstrip (no audio; use Open for real playback)",
        ));
    }
    if media.is_some() {
        actions = actions.push(with_tooltip(
            button(text("Open").size(12))
                .padding([4, 10])
                .on_press(Message::OpenExternally)
                .style(theme::secondary_button),
            "Open in the default app (e.g. your video player)",
        ));
    }
    if media.is_some_and(|m| m.source == gallery::Source::Bytes) {
        actions = actions.push(with_tooltip(
            button(text("Save as…").size(12))
                .padding([4, 10])
                .on_press(Message::SaveCell)
                .style(theme::secondary_button),
            "Write these bytes to a file",
        ));
    }
    if app.similar_target().is_some() {
        actions = actions.push(with_tooltip(
            button(text("Find similar").size(12).font(BOLD))
                .padding([4, 10])
                .on_press(Message::FindSimilar)
                .style(theme::primary_button),
            "Run vector_search() with this vector",
        ));
    }
    if app.similar_image_target().is_some() {
        let label = if is_video {
            "Find similar videos"
        } else {
            "Find similar images"
        };
        actions = actions.push(with_tooltip(
            button(text(label).size(12).font(BOLD))
                .padding([4, 10])
                .on_press(Message::FindSimilarImages)
                .style(theme::primary_button),
            "vector_search() on the picture's colour vector",
        ));
    }

    let mut details = column![
        row![
            text(&meta.name).size(12).font(BOLD),
            faint(format!("{} · row {}", meta.type_label, thousands(row + 1))).size(11),
            Space::new().width(Fill),
            actions,
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        scrollable(text(value).size(12).font(Font::MONOSPACE))
            .direction(thin_scrollbar())
            .height(Length::Shrink),
    ]
    .spacing(6)
    .width(Fill);

    if let Some(info) = preview.and_then(|p| p.info.as_ref()) {
        let mut facts = Vec::new();
        if let Some(fps) = info.frame_rate {
            facts.push(format!("{fps:.2} fps").replace(".00 fps", " fps"));
        }
        if let Some(audio) = &info.audio_codec
            && info.has_video()
        {
            facts.push(format!("audio: {audio}"));
        }
        if !facts.is_empty() {
            details = details.push(faint(facts.join(" · ")).size(11));
        }
    }

    // Filmstrip: click a frame to show it.
    if let Some(preview) = preview.filter(|p| !p.frames.is_empty()) {
        let strip = preview.frames.iter().enumerate().map(|(index, frame)| {
            button(
                iced::widget::image(frame.clone())
                    .content_fit(iced::ContentFit::Contain)
                    .width(FILMSTRIP_FRAME)
                    .height(FILMSTRIP_FRAME * 0.6),
            )
            .padding(2)
            .on_press(Message::ShowFrame(index))
            .style(theme::list_item(index == app.frame))
            .into()
        });
        details = details.push(iced::widget::Row::with_children(strip).spacing(4));
    } else if is_video && !crate::ffmpeg::available() {
        details = details.push(
            faint("Install ffmpeg (or set JOUST_FFMPEG) to see video frames; Open plays it.")
                .size(11),
        );
    }

    let picture = preview.and_then(|p| {
        let flipping = !p.frames.is_empty() && (app.playing || app.frame > 0);
        if flipping {
            p.frames.get(app.frame).cloned()
        } else {
            p.handle.clone()
        }
    });
    let picture: Option<Element<'_, Message>> = match picture {
        Some(handle) => Some(
            iced::widget::image(handle)
                .content_fit(iced::ContentFit::Contain)
                .width(PREVIEW_BOX)
                .height(PREVIEW_BOX)
                .into(),
        ),
        None if is_video => Some(muted("▶").size(40).into()),
        None => None,
    };
    let body: Element<'_, Message> = match picture {
        Some(picture) => row![
            container(picture)
                .center_x(PREVIEW_BOX)
                .center_y(PREVIEW_BOX)
                .style(theme::badge),
            details,
        ]
        .spacing(12)
        .into(),
        None => details.into(),
    };

    Some(
        container(body)
            .padding([8, 12])
            .width(Fill)
            .max_height(if preview.is_some() {
                PREVIEW_BOX + 16.0
            } else {
                140.0
            })
            .style(theme::chrome_panel)
            .into(),
    )
}

/// Width of a filmstrip frame in the inspector.
const FILMSTRIP_FRAME: f32 = 64.0;

fn media_tab(app: &App) -> Element<'_, Message> {
    if let Some(placeholder) = running_placeholder(app) {
        return placeholder;
    }
    gallery::view(app)
}

fn columns_tab(app: &App) -> Element<'_, Message> {
    if let Some(placeholder) = running_placeholder(app) {
        return placeholder;
    }
    match (&app.table, &app.profiles) {
        (None, _) => center(muted("No result set to profile.").size(14)).into(),
        (Some(_), None) => center(muted("Profiling columns…").size(14)).into(),
        (Some(_), Some(profiles)) => columns::view(profiles, app.theme_id()),
    }
}

fn plan_tab(app: &App) -> Element<'_, Message> {
    if let Some(placeholder) = running_placeholder(app) {
        return placeholder;
    }
    let Some(outcome) = &app.outcome else {
        return Space::new().into();
    };
    let Some(root) = &outcome.plan else {
        let hint = if app.database.as_ref().and_then(Database::remote).is_some() {
            "The server sends a plan when \"Collect plan\" is on (it runs EXPLAIN ANALYZE, \
             executing the query again)."
        } else {
            "This statement has no execution plan."
        };
        return center(muted(hint).size(14)).into();
    };

    let modes = row![
        mode_button("Diagram", PlanMode::Diagram, app.plan_mode),
        mode_button("Physical", PlanMode::Physical, app.plan_mode),
        mode_button("Logical", PlanMode::Logical, app.plan_mode),
    ]
    .spacing(4);

    let slowest = slowest_operator(root);
    let summary = row![
        muted(format!("{} operators", root.node_count())).size(12),
        muted(format!("compute {}", human_duration(root.total_elapsed()))).size(12),
    ]
    .spacing(12);
    let summary = match slowest {
        Some((name, elapsed)) => {
            summary.push(text(format!("slowest: {name} ({})", human_duration(elapsed))).size(12))
        }
        None => summary,
    };

    let toolbar = container(
        row![
            modes,
            Space::new().width(Fill),
            summary,
            faint("drag to pan · Ctrl+scroll to zoom").size(11),
        ]
        .spacing(14)
        .align_y(Alignment::Center),
    )
    .padding([6, 12])
    .width(Fill);

    let body: Element<'_, Message> = match app.plan_mode {
        PlanMode::Diagram => canvas_widget(plan_view::PlanDiagram {
            root,
            theme: app.theme_id(),
            generation: app.generation,
        })
        .width(Fill)
        .height(Fill)
        .into(),
        PlanMode::Physical => plan_text(&outcome.physical_plan),
        PlanMode::Logical => plan_text(&outcome.logical_plan),
    };
    column![toolbar, hairline(), body].into()
}

fn slowest_operator(root: &crate::db::plan::PlanNode) -> Option<(String, std::time::Duration)> {
    let mut best: Option<(String, std::time::Duration)> = None;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if let Some(elapsed) = node.elapsed
            && best.as_ref().is_none_or(|(_, b)| elapsed > *b)
        {
            best = Some((node.name.clone(), elapsed));
        }
        stack.extend(node.children.iter());
    }
    best.filter(|(_, elapsed)| !elapsed.is_zero())
}

fn plan_text(plan: &str) -> Element<'_, Message> {
    scrollable(
        container(text(plan).size(12).font(Font::MONOSPACE))
            .padding(14)
            .width(Fill),
    )
    .direction(scrollable::Direction::Both {
        vertical: scrollable::Scrollbar::new().width(6).scroller_width(6),
        horizontal: scrollable::Scrollbar::new().width(6).scroller_width(6),
    })
    .width(Fill)
    .height(Fill)
    .into()
}

fn mode_button<'a>(label: &'a str, mode: PlanMode, current: PlanMode) -> Element<'a, Message> {
    button(text(label).size(12))
        .padding([4, 10])
        .on_press(Message::SetPlanMode(mode))
        .style(if mode == current {
            theme::primary_button
        } else {
            theme::secondary_button
        })
        .into()
}

fn chart_tab(app: &App) -> Element<'_, Message> {
    if let Some(placeholder) = running_placeholder(app) {
        return placeholder;
    }
    let Some(table) = &app.table else {
        return center(muted("No result set to chart.").size(14)).into();
    };

    let choices: Vec<ColumnChoice> = table
        .columns
        .iter()
        .enumerate()
        .map(|(index, meta)| ColumnChoice {
            index,
            name: meta.name.clone(),
        })
        .collect();
    let numeric: Vec<ColumnChoice> = choices
        .iter()
        .filter(|c| table.columns[c.index].numeric)
        .cloned()
        .collect();
    let selected = |index: Option<usize>| index.and_then(|i| choices.get(i).cloned());

    let mut controls = row![
        muted("Chart").size(12),
        pick_list(
            ChartKind::ALL,
            Some(app.chart_spec.kind),
            Message::SetChartKind
        )
        .text_size(12)
        .padding([4, 8])
        .style(theme::picker),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if app.chart_spec.kind.uses_x() {
        controls = controls.push(muted("X").size(12)).push(
            pick_list(
                choices.clone(),
                selected(app.chart_spec.x),
                Message::SetChartX,
            )
            .placeholder("column")
            .text_size(12)
            .padding([4, 8])
            .style(theme::picker),
        );
    }
    controls = controls
        .push(
            muted(if app.chart_spec.kind.uses_x() {
                "Y"
            } else {
                "Values"
            })
            .size(12),
        )
        .push(
            pick_list(numeric, selected(app.chart_spec.y), Message::SetChartY)
                .placeholder("numeric column")
                .text_size(12)
                .padding([4, 8])
                .style(theme::picker),
        );

    let body: Element<'_, Message> = match &app.chart {
        Some(Ok(data)) => {
            let mut body = column![
                canvas_widget(chart::Chart {
                    data,
                    theme: app.theme_id(),
                })
                .width(Fill)
                .height(Fill)
            ];
            if let Some(note) = &data.note {
                body = body.push(container(faint(note.clone()).size(12)).padding([4, 12]));
            }
            body.into()
        }
        Some(Err(reason)) => center(muted(reason.clone()).size(14)).into(),
        None => Space::new().into(),
    };

    column![
        container(controls).padding([6, 12]).width(Fill),
        hairline(),
        container(body).padding(12).width(Fill).height(Fill),
    ]
    .into()
}

#[cfg(test)]
mod tests {
    use iced_test::Simulator;
    use iced_test::selector::{Candidate, Selector};

    use super::*;
    use crate::enterprise::profiles::AuthMethod;
    use crate::test_support::{app, opened, sample_path, send};
    use iced::{Point, mouse};

    /// A headless window showing the whole app.
    fn window(app: &App) -> Simulator<'_, Message> {
        Simulator::with_size(iced::Settings::default(), (1280.0, 800.0), view(app))
    }

    /// Switches the app to `theme` and renders it (runs every widget's and
    /// canvas's draw code, the theme's styles and the editor highlighting).
    fn render(app: &mut App, theme: ThemeId) {
        send(app, Message::SelectTheme(theme));
        window(app)
            .snapshot(&app.theme())
            .expect("snapshot renders");
    }

    /// Renders with the cursor over `label`, so hover styles are drawn.
    fn render_hovering(app: &App, label: &str) {
        let mut ui = window(app);
        let target = ui
            .find(label)
            .unwrap_or_else(|e| panic!("no {label:?}: {e:?}"));
        ui.point_at(target.bounds().center());
        let _ = ui.simulate([iced::Event::Mouse(mouse::Event::CursorMoved {
            position: target.bounds().center(),
        })]);
        ui.snapshot(&app.theme()).expect("snapshot renders");
    }

    /// Messages produced by clicking the text `label`.
    fn click(app: &App, label: &str) -> Vec<Message> {
        let mut ui = window(app);
        ui.click(label)
            .unwrap_or_else(|e| panic!("no {label:?} to click: {e:?}"));
        ui.into_messages().collect()
    }

    /// Like [`click`], for the first `label` below `min_y` (skips the top bar).
    fn click_below(app: &App, label: &'static str, min_y: f32) -> Vec<Message> {
        let mut ui = window(app);
        let mut text = label;
        ui.click(move |candidate: Candidate<'_>| {
            text.select(candidate)
                .filter(|found| found.bounds().y > min_y)
        })
        .unwrap_or_else(|e| panic!("no {label:?} to click: {e:?}"));
        ui.into_messages().collect()
    }

    /// Like [`click`], for the last `label` in the window: the Open
    /// dialog's copy when the dialog covers another.
    fn click_last(app: &App, label: &'static str) -> Vec<Message> {
        let matches = std::sync::atomic::AtomicUsize::new(0);
        {
            let mut ui = window(app);
            let mut text = label;
            let _ = ui.find(|candidate: Candidate<'_>| {
                let found = text.select(candidate);
                if found.is_some() {
                    matches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                found.filter(|_| false)
            });
        }
        let last = matches.into_inner();
        assert!(last > 0, "no {label:?} to click");
        let mut ui = window(app);
        let mut text = label;
        let mut seen = 0;
        ui.click(move |candidate: Candidate<'_>| {
            let found = text.select(candidate)?;
            seen += 1;
            (seen == last).then_some(found)
        })
        .unwrap_or_else(|e| panic!("no {label:?} to click: {e:?}"));
        ui.into_messages().collect()
    }

    fn shows(app: &App, label: &str) -> bool {
        window(app).find(label).is_ok()
    }

    fn run(app: &mut App, sql: &str) {
        send(app, Message::LoadQuery(sql.to_string()));
        send(app, Message::Run);
        assert!(app.error.is_none(), "{sql}: {:?}", app.error);
    }

    #[test]
    fn welcome_screen() {
        let mut app = app();
        app.settings.remember_database("/data/films.lancedb");
        assert!(shows(&app, "Open a LanceDB database"));
        assert!(shows(&app, "Not connected"));
        assert!(shows(&app, "No database open."));
        assert!(matches!(
            click(&app, "Create sample database")[..],
            [Message::OpenSample]
        ));
        assert!(matches!(
            &click(&app, "/data/films.lancedb")[..],
            [Message::OpenRecent(path)] if path == "/data/films.lancedb"
        ));
        assert!(matches!(
            click_below(&app, "Open…", 60.0)[..],
            [Message::OpenConnectDialog]
        ));
        // Import needs an open database.
        assert!(click(&app, "Import media…").is_empty());

        // The Open dialog's local tab.
        send(&mut app, Message::OpenConnectDialog);
        assert!(shows(&app, "Local folder"));
        assert!(matches!(
            click_last(&app, "Sample database")[..],
            [Message::OpenSample]
        ));
        assert!(matches!(
            click_last(&app, "Browse…")[..],
            [Message::BrowseDatabase]
        ));
        assert!(matches!(
            click_last(&app, "Open")[..],
            [Message::OpenDatabase]
        ));
        assert!(matches!(
            &click_last(&app, "/data/films.lancedb")[..],
            [Message::OpenRecent(path)] if path == "/data/films.lancedb"
        ));
        assert!(matches!(
            click_last(&app, "LanceDB Enterprise")[..],
            [Message::SelectOpenTab(
                crate::enterprise::dialog::OpenTab::Enterprise
            )]
        ));
        render(&mut app, ThemeId::JoustLight);
        // An empty path is refused in the dialog.
        send(&mut app, Message::OpenDatabase);
        assert!(shows(
            &app,
            "Enter the path of a LanceDB database directory"
        ));
        assert!(app.error.is_none());
        app.busy = Some("Opening /data…".into());
        assert!(click_last(&app, "Sample database").is_empty(), "busy");
        app.busy = None;
        send(&mut app, Message::CloseConnectDialog);

        for theme in ThemeId::ALL {
            render(&mut app, theme);
        }

        // Banners and the busy state.
        app.error = Some("could not open /nope".into());
        assert!(shows(&app, "could not open /nope"));
        assert!(matches!(click(&app, "✕")[..], [Message::DismissMessages]));
        app.error = None;
        app.notice = Some("Opened".into());
        assert!(shows(&app, "Opened"));
        render(&mut app, ThemeId::LanceMidnight);
        app.error = Some("could not open /nope".into());
        render(&mut app, ThemeId::GruvboxDark);
        render_hovering(&app, "Create sample database");
        render_hovering(&app, "Open…");
        render_hovering(&app, "✕");
        app.busy = Some("Opening /data…".into());
        assert!(shows(&app, "Opening /data…"));
        assert!(click(&app, "Create sample database").is_empty(), "busy");
        render(&mut app, ThemeId::JoustDark);
    }

    #[test]
    fn empty_database_and_editor_shortcuts() {
        let dir = tempfile::tempdir().unwrap();
        let mut app = opened(dir.path());
        assert!(shows(&app, "This database has no tables yet."));
        assert!(shows(&app, "Ready"));
        render(&mut app, ThemeId::Nord);

        // A database without the sample tables offers to preview the first.
        run(&mut app, "CREATE TABLE t AS SELECT 1 AS x");
        app.outcome = None;
        assert!(
            matches!(&click(&app, "Preview t")[..], [Message::PreviewTable(t)] if t.name == "t")
        );

        // Editor key bindings: Ctrl+Enter runs, Esc cancels a running query.
        send(&mut app, Message::Edit(text_editor::Action::SelectAll));
        assert!(shows(&app, "Runs the selection"));
        let editor_point = Point::new(700.0, 200.0);
        let press = |app: &App, key, modifiers| {
            let mut ui = window(app);
            ui.point_at(editor_point);
            let _ = ui.simulate(crate::test_support::click_events());
            let _ = ui.simulate([crate::test_support::key_press(key, modifiers)]);
            ui.into_messages().collect::<Vec<_>>()
        };
        let enter = || crate::test_support::named(keyboard::key::Named::Enter);
        let escape = || crate::test_support::named(keyboard::key::Named::Escape);
        assert!(
            press(&app, enter(), keyboard::Modifiers::COMMAND)
                .iter()
                .any(|m| matches!(m, Message::Run))
        );
        assert!(
            !press(&app, escape(), keyboard::Modifiers::empty())
                .iter()
                .any(|m| matches!(m, Message::Cancel)),
            "nothing to cancel"
        );
        send(
            &mut app,
            Message::LoadQuery("SELECT count(*) FROM t".into()),
        );
        let task = app.update(Message::Run);
        assert!(
            press(&app, escape(), keyboard::Modifiers::empty())
                .iter()
                .any(|m| matches!(m, Message::Cancel))
        );
        assert!(
            press(&app, enter(), keyboard::Modifiers::empty())
                .iter()
                .any(|m| matches!(m, Message::Edit(_)))
        );
        // Every tab shows the running placeholder.
        for tab in [
            ResultTab::Columns,
            ResultTab::Plan,
            ResultTab::Chart,
            ResultTab::Media,
        ] {
            send(&mut app, Message::SelectTab(tab));
            assert!(shows(&app, "Running query…"), "{tab:?}");
        }
        crate::test_support::settle(&mut app, task);

        // Media tab without media shows rows; profiles may still be pending.
        run(&mut app, "SELECT 1 AS x");
        send(&mut app, Message::SelectTab(ResultTab::Media));
        render(&mut app, ThemeId::Dracula);
        app.profiles = None;
        send(&mut app, Message::SelectTab(ResultTab::Columns));
        assert!(shows(&app, "Profiling columns…"));
        send(&mut app, Message::SelectTab(ResultTab::Plan));
        render(&mut app, ThemeId::TokyoNight);
    }

    #[test]
    fn gallery_states() {
        let (_dir, path) = sample_path();
        let mut app = opened(&path);
        // Over the thumbnail limit, no caption column, and undecodable or
        // non-visual values mixed in.
        run(
            &mut app,
            "SELECT CASE WHEN id = 1 THEN decode('89504e470d0a1a0a00', 'hex') \
                    WHEN id = 2 THEN decode('255044462d312e37', 'hex') \
                    ELSE poster END AS poster \
             FROM movies CROSS JOIN (SELECT * FROM unnest(range(7))) ORDER BY id",
        );
        send(&mut app, Message::SelectTab(ResultTab::Media));
        let gallery = app.gallery.clone().expect("gallery");
        assert!(gallery.limited);
        assert_eq!(gallery.caption, None);
        // 400 rows get thumbnails, except the 7 PDFs; the 7 broken PNGs
        // get a placeholder.
        assert_eq!(gallery.thumbs.len(), 393);
        assert_eq!(
            gallery
                .thumbs
                .values()
                .filter(|t| t.handle.is_none())
                .count(),
            7
        );
        assert!(shows(
            &app,
            "393 images · thumbnails for the first 400 rows"
        ));
        assert!(shows(&app, "row 1"));
        render(&mut app, ThemeId::SolarizedLight);
        // While thumbnails are being built.
        app.gallery = None;
        assert!(shows(&app, "Building thumbnails…"));
    }

    #[test]
    fn workspace_tabs_render_in_every_theme() {
        let (_dir, path) = sample_path();
        let mut app = opened(&path);
        let mut themes = ThemeId::ALL.into_iter().cycle();

        // Examples, sidebar and history.
        assert!(shows(&app, "TRY AN EXAMPLE"));
        assert!(shows(&app, "movies"));
        assert!(matches!(
            &click(&app, SAMPLE_EXAMPLES[0].0)[..],
            [Message::RunExample(sql)] if sql == SAMPLE_EXAMPLES[0].1
        ));
        assert!(matches!(
            &click(&app, "Preview")[..],
            [Message::PreviewTable(_)]
        ));
        assert!(matches!(&click(&app, "title")[..], [Message::InsertText(t)] if t == "title"));
        assert!(matches!(&click(&app, "↻")[..], [Message::RefreshCatalog]));
        render(&mut app, themes.next().unwrap());

        run(&mut app, "SELECT * FROM movies ORDER BY id");
        assert!(shows(&app, "64 rows"));
        assert!(shows(&app, "Media"), "poster column enables the media tab");
        assert!(
            shows(&app, "SELECT * FROM movies ORDER BY id"),
            "history entry"
        );
        assert!(matches!(
            &click(&app, "Export CSV")[..],
            [Message::ExportCsv]
        ));
        assert!(matches!(&click(&app, "▶  Run")[..], [Message::Run]));
        for tab in [
            ResultTab::Rows,
            ResultTab::Columns,
            ResultTab::Media,
            ResultTab::Plan,
            ResultTab::Chart,
        ] {
            send(&mut app, Message::SelectTab(tab));
            render(&mut app, themes.next().unwrap());
        }
        for mode in [PlanMode::Physical, PlanMode::Logical, PlanMode::Diagram] {
            send(&mut app, Message::SelectTab(ResultTab::Plan));
            send(&mut app, Message::SetPlanMode(mode));
            render(&mut app, themes.next().unwrap());
        }
        assert!(shows(&app, "drag to pan · Ctrl+scroll to zoom"));
        assert!(matches!(
            &click(&app, "Logical")[..],
            [Message::SetPlanMode(PlanMode::Logical)]
        ));

        send(&mut app, Message::SelectTab(ResultTab::Chart));
        for kind in ChartKind::ALL {
            send(&mut app, Message::SetChartKind(kind));
            render(&mut app, themes.next().unwrap());
        }

        // Aggregates chart as bars; profiling shows every column.
        run(
            &mut app,
            "SELECT genre, count(*) AS n, avg(year) AS year FROM movies GROUP BY genre ORDER BY n DESC",
        );
        for tab in [ResultTab::Chart, ResultTab::Columns, ResultTab::Rows] {
            send(&mut app, Message::SelectTab(tab));
            render(&mut app, themes.next().unwrap());
        }

        // Truncated results, statements without rows.
        send(&mut app, Message::SetRowLimit(RowLimit(1_000)));
        run(&mut app, "SELECT * FROM events");
        assert!(shows(&app, "truncated"));
        send(&mut app, Message::SelectTab(ResultTab::Columns));
        render(&mut app, themes.next().unwrap());
        run(&mut app, "CREATE TABLE few AS SELECT 1 AS x");
        for (tab, note) in [
            (
                ResultTab::Rows,
                "The last statement returned no result set.",
            ),
            (ResultTab::Columns, "No result set to profile."),
            (ResultTab::Chart, "No result set to chart."),
        ] {
            send(&mut app, Message::SelectTab(tab));
            assert!(shows(&app, note), "{note}");
        }
        send(&mut app, Message::SelectTab(ResultTab::Plan));
        assert!(shows(&app, "This statement has no execution plan."));
        assert!(shows(&app, "few"), "the new table is in the sidebar");
    }

    #[test]
    fn inspector_and_media_actions() {
        let (_dir, path) = sample_path();
        let mut app = opened(&path);
        run(
            &mut app,
            "SELECT id, title, vector, poster FROM movies ORDER BY id",
        );

        // A vector cell: copy and find similar.
        send(&mut app, Message::SelectCell(0, 2));
        assert!(matches!(click(&app, "Copy")[..], [Message::CopyCell]));
        assert!(matches!(
            click(&app, "Find similar")[..],
            [Message::FindSimilar]
        ));
        render(&mut app, ThemeId::Dracula);

        // An image cell: preview, open, save and colour search.
        send(&mut app, Message::SelectCell(1, 3));
        assert!(app.preview.is_some());
        assert!(matches!(click(&app, "Open")[..], [Message::OpenExternally]));
        assert!(matches!(click(&app, "Save as…")[..], [Message::SaveCell]));
        assert!(matches!(
            click(&app, "Find similar images")[..],
            [Message::FindSimilarImages]
        ));
        render(&mut app, ThemeId::GruvboxDark);

        // A video without frames (e.g. no ffmpeg) shows a play glyph and
        // its stream facts.
        let image_preview = app.preview.clone();
        app.preview = Some(std::sync::Arc::new(crate::app::Preview {
            row: 1,
            column: 3,
            kind: Some(MediaKind::Video),
            handle: None,
            frames: Vec::new(),
            color: None,
            info: Some(crate::av::AvInfo {
                video_codec: Some("H.264".into()),
                audio_codec: Some("AAC".into()),
                frame_rate: Some(25.0),
                ..crate::av::AvInfo::default()
            }),
        }));
        assert!(shows(&app, "25 fps · audio: AAC"));
        assert!(shows(&app, "▶"));
        render(&mut app, ThemeId::LanceMidnight);
        app.preview = image_preview;

        // The gallery shows the same selection and its own inspector.
        send(&mut app, Message::SelectTab(ResultTab::Media));
        assert!(shows(&app, "64 images"));
        assert!(matches!(
            click(&app, "Alien")[..],
            [Message::SelectCell(3, 3)]
        ));
        render(&mut app, ThemeId::SolarizedLight);

        // A running query replaces the tab contents.
        send(
            &mut app,
            Message::LoadQuery("SELECT count(*) FROM events".into()),
        );
        let task = app.update(Message::Run);
        assert!(shows(&app, "Running query…"));
        assert!(matches!(click(&app, "■  Cancel")[..], [Message::Cancel]));
        render(&mut app, ThemeId::Nord);
        crate::test_support::settle(&mut app, task);
    }

    #[test]
    fn video_inspector_has_a_filmstrip() {
        if !crate::ffmpeg::available() {
            eprintln!("skipping: ffmpeg not found");
            return;
        }
        let (_dir, path) = sample_path();
        let mut app = opened(&path);
        run(&mut app, "SELECT title, clip FROM trailers ORDER BY id");
        send(&mut app, Message::SelectTab(ResultTab::Media));
        assert!(shows(&app, "▶ 0:04"));
        send(&mut app, Message::SelectCell(0, 1));
        assert!(matches!(
            click(&app, "▶ Play")[..],
            [Message::TogglePlayback]
        ));
        assert!(matches!(
            click(&app, "Find similar videos")[..],
            [Message::FindSimilarImages]
        ));
        render(&mut app, ThemeId::CatppuccinMocha);
        send(&mut app, Message::TogglePlayback);
        send(&mut app, Message::NextFrame);
        assert!(shows(&app, "❚❚ Pause"));
        render(&mut app, ThemeId::TokyoNight);
    }

    /// A CLI config with an API-key profile and an OIDC one.
    fn profiles_file() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
            default_profile = "prod"
            [profiles.prod]
            http_server_url = "https://query.example.com"
            database = "analytics"
            [profiles.sso]
            http_server_url = "https://sso.example.com"
            [profiles.sso.oidc]
            flow = "authorization_code"
            issuer_url = "https://idp.example.com"
            client_id = "joust"
            scopes = ["openid"]
            "#,
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn connect_dialog_renders_and_responds() {
        let mut app = app();
        assert!(matches!(
            &click(&app, "Open…")[..],
            [Message::OpenConnectDialog]
        ));
        send(&mut app, Message::OpenConnectDialog);
        assert!(shows(&app, "Open a database"));
        assert!(
            shows(&app, "Path to a LanceDB directory…"),
            "no server: local first"
        );
        send(
            &mut app,
            Message::SelectOpenTab(crate::enterprise::dialog::OpenTab::Enterprise),
        );
        assert!(shows(&app, "local"));
        assert!(shows(&app, "SIGN IN"));
        assert_eq!(
            connect::offered_methods(app.connect.as_ref().unwrap()),
            [
                AuthMethod::Automatic,
                AuthMethod::ApiKey,
                AuthMethod::TokenFile
            ],
            "local has no OIDC sign-in"
        );
        render(&mut app, ThemeId::JoustLight);

        assert!(matches!(
            &click(&app, "Test connection")[..],
            [Message::TestConnection]
        ));
        assert!(matches!(
            &click(&app, "Connect")[..],
            [Message::ConnectEnterprise]
        ));
        assert!(matches!(
            &click(&app, "Cancel")[..],
            [Message::CloseConnectDialog]
        ));

        // Each sign-in method's detail, checks, errors and a busy state.
        let dialog = app.connect.as_mut().unwrap();
        dialog.auth = AuthMethod::ApiKey;
        dialog.api_key = "sk_secret".into();
        dialog.checks = Some(vec![
            crate::enterprise::Check::ok("REST API", "reachable"),
            crate::enterprise::Check::unavailable("SQL", "unreachable"),
        ]);
        dialog.error = Some("could not list tables".into());
        dialog.config_error = Some("invalid ~/.lancedb/config.toml".into());
        dialog.working = Some("Testing…");
        assert!(shows(&app, "Connection check"));
        assert!(shows(&app, "could not list tables"));
        assert!(shows(&app, "Testing…"));
        assert!(!shows(&app, "sk_secret"), "typed keys are masked");
        assert!(click(&app, "Connect").is_empty(), "disabled while working");
        render(&mut app, ThemeId::Dracula);

        app.connect.as_mut().unwrap().auth = AuthMethod::TokenFile;
        render(&mut app, ThemeId::Nord);
    }

    #[test]
    fn connect_dialog_offers_profile_sign_in() {
        let (_dir, path) = profiles_file();
        let mut app = app();
        let mut dialog = crate::enterprise::dialog::ConnectDialog::new(Some(&path));
        dialog.tab = crate::enterprise::dialog::OpenTab::Enterprise;
        app.connect = Some(dialog);
        assert!(shows(&app, "prod"));
        assert!(shows(&app, "https://query.example.com"));
        assert!(matches!(
            &click(&app, "sso")[..],
            [Message::ConnectProfile(name)] if name == "sso"
        ));
        send(&mut app, Message::ConnectProfile("sso".into()));
        assert!(
            connect::offered_methods(app.connect.as_ref().unwrap())
                .contains(&AuthMethod::ProfileSignIn)
        );
        for flow in [
            crate::enterprise::profiles::OidcFlow::AuthorizationCode,
            crate::enterprise::profiles::OidcFlow::ClientCredentials,
            crate::enterprise::profiles::OidcFlow::DeviceCode,
        ] {
            let dialog = app.connect.as_mut().unwrap();
            dialog.auth = AuthMethod::ProfileSignIn;
            let mut profiles = dialog.profiles.clone();
            profiles
                .entries
                .get_mut("sso")
                .unwrap()
                .oidc
                .as_mut()
                .unwrap()
                .flow = flow;
            dialog.profiles = profiles;
            window(&app).snapshot(&app.theme()).expect("renders");
        }
        assert!(shows(
            &app,
            "Reuses a session from `lancedb auth login` (device codes need a terminal)."
        ));
    }

    #[test]
    fn recent_enterprise_connections_are_listed() {
        let mut app = app();
        let target = crate::enterprise::EnterpriseTarget {
            profile: Some("prod".into()),
            rest_url: "https://query.example.com".into(),
            database: "analytics".into(),
            ..crate::enterprise::EnterpriseTarget::default()
        };
        app.settings.remember_enterprise(&target);
        assert!(shows(&app, "RECENT ENTERPRISE"));
        assert!(matches!(
            &click(&app, "prod · analytics")[..],
            [Message::OpenRecentEnterprise(t)] if *t == target
        ));
        render(&mut app, ThemeId::GruvboxDark);
    }

    /// Against a running server (see `db::remote::tests::against_a_running_server`).
    #[test]
    fn server_catalog_renders() {
        let Some((mut app, _server)) = crate::test_support::enterprise_app() else {
            return;
        };
        assert!(shows(&app, "Runs on the server"));
        assert!(shows(&app, "public"), "tables are grouped by namespace");
        // Expand every entry so columns, views and badges all render.
        app.expanded = app.catalog.iter().map(TableInfo::key).collect();
        render(&mut app, ThemeId::JoustDark);
        if let Some(view) = app.catalog.iter().find(|t| t.kind == TableKind::View) {
            assert!(shows(&app, &view.name));
            assert!(shows(&app, "view · a stored query"));
        }
        let picked = click_below(&app, "Preview", 60.0);
        assert!(matches!(&picked[..], [Message::PreviewTable(_)]));

        // Functions, and the freshness of any Function column.
        assert!(shows(&app, "FUNCTIONS"));
        if let Some(function) = app.functions.first().cloned() {
            send(
                &mut app,
                Message::SelectFunction(function.name.clone(), function.version.clone()),
            );
            assert!(
                matches!(&app.function.as_ref().unwrap().detail, Some(Ok(_))),
                "{:?}",
                app.function
            );
            assert!(shows(&app, "SOURCE"));
            render(&mut app, ThemeId::TokyoNight);
            send(&mut app, Message::CloseFunction);
        }
        let with_function_column = app
            .catalog
            .iter()
            .find(|table| {
                table
                    .columns
                    .iter()
                    .any(|column| column.computed == Some(Computed::Function))
            })
            .map(TableInfo::key);
        if let Some(key) = with_function_column {
            app.expanded.remove(&key);
            send(&mut app, Message::ToggleTable(key.clone()));
            assert!(
                matches!(app.column_status.get(&key), Some(Ok(_))),
                "{:?}",
                app.column_status
            );
            render(&mut app, ThemeId::JoustLight);
        }

        // The plan tab explains how to get a server plan, then shows one.
        send(&mut app, Message::LoadQuery("SELECT 1 AS x".into()));
        send(&mut app, Message::Run);
        send(&mut app, Message::SelectTab(ResultTab::Plan));
        assert!(shows(
            &app,
            "The server sends a plan when \"Collect plan\" is on (it runs EXPLAIN ANALYZE, executing the query again)."
        ));
        send(&mut app, Message::SetCollectPlan(true));
        send(&mut app, Message::Run);
        assert!(
            app.outcome.as_ref().unwrap().plan.is_some(),
            "{:?}",
            app.error
        );
        render(&mut app, ThemeId::Nord);

        // The database picker sits in the top bar; Jobs and Dashboards use it.
        assert!(shows(&app, "Database"));
        send(&mut app, Message::ShowScreen(crate::app::Screen::Jobs));
        assert!(
            matches!(&app.jobs.jobs, Some(Ok(jobs)) if !jobs.is_empty()),
            "{:?}",
            app.jobs.jobs
        );
        let first = app.jobs.visible()[0].job_id.clone();
        send(&mut app, Message::SelectJob(first));
        assert!(
            matches!(&app.jobs.detail, Some(Ok(_))),
            "{:?}",
            app.jobs.detail
        );
        assert!(shows(&app, "TIMELINE"));
        render(&mut app, ThemeId::JoustLight);
        send(
            &mut app,
            Message::ShowScreen(crate::app::Screen::Dashboards),
        );
        use crate::enterprise::dashboard::{TILES, TileId};
        assert_eq!(app.dashboard.tiles.len(), TILES.len());
        assert!(
            app.dashboard.tiles[&TileId::TableHealth].is_ok(),
            "{:?}",
            app.dashboard.tiles[&TileId::TableHealth]
        );
        assert_eq!(app.dashboard.pending, 0);
        // The timer refreshes the metering tiles but not fresh table health.
        let health_at = app.dashboard.health_at;
        let generation = app.dashboard.generation;
        send(&mut app, Message::DashboardTick);
        assert_eq!(app.dashboard.generation, generation + 1);
        assert_eq!(
            app.dashboard.health_at, health_at,
            "health was not re-checked"
        );
        render(&mut app, ThemeId::TokyoNight);
    }

    #[test]
    fn computed_columns_and_function_tab_render() {
        use crate::db::remote::{ColumnStatus, FunctionDetail, FunctionInfo};
        use crate::db::{ColumnInfo, Computed};
        use lancedb::arrow::arrow_schema::DataType;

        let (_dir, path) = sample_path();
        let mut app = opened(&path);
        run(&mut app, "SELECT 1 AS x");

        // A server-like catalog entry with computed columns and a status.
        let mut table = app.catalog[0].clone();
        let column = |name: &str, computed| ColumnInfo {
            name: name.into(),
            data_type: DataType::Int64,
            nullable: true,
            computed: Some(computed),
        };
        table.columns.push(column("plus", Computed::Function));
        table
            .columns
            .push(column("doubled", Computed::Expression("x * 2".into())));
        let key = table.key();
        app.catalog[0] = table;
        app.expanded.insert(key.clone());
        app.column_status.insert(
            key.clone(),
            Ok(vec![ColumnStatus {
                column: "plus".into(),
                function: "add_one".into(),
                function_version: "1".into(),
                rows: 50,
                fresh: 48,
                stale: 2,
                ..ColumnStatus::default()
            }]),
        );
        assert!(shows(&app, "48 fresh · 2 stale · 0 failed"));
        assert!(matches!(
            &click(&app, "Refresh")[..],
            [Message::RefreshColumn(_, column)] if column == "plus"
        ));
        assert!(matches!(
            &click(&app, "Errors")[..],
            [Message::ShowColumnErrors(_)]
        ));
        render(&mut app, ThemeId::Nord);
        app.column_status.insert(key.clone(), Ok(Vec::new()));
        assert!(shows(&app, "No refresh status yet"));
        app.column_status.insert(key, Err("denied".into()));
        assert!(shows(&app, "Status unavailable"));

        // The Function tab: loading, failed, loaded.
        let version = |version: &str| FunctionInfo {
            name: "add_one".into(),
            version: version.into(),
            created_at_millis: Some(0),
            python_version: "3.12".into(),
            signature: "(x int64) -> int64".into(),
            ..FunctionInfo::default()
        };
        app.functions = vec![version("2"), version("1")];
        app.function = Some(crate::app::FunctionView {
            name: "add_one".into(),
            version: "2".into(),
            detail: None,
        });
        app.tab = ResultTab::Function;
        assert!(shows(&app, "ƒ add_one"));
        assert!(shows(&app, "Loading the Function…"));
        app.function.as_mut().unwrap().detail = Some(Err("gone".into()));
        assert!(shows(&app, "gone"));
        let detail = FunctionDetail {
            name: "add_one".into(),
            version: "2".into(),
            source: Some("def add_one(x):\n    return x + 1".into()),
            python_version: Some("3.12".into()),
            environment: Some("pip".into()),
            packages: vec!["pyarrow==26.0.1".into()],
            inputs: vec!["x int64 NOT NULL".into()],
            output: "int64".into(),
            image_digest: Some("sha256:0123456789abcdef0123456789".into()),
            disabled: true,
        };
        app.function.as_mut().unwrap().detail = Some(Ok(detail.clone()));
        app.column_status.insert(
            "events".into(),
            Ok(vec![ColumnStatus {
                column: "plus".into(),
                function: "add_one".into(),
                function_version: "2".into(),
                fresh: 7,
                never_computed: 1,
                ..ColumnStatus::default()
            }]),
        );
        assert!(shows(&app, "events.plus"));
        assert!(shows(
            &app,
            "7 fresh · 0 stale · 0 failed · 1 never computed"
        ));
        assert!(shows(&app, "Python 3.12 · pip"));
        assert!(shows(&app, "disabled"));
        assert!(shows(&app, "created 1970-01-01 00:00 UTC"));
        assert!(matches!(
            &click(&app, "New version…")[..],
            [Message::EditFunctionTemplate]
        ));
        assert!(matches!(
            &click(&app, "Drop…")[..],
            [Message::DropFunctionTemplate]
        ));
        assert!(matches!(
            &click(&app, "Close")[..],
            [Message::CloseFunction]
        ));
        assert!(matches!(
            &click(&app, "v1")[..],
            [Message::SelectFunction(name, v)] if name == "add_one" && v == "1"
        ));
        render(&mut app, ThemeId::Dracula);
        // Without packages, image or source the facts still read well.
        app.function.as_mut().unwrap().detail = Some(Ok(FunctionDetail {
            source: None,
            image_digest: None,
            packages: Vec::new(),
            python_version: None,
            environment: None,
            ..detail
        }));
        assert!(shows(&app, "The server sent no source for this version."));
        assert!(shows(&app, "not built"));
        // Expanded tables, none computed by this Function; then none loaded.
        let statuses = std::mem::take(&mut app.column_status);
        app.column_status.insert("events".into(), Ok(Vec::new()));
        assert!(shows(
            &app,
            "None of the expanded tables has a column computed by it."
        ));
        app.column_status.clear();
        assert!(shows(
            &app,
            "Expand a table in the sidebar to see the Function columns it has."
        ));
        app.column_status = statuses;

        // A paged result offers its next page.
        let mut outcome = (*app.outcome.clone().unwrap()).clone();
        outcome.continuation = Some("SHOW TABLES LIMIT 1000 FROM 'x'".into());
        app.outcome = Some(std::sync::Arc::new(outcome));
        app.tab = ResultTab::Rows;
        assert!(matches!(
            &click(&app, "Next page")[..],
            [Message::RunExample(sql)] if sql.contains("FROM 'x'")
        ));
    }

    fn sample_jobs() -> Vec<crate::db::remote::JobRow> {
        use crate::db::remote::JobRow;
        let job = |id: &str, job_type: &str, state: &str| JobRow {
            job_id: id.into(),
            job_type: job_type.into(),
            table: "images".into(),
            state: state.into(),
            created_at_millis: Some(1_795_554_000_000),
            ..JobRow::default()
        };
        vec![
            JobRow {
                rows_planned: Some(2_000),
                rows_computed: Some(1_240),
                active_workers: Some(2),
                workers_desired: Some(4),
                phase: "compute".into(),
                function: "image_embedding v2".into(),
                ..job("j1-running", "function_refresh", "running")
            },
            JobRow {
                queue_reason: "waiting for a worker".into(),
                ..job("j1-queued", "reindex_ivf_pq", "queued")
            },
            JobRow {
                paused: true,
                rows_computed: Some(10),
                ..job("j1-paused", "compaction", "running")
            },
            JobRow {
                failure_message: "worker ran out of memory".into(),
                completed_at_millis: Some(1_795_554_060_000),
                ..job("j1-failed", "reindex_fts", "failed")
            },
            JobRow {
                duration_millis: Some(7_500_000),
                ..job("j1-done", "reindex_btree", "succeeded")
            },
        ]
    }

    #[test]
    fn jobs_screen_renders_and_responds() {
        use crate::app::{JobFilter, Screen};
        use crate::db::remote::{JobAction, JobDetail, JobEvent, JobPhase};

        let mut app = app();
        app.screen = Screen::Jobs;
        assert!(shows(&app, "Loading jobs…"));
        app.jobs.jobs = Some(Err("permission denied".into()));
        assert!(shows(&app, "permission denied"));
        app.jobs.jobs = Some(Ok(Vec::new()));
        assert!(shows(
            &app,
            "No jobs yet. Building an index, compacting a table, or refreshing a materialized \
             view or Function column starts one."
        ));

        app.jobs.jobs = Some(Ok(sample_jobs()));
        assert!(shows(&app, "62% · 1,240 / 2,000 rows"));
        assert!(shows(&app, "2/4 workers"));
        assert!(shows(&app, "waiting for a worker"));
        assert!(shows(&app, "ƒ image_embedding v2"));
        assert!(shows(&app, "paused"));
        assert!(shows(&app, "2h 5m"));
        assert!(matches!(
            &click(&app, "Failed")[..],
            [Message::SetJobFilter(JobFilter::Failed)]
        ));
        assert!(matches!(
            &click(&app, "reindex_fts")[..],
            [Message::SelectJob(id)] if id == "j1-failed"
        ));
        render(&mut app, ThemeId::JoustLight);

        send(&mut app, Message::SetJobFilter(JobFilter::Active));
        assert!(
            !shows(&app, "reindex_btree"),
            "finished jobs are filtered out"
        );
        send(&mut app, Message::SearchJobs("nothing like it".into()));
        assert!(shows(&app, "No jobs match."));
        send(&mut app, Message::SearchJobs(String::new()));

        // The running job: actions, then the detail as it loads.
        app.jobs.selected = Some("j1-running".into());
        assert!(shows(&app, "Loading the job's timeline and events…"));
        assert!(matches!(
            &click(&app, "Pause")[..],
            [Message::RunJobAction(id, JobAction::Pause)] if id == "j1-running"
        ));
        assert!(matches!(
            &click(&app, "Cancel…")[..],
            [Message::RunJobAction(id, JobAction::Cancel)] if id == "j1-running"
        ));
        app.jobs.confirm_cancel = Some("j1-running".into());
        assert!(shows(&app, "Confirm cancel"));
        assert!(shows(&app, "Work in progress is discarded."));
        render_hovering(&app, "Confirm cancel");
        app.jobs.detail = Some(Err("no such job".into()));
        assert!(shows(&app, "no such job"));
        app.jobs.detail = Some(Ok(JobDetail {
            job_id: "j1-running".into(),
            state: "running".into(),
            total_millis: Some(400),
            facts: vec![("rows processed".into(), "1240".into())],
            timeline: vec![
                JobPhase {
                    name: "queued".into(),
                    at_millis: 0,
                    millis: 300,
                },
                JobPhase {
                    name: "compute".into(),
                    at_millis: 300,
                    millis: 100,
                },
            ],
            events: (0..60)
                .map(|seq| JobEvent {
                    seq,
                    state: "in_progress".into(),
                    at_millis: Some(1_795_554_000_000),
                    by: "lance-agent:0".into(),
                    rows: Some(seq * 10),
                })
                .rev()
                .collect(),
            failure: None,
        }));
        assert!(shows(&app, "TIMELINE"));
        assert!(shows(&app, "rows processed"));
        assert!(shows(&app, "10 older events not shown"));
        render(&mut app, ThemeId::TokyoNight);
        if let Some(Ok(detail)) = &mut app.jobs.detail {
            detail.timeline.clear();
            detail.events.clear();
        }
        assert!(shows(&app, "No timeline yet."));
        assert!(shows(&app, "No events recorded."));

        // Paused jobs resume; failed jobs show why and offer no actions.
        app.jobs.filter = JobFilter::All;
        app.jobs.selected = Some("j1-paused".into());
        app.jobs.confirm_cancel = None;
        assert!(matches!(
            &click(&app, "Resume")[..],
            [Message::RunJobAction(id, JobAction::Resume)] if id == "j1-paused"
        ));
        app.jobs.selected = Some("j1-failed".into());
        assert!(shows(&app, "FAILURE"));
        assert!(!shows(&app, "Cancel…"));
        assert!(matches!(&click(&app, "Close")[..], [Message::CloseJob]));
        render(&mut app, ThemeId::SolarizedLight);

        // A job the list no longer has still shows its detail.
        app.jobs.selected = Some("j1-elsewhere".into());
        assert!(shows(&app, "j1-elsewhere"));
    }

    #[test]
    fn dashboards_screen_renders_every_kind_of_tile() {
        use crate::app::Screen;
        use crate::enterprise::dashboard::{Bars, Range, TileContent, TileId};

        let mut app = app();
        app.screen = Screen::Dashboards;
        assert!(shows(&app, "Loading…"));
        assert!(shows(&app, "Tables"));
        assert!(matches!(
            &click(&app, "Last 30 days")[..],
            [Message::SetDashboardRange(Range::Month)]
        ));
        assert!(matches!(
            &click(&app, "Refresh")[..],
            [Message::RefreshDashboard]
        ));
        assert!(matches!(
            &click(&app, "Open in editor")[..],
            [Message::OpenTileQuery(_)]
        ));

        let tiles = &mut app.dashboard.tiles;
        tiles.insert(
            TileId::Tables,
            Ok(TileContent::Kpi {
                value: "12".into(),
                note: "3 views".into(),
            }),
        );
        tiles.insert(TileId::Jobs, Ok(TileContent::Empty));
        tiles.insert(
            TileId::Storage,
            Err("table not found: storage_metering".into()),
        );
        let series = ["image_embedding", "reindex_ivf_pq", "other"];
        tiles.insert(
            TileId::ServiceCpu,
            Ok(TileContent::Bars(Bars {
                x: vec!["Nov 22".into(), "Nov 23".into(), "Nov 24".into()],
                series: series.iter().map(|s| s.to_string()).collect(),
                values: vec![
                    vec![1.0, 2.0, 3.0],
                    vec![0.5, 0.0, 1.5],
                    vec![0.1, 0.2, 0.0],
                ],
                unit: "CPU-h",
            })),
        );
        let table = crate::test_support::numbers_table(25);
        tiles.insert(TileId::SlowStatements, Ok(TileContent::Table(table)));
        app.dashboard.loaded_at = Some(1_795_554_564_300);
        assert!(shows(&app, "12"));
        assert!(shows(&app, "No jobs finished in this range"));
        assert!(shows(&app, "table not found: storage_metering"));
        assert!(shows(&app, "image_embedding"));
        assert!(shows(&app, "6 CPU-h"), "series totals in the legend");
        assert!(shows(&app, "5 more rows. Open in editor to see them all."));
        assert!(shows(
            &app,
            "Updated 2026-11-24 21:09 UTC · refreshes every minute"
        ));
        render(&mut app, ThemeId::JoustLight);
        render(&mut app, ThemeId::CatppuccinMocha);

        // Table health: every checked table, each row opening its stats.
        use crate::enterprise::dashboard::{TableHealth, health_table};
        let health = |name: &str| TableHealth {
            table: name.into(),
            rows: 10,
            fragments: 8,
            small_fragments: 6,
            ..TableHealth::default()
        };
        app.dashboard.tiles.insert(
            TileId::TableHealth,
            Ok(health_table(
                &(0..25)
                    .map(|i| health(&format!("t{i:02}")))
                    .collect::<Vec<_>>(),
            )),
        );
        assert!(shows(&app, "t24"), "no 20-row cap for table health");
        assert!(shows(&app, "Click a table to run SHOW STATS for it."));
        assert!(matches!(
            &click(&app, "t03")[..],
            [Message::OpenTableStats(key)] if key == "t03"
        ));
        app.catalog = (0..101)
            .map(|i| TableInfo {
                namespace: Vec::new(),
                name: format!("t{i:02}"),
                kind: TableKind::Table,
                columns: Vec::new(),
                num_rows: 0,
                version: 1,
                size_bytes: 0,
                indices: Vec::new(),
            })
            .collect();
        assert!(shows(
            &app,
            "Checked the first 100 of 101 tables. Click a table to run SHOW STATS for it."
        ));
        // The tile's own Open in editor (beside its title, in the left
        // column) runs the first table's.
        let title = window(&app).find("Table health").unwrap().bounds();
        let mut ui = window(&app);
        let mut label = "Open in editor";
        ui.click(move |candidate: Candidate<'_>| {
            label.select(candidate).filter(|found| {
                let bounds = found.bounds();
                bounds.x < 640.0 && (bounds.center().y - title.center().y).abs() < 12.0
            })
        })
        .expect("the health tile's Open in editor");
        assert!(matches!(
            &ui.into_messages().collect::<Vec<_>>()[..],
            [Message::OpenTableStats(key)] if key == "t00"
        ));
        render(&mut app, ThemeId::Nord);
    }

    #[test]
    fn screen_switch_and_open_job_need_a_server() {
        use crate::app::Screen;

        let (_dir, path) = sample_path();
        let mut app = opened(&path);
        assert!(!shows(&app, "Dashboards"), "no switch on a local database");
        run(&mut app, "SELECT 'j1-abc' AS job_id");
        assert!(!shows(&app, "Open job"), "jobs live on servers");
        send(&mut app, Message::ShowScreen(Screen::Jobs));
        assert_eq!(app.screen, Screen::Workspace);
    }
}
