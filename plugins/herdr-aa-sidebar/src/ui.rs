//! Shared UI vocabulary for both sidebar views: keycap footer hints, the
//! activity-bar / settings / suggest glyphs, hit-test helpers, and the
//! pane-list parsing both views use to find their sibling panes. One home —
//! these used to be copy-mirrored between two crates.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::icons::IconTheme;
use crate::state::View;

/// Keycap chip colors for the footer hints — a subtle "keyboard key" look
/// instead of a wall of dim text.
pub const KEYCAP_BG: Color = Color::Rgb(0x32, 0x36, 0x3d);
pub const KEYCAP_FG: Color = Color::Rgb(0xc9, 0xce, 0xd6);

/// Rendered width of one `key label` hint: keycap padding + gap + label.
fn hint_width(key: &str, label: &str) -> usize {
    Span::raw(key).width() + 2 + 1 + Span::raw(label).width()
}

/// Pack hotkey hints into as many footer lines as they need at `width`
/// (max 4), instead of clipping — each as a keycap chip plus a dim label.
/// `reserve` columns stay free on the LAST line (for a corner button).
pub fn wrap_hints(
    hints: &[(&'static str, &'static str)],
    width: u16,
    reserve: u16,
) -> Vec<Line<'static>> {
    let width = usize::from(width.max(8));
    let reserve = usize::from(reserve);
    let mut lines: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut used: usize = 1;
    for (key, label) in hints {
        let w = hint_width(key, label);
        let empty = lines.last().is_some_and(Vec::is_empty);
        if !empty && used + 2 + w > width.saturating_sub(reserve) {
            lines.push(Vec::new());
            used = 1;
        }
        let line = lines.last_mut().unwrap();
        line.push(Span::raw(if line.is_empty() { " " } else { "  " }));
        line.push(Span::styled(
            format!(" {key} "),
            Style::default().bg(KEYCAP_BG).fg(KEYCAP_FG),
        ));
        line.push(Span::styled(format!(" {label}"), Style::default().dim()));
        used += if line.len() == 3 { w } else { 2 + w };
    }
    lines.into_iter().map(Line::from).collect()
}

/// True when a click at pane-local (column, row) lands on the `«` collapse
/// button: the 3-cell region at the right end of the bottom line, mirroring
/// herdr's own sidebar collapse control.
pub fn hits_collapse_button(column: u16, row: u16, pane_width: u16, pane_height: u16) -> bool {
    row == pane_height.saturating_sub(1) && column >= pane_width.saturating_sub(4)
}

/// Theme-matched activity-bar icons: (explorer, source control). Both FA
/// glyphs render two cells wide in the non-Mono Nerd Font — chips reserve
/// the second cell (see the activity-bar renderer).
pub fn activity_icons(theme: IconTheme) -> (&'static str, &'static str) {
    match theme {
        IconTheme::Material => ("\u{f07b}", "\u{f126}"),
        IconTheme::Emoji => ("📁", "🔀"),
    }
}

/// Theme-matched ⚙ settings glyph.
pub fn gear_icon(theme: IconTheme) -> &'static str {
    match theme {
        IconTheme::Material => "\u{f013}",
        IconTheme::Emoji => "⚙",
    }
}

/// Monochrome outline sparkles for the suggest button: MDI "creation"
/// (the classic three-sparkle ✨ silhouette) with a text-presentation
/// fallback for the emoji theme.
pub fn sparkle_icon(theme: IconTheme) -> &'static str {
    match theme {
        IconTheme::Material => "\u{f0674}",
        IconTheme::Emoji => "✧",
    }
}

/// Theme-matched branch glyph for repo rows.
pub fn branch_icon(theme: IconTheme) -> &'static str {
    match theme {
        IconTheme::Material => "\u{e725}",
        IconTheme::Emoji => "⎇",
    }
}

pub fn within(x: u16, (start, end): (u16, u16)) -> bool {
    (start..end).contains(&x)
}

