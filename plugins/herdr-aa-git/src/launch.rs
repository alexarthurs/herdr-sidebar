//! Launcher helpers behind `scripts/open-git-panel.{sh,ps1}` — kept in Rust so the
//! logic is unit-tested and so ids/paths extracted from herdr's JSON are validated
//! before they reach an argv (option-injection guard). Mirrors herdr-aa-filetree's
//! launch module so both panels share the left-dock approach. Three stdin→stdout modes:
//!
//! - `--launch-decision`: `herdr pane list` JSON → `OPEN` | `FOCUS <pane_id>` |
//!   `CLOSE <pane_id>`, scoped to the focused pane's tab (toggle behavior).
//! - `--focused-pane`: `herdr pane list` JSON → `<pane_id>\t<cwd>` of the focused
//!   pane (cwd stripped of the `\\?\` verbatim prefix herdr reports on Windows).
//! - `--open-plan`: `herdr pane layout` JSON → `<leftmost_pane_id>\t<ratio>`, the
//!   split target and left-slot share that docks the panel on the left edge.

use serde::Deserialize;

/// The pane label the launcher assigns (`pane rename`) and later looks for.
pub const PANE_LABEL: &str = "Source Control";

/// Source id for `pane.report_metadata`; its token marks a pane as the Source
/// Control panel independently of the label — the merged Sidebar pane is
/// labeled "Sidebar" but still carries this token.
pub const METADATA_SOURCE: &str = "herdr-aa-git";

/// Preferred panel width in columns (the commit box needs more room than the
/// filetree's 32); the ratio is derived from the target pane.
const TARGET_COLS: f64 = 40.0;

#[derive(Deserialize)]
struct PaneListMsg {
    result: PaneListResult,
}

#[derive(Deserialize)]
struct PaneListResult {
    #[serde(default)]
    panes: Vec<Pane>,
}

#[derive(Deserialize)]
struct Pane {
    pane_id: Option<String>,
    label: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    focused: bool,
    tab_id: Option<String>,
    /// Metadata tokens reported via `pane.report_metadata`; only key presence
    /// matters here.
    #[serde(default)]
    tokens: serde_json::Map<String, serde_json::Value>,
}

impl Pane {
    /// The panel is recognized by its metadata token (reported by the TUI at
    /// startup — survives the label becoming "Sidebar" in merged mode) or by
    /// the "Source Control" label (present from the moment the launcher
    /// renames the fresh pane, before the TUI has reported its token).
    fn is_panel(&self) -> bool {
        self.tokens.contains_key(METADATA_SOURCE) || self.label.as_deref() == Some(PANE_LABEL)
    }
}

#[derive(Deserialize)]
struct LayoutMsg {
    result: LayoutResult,
}

#[derive(Deserialize)]
struct LayoutResult {
    layout: Layout,
}

#[derive(Deserialize)]
struct Layout {
    #[serde(default)]
    panes: Vec<LayoutPane>,
}

#[derive(Deserialize)]
struct LayoutPane {
    pane_id: Option<String>,
    rect: Option<Rect>,
}

#[derive(Deserialize)]
struct Rect {
    x: i64,
    y: i64,
    width: i64,
}

/// Windows PowerShell 5.1 prepends a UTF-8 BOM when piping into a native
/// process's stdin (verified live by herdr-aa-filetree); serde_json rejects a
/// BOM before `{`.
fn strip_bom(input: &str) -> &str {
    input.trim_start_matches('\u{feff}')
}

/// `OPEN`, `FOCUS <id>`, or `CLOSE <id>` from a `pane list` JSON. Unparseable
/// input, no focused pane, or an unsafe id all degrade to `OPEN` — the safe
/// default is a fresh panel, never acting on a pane in an unknown tab.
pub fn launch_decision(pane_list_json: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return "OPEN".to_string();
    };
    let panes = &msg.result.panes;
    let Some(focused) = panes.iter().find(|p| p.focused) else {
        return "OPEN".to_string();
    };
    let panel = panes
        .iter()
        .find(|p| p.is_panel() && p.tab_id.as_deref() == focused.tab_id.as_deref());
    let Some(id) = panel.and_then(|p| p.pane_id.as_deref()).filter(|id| is_flag_safe(id))
    else {
        return "OPEN".to_string();
    };
    if Some(id) == focused.pane_id.as_deref() {
        format!("CLOSE {id}")
    } else {
        format!("FOCUS {id}")
    }
}

/// `<pane_id>\t<cwd>` of the focused pane, or empty on any failure. The cwd keeps
/// its spaces (hence the tab separator) but loses any `\\?\` verbatim prefix.
pub fn focused_pane(pane_list_json: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    let Some(focused) = msg.result.panes.iter().find(|p| p.focused) else {
        return String::new();
    };
    let Some(id) = focused.pane_id.as_deref().filter(|id| is_flag_safe(id)) else {
        return String::new();
    };
    let cwd = focused
        .cwd
        .as_deref()
        .map(strip_verbatim)
        .unwrap_or_default();
    format!("{id}\t{cwd}")
}

/// `<pane_id>\t<ratio>` for the left dock: the leftmost pane of the layout (the
/// one touching the spaces/agents sidebar) and the left-slot share that gives the
/// panel ~40 columns after the split+swap. Empty on any failure.
pub fn open_plan(layout_json: &str) -> String {
    let Ok(msg) = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)) else {
        return String::new();
    };
    let mut best: Option<(&str, &Rect)> = None;
    for pane in &msg.result.layout.panes {
        let (Some(id), Some(rect)) = (pane.pane_id.as_deref(), pane.rect.as_ref()) else {
            continue;
        };
        if !is_flag_safe(id) || rect.width <= 0 {
            continue;
        }
        // Leftmost wins; among a left column of stacked panes, topmost wins.
        let better = match best {
            None => true,
            Some((_, b)) => (rect.x, rect.y) < (b.x, b.y),
        };
        if better {
            best = Some((id, rect));
        }
    }
    let Some((id, rect)) = best else {
        return String::new();
    };
    let ratio = (TARGET_COLS / rect.width as f64).clamp(0.15, 0.5);
    format!("{id}\t{ratio:.2}")
}

