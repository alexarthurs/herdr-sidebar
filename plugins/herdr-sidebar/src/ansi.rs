//! Minimal ANSI-SGR → ratatui converter for `git diff --color=always`
//! output (and anything else colored the same way): 16-color + bright,
//! 256-color, bold/dim/italic/underline/reverse, resets. Unknown escape
//! sequences are dropped; everything else passes through as styled spans.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Convert one chunk of ANSI-colored text into ratatui lines.
pub fn to_lines(input: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for raw in input.lines() {
        lines.push(parse_line(raw));
    }
    if lines.is_empty() {
        lines.push(Line::raw(""));
    }
    lines
}

fn parse_line(raw: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();
    let mut text = String::new();
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            if c != '\r' {
                text.push(c);
            }
            continue;
        }
        // Escape sequence: only CSI ... 'm' (SGR) is interpreted.
        if chars.peek() != Some(&'[') {
            continue;
        }
        chars.next();
        let mut params = String::new();
        let mut terminator = None;
        for c in chars.by_ref() {
            if c.is_ascii_digit() || c == ';' || c == ':' {
                params.push(c);
            } else {
                terminator = Some(c);
                break;
            }
        }
        if terminator != Some('m') {
            continue; // cursor moves, erases, … — irrelevant for static text
        }
        if !text.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut text), style));
        }
        style = apply_sgr(style, &params);
    }
    if !text.is_empty() {
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

fn apply_sgr(mut style: Style, params: &str) -> Style {
    let codes: Vec<u16> = params
        .split([';', ':'])
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let codes = if codes.is_empty() { vec![0] } else { codes };
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            7 => style = style.add_modifier(Modifier::REVERSED),
            22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
            23 => style = style.remove_modifier(Modifier::ITALIC),
            24 => style = style.remove_modifier(Modifier::UNDERLINED),
            27 => style = style.remove_modifier(Modifier::REVERSED),
            30..=37 => style = style.fg(basic_color(codes[i] - 30, false)),
            39 => style = style.fg(Color::Reset),
            40..=47 => style = style.bg(basic_color(codes[i] - 40, false)),
            49 => style = style.bg(Color::Reset),
            90..=97 => style = style.fg(basic_color(codes[i] - 90, true)),
            100..=107 => style = style.bg(basic_color(codes[i] - 100, true)),
            38 | 48 => {
                // 38;5;n (indexed) or 38;2;r;g;b (truecolor)
                let is_fg = codes[i] == 38;
                if codes.get(i + 1) == Some(&5)
                    && let Some(&n) = codes.get(i + 2)
                {
                    let c = Color::Indexed(n as u8);
                    style = if is_fg { style.fg(c) } else { style.bg(c) };
                    i += 2;
                } else if codes.get(i + 1) == Some(&2)
                    && let (Some(&r), Some(&g), Some(&b)) =
                        (codes.get(i + 2), codes.get(i + 3), codes.get(i + 4))
                {
                    let c = Color::Rgb(r as u8, g as u8, b as u8);
                    style = if is_fg { style.fg(c) } else { style.bg(c) };
                    i += 4;
                }
            }
            _ => {}
        }
        i += 1;
    }
    style
}

fn basic_color(n: u16, bright: bool) -> Color {
    match (n, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::Gray,
        (0, true) => Color::DarkGray,
        (1, true) => Color::LightRed,
        (2, true) => Color::LightGreen,
        (3, true) => Color::LightYellow,
        (4, true) => Color::LightBlue,
        (5, true) => Color::LightMagenta,
        (6, true) => Color::LightCyan,
        _ => Color::White,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        let lines = to_lines("hello\nworld");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].to_string(), "hello");
    }

    #[test]
    fn git_diff_colors_map() {
        // Green addition, red removal — git's default diff palette.
        let lines = to_lines("\u{1b}[32m+added\u{1b}[m\n\u{1b}[31m-removed\u{1b}[m");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
        assert_eq!(lines[0].to_string(), "+added");
        assert_eq!(lines[1].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn bold_headers_and_resets() {
        let lines = to_lines("\u{1b}[1mdiff --git a/x b/x\u{1b}[m plain");
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[0].to_string(), "diff --git a/x b/x plain");
    }

    #[test]
    fn indexed_and_truecolor() {
        let lines = to_lines("\u{1b}[38;5;208morange\u{1b}[m \u{1b}[38;2;10;20;30mrgb\u{1b}[m");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(208)));
        assert_eq!(lines[0].spans[1].style.fg, None);
        assert_eq!(lines[0].spans[2].style.fg, Some(Color::Rgb(10, 20, 30)));
    }

    #[test]
    fn non_sgr_escapes_are_dropped() {
        let lines = to_lines("a\u{1b}[2Kb\u{1b}[Hc");
        assert_eq!(lines[0].to_string(), "abc");
    }
}
