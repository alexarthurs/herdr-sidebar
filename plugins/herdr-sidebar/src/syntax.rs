//! Syntax highlighting for the file preview: syntect with bat's extended
//! grammar set via `two-face` (syntect's own defaults lack TypeScript, TOML,
//! Dockerfile, …), on the pure-Rust `regex-fancy` engine — no oniguruma C
//! build on Windows. Foreground colors only: the terminal keeps its own
//! background, and unknown file types fall back to plain lines.

use std::collections::{BTreeMap, HashMap};
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Highlighter, Style as SyntectStyle, Theme, ThemeSet};
use syntect::parsing::{Scope, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Lines longer than this skip highlighting entirely. syntect's `regex-fancy`
/// backtracking engine is roughly quadratic in line length on pathological
/// input (minified JS/CSS, generated data) — a single multi-hundred-KB line
/// can take tens of seconds and freeze the render thread.
const MAX_HIGHLIGHT_LINE_LEN: usize = 2000;

/// The historical defaults, used whenever no theme is configured or the
/// configured one fails to load.
pub const DEFAULT_DARK_THEME: &str = "base16-ocean.dark";
pub const DEFAULT_LIGHT_THEME: &str = "InspiredGitHub";

/// Themes compiled into the binary on top of syntect's defaults.
const EXTRA_THEMES: &[(&str, &[u8])] = &[
    (
        "Darcula",
        include_bytes!("../assets/themes/Darcula.tmTheme"),
    ),
    (
        "IntelliJ Dark",
        include_bytes!("../assets/themes/IntelliJDark.tmTheme"),
    ),
];

struct Assets {
    syntaxes: SyntaxSet,
    /// Every bundled theme by name; built up front so switching the color
    /// theme never stalls a render (the dumps are small).
    themes: BTreeMap<String, Theme>,
}

fn assets() -> &'static Assets {
    static ASSETS: OnceLock<Assets> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let mut themes = ThemeSet::load_defaults().themes;
        for (name, bytes) in EXTRA_THEMES {
            if let Ok(theme) = ThemeSet::load_from_reader(&mut Cursor::new(*bytes)) {
                themes.insert((*name).to_string(), theme);
            }
        }
        Assets {
            syntaxes: two_face::syntax::extra_newlines(),
            themes,
        }
    })
}

/// Names a user can pick without a file, bundled extras first.
pub fn bundled_theme_names() -> Vec<String> {
    let themes = &assets().themes;
    let mut names: Vec<String> = EXTRA_THEMES
        .iter()
        .map(|(name, _)| name.to_string())
        .filter(|name| themes.contains_key(name))
        .collect();
    names.extend(
        themes
            .keys()
            .filter(|n| !names.contains(n))
            .cloned()
            .collect::<Vec<_>>(),
    );
    names
}

fn default_theme(light: bool) -> &'static Theme {
    static FALLBACK: OnceLock<Theme> = OnceLock::new();
    let themes = &assets().themes;
    let name = if light {
        DEFAULT_LIGHT_THEME
    } else {
        DEFAULT_DARK_THEME
    };
    themes
        .get(name)
        .or_else(|| themes.values().next())
        .unwrap_or_else(|| FALLBACK.get_or_init(Theme::default))
}

/// User `.tmTheme` files, keyed by path and reloaded when the file's mtime
/// changes. Each load is leaked to get the `'static` lifetime the stateful
/// highlighters need; that is bounded by how often the user edits the file.
type FileThemes = HashMap<PathBuf, (Option<SystemTime>, &'static Theme)>;

fn load_theme_file(path: &Path) -> Option<&'static Theme> {
    static CACHE: OnceLock<Mutex<FileThemes>> = OnceLock::new();
    let mtime = std::fs::metadata(path).ok()?.modified().ok();
    let mut cache = CACHE.get_or_init(Default::default).lock().ok()?;
    if let Some((cached, theme)) = cache.get(path)
        && *cached == mtime
    {
        return Some(theme);
    }
    let theme: &'static Theme = Box::leak(Box::new(ThemeSet::get_theme(path).ok()?));
    cache.insert(path.to_path_buf(), (mtime, theme));
    Some(theme)
}

