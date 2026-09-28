//! A small SQL syntax highlighter for iced's text editor.
//!
//! Colours come from the active [`ThemeId`], which is passed in as the
//! highlighter settings so a theme switch re-highlights the buffer.

use std::ops::Range;

use iced::Color;
use iced::Font;
use iced::advanced::text::highlighter::{self, Format};

use crate::theme::ThemeId;

/// SQL keywords (upper-case) that get keyword colouring.
const KEYWORDS: &[&str] = &[
    "ALL",
    "ALTER",
    "AND",
    "ANTI",
    "AS",
    "ASC",
    "BETWEEN",
    "BY",
    "CASE",
    "CAST",
    "COPY",
    "CREATE",
    "CROSS",
    "CURRENT",
    "DELETE",
    "DESC",
    "DESCRIBE",
    "DISTINCT",
    "DROP",
    "ELSE",
    "END",
    "EXCEPT",
    "EXISTS",
    "EXPLAIN",
    "ANALYZE",
    "FALSE",
    "FILTER",
    "FIRST",
    "FOLLOWING",
    "FROM",
    "FULL",
    "GROUP",
    "HAVING",
    "IF",
    "ILIKE",
    "IN",
    "INNER",
    "INSERT",
    "INTERSECT",
    "INTERVAL",
    "INTO",
    "IS",
    "JOIN",
    "LAST",
    "LATERAL",
    "LEFT",
    "LIKE",
    "LIMIT",
    "NATURAL",
    "NOT",
    "NULL",
    "NULLS",
    "OFFSET",
    "ON",
    "OR",
    "ORDER",
    "OUTER",
    "OVER",
    "PARTITION",
    "PRECEDING",
    "QUALIFY",
    "RANGE",
    "RECURSIVE",
    "REPLACE",
    "RIGHT",
    "ROW",
    "ROWS",
    "SELECT",
    "SEMI",
    "SET",
    "SHOW",
    "TABLE",
    "TABLES",
    "THEN",
    "TRUE",
    "UNBOUNDED",
    "UNION",
    "UNNEST",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "WHEN",
    "WHERE",
    "WINDOW",
    "WITH",
    "COLUMNS",
    "BIGINT",
    "INT",
    "INTEGER",
    "DOUBLE",
    "FLOAT",
    "REAL",
    "VARCHAR",
    "TEXT",
    "BOOLEAN",
    "DATE",
    "TIMESTAMP",
    "DECIMAL",
    "SMALLINT",
    "TINYINT",
];

/// Token classes the highlighter distinguishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    Keyword,
    Function,
    String,
    Number,
    Comment,
    Operator,
}

/// Highlighter state: which lines start inside a `/* ... */` block comment.
pub struct SqlHighlighter {
    theme: ThemeId,
    /// `in_comment[i]` = whether line `i` starts inside a block comment.
    in_comment: Vec<bool>,
    current_line: usize,
}

impl highlighter::Highlighter for SqlHighlighter {
    type Settings = ThemeId;
    type Highlight = Color;
    type Iterator<'a> = std::vec::IntoIter<(Range<usize>, Color)>;

    fn new(settings: &Self::Settings) -> Self {
        Self {
            theme: *settings,
            in_comment: vec![false],
            current_line: 0,
        }
    }

    fn update(&mut self, new_settings: &Self::Settings) {
        self.theme = *new_settings;
        self.change_line(0);
    }

    fn change_line(&mut self, line: usize) {
        let line = line.min(self.in_comment.len() - 1);
        self.in_comment.truncate(line + 1);
        self.current_line = line;
    }

    fn highlight_line(&mut self, line: &str) -> Self::Iterator<'_> {
        let starts_in_comment = self
            .in_comment
            .get(self.current_line)
            .copied()
            .unwrap_or(false);
        let (tokens, ends_in_comment) = tokenize_line(line, starts_in_comment);

        // `change_line` truncates the state, so the next line's entry is
        // always new.
        self.current_line += 1;
        self.in_comment.truncate(self.current_line);
        self.in_comment.push(ends_in_comment);

        let colors = self.theme.syntax();
        tokens
            .into_iter()
            .map(|(range, token)| {
                let color = match token {
                    Token::Keyword => colors.keyword,
                    Token::Function => colors.function,
                    Token::String => colors.string,
                    Token::Number => colors.number,
                    Token::Comment => colors.comment,
                    Token::Operator => colors.operator,
                };
                (range, color)
            })
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn current_line(&self) -> usize {
        self.current_line
    }
}

/// Turns a highlight colour into a text format (used by the editor widget).
pub fn to_format(color: &Color, _theme: &iced::Theme) -> Format<Font> {
    Format {
        color: Some(*color),
        font: None,
    }
}