/// The created pane's id from a `pane.split` response
/// (`{"result":{"pane":{"pane_id":..}}}`), validated flag-safe.
/// Copy-mirrored from herdr-aa-filetree's launch.rs.
pub fn split_pane_id(response_json: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Msg {
        result: Res,
    }
    #[derive(Deserialize)]
    struct Res {
        pane: Option<Info>,
    }
    #[derive(Deserialize)]
    struct Info {
        pane_id: Option<String>,
    }
    serde_json::from_str::<Msg>(strip_bom(response_json))
        .ok()?
        .result
        .pane?
        .pane_id
        .filter(|id| is_flag_safe(id))
}

/// True when the id can be passed as a positional argument to the herdr CLI
/// without any risk of being parsed as a flag.
fn is_flag_safe(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-'))
}

fn strip_verbatim(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_list(panes: &str) -> String {
        format!(r#"{{"id":"cli:pane:list","result":{{"panes":[{panes}]}}}}"#)
    }

    const FOCUSED: &str = r#"{"pane_id":"w1:p1","focused":true,"tab_id":"w1:t1","cwd":"C:\\work\\my repo"}"#;

    #[test]
    fn decision_opens_when_no_panel_in_tab() {
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p9","label":"Source Control","tab_id":"w1:t2"}}"#
        ));
        assert_eq!(launch_decision(&json), "OPEN", "other-tab panel is ignored");
    }

    #[test]
    fn decision_focuses_unfocused_panel_in_same_tab() {
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","label":"Source Control","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision(&json), "FOCUS w1:p2");
    }

    #[test]
    fn decision_closes_when_panel_is_focused() {
        let json = pane_list(
            r#"{"pane_id":"w1:p2","label":"Source Control","tab_id":"w1:t1","focused":true}"#,
        );
        assert_eq!(launch_decision(&json), "CLOSE w1:p2");
    }

    #[test]
    fn decision_recognizes_panel_by_metadata_token_without_label() {
        // The merged Sidebar pane is labeled "Sidebar" but carries the token.
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","label":"Sidebar","tab_id":"w1:t1","tokens":{{"herdr-aa-git":{{"value":"source-control"}}}}}}"#
        ));
        assert_eq!(launch_decision(&json), "FOCUS w1:p2");
    }

    #[test]
    fn decision_degrades_to_open_on_garbage_or_unsafe_ids() {
        assert_eq!(launch_decision("not json"), "OPEN");
        assert_eq!(launch_decision(&pane_list(r#"{"pane_id":"w1:p1"}"#)), "OPEN");
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"--evil","label":"Source Control","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision(&json), "OPEN");
    }

    #[test]
    fn utf8_bom_from_powershell_pipe_is_stripped() {
        let json = format!("\u{feff}{}", pane_list(FOCUSED));
        assert_eq!(launch_decision(&json), "OPEN");
        assert!(focused_pane(&json).starts_with("w1:p1\t"));
        let layout_json = format!(
            "\u{feff}{}",
            layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":100,"height":50}}"#)
        );
        assert_eq!(open_plan(&layout_json), "w1:p1\t0.40");
    }

    #[test]
    fn focused_pane_reports_id_and_stripped_cwd() {
        let json = pane_list(&format!(
            r#"{{"pane_id":"w1:p3","focused":true,"tab_id":"w1:t1","cwd":"\\\\?\\C:\\work\\my repo"}},{FOCUSED}"#
        ));
        assert_eq!(focused_pane(&json), "w1:p3\tC:\\work\\my repo");
        assert_eq!(focused_pane("not json"), "");
        assert_eq!(focused_pane(&pane_list(r#"{"pane_id":"w1:p1"}"#)), "");
    }

    fn layout(panes: &str) -> String {
        format!(r#"{{"id":"cli:pane:layout","result":{{"layout":{{"panes":[{panes}]}}}}}}"#)
    }

    #[test]
    fn open_plan_picks_leftmost_topmost_pane() {
        let json = layout(
            r#"{"pane_id":"w1:p2","rect":{"x":119,"y":1,"width":100,"height":70}},
               {"pane_id":"w1:p3","rect":{"x":29,"y":36,"width":100,"height":35}},
               {"pane_id":"w1:p1","rect":{"x":29,"y":1,"width":100,"height":35}}"#,
        );
        let plan = open_plan(&json);
        let (id, ratio) = plan.split_once('\t').unwrap();
        assert_eq!(id, "w1:p1");
        assert_eq!(ratio, "0.40"); // 40 / 100
    }

    #[test]
    fn open_plan_clamps_ratio() {
        let wide = layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":400,"height":50}}"#);
        assert_eq!(open_plan(&wide), "w1:p1\t0.15");
        let narrow = layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":50,"height":50}}"#);
        assert_eq!(open_plan(&narrow), "w1:p1\t0.50");
    }

    #[test]
    fn open_plan_is_empty_on_failure() {
        assert_eq!(open_plan("not json"), "");
        assert_eq!(open_plan(&layout("")), "");
        let unsafe_id = layout(r#"{"pane_id":"--x","rect":{"x":0,"y":0,"width":90,"height":50}}"#);
        assert_eq!(open_plan(&unsafe_id), "");
    }
}