fn expand_home(spec: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    if spec == "~" {
        return home()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(spec));
    }
    if let Some(rest) = spec.strip_prefix("~/").or_else(|| spec.strip_prefix("~\\"))
        && let Some(home) = home()
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(spec)
}

/// Resolve a configured theme: a bundled name (case-insensitive) or a path to
/// a `.tmTheme` file. Anything that does not resolve falls back to the
/// historical default for that mode, so a bad setting never breaks previews.
pub fn resolve_theme(spec: Option<&str>, light: bool) -> &'static Theme {
    let Some(spec) = spec.map(str::trim).filter(|s| !s.is_empty()) else {
        return default_theme(light);
    };
    let themes = &assets().themes;
    if let Some(theme) = themes.get(spec).or_else(|| {
        themes
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(spec))
            .map(|(_, theme)| theme)
    }) {
        return theme;
    }
    load_theme_file(&expand_home(spec)).unwrap_or_else(|| default_theme(light))
}

/// True when `spec` names a bundled theme or a loadable `.tmTheme` file.
pub fn theme_spec_is_valid(spec: &str) -> bool {
    let themes = &assets().themes;
    themes
        .keys()
        .any(|name| name.eq_ignore_ascii_case(spec.trim()))
        || load_theme_file(&expand_home(spec.trim())).is_some()
}

/// What the ⚙ Settings row shows for the active color theme's mode.
pub fn settings_value() -> String {
    let light = crate::ui::is_light();
    match crate::state::load_syntax_themes().for_mode(light) {
        Some(spec) if theme_spec_is_valid(spec) => spec.to_string(),
        Some(_) => "default (invalid)".into(),
        None => "default".into(),
    }
}

/// Advance the active mode's theme: default → each bundled theme → default.
/// A custom file path (set by editing `syntax-theme.json`) restarts the cycle.
pub fn cycle_configured_theme() {
    let light = crate::ui::is_light();
    let current = crate::state::load_syntax_themes()
        .for_mode(light)
        .map(str::to_string);
    let names = bundled_theme_names();
    let index = current
        .as_deref()
        .and_then(|spec| names.iter().position(|n| n.eq_ignore_ascii_case(spec)));
    let next = match (current.is_some(), index) {
        (false, _) | (true, None) => names.first().cloned(),
        (true, Some(i)) => names.get(i + 1).cloned(),
    };
    crate::state::save_syntax_theme(light, next.as_deref());
}

/// The grammar set plus the configured theme for the active color theme.
/// syntect colors foregrounds only, so a dark grammar theme on a light
/// terminal is exactly the washed-out case the light palette exists to fix.
/// The setting is re-read per document, so a change applies to the next
/// preview without restarting the viewer.
fn syntaxes_and_theme() -> (&'static SyntaxSet, &'static Theme) {
    (&assets().syntaxes, active_theme())
}

/// The configured theme for the active color theme's mode.
fn active_theme() -> &'static Theme {
    let light = crate::ui::is_light();
    resolve_theme(configured().for_mode(light), light)
}

/// `syntax-theme.json`, re-read at most every two seconds: the preview asks
/// for its colors on every frame, and a file read per frame is wasteful.
fn configured() -> crate::state::SyntaxThemes {
    static CACHE: OnceLock<Mutex<Option<(Instant, crate::state::SyntaxThemes)>>> = OnceLock::new();
    let Ok(mut cache) = CACHE.get_or_init(Default::default).lock() else {
        return crate::state::load_syntax_themes();
    };
    match cache.as_ref() {
        Some((at, themes)) if at.elapsed() < Duration::from_secs(2) => themes.clone(),
        _ => {
            let themes = crate::state::load_syntax_themes();
            *cache = Some((Instant::now(), themes.clone()));
            themes
        }
    }
}

/// Editor-style colors for the preview pane, present only when the user
/// opted in with `"background": true` and the theme declares a background.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreviewColors {
    pub background: Color,
    /// Text without a highlight of its own (plain files, header, footer).
    pub foreground: Option<Color>,
    /// Line-number color (`gutterForeground`); `None` keeps the dim default.
    pub gutter: Option<Color>,
}

