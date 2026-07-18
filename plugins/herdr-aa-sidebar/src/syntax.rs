//! Syntax highlighting for the file preview: syntect with bat's extended
//! grammar set via `two-face` (syntect's own defaults lack TypeScript, TOML,
//! Dockerfile, …), on the pure-Rust `regex-fancy` engine — no oniguruma C
//! build on Windows. Foreground colors only: the terminal keeps its own
//! background, and unknown file types fall back to plain lines.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

/// Grammar + theme assets, loaded once (the bundled dumps take a few ms).
fn assets() -> &'static (SyntaxSet, Theme) {
    static ASSETS: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let syntaxes = two_face::syntax::extra_newlines();
        let mut themes = ThemeSet::load_defaults();
        let theme = themes
            .themes
            .remove("base16-ocean.dark")
            .or_else(|| themes.themes.pop_first().map(|(_, t)| t))
            .unwrap_or_default();
        (syntaxes, theme)
    })
}

/// Highlight `text` for a file called `name`, up to `max` lines. `None` when
/// no grammar matches (caller falls back to plain lines).
pub fn highlight(name: &str, text: &str, max: usize) -> Option<Vec<Line<'static>>> {
    let (syntaxes, theme) = assets();
    let ext = name.rsplit('.').next().unwrap_or("");
    let syntax = syntaxes
        .find_syntax_by_extension(ext)
        .or_else(|| syntaxes.find_syntax_by_extension(name))
        .or_else(|| text.lines().next().and_then(|l| syntaxes.find_syntax_by_first_line(l)))?;

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    for raw in LinesWithEndings::from(text).take(max) {
        let Ok(regions) = highlighter.highlight_line(raw, syntaxes) else {
            lines.push(Line::raw(raw.trim_end_matches(['\n', '\r']).to_string()));
            continue;
        };
        let spans: Vec<Span<'static>> = regions
            .into_iter()
            .filter_map(|(style, chunk)| {
                let chunk = chunk.trim_end_matches(['\n', '\r']);
                if chunk.is_empty() {
                    return None;
                }
                let fg = style.foreground;
                let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
                if style.font_style.contains(FontStyle::BOLD) {
                    out = out.add_modifier(Modifier::BOLD);
                }
                if style.font_style.contains(FontStyle::ITALIC) {
                    out = out.add_modifier(Modifier::ITALIC);
                }
                Some(Span::styled(chunk.to_string(), out))
            })
            .collect();
        lines.push(Line::from(spans));
    }
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_extensions_highlight_with_colors() {
        let lines = highlight("main.rs", "fn main() {}\n", 10).expect("rust grammar");
        assert_eq!(lines.len(), 1);
        // The `fn` keyword must carry a non-default foreground color.
        let colored = lines[0]
            .spans
            .iter()
            .any(|s| s.content.contains("fn") && s.style.fg.is_some());
        assert!(colored, "expected a colored keyword span");
        assert_eq!(lines[0].to_string(), "fn main() {}");
    }

    #[test]
    fn extended_grammars_cover_typescript_and_toml() {
        assert!(highlight("app.ts", "const x: string = \"hi\";
", 10).is_some());
        assert!(highlight("Cargo.toml", "[package]
name = \"x\"
", 10).is_some());
    }

    #[test]
    fn unknown_extensions_fall_back_to_none() {
        assert!(highlight("data.qqzz", "gibberish content\n", 10).is_none());
    }
}
