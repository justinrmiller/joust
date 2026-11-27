//! The Jobs screen: a LanceDB Enterprise server's background jobs (index
//! builds, compactions, materialized view and Function refreshes), with
//! pause, resume and cancel.

use iced::widget::{
    Space, button, center, column, container, progress_bar, row, scrollable, text, text_input,
};
use iced::{Alignment, Color, Element, Fill, Font, Theme};

use super::function::utc_minute;
use super::{BOLD, faint, muted, section_label, thin_scrollbar, with_tooltip};
use crate::app::{App, JobFilter, JobsView, MAX_LISTED_JOBS, Message, now_millis};
use crate::db::remote::{JobAction, JobDetail, JobRow};
use crate::results::thousands;
use crate::theme;

/// Width of the filter rail and of the detail panel.
const RAIL_WIDTH: f32 = 170.0;
const DETAIL_WIDTH: f32 = 420.0;
/// Width of a fact's label in the detail panel.
const LABEL_WIDTH: f32 = 110.0;
/// Room on the right of scrolled content for the scrollbar.
const SCROLLBAR_GUTTER: iced::Padding = iced::Padding {
    top: 0.0,
    right: 12.0,
    bottom: 0.0,
    left: 0.0,
};
/// Events the detail panel lists (newest first).
const MAX_EVENTS: usize = 50;

pub fn view(app: &App) -> Element<'_, Message> {
    let jobs = &app.jobs;
    let now = now_millis();
    let mut body = row![
        container(filter_rail(jobs)).width(RAIL_WIDTH),
        job_list(jobs, now),
    ]
    .spacing(16)
    .height(Fill);
    if let Some(job_id) = &jobs.selected {
        body = body.push(
            container(detail(jobs, job_id, jobs.selected_job(), now))
                .width(DETAIL_WIDTH)
                .height(Fill),
        );
    }
    // Workspace errors and notices stay on the SQL screen; this screen
    // shows its own (loading errors in the list, action results in the
    // detail).
    container(body).padding(16).width(Fill).height(Fill).into()
}

fn filter_rail(jobs: &JobsView) -> Element<'_, Message> {
    let mut rail = column![section_label("Jobs"), Space::new().height(4)].spacing(2);
    for filter in JobFilter::ALL {
        rail = rail.push(
            button(
                row![
                    text(filter.label()).size(13),
                    Space::new().width(Fill),
                    faint(thousands(jobs.count(filter))).size(12),
                ]
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .padding([5, 8])
            .on_press(Message::SetJobFilter(filter))
            .style(theme::list_item(jobs.filter == filter)),
        );
    }
    rail.push(Space::new().height(8))
        .push(faint("Refreshes every few seconds while this screen shows.").size(11))
        .into()
}

fn job_list(jobs: &JobsView, now: i64) -> Element<'_, Message> {
    let search = text_input("Search by job id, type, table or Function", &jobs.search)
        .on_input(Message::SearchJobs)
        .padding([6, 10])
        .size(13)
        .style(theme::input);
    let refresh = button(
        text(if jobs.loading {
            "Refreshing…"
        } else {
            "Refresh"
        })
        .size(12),
    )
    .padding([6, 12])
    .on_press_maybe((!jobs.loading).then_some(Message::RefreshJobs))
    .style(theme::secondary_button);
    let toolbar = row![search, refresh].spacing(8).align_y(Alignment::Center);

    let body: Element<'_, Message> = match &jobs.jobs {
        None => center(muted("Loading jobs…").size(13)).into(),
        Some(Err(error)) => error_box(error),
        Some(Ok(all)) if all.is_empty() => center(
            muted(
                "No jobs yet. Building an index, compacting a table, or refreshing a \
                 materialized view or Function column starts one.",
            )
            .size(13),
        )
        .into(),
        Some(Ok(_)) => {
            let visible = jobs.visible();
            if visible.is_empty() {
                center(muted("No jobs match.").size(13)).into()
            } else {
                let shown = visible.len();
                let mut list = column![].spacing(2);
                for job in visible {
                    let selected = jobs.selected.as_deref() == Some(job.job_id.as_str());
                    list = list.push(job_row(job, selected, now));
                }
                if shown == MAX_LISTED_JOBS {
                    list = list.push(
                        faint(format!(
                            "Showing the newest {MAX_LISTED_JOBS}. Search to narrow the list."
                        ))
                        .size(11),
                    );
                }
                scrollable(list.padding(SCROLLBAR_GUTTER))
                    .direction(thin_scrollbar())
                    .height(Fill)
                    .into()
            }
        }
    };
    column![toolbar, body]
        .spacing(10)
        .width(Fill)
        .height(Fill)
        .into()
}