/// Tokenises one line. Returns the coloured byte ranges and whether the line
/// ends inside an unterminated block comment.
pub fn tokenize_line(line: &str, mut in_comment: bool) -> (Vec<(Range<usize>, Token)>, bool) {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        if in_comment {
            let start = i;
            match line[i..].find("*/") {
                Some(end) => {
                    i += end + 2;
                    in_comment = false;
                }
                None => i = bytes.len(),
            }
            tokens.push((start..i, Token::Comment));
            continue;
        }

        let c = bytes[i];
        match c {
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                tokens.push((i..bytes.len(), Token::Comment));
                break;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                in_comment = true;
                let start = i;
                i += 2;
                match line[i..].find("*/") {
                    Some(end) => {
                        i += end + 2;
                        in_comment = false;
                    }
                    None => i = bytes.len(),
                }
                tokens.push((start..i, Token::Comment));
            }
            b'\'' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        // '' is an escaped quote inside a string.
                        if bytes.get(i + 1) == Some(&b'\'') {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                tokens.push((start..i, Token::String));
            }
            b'"' => {
                // Quoted identifier: leave uncoloured but skip it whole.
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += 1;
                }
                i = (i + 1).min(bytes.len());
            }
            b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'.') {
                    i += 1;
                }
                tokens.push((start..i, Token::Number));
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let word = &line[start..i];
                let next = line[i..].trim_start().as_bytes().first().copied();
                if KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(word)) {
                    tokens.push((start..i, Token::Keyword));
                } else if next == Some(b'(') {
                    tokens.push((start..i, Token::Function));
                }
            }
            b'=' | b'<' | b'>' | b'!' | b'+' | b'*' | b'/' | b'%' | b'|' | b'-' => {
                tokens.push((i..i + 1, Token::Operator));
                i += 1;
            }
            _ => {
                // Skip any other byte, respecting UTF-8 boundaries.
                i += line[i..].chars().next().map_or(1, char::len_utf8);
            }
        }
    }

    (tokens, in_comment)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &str) -> Vec<(&str, Token)> {
        tokenize_line(line, false)
            .0
            .into_iter()
            .map(|(range, token)| (&line[range], token))
            .collect()
    }

    #[test]
    fn highlights_basic_select() {
        assert_eq!(
            kinds("SELECT count(*) FROM t WHERE x = 'a''b' -- hi"),
            vec![
                ("SELECT", Token::Keyword),
                ("count", Token::Function),
                ("*", Token::Operator),
                ("FROM", Token::Keyword),
                ("WHERE", Token::Keyword),
                ("=", Token::Operator),
                ("'a''b'", Token::String),
                ("-- hi", Token::Comment),
            ]
        );
    }

    #[test]
    fn numbers_and_quoted_identifiers() {
        assert_eq!(
            kinds("select \"Select\", 1.5e3"),
            vec![("select", Token::Keyword), ("1.5e3", Token::Number)]
        );
    }

    #[test]
    fn block_comments_span_lines() {
        let (tokens, open) = tokenize_line("SELECT /* start", false);
        assert!(open);
        assert_eq!(tokens.last().unwrap().1, Token::Comment);
        let (tokens, open) = tokenize_line("still */ FROM", true);
        assert!(!open);
        assert_eq!(tokens[0], (0..8, Token::Comment));
        assert_eq!(tokens[1], (9..13, Token::Keyword));
    }

    #[test]
    fn highlighter_carries_comment_state_between_lines() {
        use iced::advanced::text::highlighter::Highlighter as _;
        let colors = ThemeId::Nord.syntax();
        let mut highlighter = SqlHighlighter::new(&ThemeId::Nord);
        let first: Vec<_> = highlighter.highlight_line("SELECT /* note").collect();
        assert_eq!(first[0], (0..6, colors.keyword));
        let second: Vec<_> = highlighter.highlight_line("still inside").collect();
        assert_eq!(second, vec![(0..12, colors.comment)]);
        let third: Vec<_> = highlighter.highlight_line("end */ 42").collect();
        assert_eq!(third, vec![(0..6, colors.comment), (7..9, colors.number)]);
        assert_eq!(highlighter.current_line(), 3);

        // Re-highlighting after an edit resumes from the stored state.
        highlighter.change_line(1);
        let again: Vec<_> = highlighter.highlight_line("still inside").collect();
        assert_eq!(again, second);

        // A theme change restarts from the top with the new colours.
        highlighter.update(&ThemeId::Dracula);
        assert_eq!(highlighter.current_line(), 0);
        let line: Vec<_> = highlighter.highlight_line("'x' /* c */ + 1").collect();
        let dracula = ThemeId::Dracula.syntax();
        assert_eq!(line[0], (0..3, dracula.string));
        assert_eq!(line[1], (4..11, dracula.comment));
        assert_eq!(line[2], (12..13, dracula.operator));
        let format = to_format(&dracula.keyword, &iced::Theme::Dark);
        assert_eq!(format.color, Some(dracula.keyword));
    }

    #[test]
    fn handles_multibyte_text() {
        let line = "SELECT 'Nausicaä' AS título";
        let tokens = kinds(line);
        assert!(tokens.contains(&("'Nausicaä'", Token::String)));
        assert!(tokens.contains(&("AS", Token::Keyword)));
    }
}
