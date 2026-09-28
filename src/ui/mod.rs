//! View code: layout of the main window and its panes.

pub mod chart;
pub mod columns;
pub mod gallery;
pub mod grid;
pub mod plan_view;

use iced::widget::canvas::{self, Frame, Geometry, Path, Stroke};
use iced::widget::{
    Space, button, canvas as canvas_widget, center, column, container, hover, pane_grid, pick_list,
    row, scrollable, text, text_editor, text_input, tooltip,
};
use iced::{
    Alignment, Element, Fill, Font, Length, Point, Rectangle, Renderer, Shrink, Theme, font,
    keyboard, mouse,
};

use crate::app::{App, Message, PaneKind, PlanMode, ResultTab, RowLimit, SAMPLE_EXAMPLES};
use crate::db::{IndexKind, TableInfo, quote_ident};
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

    let body = container(panes)
        .width(Fill)
        .height(Fill)
        .style(|theme: &Theme| container::Style {
            // The 1px pane spacing shows this colour as dividers.
            background: Some(theme::divider(theme).into()),
            ..container::Style::default()
        });

    container(column![
        top_bar(app),
        hairline(),
        body,
        hairline(),
        status_bar(app)
    ])
    .width(Fill)
    .height(Fill)
    .style(theme::app)
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
    let logo = row![
        canvas_widget(LanceMark).width(22).height(22),
        text("joust").size(18).font(BOLD),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let path = text_input("Path to a LanceDB directory…", &app.path_input)
        .on_input(Message::PathChanged)
        .on_submit(Message::OpenDatabase)
        .padding([6, 10])
        .size(13)
        .width(Length::FillPortion(3))
        .style(theme::input);

    let busy = app.busy.is_some();
    let open = button(text("Open").size(13))
        .padding([6, 12])
        .on_press_maybe((!busy).then_some(Message::OpenDatabase))
        .style(theme::secondary_button);
    let browse = button(text("Browse…").size(13))
        .padding([6, 12])
        .on_press_maybe((!busy).then_some(Message::BrowseDatabase))
        .style(theme::secondary_button);
    let sample = with_tooltip(
        button(text("Sample database").size(13))
            .padding([6, 12])
            .on_press_maybe((!busy).then_some(Message::OpenSample))
            .style(theme::secondary_button),
        "Create (or recreate) a demo database: movies with posters, events, and (with ffmpeg) video trailers",
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
            logo,
            Space::new().width(12),
            path,
            open,
            browse,
            sample,
            import,
            Space::new().width(Length::FillPortion(1)),
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

fn status_bar(app: &App) -> Element<'_, Message> {
    let connection: Element<'_, Message> = match (&app.database, &app.busy) {
        (_, Some(busy)) => row![status_dot(DotKind::Busy), text(busy.clone()).size(12)]
            .spacing(6)
            .align_y(Alignment::Center)
            .into(),
        (Some(db), None) => row![
            status_dot(DotKind::Ok),
            text(db.uri().to_string()).size(12),
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
        text(format!("Running… {}", human_duration(elapsed)))
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

    container(
        row![
            connection,
            Space::new().width(Fill),
            query,
            faint(format!("DataFusion SQL · LanceDB {}", LANCEDB_VERSION)).size(12),
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
const LANCEDB_VERSION: &str = "0.39";

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

    let header = row![
        section_label("Database"),
        Space::new().width(Fill),
        with_tooltip(
            button(text("↻").size(14))
                .padding([0, 6])
                .on_press_maybe(app.database.as_ref().map(|_| Message::RefreshCatalog))
                .style(theme::ghost_button),
            "Refresh tables",
        ),
    ]
    .align_y(Alignment::Center);
    content = content.push(header);

    if app.database.is_none() {
        content = content.push(Space::new().height(4));
        content = content.push(muted("No database open.").size(13));
        if !app.settings.recent_databases.is_empty() {
            content = content.push(Space::new().height(8));
            content = content.push(section_label("Recent"));
            for recent in &app.settings.recent_databases {
                content = content.push(
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
    } else if app.catalog.is_empty() {
        content = content.push(muted("This database has no tables yet.").size(13));
    }

    for table in &app.catalog {
        content = content.push(table_entry(app, table));
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

fn table_entry<'a>(app: &'a App, table: &'a TableInfo) -> Element<'a, Message> {
    let expanded = app.expanded.contains(&table.name);
    let caret = if expanded { "▾" } else { "▸" };
    let title = button(
        row![
            faint(caret).size(12).width(12),
            text(&table.name).size(13).font(BOLD),
            Space::new().width(Fill),
            faint(thousands(table.num_rows))
                .size(11)
                .font(Font::MONOSPACE),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .width(Fill)
    .padding([5, 6])
    .on_press(Message::ToggleTable(table.name.clone()))
    .style(theme::list_item(false));

    if !expanded {
        return title.into();
    }

    let mut details = column![].spacing(1).padding(iced::Padding {
        left: 18.0,
        ..iced::Padding::ZERO
    });

    let meta = format!(
        "v{} · {} · {} index{}",
        table.version,
        human_bytes(table.size_bytes),
        table.indices.len(),
        if table.indices.len() == 1 { "" } else { "es" }
    );
    details = details.push(
        row![
            faint(meta).size(11),
            Space::new().width(Fill),
            button(text("Preview").size(11))
                .padding([2, 8])
                .on_press(Message::PreviewTable(table.name.clone()))
                .style(theme::secondary_button),
        ]
        .align_y(Alignment::Center)
        .spacing(6),
    );

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
                        table.name.clone(),
                        column.name.clone(),
                        kind,
                    ))
                    .style(theme::secondary_button),
                tip,
            );
            hover(
                base,
                container(
                    row![
                        Space::new().width(Fill),
                        action,
                        Space::new().width(TYPE_SLOT + 4.0)
                    ]
                    .align_y(Alignment::Center),
                )
                .height(Fill)
                .align_y(Alignment::Center),
            )
        };
        details = details.push(entry);
    }

    column![title, details].spacing(2).into()
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

    let toolbar = row![
        run_or_cancel,
        faint(hint).size(12),
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
    let tabs = tabs
        .push(tab_button("Plan", ResultTab::Plan, app.tab))
        .push(tab_button("Chart", ResultTab::Chart, app.tab));

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

    let body: Element<'_, Message> = if app.database.is_none() {
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
        }
    };

    let mut layout = column![header, hairline()];
    if let Some(banner) = banners(app) {
        layout = layout.push(banner);
    }
    layout.push(body).into()
}

fn tab_button<'a>(label: &'a str, tab: ResultTab, current: ResultTab) -> Element<'a, Message> {
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
        muted("Enter a directory path above, browse for one, or start with the sample database (movies with vector embeddings and poster images, 50k analytics events, and short video trailers when ffmpeg is installed).").size(14),
        row![
            button(text("Create sample database").size(14).font(BOLD))
                .padding([8, 16])
                .on_press_maybe(app.busy.is_none().then_some(Message::OpenSample))
                .style(theme::primary_button),
            button(text("Browse…").size(14))
                .padding([8, 16])
                .on_press_maybe(app.busy.is_none().then_some(Message::BrowseDatabase))
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
                .on_press(Message::PreviewTable(first.name.clone()))
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
        return center(muted("This statement has no execution plan.").size(14)).into();
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

// ---------------------------------------------------------------------------
// Logo
// ---------------------------------------------------------------------------

/// A tiny jousting lance, drawn in the theme's accent colour.
struct LanceMark;

impl canvas::Program<Message> for LanceMark {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let accent = theme.extended_palette().primary.base.color;
        let ink = theme.extended_palette().background.base.text;
        let mut frame = Frame::new(renderer, bounds.size());
        let s = bounds.width;
        // Shaft: a long, thin triangle from the grip to the tip.
        let shaft = Path::new(|b| {
            b.move_to(Point::new(s * 0.08, s * 0.80));
            b.line_to(Point::new(s * 0.94, s * 0.06));
            b.line_to(Point::new(s * 0.22, s * 0.94));
            b.close();
        });
        frame.fill(&shaft, accent);
        // Vamplate (hand guard).
        frame.stroke(
            &Path::circle(Point::new(s * 0.27, s * 0.73), s * 0.17),
            Stroke::default().with_width(2.0).with_color(ink),
        );
        vec![frame.into_geometry()]
    }
}
