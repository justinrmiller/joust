//! Splits editor text into SQL statements for servers that take one per
//! request.
//!
//! joust's own SQL dialect is parsed by DataFusion, but a LanceDB Enterprise
//! server has syntax DataFusion cannot parse (`CREATE FUNCTION … AS $$ … $$`,
//! `REFRESH COLUMN …`), so statements are split on `;` while skipping quoted
//! text, comments and dollar-quoted bodies instead of being parsed.

/// The statements in `sql`, trimmed, without their `;`. Statements that are
/// only comments are dropped.
pub fn split_statements(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => i = skip_quoted(bytes, i),
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = memchr(bytes, i, b'\n').map_or(bytes.len(), |end| end + 1);
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = find(bytes, i + 2, b"*/").map_or(bytes.len(), |end| end + 2);
            }
            b'$' => match dollar_tag(bytes, i) {
                Some(tag) => {
                    i = find(bytes, i + tag.len(), tag).map_or(bytes.len(), |end| end + tag.len());
                }
                None => i += 1,
            },
            b';' => {
                push_statement(&mut statements, &sql[start..i]);
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    push_statement(&mut statements, &sql[start..]);
    statements
}

fn push_statement(statements: &mut Vec<String>, text: &str) {
    let text = text.trim();
    if has_code(text) {
        statements.push(text.to_string());
    }
}

/// Whether `text` holds anything besides comments and whitespace.
pub fn has_code(text: &str) -> bool {
    let mut rest = text;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, tail)| tail);
        } else {
            return !rest.is_empty();
        }
    }
}

/// Index just past the quoted text opening at `start`; a doubled quote
/// character inside is an escaped one.
fn skip_quoted(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

/// The `$tag$` (or `$$`) opening a dollar-quoted string at `start`. A `$`
/// right after an identifier character is not one: `team$reports` is a
/// namespace path, and `$1` a placeholder.
fn dollar_tag(bytes: &[u8], start: usize) -> Option<&[u8]> {
    if start > 0 && is_identifier_byte(bytes[start - 1]) {
        return None;
    }
    let mut end = start + 1;
    while end < bytes.len() && is_identifier_byte(bytes[end]) {
        end += 1;
    }
    let tag_starts_with_digit = bytes.get(start + 1).is_some_and(u8::is_ascii_digit);
    (bytes.get(end) == Some(&b'$') && !tag_starts_with_digit).then(|| &bytes[start..=end])
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn memchr(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    bytes[from..]
        .iter()
        .position(|&b| b == needle)
        .map(|offset| from + offset)
}

fn find(bytes: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    bytes
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_semicolons_outside_text() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2;"),
            ["SELECT 1", "SELECT 2"]
        );
        assert_eq!(
            split_statements("SELECT 'a;b', \"c;d\", `e;f` FROM t; SELECT 'it''s; ok'"),
            ["SELECT 'a;b', \"c;d\", `e;f` FROM t", "SELECT 'it''s; ok'"]
        );
        assert_eq!(
            split_statements("-- intro; still a comment\nSELECT 1 /* a ; b */ ;\n-- trailing;"),
            ["-- intro; still a comment\nSELECT 1 /* a ; b */"]
        );
        assert!(split_statements("  ;; -- nothing\n/* here */ ;").is_empty());
        assert_eq!(
            split_statements("SELECT 'unclosed; text"),
            ["SELECT 'unclosed; text"]
        );
        assert_eq!(
            split_statements("SELECT 1 /* open; comment"),
            ["SELECT 1 /* open; comment"]
        );
        assert_eq!(
            split_statements("SELECT 1 -- no newline;"),
            ["SELECT 1 -- no newline;"]
        );
    }

    #[test]
    fn keeps_dollar_quoted_bodies_whole() {
        let sql = "CREATE FUNCTION f(x BIGINT) RETURNS BIGINT LANGUAGE python AS $$\n\
                   def f(x):\n    return x + 1; # not the end\n$$;\n\
                   SELECT f(1)";
        let statements = split_statements(sql);
        assert_eq!(statements.len(), 2);
        assert!(statements[0].ends_with("$$"));
        assert_eq!(statements[1], "SELECT f(1)");

        let tagged = "SELECT $body$ a; $$ b; $body$; SELECT 2";
        assert_eq!(
            split_statements(tagged),
            ["SELECT $body$ a; $$ b; $body$", "SELECT 2"]
        );
        assert_eq!(
            split_statements("SELECT $$ open; body"),
            ["SELECT $$ open; body"]
        );
    }

    #[test]
    fn dollars_in_names_and_placeholders_are_not_quotes() {
        assert_eq!(
            split_statements("SELECT * FROM team$reports$x.t WHERE id = $1; SELECT $2"),
            ["SELECT * FROM team$reports$x.t WHERE id = $1", "SELECT $2"]
        );
        assert_eq!(
            split_statements("SELECT $1$; SELECT 3"),
            ["SELECT $1$", "SELECT 3"]
        );
    }

    #[test]
    fn recognises_comment_only_text() {
        assert!(has_code("SELECT 1"));
        assert!(has_code("-- c\n SELECT 1"));
        assert!(!has_code("-- only"));
        assert!(!has_code("/* a */ -- b\n  "));
        assert!(!has_code("/* never closed"));
        assert!(!has_code(""));
    }
}