/// Two lines: what the job is, then how far it has got.
fn job_row(job: &JobRow, selected: bool, now: i64) -> Element<'_, Message> {
    let what = row![
        state_pill(job),
        text(job.job_type.as_str()).size(13).font(BOLD),
        text(job.table.as_str()).size(12).font(Font::MONOSPACE),
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    let function = if job.function.is_empty() {
        String::new()
    } else {
        format!("ƒ {}", job.function)
    };
    let what = what
        .push(
            container(faint(function).size(11).wrapping(text::Wrapping::None))
                .width(Fill)
                .clip(true),
        )
        .push(
            faint(job.created_at_millis.map(utc_minute).unwrap_or_default())
                .size(11)
                .wrapping(text::Wrapping::None),
        );

    let mut progress = row![].spacing(10).align_y(Alignment::Center);
    if let Some(bar) = progress_of(job) {
        progress = progress.push(bar);
    }
    if let Some(workers) = workers(job) {
        progress = progress.push(faint(workers).size(11));
    }
    // Finished jobs show their state in the pill; their phase adds nothing.
    let note = if !job.queue_reason.is_empty() {
        job.queue_reason.as_str()
    } else if job.is_active() {
        job.phase.as_str()
    } else {
        ""
    };
    if !note.is_empty() {
        progress = progress.push(faint(note).size(11));
    }
    let progress = progress
        .push(Space::new().width(Fill))
        .push(faint(elapsed(job, now).unwrap_or_default()).size(11));

    button(column![what, progress].spacing(4))
        .width(Fill)
        .padding([6, 10])
        .on_press(Message::SelectJob(job.job_id.clone()))
        .style(theme::list_item(selected))
        .into()
}

/// A bar and `62% · 1,240 / 2,000 rows`, or the rows computed so far.
fn progress_of(job: &JobRow) -> Option<Element<'_, Message>> {
    match (job.fraction(), job.rows_computed) {
        (Some(fraction), _) => Some(
            row![
                progress_bar(0.0..=1.0, fraction as f32)
                    .length(90)
                    .girth(5)
                    .style(bar_style),
                faint(format!(
                    "{:.0}% · {} / {} rows",
                    fraction * 100.0,
                    thousands(job.rows_computed.unwrap_or(0) as usize),
                    thousands(job.rows_planned.unwrap_or(0) as usize)
                ))
                .size(11),
            ]
            .spacing(6)
            .align_y(Alignment::Center)
            .into(),
        ),
        (None, Some(rows)) => Some(
            faint(format!("{} rows", thousands(rows as usize)))
                .size(11)
                .into(),
        ),
        (None, None) => None,
    }
}

fn bar_style(theme: &Theme) -> progress_bar::Style {
    progress_bar::Style {
        background: theme::divider(theme).into(),
        bar: theme.extended_palette().primary.base.color.into(),
        border: iced::Border {
            radius: 3.0.into(),
            ..iced::Border::default()
        },
    }
}

/// `2/4 workers` while the job runs.
fn workers(job: &JobRow) -> Option<String> {
    if !job.is_active() {
        return None;
    }
    match (job.active_workers, job.workers_desired) {
        (Some(active), Some(desired)) if desired > 0 => Some(format!("{active}/{desired} workers")),
        (Some(active), _) => Some(format!(
            "{active} worker{}",
            if active == 1 { "" } else { "s" }
        )),
        _ => None,
    }
}

/// How long the job ran, or has been running.
fn elapsed(job: &JobRow, now: i64) -> Option<String> {
    let millis = job.duration_millis.or_else(|| {
        let created = job.created_at_millis?;
        match job.completed_at_millis {
            Some(completed) => Some(completed - created),
            None if job.is_active() => Some(now - created),
            None => None,
        }
    })?;
    Some(job_duration(millis.max(0) as u64))
}

