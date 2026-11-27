//! A small Python tokenizer, for showing Function source (read-only) in the
//! same theme colours as SQL.

use std::ops::Range;

use super::Token;

/// Python's keywords (and soft keywords).
const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "case", "class",
    "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if",
    "import", "in", "is", "lambda", "match", "nonlocal", "not", "or", "pass", "raise", "return",
    "try", "while", "with", "yield",
];

/// Letters that may prefix a string literal (`r"…"`, `b'…'`, `f"…"`, `rb'…'`).
const STRING_PREFIXES: &[&str] = &["r", "b", "f", "u", "rb", "br", "fr", "rf"];

/// Tokenises one line of Python. `open` is the triple quote a string from an
/// earlier line is still waiting for; returns the coloured byte ranges and
/// the triple quote still open at the end of this line.
pub fn tokenize_line(
    line: &str,
    open: Option<&'static str>,
) -> (Vec<(Range<usize>, Token)>, Option<&'static str>) {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;

    if let Some(quote) = open {
        match find_unescaped(line, 0, quote) {
            Some(end) => {
                tokens.push((0..end + quote.len(), Token::String));
                i = end + quote.len();
            }
            None => {
                if !line.is_empty() {
                    tokens.push((0..line.len(), Token::String));
                }
                return (tokens, open);
            }
        }
    }

    // The next name is the one a `def` or `class` defines.
    let mut defining = false;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'#' => {
                tokens.push((i..bytes.len(), Token::Comment));
                break;
            }
            b'\'' | b'"' => {
                let (end, still_open) = scan_string(line, i);
                tokens.push((i..end, Token::String));
                if still_open.is_some() {
                    return (tokens, still_open);
                }
                i = end;
            }
            b'@' if line[..i].trim().is_empty() => {
                // A decorator: `@name` or `@module.name`.
                let start = i;
                i += 1;
                while i < bytes.len() && (is_name_byte(bytes[i]) || bytes[i] == b'.') {
                    i += 1;
                }
                tokens.push((start..i, Token::Function));
            }
            b'0'..=b'9' => {
                let start = i;
                while i < bytes.len() {
                    let b = bytes[i];
                    let exponent_sign =
                        matches!(b, b'+' | b'-') && matches!(bytes[i - 1], b'e' | b'E');
                    if is_name_byte(b) || b == b'.' || exponent_sign {
                        i += 1;
                    } else {
                        break;
                    }
                }
                tokens.push((start..i, Token::Number));
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                while i < bytes.len() && is_name_byte(bytes[i]) {
                    i += 1;
                }
                let word = &line[start..i];
                if matches!(bytes.get(i), Some(b'\'' | b'"'))
                    && STRING_PREFIXES
                        .iter()
                        .any(|prefix| prefix.eq_ignore_ascii_case(word))
                {
                    let (end, still_open) = scan_string(line, i);
                    tokens.push((start..end, Token::String));
                    if still_open.is_some() {
                        return (tokens, still_open);
                    }
                    i = end;
                } else if KEYWORDS.contains(&word) {
                    tokens.push((start..i, Token::Keyword));
                    defining = matches!(word, "def" | "class");
                } else if std::mem::take(&mut defining) || line[i..].trim_start().starts_with('(') {
                    tokens.push((start..i, Token::Function));
                }
            }
            b'=' | b'<' | b'>' | b'!' | b'+' | b'-' | b'*' | b'/' | b'%' | b'&' | b'|' | b'^'
            | b'~' | b'@' => {
                tokens.push((i..i + 1, Token::Operator));
                i += 1;
            }
            _ => {
                // Any other byte, respecting UTF-8 boundaries.
                i += line[i..].chars().next().map_or(1, char::len_utf8);
            }
        }
    }
    (tokens, None)
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Scans a string whose opening quote is at `quote_at`. Returns where it
/// ends, and the triple quote still open when it runs past the line.
fn scan_string(line: &str, quote_at: usize) -> (usize, Option<&'static str>) {
    let bytes = line.as_bytes();
    let quote = bytes[quote_at];
    let triple = if quote == b'"' { "\"\"\"" } else { "'''" };
    if line[quote_at..].starts_with(triple) {
        return match find_unescaped(line, quote_at + triple.len(), triple) {
            Some(end) => (end + triple.len(), None),
            None => (line.len(), Some(triple)),
        };
    }
    let mut i = quote_at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == quote => return (i + 1, None),
            _ => i += 1,
        }
    }
    // Unterminated: the string runs to the end of the line.
    (line.len(), None)
}

