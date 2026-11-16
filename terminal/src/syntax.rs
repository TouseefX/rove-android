//! Small display-only lexer. The original source always goes to Luau unchanged.
use crate::theme::Theme;
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};

pub fn line(source: &str, theme: Theme) -> Line<'static> {
    let mut spans = Vec::new();
    let mut offset = 0;
    while offset < source.len() {
        let tail = &source[offset..];
        if tail.starts_with("--") {
            spans.push(Span::styled(
                tail.to_owned(),
                Style::default().fg(theme.muted),
            ));
            break;
        }
        let c = tail.chars().next().unwrap();
        let mut size = c.len_utf8();
        let mut style = Style::default().fg(theme.text);
        if matches!(c, '\'' | '"' | '`') {
            let mut escape = false;
            for next in tail[c.len_utf8()..].chars() {
                size += next.len_utf8();
                if !escape && next == c {
                    break;
                }
                escape = !escape && next == '\\';
            }
            style = style.fg(theme.green);
        } else if c.is_ascii_digit() {
            size = tail
                .char_indices()
                .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '.' || *c == '_')
                .map(|(i, c)| i + c.len_utf8())
                .last()
                .unwrap_or(size);
            style = style.fg(theme.amber);
        } else if c.is_alphabetic() || c == '_' {
            size = tail
                .char_indices()
                .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
                .map(|(i, c)| i + c.len_utf8())
                .last()
                .unwrap_or(size);
            if matches!(
                &tail[..size],
                "local"
                    | "function"
                    | "end"
                    | "return"
                    | "if"
                    | "then"
                    | "else"
                    | "elseif"
                    | "for"
                    | "in"
                    | "do"
                    | "while"
                    | "repeat"
                    | "until"
                    | "and"
                    | "or"
                    | "not"
                    | "true"
                    | "false"
                    | "nil"
                    | "break"
                    | "continue"
            ) {
                style = style.fg(theme.accent).add_modifier(Modifier::BOLD);
            } else if tail[size..].starts_with('(') {
                style = style.fg(theme.blue);
            }
        } else if matches!(
            c,
            '(' | ')' | '{' | '}' | '[' | ']' | '=' | '+' | '-' | '*' | '/' | ':' | '.'
        ) {
            style = style.fg(theme.muted);
        }
        spans.push(Span::styled(tail[..size].to_owned(), style));
        offset += size;
    }
    Line::from(spans)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn highlighting_preserves_quotes_unicode_and_comments() {
        for text in [
            "local world = '世界' -- hello",
            r#"print("escaped \" quote"); return 42"#,
            "local α = 12.3",
        ] {
            let result = line(text, crate::theme::THEMES[0]);
            assert_eq!(
                result
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>(),
                text
            );
            assert!(result.spans.len() > 1);
        }
    }
}