pub fn hits(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

/// Cut `s` down to at most `max` display columns, ending in `…` when trimmed.
/// Empty when even the ellipsis wouldn't fit.
pub fn truncate_to(s: String, max: usize) -> String {
    if Span::raw(s.as_str()).width() <= max {
        return s;
    }
    if max < 2 {
        return String::new();
    }
    let mut out = String::new();
    for c in s.chars() {
        let mut candidate = out.clone();
        candidate.push(c);
        if Span::raw(candidate.as_str()).width() + 1 > max {
            break;
        }
        out = candidate;
    }
    out.push('…');
    out
}

/// Pane ids in the same tab as `my_pane_id` that belong to the `other` view
/// (matched by its standalone label or its metadata token), from a
/// `pane.list` response.
pub fn sibling_panes_of(pane_list_json: &str, my_pane_id: &str, other: View) -> Vec<String> {
    #[derive(serde::Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(serde::Deserialize)]
    struct Res {
        #[serde(default)]
        panes: Vec<Pane>,
    }
    #[derive(serde::Deserialize)]
    struct Pane {
        pane_id: Option<String>,
        label: Option<String>,
        tab_id: Option<String>,
        #[serde(default)]
        tokens: serde_json::Map<String, serde_json::Value>,
    }
    let Ok(msg) = serde_json::from_str::<Msg>(pane_list_json.trim_start_matches('\u{feff}'))
    else {
        return Vec::new();
    };
    let panes = &msg.result.panes;
    let Some(my_tab) = panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(my_pane_id))
        .and_then(|p| p.tab_id.clone())
    else {
        return Vec::new();
    };
    panes
        .iter()
        .filter(|p| p.tab_id.as_deref() == Some(my_tab.as_str()))
        .filter(|p| p.pane_id.as_deref() != Some(my_pane_id))
        .filter(|p| {
            p.label.as_deref() == Some(other.label()) || p.tokens.contains_key(other.plugin_id())
        })
        .filter_map(|p| p.pane_id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_wrap_instead_of_clipping() {
        let hints = [("⏎", "stage"), ("a", "all"), ("u", "none"), ("q", "quit")];
        let wide = wrap_hints(&hints, 80, 0);
        assert_eq!(wide.len(), 1);
        let narrow = wrap_hints(&hints, 14, 0);
        assert!(narrow.len() >= 2, "narrow pane stacks hints");
        for line in &narrow {
            assert!(line.width() <= 14, "no line exceeds the pane width");
        }
    }

    #[test]
    fn hints_have_no_line_cap() {
        let hints = [
            ("a", "aaa"),
            ("b", "bbb"),
            ("c", "ccc"),
            ("d", "ddd"),
            ("e", "eee"),
            ("f", "fff"),
            ("g", "ggg"),
            ("h", "hhh"),
        ];
        // Every chip lands on its own line rather than overflowing the
        // width — there is no line cap to clip against anymore.
        let lines = wrap_hints(&hints, 10, 0);
        assert_eq!(lines.len(), hints.len());
        for line in &lines {
            assert!(line.width() <= 10, "no line exceeds the pane width");
        }
    }

    #[test]
    fn truncation_keeps_width_budget() {
        assert_eq!(truncate_to("short".into(), 10), "short");
        let cut = truncate_to("averylongdirectoryname".into(), 8);
        assert!(cut.ends_with('…'));
        assert!(Span::raw(cut.as_str()).width() <= 8);
        assert_eq!(truncate_to("abc".into(), 1), "");
    }

    #[test]
    fn sibling_lookup_matches_label_or_token() {
        let json = r#"{"result":{"panes":[
            {"pane_id":"w1:p1","tab_id":"w1:t1","label":"Sidebar"},
            {"pane_id":"w1:p2","tab_id":"w1:t1","label":"Source Control"},
            {"pane_id":"w1:p3","tab_id":"w1:t1","tokens":{"herdr-aa-sidebar-git":{}}},
            {"pane_id":"w1:p9","tab_id":"w1:t2","label":"Source Control"}
        ]}}"#;
        let found = sibling_panes_of(json, "w1:p1", View::SourceControl);
        assert_eq!(found, ["w1:p2", "w1:p3"], "same tab only, label or token");
    }
}
