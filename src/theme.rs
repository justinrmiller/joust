//! joust's theme catalog and the widget styles built on top of it.
//!
//! Each [`ThemeId`] maps to an iced [`Theme`] (three custom palettes plus six
//! of iced's built-ins) and carries the extra colours iced does not model:
//! SQL syntax colours and chart colours.

use iced::border::Radius;
use iced::widget::{button, container, pick_list, text_editor, text_input};
use iced::{Background, Border, Color, Shadow, Theme, Vector, color};
use serde::{Deserialize, Serialize};

/// Every theme joust ships with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum ThemeId {
    #[default]
    JoustLight,
    JoustDark,
    LanceMidnight,
    Nord,
    Dracula,
    SolarizedLight,
    GruvboxDark,
    TokyoNight,
    CatppuccinMocha,
}

/// Colours used by the SQL highlighter.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyntaxColors {
    pub keyword: Color,
    pub function: Color,
    pub string: Color,
    pub number: Color,
    pub comment: Color,
    pub operator: Color,
}

impl ThemeId {
    pub const ALL: [ThemeId; 9] = [
        ThemeId::JoustLight,
        ThemeId::JoustDark,
        ThemeId::LanceMidnight,
        ThemeId::Nord,
        ThemeId::Dracula,
        ThemeId::SolarizedLight,
        ThemeId::GruvboxDark,
        ThemeId::TokyoNight,
        ThemeId::CatppuccinMocha,
    ];