/// The job's state as a dot and a word (never colour alone).
fn state_pill(job: &JobRow) -> Element<'_, Message> {
    let label = if job.paused {
        "paused"
    } else {
        job.state.as_str()
    };
    let kind = state_kind(&job.state, job.paused);
    container(
        row![
            container(Space::new().width(7).height(7)).style(move |theme: &Theme| {
                container::Style {
                    background: Some(state_color(theme, kind).into()),
                    border: iced::Border {
                        radius: 4.0.into(),
                        ..iced::Border::default()
                    },
                    ..container::Style::default()
                }
            }),
            text(label).size(11),
        ]
        .spacing(5)
        .align_y(Alignment::Center),
    )
    .padding([1, 7])
    .style(theme::badge)
    .into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateKind {
    Running,
    Waiting,
    Succeeded,
    Failed,
    Stopped,
}

fn state_kind(state: &str, paused: bool) -> StateKind {
    if paused {
        return StateKind::Waiting;
    }
    match state {
        "succeeded" | "done" => StateKind::Succeeded,
        "failed" | "timed_out" => StateKind::Failed,
        "cancelled" | "canceled" => StateKind::Stopped,
        "queued" | "pending" | "waiting" | "paused" => StateKind::Waiting,
        _ => StateKind::Running,
    }
}

fn state_color(theme: &Theme, kind: StateKind) -> Color {
    let palette = theme.extended_palette();
    match kind {
        StateKind::Running => palette.primary.base.color,
        StateKind::Waiting => palette.warning.base.color,
        StateKind::Succeeded => palette.success.base.color,
        StateKind::Failed => palette.danger.base.color,
        StateKind::Stopped => theme::faint(theme),
    }
}

/// The selected job: actions, facts, failure, timeline and events.
fn detail<'a>(
    jobs: &'a JobsView,
    job_id: &'a str,
    job: Option<&'a JobRow>,
    now: i64,
) -> Element<'a, Message> {
    let mut header = row![text("Job").size(15).font(BOLD)]
        .spacing(8)
        .align_y(Alignment::Center);
    if let Some(job) = job {
        header = header.push(state_pill(job));
    }
    let header = header.push(Space::new().width(Fill)).push(
        button(text("Close").size(12))
            .padding([4, 10])
            .on_press(Message::CloseJob)
            .style(theme::secondary_button),
    );

    let mut panel = column![
        header,
        text(job_id)
            .size(11)
            .font(Font::MONOSPACE)
            .wrapping(text::Wrapping::Glyph),
    ]
    .spacing(8);
    if let Some(actions) = job.and_then(|job| actions(jobs, job)) {
        panel = panel.push(actions);
    }
    match &jobs.action {
        Some(Ok(note)) => panel = panel.push(muted(note.as_str()).size(12)),
        Some(Err(error)) => panel = panel.push(error_box(error)),
        None => {}
    }

    let mut facts: Vec<(String, String)> = Vec::new();
    if let Some(job) = job {
        facts.extend(row_facts(job, now));
    }
    let loaded = match &jobs.detail {
        Some(Ok(detail)) => Some(detail),
        _ => None,
    };
    if let Some(detail) = loaded {
        // What the jobs list already said (rows, workers) is not repeated.
        for (label, value) in &detail.facts {
            if !facts.iter().any(|(known, _)| known == label) {
                facts.push((label.clone(), value.clone()));
            }
        }
    }
    if !facts.is_empty() {
        panel = panel.push(section_label("Details"));
        for (label, value) in facts {
            panel = panel.push(fact(label, value));
        }
    }

    let failure = loaded
        .and_then(|detail| detail.failure.as_deref())
        .or(job.map(|job| job.failure_message.as_str()))
        .filter(|failure| !failure.is_empty());
    if let Some(failure) = failure {
        panel = panel
            .push(section_label("Failure"))
            .push(error_box(failure));
    }

    match &jobs.detail {
        None => panel = panel.push(faint("Loading the job's timeline and events…").size(12)),
        Some(Err(error)) => panel = panel.push(error_box(error)),
        Some(Ok(detail)) => {
            panel = panel.push(section_label("Timeline"));
            panel = match timeline(detail) {
                Some(timeline) => panel.push(timeline),
                None => panel.push(faint("No timeline yet.").size(12)),
            };
            panel = panel.push(section_label("Events")).push(events(detail));
        }
    }

    container(
        scrollable(panel.padding(SCROLLBAR_GUTTER))
            .direction(thin_scrollbar())
            .height(Fill),
    )
    .padding(12)
    .height(Fill)
    .style(theme::card)
    .into()
}

