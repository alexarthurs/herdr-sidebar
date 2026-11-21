//! Syntax highlighting for the file preview: syntect with bat's extended
//! grammar set via `two-face` (syntect's own defaults lack TypeScript, TOML,
//! Dockerfile, …), on the pure-Rust `regex-fancy` engine — no oniguruma C
//! build on Windows. Foreground colors only by default: the terminal keeps its
//! own background unless the user opts into the theme's (`syntax_background`),
//! and unknown file types fall back to plain lines.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Highlighter, Style as SyntectStyle, Theme, ThemeSet};
use syntect::parsing::{Scope, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::state::{State, SyntaxTheme};

/// Lines longer than this skip highlighting entirely. syntect's `regex-fancy`
/// backtracking engine is roughly quadratic in line length on pathological
/// input (minified JS/CSS, generated data) — a single multi-hundred-KB line
/// can take tens of seconds and freeze the render thread.
const MAX_HIGHLIGHT_LINE_LEN: usize = 2000;

/// Grammar + theme assets, loaded once (the bundled dumps take a few ms).
/// EVERY bundled theme is built up front: switching the color or syntax theme
/// must not stall a render, and the dumps are small. The JetBrains palettes
/// from [`crate::themes`] join syntect's set under their own names.
fn assets() -> &'static (SyntaxSet, BTreeMap<String, Theme>) {
    static ASSETS: OnceLock<(SyntaxSet, BTreeMap<String, Theme>)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults().themes;
        for spec in [&crate::themes::DARCULA, &crate::themes::INTELLIJ_DARK] {
            themes.insert(spec.name.to_string(), spec.build());
        }
        (two_face::syntax::extra_newlines(), themes)
    })
}

/// The configured theme per background, as `SyntaxTheme` discriminants. Like
/// `ui::set_color_theme`, the viewer process sets these from `state.json` at
/// startup and on its heartbeat.
static DARK_THEME: AtomicU8 = AtomicU8::new(SyntaxTheme::OceanDark as u8);
static LIGHT_THEME: AtomicU8 = AtomicU8::new(SyntaxTheme::InspiredGitHub as u8);
static THEMED_BACKGROUND: AtomicBool = AtomicBool::new(false);

pub fn apply_settings(state: &State) {
    DARK_THEME.store(state.syntax_dark as u8, Ordering::Relaxed);
    LIGHT_THEME.store(state.syntax_light as u8, Ordering::Relaxed);
    THEMED_BACKGROUND.store(state.syntax_background, Ordering::Relaxed);
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

/// Editor-style colors for the preview pane, present only when the user
/// opted in (`syntax_background`) and the theme declares a background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewColors {
    pub background: Color,
    /// Text without a highlight of its own (plain files, header, footer).
    pub foreground: Option<Color>,
    /// Line-number color (`gutterForeground`); `None` keeps the dim default.
    pub gutter: Option<Color>,
}

pub fn preview_colors() -> Option<PreviewColors> {
    if !THEMED_BACKGROUND.load(Ordering::Relaxed) {
        return None;
    }
    theme_preview_colors(syntaxes_and_theme().1)
}

fn theme_preview_colors(theme: &Theme) -> Option<PreviewColors> {
    let rgb = |c: syntect::highlighting::Color| Color::Rgb(c.r, c.g, c.b);
    Some(PreviewColors {
        background: rgb(theme.settings.background?),
        foreground: theme.settings.foreground.map(rgb),
        gutter: theme.settings.gutter_foreground.map(rgb),
    })
}

/// Byte ranges of `[LABEL]` shortcut references in a Markdown line — text the
/// grammar leaves unscoped (IntelliJ shows them as link labels). Task-list
/// boxes (`[ ]`, `[x]`, `[~]`), inline links `[a](b)`, full references
/// `[a][b]`, definitions `[a]:` and images `![a]` are excluded.
fn shortcut_references(line: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'[' || (i > 0 && matches!(bytes[i - 1], b'!' | b']' | b'\\')) {
            i += 1;
            continue;
        }
        let Some(len) = line[i + 1..].find([']', '[']) else {
            break;
        };
        let close = i + 1 + len;
        if bytes[close] == b'[' {
            i = close;
            continue;
        }
        let label = &line[i + 1..close];
        let next = bytes.get(close + 1);
        let task_box = label.trim().len() <= 1;
        if !task_box
            && label.chars().any(char::is_alphanumeric)
            && !matches!(next, Some(b'(' | b'[' | b':'))
        {
            out.push(i..close + 1);
        }
        i = close + 1;
    }
    out
}