    /// Display name.
    pub fn name(self) -> &'static str {
        match self {
            ThemeId::JoustLight => "Joust Light",
            ThemeId::JoustDark => "Joust Dark",
            ThemeId::LanceMidnight => "Lance Midnight",
            ThemeId::Nord => "Nord",
            ThemeId::Dracula => "Dracula",
            ThemeId::SolarizedLight => "Solarized Light",
            ThemeId::GruvboxDark => "Gruvbox Dark",
            ThemeId::TokyoNight => "Tokyo Night",
            ThemeId::CatppuccinMocha => "Catppuccin Mocha",
        }
    }

    /// Whether the theme has a dark background.
    pub fn is_dark(self) -> bool {
        !matches!(self, ThemeId::JoustLight | ThemeId::SolarizedLight)
    }

    /// Builds the iced theme.
    pub fn to_theme(self) -> Theme {
        use iced::theme::Palette;
        match self {
            ThemeId::JoustLight => Theme::custom(
                self.name(),
                Palette {
                    background: color!(0xffffff),
                    text: color!(0x1f1f1f),
                    primary: color!(0xf2c200),
                    success: color!(0x2e9d5b),
                    warning: color!(0xd98e04),
                    danger: color!(0xd64545),
                },
            ),
            ThemeId::JoustDark => Theme::custom(
                self.name(),
                Palette {
                    background: color!(0x1b1b1d),
                    text: color!(0xe8e8e6),
                    primary: color!(0xffd84d),
                    success: color!(0x4cc38a),
                    warning: color!(0xf0a020),
                    danger: color!(0xf16a6a),
                },
            ),
            ThemeId::LanceMidnight => Theme::custom(
                self.name(),
                Palette {
                    background: color!(0x0e1022),
                    text: color!(0xe4e6f5),
                    primary: color!(0x8b7cf6),
                    success: color!(0x3dd68c),
                    warning: color!(0xf5b84c),
                    danger: color!(0xf2677e),
                },
            ),
            ThemeId::Nord => Theme::Nord,
            ThemeId::Dracula => Theme::Dracula,
            ThemeId::SolarizedLight => Theme::SolarizedLight,
            ThemeId::GruvboxDark => Theme::GruvboxDark,
            ThemeId::TokyoNight => Theme::TokyoNight,
            ThemeId::CatppuccinMocha => Theme::CatppuccinMocha,
        }
    }

    /// SQL syntax colours tuned for the theme's background.
    pub fn syntax(self) -> SyntaxColors {
        match self {
            ThemeId::JoustLight => SyntaxColors {
                keyword: color!(0x0b57d0),
                function: color!(0x8430ce),
                string: color!(0x1a7f37),
                number: color!(0xb35900),
                comment: color!(0x8a8a8a),
                operator: color!(0x5c5c5c),
            },
            ThemeId::JoustDark => SyntaxColors {
                keyword: color!(0xffd84d),
                function: color!(0x82aaff),
                string: color!(0x9ece6a),
                number: color!(0xff9e64),
                comment: color!(0x7a7a7a),
                operator: color!(0xb8b8b8),
            },
            ThemeId::LanceMidnight => SyntaxColors {
                keyword: color!(0xa99bff),
                function: color!(0x5ccfe6),
                string: color!(0x3dd68c),
                number: color!(0xf5b84c),
                comment: color!(0x6b6f99),
                operator: color!(0xb5b8d6),
            },
            ThemeId::Nord => SyntaxColors {
                keyword: color!(0x81a1c1),
                function: color!(0x88c0d0),
                string: color!(0xa3be8c),
                number: color!(0xb48ead),
                comment: color!(0x7b88a1),
                operator: color!(0xd8dee9),
            },
            ThemeId::Dracula => SyntaxColors {
                keyword: color!(0xff79c6),
                function: color!(0x50fa7b),
                string: color!(0xf1fa8c),
                number: color!(0xbd93f9),
                comment: color!(0x7c86b8),
                operator: color!(0xf8f8f2),
            },
            ThemeId::SolarizedLight => SyntaxColors {
                keyword: color!(0x859900),
                function: color!(0x268bd2),
                string: color!(0x2aa198),
                number: color!(0xd33682),
                comment: color!(0x93a1a1),
                operator: color!(0x657b83),
            },
            ThemeId::GruvboxDark => SyntaxColors {
                keyword: color!(0xfb4934),
                function: color!(0xfabd2f),
                string: color!(0xb8bb26),
                number: color!(0xd3869b),
                comment: color!(0x928374),
                operator: color!(0xebdbb2),
            },
            ThemeId::TokyoNight => SyntaxColors {
                keyword: color!(0xbb9af7),
                function: color!(0x7aa2f7),
                string: color!(0x9ece6a),
                number: color!(0xff9e64),
                comment: color!(0x5f6996),
                operator: color!(0x89ddff),
            },
            ThemeId::CatppuccinMocha => SyntaxColors {
                keyword: color!(0xcba6f7),
                function: color!(0x89b4fa),
                string: color!(0xa6e3a1),
                number: color!(0xfab387),
                comment: color!(0x7f849c),
                operator: color!(0x94e2d5),
            },
        }
    }

    /// The single-series chart colour.
    ///
    /// Slot 1 of the validated reference categorical palette, stepped for the
    /// theme's mode; it clears 3:1 contrast on every theme surface above.
    pub fn series(self) -> Color {
        if self.is_dark() {
            color!(0x3987e5)
        } else {
            color!(0x2a78d6)
        }
    }
}

impl std::fmt::Display for ThemeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Linear blend of `a` towards `b` by `t` (0 = `a`, 1 = `b`).
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

/// Secondary text colour (labels, hints).
pub fn muted(theme: &Theme) -> Color {
    let palette = theme.extended_palette();
    mix(
        palette.background.base.text,
        palette.background.base.color,
        0.42,
    )
}

/// Faint text colour (types, row numbers).
pub fn faint(theme: &Theme) -> Color {
    let palette = theme.extended_palette();
    mix(
        palette.background.base.text,
        palette.background.base.color,
        0.6,
    )
}

/// Hairline divider colour.
pub fn divider(theme: &Theme) -> Color {
    let palette = theme.extended_palette();
    mix(
        palette.background.base.text,
        palette.background.base.color,
        0.86,
    )
}

/// Background of chrome surfaces (sidebar, toolbars, headers).
pub fn chrome(theme: &Theme) -> Color {
    let palette = theme.extended_palette();
    mix(
        palette.background.base.color,
        palette.background.base.text,
        0.035,
    )
}