/// Pause or resume, and cancel (which needs a second click).
fn actions<'a>(jobs: &'a JobsView, job: &'a JobRow) -> Option<Element<'a, Message>> {
    if !job.is_active() {
        return None;
    }
    let id = || job.job_id.clone();
    let toggle = if job.paused {
        button(text("Resume").size(12))
            .padding([5, 12])
            .on_press(Message::RunJobAction(id(), JobAction::Resume))
            .style(theme::secondary_button)
    } else {
        button(text("Pause").size(12))
            .padding([5, 12])
            .on_press(Message::RunJobAction(id(), JobAction::Pause))
            .style(theme::secondary_button)
    };
    let confirming = jobs.confirm_cancel.as_deref() == Some(job.job_id.as_str());
    let cancel = button(
        text(if confirming {
            "Confirm cancel"
        } else {
            "Cancel…"
        })
        .size(12),
    )
    .padding([5, 12])
    .on_press(Message::RunJobAction(id(), JobAction::Cancel))
    .style(if confirming {
        danger_button
    } else {
        theme::secondary_button
    });
    let mut actions = row![
        with_tooltip(toggle, "Workers stop picking up new work while paused"),
        cancel
    ]
    .spacing(8)
    .align_y(Alignment::Center);
    if confirming {
        actions = actions.push(faint("Work in progress is discarded.").size(11));
    }
    Some(actions.into())
}

fn danger_button(theme: &Theme, status: button::Status) -> button::Style {
    let danger = theme.extended_palette().danger;
    button::Style {
        background: Some(
            match status {
                button::Status::Hovered | button::Status::Pressed => danger.strong.color,
                _ => danger.base.color,
            }
            .into(),
        ),
        text_color: danger.base.text,
        border: iced::Border {
            radius: 5.0.into(),
            ..iced::Border::default()
        },
        ..button::Style::default()
    }
}

/// What the jobs list knows about a job.
fn row_facts(job: &JobRow, now: i64) -> Vec<(String, String)> {
    let mut facts = vec![
        ("type".to_string(), job.job_type.clone()),
        ("table".to_string(), job.table.clone()),
    ];
    if !job.function.is_empty() {
        facts.push(("Function".to_string(), job.function.clone()));
    }
    if let Some(created) = job.created_at_millis {
        facts.push(("created".to_string(), utc_minute(created)));
    }
    if let Some(completed) = job.completed_at_millis {
        facts.push(("finished".to_string(), utc_minute(completed)));
    }
    if let Some(elapsed) = elapsed(job, now) {
        facts.push(("duration".to_string(), elapsed));
    }
    if !job.phase.is_empty() {
        facts.push(("phase".to_string(), job.phase.clone()));
    }
    if !job.queue_reason.is_empty() {
        facts.push(("waiting for".to_string(), job.queue_reason.clone()));
    }
    let rows = [
        ("planned", job.rows_planned),
        ("computed", job.rows_computed),
        ("failed", job.rows_failed),
    ]
    .into_iter()
    .filter_map(|(label, rows)| Some(format!("{} {label}", thousands(rows? as usize))))
    .collect::<Vec<_>>();
    if !rows.is_empty() {
        facts.push(("rows".to_string(), rows.join(" · ")));
    }
    if let Some(workers) = workers(job) {
        facts.push(("workers".to_string(), workers));
    }
    facts
}

fn fact<'a>(label: String, value: String) -> Element<'a, Message> {
    row![
        container(muted(label).size(12)).width(LABEL_WIDTH),
        text(value)
            .size(12)
            .wrapping(text::Wrapping::WordOrGlyph)
            .width(Fill),
    ]
    .spacing(8)
    .into()
}

/// Each phase with a bar for its share of the job's time.
fn timeline(detail: &JobDetail) -> Option<Element<'_, Message>> {
    if detail.timeline.is_empty() {
        return None;
    }
    let total = detail
        .total_millis
        .unwrap_or_else(|| detail.timeline.iter().map(|phase| phase.millis).sum())
        .max(1);
    let mut phases = column![].spacing(4);
    for phase in &detail.timeline {
        phases = phases.push(
            row![
                container(text(phase.name.as_str()).size(12)).width(LABEL_WIDTH),
                progress_bar(0.0..=1.0, phase.millis as f32 / total as f32)
                    .length(Fill)
                    .girth(6)
                    .style(bar_style),
                container(faint(job_duration(phase.millis)).size(11))
                    .width(64)
                    .align_x(iced::alignment::Horizontal::Right),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        );
    }
    Some(phases.into())
}

