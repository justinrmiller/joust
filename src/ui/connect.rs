//! The Open dialog: a LanceDB directory on this machine, or a LanceDB
//! Enterprise server.

use iced::widget::{
    Space, button, center, column, container, opaque, radio, row, scrollable, text, text_input,
};
use iced::{Alignment, Element, Fill, Font, Length, Theme};

use super::{BOLD, faint, muted, section_label, thin_scrollbar};
use crate::app::{App, Message};
use crate::enterprise::Check;
use crate::enterprise::dialog::{ConnectDialog, ConnectField, OpenTab};
use crate::enterprise::profiles::{
    API_KEY_ENV, AuthMethod, OIDC_CLIENT_SECRET_ENV, OIDC_TOKEN_ENV, OIDC_TOKEN_FILE_ENV, OidcFlow,
};
use crate::theme;

/// Widest the dialog grows.
const MAX_WIDTH: f32 = 900.0;
const PROFILE_LIST_WIDTH: f32 = 230.0;
const CHECK_LABEL_WIDTH: f32 = 120.0;

/// The dialog over the dimmed window.
pub fn overlay<'a>(app: &'a App, dialog: &'a ConnectDialog) -> Element<'a, Message> {
    opaque(
        center(card(app, dialog))
            .padding(24)
            .width(Fill)
            .height(Fill)
            .style(theme::backdrop),
    )
}

fn card<'a>(app: &'a App, dialog: &'a ConnectDialog) -> Element<'a, Message> {
    let tab = |label: &'static str, tab: OpenTab| {
        let active = dialog.tab == tab;
        column![
            button(
                text(label)
                    .size(13)
                    .font(if active { BOLD } else { Font::DEFAULT })
            )
            .padding([6, 12])
            .on_press(Message::SelectOpenTab(tab))
            .style(theme::tab(active)),
            container(Space::new().width(Fill).height(2)).style(theme::tab_indicator(active)),
        ]
        .width(Length::Shrink)
    };
    let header = row![
        text("Open a database").size(17).font(BOLD),
        Space::new().width(Fill),
        button(text("×").size(18))
            .padding([0, 8])
            .on_press(Message::CloseConnectDialog)
            .style(theme::ghost_button),
    ]
    .align_y(Alignment::Center);
    let tabs = row![
        tab("Local folder", OpenTab::Local),
        tab("LanceDB Enterprise", OpenTab::Enterprise),
    ]
    .spacing(4);

    let mut content = column![header, tabs].spacing(12);
    content = match dialog.tab {
        OpenTab::Local => content.push(local(app)),
        OpenTab::Enterprise => {
            let body = row![
                container(profile_list(dialog)).width(PROFILE_LIST_WIDTH),
                container(form(dialog)).width(Fill),
            ]
            .spacing(20);
            content
                .push(muted("Profiles come from the lancedb CLI. SQL runs on the server.").size(12))
                .push(body)
        }
    };
    if dialog.tab == OpenTab::Enterprise
        && let Some(checks) = &dialog.checks
    {
        content = content.push(checks_view(checks));
    }
    if let Some(error) = &dialog.error {
        content = content.push(
            container(text(error.as_str()).size(12))
                .padding([8, 12])
                .width(Fill)
                .style(theme::error_banner),
        );
    }
    content = content.push(match dialog.tab {
        OpenTab::Local => local_actions(app),
        OpenTab::Enterprise => actions(dialog),
    });

    container(scrollable(content.padding(22)).direction(thin_scrollbar()))
        .max_width(MAX_WIDTH)
        .style(theme::dialog)
        .into()
}

