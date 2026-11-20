//! Syntax highlighting for the file preview: syntect with bat's extended
//! grammar set via `two-face` (syntect's own defaults lack TypeScript, TOML,
//! Dockerfile, …), on the pure-Rust `regex-fancy` engine — no oniguruma C
//! build on Windows. Foreground colors only: the terminal keeps its own
//! background, and unknown file types fall back to plain lines.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU8, Ordering};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use crate::state::SyntaxTheme;

/// Lines longer than this skip highlighting entirely. syntect's `regex-fancy`
/// backtracking engine is roughly quadratic in line length on pathological
/// input (minified JS/CSS, generated data) — a single multi-hundred-KB line
/// can take tens of seconds and freeze the render thread.
const MAX_HIGHLIGHT_LINE_LEN: usize = 2000;

/// Grammar + theme assets, loaded once (the bundled dumps take a few ms).
/// EVERY bundled theme is built up front: switching the color or syntax theme
/// must not stall a render, and the dumps are small.
fn assets() -> &'static (SyntaxSet, BTreeMap<String, Theme>) {
    static ASSETS: OnceLock<(SyntaxSet, BTreeMap<String, Theme>)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        (
            two_face::syntax::extra_newlines(),
            ThemeSet::load_defaults().themes,
        )
    })
}

/// The configured theme per background, as `SyntaxTheme` discriminants. Like
/// `ui::set_color_theme`, the viewer process sets these from `state.json` at
/// startup and on its heartbeat.
static DARK_THEME: AtomicU8 = AtomicU8::new(SyntaxTheme::OceanDark as u8);
static LIGHT_THEME: AtomicU8 = AtomicU8::new(SyntaxTheme::InspiredGitHub as u8);

pub fn set_themes(dark: SyntaxTheme, light: SyntaxTheme) {
    DARK_THEME.store(dark as u8, Ordering::Relaxed);
    LIGHT_THEME.store(light as u8, Ordering::Relaxed);
}

fn configured_theme(light: bool) -> SyntaxTheme {
    let stored = if light { &LIGHT_THEME } else { &DARK_THEME }.load(Ordering::Relaxed);
    SyntaxTheme::ALL
        .into_iter()
        .find(|theme| *theme as u8 == stored)
        .unwrap_or(SyntaxTheme::default_for(light))
}

/// A bundled theme by choice, falling back to that mode's historical default
/// (and then to any theme) should syntect ever rename one.
fn theme_for(theme: SyntaxTheme) -> &'static Theme {
    static FALLBACK: OnceLock<Theme> = OnceLock::new();
    let (_, themes) = assets();
    themes
        .get(theme.syntect_name())
        .or_else(|| themes.get(SyntaxTheme::default_for(theme.is_light()).syntect_name()))
        .or_else(|| themes.values().next())
        .unwrap_or_else(|| FALLBACK.get_or_init(Theme::default))
}

/// The grammar set plus the configured theme for the active color theme.
/// syntect colors foregrounds only, so a dark grammar theme on a light
/// terminal is exactly the washed-out case the light palette exists to fix —
/// which is why each background keeps its own choice.
fn syntaxes_and_theme() -> (&'static SyntaxSet, &'static Theme) {
    let (syntaxes, _) = assets();
    (syntaxes, theme_for(configured_theme(crate::ui::is_light())))
}

/// Highlight `text` for a file called `name`, up to `max` lines. `None` when
/// no grammar matches (caller falls back to plain lines).
pub fn highlight(name: &str, text: &str, max: usize) -> Option<Vec<Line<'static>>> {
    let (syntaxes, theme) = syntaxes_and_theme();
    let ext = name.rsplit('.').next().unwrap_or("");
    let syntax = syntaxes
        .find_syntax_by_extension(ext)
        .or_else(|| syntaxes.find_syntax_by_extension(name))
        .or_else(|| {
            text.lines()
                .next()
                .and_then(|l| syntaxes.find_syntax_by_first_line(l))
        })?;

    let mut highlighter = HighlightLines::new(syntax, theme);
    let mut lines = Vec::new();
    for raw in LinesWithEndings::from(text).take(max) {
        if raw.len() > MAX_HIGHLIGHT_LINE_LEN {
            lines.push(Line::raw(raw.trim_end_matches(['\n', '\r']).to_string()));
            continue;
        }
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

/// Stateful per-line highlighter (for diff rendering, where old/new file
/// contexts advance independently). Plain spans when no grammar matches.
pub struct LineHighlighter {
    inner: Option<HighlightLines<'static>>,
}

impl LineHighlighter {
    pub fn new(name: &str) -> Self {
        let (syntaxes, theme) = syntaxes_and_theme();
        let ext = name.rsplit('.').next().unwrap_or("");
        let syntax = syntaxes
            .find_syntax_by_extension(ext)
            .or_else(|| syntaxes.find_syntax_by_extension(name));
        Self {
            inner: syntax.map(|s| HighlightLines::new(s, theme)),
        }
    }

    /// Highlight one line (no trailing newline in, none out).
    pub fn line(&mut self, text: &str) -> Vec<Span<'static>> {
        let Some(hl) = self.inner.as_mut() else {
            return vec![Span::raw(text.to_string())];
        };
        if text.len() > MAX_HIGHLIGHT_LINE_LEN {
            return vec![Span::raw(text.to_string())];
        }
        let (syntaxes, _) = syntaxes_and_theme();
        let with_nl = format!("{text}\n");
        match hl.highlight_line(&with_nl, syntaxes) {
            Ok(regions) => regions
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
                .collect(),
            Err(_) => vec![Span::raw(text.to_string())],
        }
    }
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
        assert!(
            highlight(
                "app.ts",
                "const x: string = \"hi\";
",
                10
            )
            .is_some()
        );
        assert!(
            highlight(
                "Cargo.toml",
                "[package]
name = \"x\"
",
                10
            )
            .is_some()
        );
    }

    #[test]
    fn unknown_extensions_fall_back_to_none() {
        assert!(highlight("data.qqzz", "gibberish content\n", 10).is_none());
    }

    #[test]
    fn pathologically_long_lines_skip_highlighting_instead_of_hanging() {
        let long_line = format!(
            "const x = \"{}\";\n",
            "a".repeat(MAX_HIGHLIGHT_LINE_LEN + 1)
        );
        let lines = highlight("bundle.min.js", &long_line, 10).expect("js grammar matches");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), long_line.trim_end_matches('\n'));

        let mut hl = LineHighlighter::new("bundle.min.js");
        let spans = hl.line(long_line.trim_end_matches('\n'));
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content, long_line.trim_end_matches('\n'));
    }

    #[test]
    fn every_selectable_theme_is_bundled_with_syntect() {
        let (_, themes) = assets();
        for theme in SyntaxTheme::ALL {
            assert!(themes.contains_key(theme.syntect_name()), "{theme:?}");
            assert_eq!(theme_for(theme).name, themes[theme.syntect_name()].name);
        }
    }
}