/// The job's events, newest first.
fn events(detail: &JobDetail) -> Element<'_, Message> {
    if detail.events.is_empty() {
        return faint("No events recorded.").size(12).into();
    }
    let mut list = column![].spacing(3);
    for event in detail.events.iter().take(MAX_EVENTS) {
        let mut line = row![
            faint(format!("#{}", event.seq))
                .size(11)
                .font(Font::MONOSPACE),
            text(event.state.as_str()).size(12),
        ]
        .spacing(8)
        .align_y(Alignment::Center);
        if let Some(rows) = event.rows {
            line = line.push(faint(format!("{} rows", thousands(rows as usize))).size(11));
        }
        line = line
            .push(Space::new().width(Fill))
            .push(faint(event.at_millis.map(utc_minute).unwrap_or_default()).size(11));
        let mut entry = column![line].spacing(1);
        if !event.by.is_empty() {
            entry = entry.push(faint(event.by.as_str()).size(10).font(Font::MONOSPACE));
        }
        list = list.push(entry);
    }
    if detail.events.len() > MAX_EVENTS {
        list = list.push(
            faint(format!(
                "{} older events not shown",
                detail.events.len() - MAX_EVENTS
            ))
            .size(11),
        );
    }
    list.into()
}

fn error_box(error: &str) -> Element<'_, Message> {
    container(
        text(error)
            .size(12)
            .font(Font::MONOSPACE)
            .wrapping(text::Wrapping::WordOrGlyph),
    )
    .padding([8, 12])
    .width(Fill)
    .style(theme::error_banner)
    .into()
}

/// `350ms`, `4.2s`, `1m 30s`, `2h 5m`: jobs run for minutes or hours.
pub fn job_duration(millis: u64) -> String {
    let seconds = millis / 1000;
    match seconds {
        0 => format!("{millis}ms"),
        1..60 => format!("{:.1}s", millis as f64 / 1000.0),
        60..3600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3600, seconds % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn job(state: &str) -> JobRow {
        JobRow {
            job_id: format!("job-{state}"),
            job_type: "index_build".into(),
            table: "docs".into(),
            state: state.into(),
            created_at_millis: Some(1_795_554_000_000),
            ..JobRow::default()
        }
    }

    #[test]
    fn states_workers_and_durations() {
        assert_eq!(state_kind("running", false), StateKind::Running);
        assert_eq!(state_kind("running", true), StateKind::Waiting);
        assert_eq!(state_kind("queued", false), StateKind::Waiting);
        assert_eq!(state_kind("succeeded", false), StateKind::Succeeded);
        assert_eq!(state_kind("failed", false), StateKind::Failed);
        assert_eq!(state_kind("timed_out", false), StateKind::Failed);
        let timed_out = job("timed_out");
        assert!(!timed_out.is_active() && timed_out.failed());
        assert_eq!(state_kind("cancelled", false), StateKind::Stopped);

        let mut running = job("running");
        running.active_workers = Some(2);
        running.workers_desired = Some(4);
        assert_eq!(workers(&running).as_deref(), Some("2/4 workers"));
        running.workers_desired = None;
        running.active_workers = Some(1);
        assert_eq!(workers(&running).as_deref(), Some("1 worker"));
        running.active_workers = None;
        assert_eq!(workers(&running), None);
        assert_eq!(
            workers(&job("succeeded")),
            None,
            "finished jobs have no workers"
        );

        let now = 1_795_554_090_000;
        assert_eq!(elapsed(&running, now).as_deref(), Some("1m 30s"));
        let mut done = job("succeeded");
        done.completed_at_millis = Some(1_795_554_002_000);
        assert_eq!(elapsed(&done, now).as_deref(), Some("2.0s"));
        done.duration_millis = Some(500);
        assert_eq!(elapsed(&done, now).as_deref(), Some("500ms"));
        assert_eq!(elapsed(&job("failed"), now), None);

        assert_eq!(job_duration(7_500_000), "2h 5m");

        let facts = row_facts(&running, now);
        assert!(
            facts
                .iter()
                .any(|(label, value)| label == "duration" && value == "1m 30s")
        );
        running.rows_planned = Some(2_000);
        running.rows_computed = Some(1_240);
        let facts = row_facts(&running, now);
        assert!(facts.contains(&(
            "rows".to_string(),
            "2,000 planned · 1,240 computed".to_string()
        )));
    }
}