/// Restyle the plain-text parts of `regions` that fall inside `ranges`.
/// Only regions drawn in the theme's default foreground change, so text that
/// is already code, emphasis or a quote keeps its own color.
fn restyle_ranges<'a>(
    regions: Vec<(SyntectStyle, &'a str)>,
    ranges: &[std::ops::Range<usize>],
    plain: Option<syntect::highlighting::Color>,
    style: SyntectStyle,
) -> Vec<(SyntectStyle, &'a str)> {
    if ranges.is_empty() {
        return regions;
    }
    let mut out = Vec::with_capacity(regions.len() + ranges.len() * 2);
    let mut offset = 0;
    for (region_style, text) in regions {
        let start = offset;
        offset += text.len();
        let eligible = Some(region_style.foreground) == plain && region_style.font_style.is_empty();
        if !eligible {
            out.push((region_style, text));
            continue;
        }
        let mut cut = start;
        for range in ranges {
            let (lo, hi) = (range.start.max(start), range.end.min(offset));
            if lo >= hi {
                continue;
            }
            if lo > cut {
                out.push((region_style, &text[cut - start..lo - start]));
            }
            out.push((style, &text[lo - start..hi - start]));
            cut = hi;
        }
        if cut < offset {
            out.push((region_style, &text[cut - start..]));
        }
    }
    out
}

/// Highlight `text` for a file called `name`, up to `max` lines. `None` when
/// no grammar matches (caller falls back to plain lines).
pub fn highlight(name: &str, text: &str, max: usize) -> Option<Vec<Line<'static>>> {
    let (syntaxes, theme) = syntaxes_and_theme();
    highlight_with(syntaxes, theme, name, text, max)
}

fn highlight_with(
    syntaxes: &SyntaxSet,
    theme: &Theme,
    name: &str,
    text: &str,
    max: usize,
) -> Option<Vec<Line<'static>>> {
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
    // `[LABEL]` gets the theme's style for a synthesized scope; a theme
    // without that rule leaves it as the plain foreground it already was.
    // The scope has its OWN top-level name: a `meta.link.*` variant was
    // matched by base16-ocean.dark's generic `meta.link` rule.
    let shortcut = (syntax.name == "Markdown")
        .then(|| {
            let scopes = [
                "text.html.markdown",
                "herdr-sidebar.shortcut-reference.markdown",
            ]
            .map(|s| Scope::new(s).expect("static scope"));
            Highlighter::new(theme).style_for_stack(&scopes)
        })
        .filter(|style| Some(style.foreground) != theme.settings.foreground);
    let mut lines = Vec::new();
    for raw in LinesWithEndings::from(text).take(max) {
        if raw.len() > MAX_HIGHLIGHT_LINE_LEN {
            lines.push(Line::raw(raw.trim_end_matches(['\n', '\r']).to_string()));
            continue;
        }
        let Ok(mut regions) = highlighter.highlight_line(raw, syntaxes) else {
            lines.push(Line::raw(raw.trim_end_matches(['\n', '\r']).to_string()));
            continue;
        };
        if let Some(style) = shortcut {
            regions = restyle_ranges(
                regions,
                &shortcut_references(raw),
                theme.settings.foreground,
                style,
            );
        }
        lines.push(Line::from(to_spans(regions)));
    }
    Some(lines)
}