/// A LanceDB directory: its path, a folder picker, the sample database and
/// recent folders.
fn local(app: &App) -> Element<'_, Message> {
    let idle = app.busy.is_none();
    let path = text_input("Path to a LanceDB directory…", &app.path_input)
        .on_input(Message::PathChanged)
        .on_submit(Message::OpenDatabase)
        .padding([7, 10])
        .size(13)
        .style(theme::input);
    let mut content = column![
        muted("A directory of Lance tables on this machine. SQL runs locally with DataFusion.")
            .size(12),
        row![
            path,
            button(text("Browse…").size(13))
                .padding([7, 14])
                .on_press_maybe(idle.then_some(Message::BrowseDatabase))
                .style(theme::secondary_button),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
        row![
            button(text("Sample database").size(13))
                .padding([7, 14])
                .on_press_maybe(idle.then_some(Message::OpenSample))
                .style(theme::secondary_button),
            faint(
                "Creates (or recreates) a demo database: movies with posters, events, and (with \
                 ffmpeg) video trailers."
            )
            .size(11),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    ]
    .spacing(10);
    if !app.settings.recent_databases.is_empty() {
        content = content
            .push(Space::new().height(4))
            .push(section_label("Recent"));
        for recent in app.settings.recent_databases.iter().take(6) {
            content = content.push(
                button(text(recent.as_str()).size(12).font(Font::MONOSPACE))
                    .width(Fill)
                    .padding([4, 8])
                    .on_press_maybe(idle.then(|| Message::OpenRecent(recent.clone())))
                    .style(theme::list_item(false)),
            );
        }
    }
    content.into()
}

fn local_actions(app: &App) -> Element<'_, Message> {
    let idle = app.busy.is_none();
    row![
        faint(app.busy.as_deref().unwrap_or_default()).size(12),
        Space::new().width(Fill),
        button(text("Cancel").size(13))
            .padding([7, 14])
            .on_press(Message::CloseConnectDialog)
            .style(theme::secondary_button),
        button(text("Open").size(13).font(BOLD))
            .padding([7, 18])
            .on_press_maybe(idle.then_some(Message::OpenDatabase))
            .style(theme::primary_button),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn profile_list(dialog: &ConnectDialog) -> Element<'_, Message> {
    let mut list = column![
        section_label("Profiles"),
        faint("~/.lancedb/config.toml")
            .size(11)
            .font(Font::MONOSPACE),
        Space::new().height(4),
    ]
    .spacing(3);
    for (name, profile) in &dialog.profiles.entries {
        let address = profile
            .http_server_url
            .as_deref()
            .unwrap_or("no REST address");
        let selected = dialog.selected.as_deref() == Some(name.as_str());
        list = list.push(
            button(
                column![
                    text(name.as_str()).size(13).font(BOLD),
                    faint(address).size(11).font(Font::MONOSPACE),
                ]
                .spacing(2),
            )
            .width(Fill)
            .padding([6, 8])
            .on_press(Message::ConnectProfile(name.clone()))
            .style(theme::list_item(selected)),
        );
    }
    if let Some(error) = &dialog.config_error {
        list = list.push(danger(error.as_str()).size(11));
    }
    list.into()
}

fn form(dialog: &ConnectDialog) -> Element<'_, Message> {
    let endpoints = row![
        field(
            "REST endpoint",
            "https://query.example.com",
            &dialog.rest_url,
            ConnectField::RestUrl,
            None
        ),
        field(
            "SQL endpoint (Flight SQL)",
            "REST port + 1",
            &dialog.sql_url,
            ConnectField::SqlUrl,
            Some("Leave empty to use the REST port + 1")
        ),
    ]
    .spacing(14);
    let place = row![
        field(
            "Database",
            "analytics",
            &dialog.database,
            ConnectField::Database,
            None
        ),
        field(
            "Default namespace",
            "public",
            &dialog.namespace,
            ConnectField::Namespace,
            Some("For unqualified table names; nested: team$reports")
        ),
    ]
    .spacing(14);

    let mut methods = row![].spacing(14);
    for method in offered_methods(dialog) {
        methods = methods.push(
            radio(
                method.to_string(),
                method,
                Some(dialog.auth),
                Message::ConnectAuth,
            )
            .size(14)
            .text_size(13),
        );
    }

    column![
        endpoints,
        place,
        section_label("Sign in"),
        methods.wrap(),
        sign_in_detail(dialog),
    ]
    .spacing(12)
    .into()
}

/// The sign-in methods to offer: profile sign-in only for a profile that
/// has an `[oidc]` table.
pub(crate) fn offered_methods(dialog: &ConnectDialog) -> Vec<AuthMethod> {
    AuthMethod::ALL
        .into_iter()
        .filter(|method| *method != AuthMethod::ProfileSignIn || dialog.has_profile_sign_in())
        .collect()
}