/// The first `needle` at or after `from` that no backslash escapes.
fn find_unescaped(line: &str, from: usize, needle: &str) -> Option<usize> {
    let mut search = from;
    while let Some(offset) = line.get(search..)?.find(needle) {
        let at = search + offset;
        let backslashes = line[..at].bytes().rev().take_while(|&b| b == b'\\').count();
        if backslashes % 2 == 0 {
            return Some(at);
        }
        search = at + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds<'a>(
        line: &'a str,
        open: Option<&'static str>,
    ) -> (Vec<(&'a str, Token)>, Option<&'static str>) {
        let (tokens, still_open) = tokenize_line(line, open);
        let kinds = tokens
            .into_iter()
            .map(|(range, token)| (&line[range], token))
            .collect();
        (kinds, still_open)
    }

    #[test]
    fn colours_definitions_calls_and_literals() {
        let (tokens, open) = kinds("def add_one(x: int) -> int:  # one more", None);
        assert_eq!(open, None);
        assert_eq!(
            tokens,
            [
                ("def", Token::Keyword),
                ("add_one", Token::Function),
                ("-", Token::Operator),
                (">", Token::Operator),
                ("# one more", Token::Comment),
            ]
        );
        let (tokens, _) = kinds("    return list(model.embed([x]))[0] + 1.5e-3", None);
        assert_eq!(tokens[0], ("return", Token::Keyword));
        assert!(tokens.contains(&("list", Token::Function)));
        assert!(tokens.contains(&("embed", Token::Function)));
        assert!(tokens.contains(&("0", Token::Number)));
        assert!(tokens.contains(&("1.5e-3", Token::Number)));
        let (tokens, _) = kinds("class Model(Base): pass", None);
        assert_eq!(tokens[1], ("Model", Token::Function));
        assert_eq!(tokens.last(), Some(&("pass", Token::Keyword)));
    }

    #[test]
    fn strings_prefixes_and_escapes() {
        let (tokens, _) = kinds(r#"s = 'it\'s' + "a#b" + rb'\x00' + f"{x}""#, None);
        let strings: Vec<&str> = tokens
            .iter()
            .filter(|(_, token)| *token == Token::String)
            .map(|(text, _)| *text)
            .collect();
        assert_eq!(strings, [r"'it\'s'", "\"a#b\"", r"rb'\x00'", "f\"{x}\""]);
        assert!(!tokens.iter().any(|(_, token)| *token == Token::Comment));
        let (tokens, open) = kinds("x = 'unterminated", None);
        assert_eq!((tokens.last().unwrap().0, open), ("'unterminated", None));
        assert_eq!(kinds("rate = r", None).0, [("=", Token::Operator)]);
    }

    #[test]
    fn triple_quoted_strings_span_lines() {
        let (tokens, open) = kinds("    \"\"\"Embeds a title.", None);
        assert_eq!(tokens, [("\"\"\"Embeds a title.", Token::String)]);
        assert_eq!(open, Some("\"\"\""));
        let (tokens, open) = kinds("    still inside", open);
        assert_eq!((tokens.len(), open), (1, Some("\"\"\"")));
        let (tokens, open) = kinds("    with \\\"\"\" escaped\"\"\"  # done", open);
        assert_eq!(open, None);
        assert_eq!(tokens[0].1, Token::String);
        assert!(tokens[0].0.ends_with("escaped\"\"\""));
        assert_eq!(tokens[1], ("# done", Token::Comment));
        assert_eq!(kinds("", Some("'''")), (Vec::new(), Some("'''")));
        let (tokens, open) = kinds("doc = '''one''' if x else r'''two", None);
        assert_eq!(tokens[1], ("'''one'''", Token::String));
        assert_eq!(open, Some("'''"));
    }

    #[test]
    fn decorators_and_other_text() {
        let (tokens, _) = kinds("  @functools.cache", None);
        assert_eq!(tokens, [("@functools.cache", Token::Function)]);
        let (tokens, _) = kinds("y = a @ b  # é ✓", None);
        assert!(tokens.contains(&("@", Token::Operator)));
        assert_eq!(tokens.last().unwrap().1, Token::Comment);
        assert_eq!(
            kinds("naïve = 1", None).0.last(),
            Some(&("1", Token::Number))
        );
    }
}