/// Background for hovered rows and list items.
pub fn hover(theme: &Theme) -> Color {
    let palette = theme.extended_palette();
    mix(
        palette.background.base.color,
        palette.background.base.text,
        0.07,
    )
}

/// Background for selected rows and cells.
pub fn selection(theme: &Theme) -> Color {
    let palette = theme.extended_palette();
    Color {
        a: 0.22,
        ..palette.primary.base.color
    }
}

// ---------------------------------------------------------------------------
// Container styles
// ---------------------------------------------------------------------------

/// Main application background.
pub fn app(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.base.color.into()),
        text_color: Some(palette.background.base.text),
        ..container::Style::default()
    }
}

/// Sidebar / toolbar chrome.
pub fn chrome_panel(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(chrome(theme).into()),
        ..app(theme)
    }
}

/// Top bar: chrome with a divider underneath.
pub fn top_bar(theme: &Theme) -> container::Style {
    container::Style {
        border: Border {
            color: divider(theme),
            width: 0.0,
            radius: Radius::default(),
        },
        ..chrome_panel(theme)
    }
}

/// A bordered card on the main background.
pub fn card(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.base.color.into()),
        text_color: Some(palette.background.base.text),
        border: Border {
            color: divider(theme),
            width: 1.0,
            radius: 8.0.into(),
        },
        ..container::Style::default()
    }
}

/// Card lifted slightly off the chrome background.
pub fn raised_card(theme: &Theme) -> container::Style {
    container::Style {
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.12),
            offset: Vector::new(0.0, 1.0),
            blur_radius: 4.0,
        },
        ..card(theme)
    }
}