/// Foreground-only spans for one highlighted line (no trailing newline).
fn to_spans(regions: Vec<(SyntectStyle, &str)>) -> Vec<Span<'static>> {
    regions
        .into_iter()
        .filter_map(|(style, chunk)| {
            let chunk = chunk.trim_end_matches(['\n', '\r']);
            if chunk.is_empty() {
                return None;
            }
            let fg = style.foreground;
            let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
            for (font, modifier) in [
                (FontStyle::BOLD, Modifier::BOLD),
                (FontStyle::ITALIC, Modifier::ITALIC),
                (FontStyle::UNDERLINE, Modifier::UNDERLINED),
            ] {
                if style.font_style.contains(font) {
                    out = out.add_modifier(modifier);
                }
            }
            Some(Span::styled(chunk.to_string(), out))
        })
        .collect()
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
            Ok(regions) => to_spans(regions),
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
    fn every_selectable_theme_is_bundled() {
        let (_, themes) = assets();
        for theme in SyntaxTheme::ALL {
            assert!(themes.contains_key(theme.syntect_name()), "{theme:?}");
            assert_eq!(theme_for(theme).name, themes[theme.syntect_name()].name);
        }
    }

    fn themed_markdown(theme: SyntaxTheme, text: &str) -> Vec<Line<'static>> {
        highlight_with(&assets().0, theme_for(theme), "README.md", text, 100).expect("md grammar")
    }

    fn fg_of(line: &Line<'_>, needle: &str) -> Option<Color> {
        line.spans
            .iter()
            .find(|s| s.content.contains(needle))
            .and_then(|s| s.style.fg)
    }

    fn distinct_fgs(line: &Line<'_>) -> std::collections::HashSet<Color> {
        line.spans
            .iter()
            .filter(|s| !s.content.trim().is_empty())
            .filter_map(|s| s.style.fg)
            .collect()
    }

    /// base16-ocean.dark colors a whole list item or quote one flat color;
    /// Darcula colors only the markers and keeps the text plain.
    #[test]
    fn darcula_markdown_lists_and_quotes_are_not_one_flat_color() {
        let lines = themed_markdown(
            SyntaxTheme::Darcula,
            "- item one\n1. numbered\n> quoted `code`\n",
        );
        for line in &lines {
            assert!(
                distinct_fgs(line).len() >= 2,
                "single-color line {:?}",
                line.to_string()
            );
        }
        assert_eq!(fg_of(&lines[0], "item"), Some(Color::Rgb(0xA9, 0xB7, 0xC6)));
    }

    #[test]
    fn darcula_markdown_heading_is_italic_and_inline_code_green() {
        let lines = themed_markdown(SyntaxTheme::Darcula, "# Title\n\nsome `code` here\n");
        let title = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("Title"))
            .unwrap();
        assert!(title.style.add_modifier.contains(Modifier::ITALIC));
        let code = lines[2].spans.iter().find(|s| s.content == "code").unwrap();
        assert_eq!(code.style.fg, Some(Color::Rgb(0x6A, 0x87, 0x59)));
    }

    #[test]
    fn shortcut_references_skip_task_boxes_links_and_definitions() {
        let line = "- [ ] #146 [BUG] {Leads} [x] [~] [a](u) [b][c] ![i] [d]: \\[e] [FEATURE]";
        let found: Vec<&str> = shortcut_references(line)
            .into_iter()
            .map(|r| &line[r])
            .collect();
        assert_eq!(found, ["[BUG]", "[FEATURE]"]);
    }

    #[test]
    fn intellij_dark_colors_bracketed_labels_like_the_ide() {
        let text = "- [ ] #146 🟡 [BUG] {Leads} falla ~50 % con `x` real\n";
        let lines = themed_markdown(SyntaxTheme::IntelliJDark, text);
        let orange = Some(Color::Rgb(0xCF, 0x8E, 0x6D));
        let plain = Some(Color::Rgb(0xBC, 0xBE, 0xC4));
        assert_eq!(fg_of(&lines[0], "[BUG]"), orange);
        assert_eq!(fg_of(&lines[0], "-"), orange);
        assert_eq!(fg_of(&lines[0], "{Leads}"), plain);
        assert_eq!(fg_of(&lines[0], "[ ]"), plain, "task boxes stay plain");
        // A lone `~` opens a never-closed strikethrough in the grammar; the
        // rest of the line must keep the normal text color.
        assert_eq!(fg_of(&lines[0], " real"), plain);
        assert_eq!(lines[0].to_string(), text.trim_end());
    }

    #[test]
    fn themes_without_the_shortcut_rule_leave_labels_untouched() {
        let lines = themed_markdown(SyntaxTheme::OceanDark, "Plain [BUG] text\n");
        assert_eq!(lines[0].spans.len(), 1, "{:?}", lines[0].spans);
    }

    #[test]
    fn jetbrains_themes_expose_editor_background_and_gutter() {
        let colors = theme_preview_colors(theme_for(SyntaxTheme::IntelliJDark)).unwrap();
        assert_eq!(colors.background, Color::Rgb(0x1E, 0x1F, 0x22));
        assert_eq!(colors.foreground, Some(Color::Rgb(0xBC, 0xBE, 0xC4)));
        assert_eq!(colors.gutter, Some(Color::Rgb(0x4B, 0x50, 0x59)));
    }
}