/// What the chosen sign-in method needs or will do.
fn sign_in_detail(dialog: &ConnectDialog) -> Element<'_, Message> {
    match dialog.auth {
        AuthMethod::Automatic => faint(format!(
            "Uses ${API_KEY_ENV}, ${OIDC_TOKEN_ENV} or ${OIDC_TOKEN_FILE_ENV} if set, then the \
             profile's credentials, then the local server key."
        ))
        .size(11)
        .into(),
        AuthMethod::ApiKey => column![
            text_input("sk_…", &dialog.api_key)
                .secure(true)
                .on_input(|value| Message::ConnectEdit(ConnectField::ApiKey, value))
                .padding([6, 10])
                .size(13)
                .style(theme::input),
            faint("Kept in memory for this session only; never saved.").size(11),
        ]
        .spacing(4)
        .into(),
        AuthMethod::TokenFile => text_input("/path/to/oidc-token", &dialog.token_file)
            .on_input(|value| Message::ConnectEdit(ConnectField::TokenFile, value))
            .padding([6, 10])
            .size(13)
            .font(Font::MONOSPACE)
            .style(theme::input)
            .into(),
        AuthMethod::ProfileSignIn => {
            let flow = dialog
                .profile()
                .and_then(|profile| profile.oidc.as_ref())
                .map(|oidc| oidc.flow);
            faint(match flow {
                Some(OidcFlow::AuthorizationCode) => {
                    "Opens your browser to sign in. The session is shared with `lancedb auth login`."
                        .to_string()
                }
                Some(OidcFlow::ClientCredentials) => format!(
                    "Signs in as a service account; the client secret comes from \
                     ${OIDC_CLIENT_SECRET_ENV} or the profile's client_secret_file."
                ),
                Some(OidcFlow::DeviceCode) | None => {
                    "Reuses a session from `lancedb auth login` (device codes need a terminal)."
                        .to_string()
                }
            })
            .size(11)
            .into()
        }
    }
}

fn field<'a>(
    label: &'a str,
    placeholder: &'a str,
    value: &'a str,
    field: ConnectField,
    hint: Option<&'a str>,
) -> Element<'a, Message> {
    let mut column = column![
        text(label).size(12).font(BOLD),
        text_input(placeholder, value)
            .on_input(move |value| Message::ConnectEdit(field, value))
            .padding([6, 10])
            .size(13)
            .font(Font::MONOSPACE)
            .style(theme::input),
    ]
    .spacing(4)
    .width(Fill);
    if let Some(hint) = hint {
        column = column.push(faint(hint).size(11));
    }
    column.into()
}

fn checks_view(checks: &[Check]) -> Element<'_, Message> {
    let mut list = column![text("Connection check").size(13).font(BOLD)].spacing(6);
    for check in checks {
        let mark = if check.ok {
            success("✓").size(13)
        } else {
            caution("!").size(13)
        };
        list = list.push(
            row![
                container(mark).width(16),
                text(check.label)
                    .size(12)
                    .font(BOLD)
                    .width(CHECK_LABEL_WIDTH),
                muted(check.detail.as_str()).size(12).width(Fill),
            ]
            .spacing(6)
            .align_y(Alignment::Start),
        );
    }
    container(list)
        .padding([10, 12])
        .width(Fill)
        .style(theme::chrome_panel)
        .into()
}

fn actions(dialog: &ConnectDialog) -> Element<'_, Message> {
    let idle = dialog.working.is_none();
    row![
        faint(dialog.working.unwrap_or_default()).size(12),
        Space::new().width(Fill),
        button(text("Cancel").size(13))
            .padding([7, 14])
            .on_press(Message::CloseConnectDialog)
            .style(theme::secondary_button),
        button(text("Test connection").size(13))
            .padding([7, 14])
            .on_press_maybe(idle.then_some(Message::TestConnection))
            .style(theme::secondary_button),
        button(text("Connect").size(13).font(BOLD))
            .padding([7, 18])
            .on_press_maybe(idle.then_some(Message::ConnectEnterprise))
            .style(theme::primary_button),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .width(Length::Fill)
    .into()
}

fn success<'a>(content: &'a str) -> text::Text<'a> {
    text(content).style(|theme: &Theme| text::Style {
        color: Some(theme.extended_palette().success.base.color),
    })
}

fn caution<'a>(content: &'a str) -> text::Text<'a> {
    text(content).style(|theme: &Theme| text::Style {
        color: Some(theme.extended_palette().warning.base.color),
    })
}

fn danger<'a>(content: &'a str) -> text::Text<'a> {
    text(content).style(|theme: &Theme| text::Style {
        color: Some(theme.extended_palette().danger.base.color),
    })
}