/// Small rounded pill (type badges, counters).
pub fn badge(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(hover(theme).into()),
        text_color: Some(muted(theme)),
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// Accent-tinted pill (index badges, vector columns).
pub fn accent_badge(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(selection(theme).into()),
        text_color: Some(palette.background.base.text),
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// Banner for errors.
pub fn error_banner(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(
            Color {
                a: 0.14,
                ..palette.danger.base.color
            }
            .into(),
        ),
        text_color: Some(palette.background.base.text),
        border: Border {
            color: palette.danger.base.color,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..container::Style::default()
    }
}

/// Banner for informational notes.
pub fn info_banner(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(
            Color {
                a: 0.12,
                ..palette.success.base.color
            }
            .into(),
        ),
        text_color: Some(palette.background.base.text),
        border: Border {
            color: Color {
                a: 0.6,
                ..palette.success.base.color
            },
            width: 1.0,
            radius: 6.0.into(),
        },
        ..container::Style::default()
    }
}

/// Floating tooltip body.
pub fn tooltip(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.base.text.into()),
        text_color: Some(palette.background.base.color),
        border: Border {
            radius: 4.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// A 1px divider line (use in a container with fixed height/width).
pub fn rule(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(divider(theme).into()),
        ..container::Style::default()
    }
}

// ---------------------------------------------------------------------------
// Button styles
// ---------------------------------------------------------------------------

/// Filled accent button (Run).
pub fn primary_button(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let base = match status {
        button::Status::Hovered | button::Status::Pressed => palette.primary.strong,
        _ => palette.primary.base,
    };
    button::Style {
        background: Some(Background::Color(base.color)),
        text_color: base.text,
        border: Border {
            radius: 6.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
    .with_disabled(status, theme)
}

/// Outlined neutral button.
pub fn secondary_button(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    let background = match status {
        button::Status::Hovered | button::Status::Pressed => hover(theme),
        _ => palette.background.base.color,
    };
    button::Style {
        background: Some(Background::Color(background)),
        text_color: palette.background.base.text,
        border: Border {
            color: divider(theme),
            width: 1.0,
            radius: 6.0.into(),
        },
        ..button::Style::default()
    }
    .with_disabled(status, theme)
}

/// Borderless button that only shows a background on hover.
pub fn ghost_button(theme: &Theme, status: button::Status) -> button::Style {
    let palette = theme.extended_palette();
    button::Style {
        background: match status {
            button::Status::Hovered | button::Status::Pressed => Some(hover(theme).into()),
            _ => None,
        },
        text_color: palette.background.base.text,
        border: Border {
            radius: 5.0.into(),
            ..Border::default()
        },
        ..button::Style::default()
    }
    .with_disabled(status, theme)
}

/// A list row (sidebar items); `selected` keeps it highlighted.
pub fn list_item(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let palette = theme.extended_palette();
        let background = if selected {
            Some(selection(theme).into())
        } else {
            match status {
                button::Status::Hovered | button::Status::Pressed => Some(hover(theme).into()),
                _ => None,
            }
        };
        button::Style {
            background,
            text_color: palette.background.base.text,
            border: Border {
                radius: 5.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

/// Tab header; the active tab gets an accent underline via its container.
pub fn tab(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let palette = theme.extended_palette();
        let text_color = if active {
            palette.background.base.text
        } else {
            muted(theme)
        };
        button::Style {
            background: match status {
                button::Status::Hovered | button::Status::Pressed if !active => {
                    Some(hover(theme).into())
                }
                _ => None,
            },
            text_color,
            border: Border {
                radius: 5.0.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

/// Accent underline shown beneath the active tab.
pub fn tab_indicator(active: bool) -> impl Fn(&Theme) -> container::Style {
    move |theme| container::Style {
        background: active.then(|| theme.extended_palette().primary.base.color.into()),
        border: Border {
            radius: 1.0.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

trait WithDisabled {
    fn with_disabled(self, status: button::Status, theme: &Theme) -> Self;
}

impl WithDisabled for button::Style {
    fn with_disabled(self, status: button::Status, theme: &Theme) -> Self {
        if status == button::Status::Disabled {
            button::Style {
                background: self
                    .background
                    .map(|background| background.scale_alpha(0.45)),
                text_color: faint(theme),
                ..self
            }
        } else {
            self
        }
    }
}

// ---------------------------------------------------------------------------
// Input styles
// ---------------------------------------------------------------------------

/// SQL editor: flush with the main background, no border.
pub fn editor(theme: &Theme, status: text_editor::Status) -> text_editor::Style {
    let palette = theme.extended_palette();
    let default = text_editor::default(theme, status);
    text_editor::Style {
        background: palette.background.base.color.into(),
        border: Border {
            width: 0.0,
            ..default.border
        },
        placeholder: faint(theme),
        selection: selection(theme),
        ..default
    }
}

/// Text inputs.
pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let palette = theme.extended_palette();
    let default = text_input::default(theme, status);
    let border_color = match status {
        text_input::Status::Focused { .. } => palette.primary.base.color,
        _ => divider(theme),
    };
    text_input::Style {
        background: palette.background.base.color.into(),
        border: Border {
            color: border_color,
            width: 1.0,
            radius: 6.0.into(),
        },
        placeholder: faint(theme),
        selection: selection(theme),
        ..default
    }
}

/// Compact pick lists in toolbars.
pub fn picker(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let palette = theme.extended_palette();
    let default = pick_list::default(theme, status);
    pick_list::Style {
        background: match status {
            pick_list::Status::Hovered | pick_list::Status::Opened { .. } => hover(theme).into(),
            _ => palette.background.base.color.into(),
        },
        border: Border {
            color: divider(theme),
            width: 1.0,
            radius: 6.0.into(),
        },
        placeholder_color: faint(theme),
        ..default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn at_least_five_distinct_themes() {
        let names: std::collections::HashSet<_> = ThemeId::ALL.iter().map(|t| t.name()).collect();
        assert!(names.len() >= 5);
        assert_eq!(names.len(), ThemeId::ALL.len());
    }

    #[test]
    fn theme_names_round_trip_through_iced() {
        for id in ThemeId::ALL {
            assert_eq!(id.to_theme().to_string(), id.name());
        }
    }

    #[test]
    fn dark_flag_matches_palette() {
        for id in ThemeId::ALL {
            assert_eq!(
                id.to_theme().extended_palette().is_dark,
                id.is_dark(),
                "{id}"
            );
        }
    }
}