pub fn preview_colors() -> Option<PreviewColors> {
    if !configured().background {
        return None;
    }
    theme_preview_colors(active_theme())
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
        let syntaxes = &assets().syntaxes;
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

    fn distinct_fgs(line: &Line<'_>) -> std::collections::HashSet<Color> {
        line.spans
            .iter()
            .filter(|s| !s.content.trim().is_empty())
            .filter_map(|s| s.style.fg)
            .collect()
    }

    fn darcula_markdown(text: &str) -> Vec<Line<'static>> {
        let theme = resolve_theme(Some("Darcula"), false);
        highlight_with(&assets().syntaxes, theme, "README.md", text, 100).expect("md grammar")
    }

    #[test]
    fn darcula_is_bundled_and_resolves_case_insensitively() {
        assert!(
            bundled_theme_names()
                .first()
                .is_some_and(|n| n == "Darcula")
        );
        let theme = resolve_theme(Some("darcula"), false);
        assert_eq!(theme.name.as_deref(), Some("Darcula"));
        assert!(theme_spec_is_valid("Darcula"));
    }

    #[test]
    fn darcula_markdown_lists_and_quotes_are_not_one_flat_color() {
        let lines = darcula_markdown("- item one\n1. numbered\n> quoted `code`\n");
        for line in &lines {
            assert!(
                distinct_fgs(line).len() >= 2,
                "single-color line {:?}",
                line.to_string()
            );
        }
        // The bullet is the accent; the item text is the plain foreground.
        let plain = Color::Rgb(0xA9, 0xB7, 0xC6);
        let item = lines[0]
            .spans
            .iter()
            .find(|s| s.content.contains("item"))
            .unwrap();
        assert_eq!(item.style.fg, Some(plain));
    }

    #[test]
    fn darcula_markdown_heading_is_italic_and_inline_code_green() {
        let lines = darcula_markdown("# Title\n\nsome `code` here\n");
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
    fn missing_or_unknown_theme_falls_back_to_the_historical_default() {
        let dark = default_theme(false);
        let light = default_theme(true);
        assert_eq!(dark.name.as_deref(), Some("Base16 Ocean Dark"));
        for spec in [
            None,
            Some(""),
            Some("no-such-theme"),
            Some("/no/such/file.tmTheme"),
        ] {
            assert!(std::ptr::eq(resolve_theme(spec, false), dark), "{spec:?}");
            assert!(std::ptr::eq(resolve_theme(spec, true), light), "{spec:?}");
        }
        assert!(!theme_spec_is_valid("/no/such/file.tmTheme"));
    }

    #[test]
    fn a_corrupt_theme_file_falls_back_and_a_valid_one_loads() {
        let dir = std::env::temp_dir().join(format!("hs-syntax-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("bad.tmTheme");
        std::fs::write(&bad, "not a plist").unwrap();
        let spec = bad.display().to_string();
        assert!(std::ptr::eq(
            resolve_theme(Some(&spec), false),
            default_theme(false)
        ));

        let good = dir.join("mine.tmTheme");
        std::fs::write(&good, EXTRA_THEMES[0].1).unwrap();
        let spec = good.display().to_string();
        assert_eq!(
            resolve_theme(Some(&spec), false).name.as_deref(),
            Some("Darcula")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    fn themed_markdown(theme: &str, text: &str) -> Vec<Line<'static>> {
        let theme = resolve_theme(Some(theme), false);
        highlight_with(&assets().syntaxes, theme, "README.md", text, 100).expect("md grammar")
    }

    fn fg_of(line: &Line<'_>, needle: &str) -> Option<Color> {
        line.spans
            .iter()
            .find(|s| s.content.contains(needle))
            .and_then(|s| s.style.fg)
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
        let lines = themed_markdown("IntelliJ Dark", text);
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
        let lines = themed_markdown("base16-ocean.dark", "Plain [BUG] text\n");
        assert_eq!(lines[0].spans.len(), 1, "{:?}", lines[0].spans);
    }

    #[test]
    fn bundled_themes_expose_editor_background_and_gutter() {
        let colors = theme_preview_colors(resolve_theme(Some("IntelliJ Dark"), false)).unwrap();
        assert_eq!(colors.background, Color::Rgb(0x1E, 0x1F, 0x22));
        assert_eq!(colors.foreground, Some(Color::Rgb(0xBC, 0xBE, 0xC4)));
        assert_eq!(colors.gutter, Some(Color::Rgb(0x4B, 0x50, 0x59)));
    }
}
