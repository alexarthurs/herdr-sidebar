//! Launcher helpers behind `scripts/open-explorer.{sh,ps1}` — kept in Rust so the
//! logic is unit-tested and so ids/paths extracted from herdr's JSON are validated
//! before they reach an argv (option-injection guard). Three stdin→stdout modes:
//!
//! - `--launch-decision`: `herdr pane list` JSON → `OPEN` | `FOCUS <pane_id>` |
//!   `CLOSE <pane_id>`, scoped to the focused pane's tab (toggle behavior).
//! - `--focused-pane`: `herdr pane list` JSON → `<pane_id>\t<cwd>` of the focused
//!   pane (cwd stripped of the `\\?\` verbatim prefix herdr reports on Windows).
//! - `--open-plan`: `herdr pane layout` JSON → `<leftmost_pane_id>\t<ratio>`, the
//!   split target and left-slot share that docks the explorer on the left edge.

use serde::Deserialize;

/// The pane label the launcher assigns (`pane rename`) and later looks for.
pub const PANE_LABEL: &str = "Explorer";

/// Preferred explorer width in columns; the ratio is derived from the target pane.
const TARGET_COLS: f64 = 32.0;

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
    #[serde(default)]
    splits: Vec<LayoutSplit>,
}

#[derive(Deserialize)]
struct LayoutSplit {
    direction: Option<String>,
    ratio: Option<f64>,
    rect: Option<Rect>,
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
/// process's stdin (verified live); serde_json rejects a BOM before `{`.
fn strip_bom(input: &str) -> &str {
    input.trim_start_matches('\u{feff}')
}

/// `OPEN`, `FOCUS <id>`, or `CLOSE <id>` from a `pane list` JSON. Unparseable
/// input, no focused pane, or an unsafe id all degrade to `OPEN` — the safe
/// default is a fresh explorer, never acting on a pane in an unknown tab.
pub fn launch_decision(pane_list_json: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return "OPEN".to_string();
    };
    let panes = &msg.result.panes;
    let Some(focused) = panes.iter().find(|p| p.focused) else {
        return "OPEN".to_string();
    };
    let explorer = panes.iter().find(|p| {
        p.label.as_deref() == Some(PANE_LABEL) && p.tab_id.as_deref() == focused.tab_id.as_deref()
    });
    let Some(id) = explorer.and_then(|p| p.pane_id.as_deref()).filter(|id| is_flag_safe(id))
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
/// explorer ~32 columns after the split+swap. Empty on any failure.
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

/// One `herdr pane resize` invocation: which way to move our right edge and by
/// how much. `amount` is a RATIO delta — herdr adds it to the nearest split's
/// ratio (`layout.rs::resize_focused`: `current_ratio ± delta`), it is NOT
/// columns.
pub struct ResizeStep {
    pub direction: &'static str,
    pub amount: f64,
}

/// Compute the resize step that brings `pane_id` from `term_cols_now` to
/// `term_cols_target` terminal columns, from a `pane layout` JSON.
///
/// The explorer is a left column, so its right edge is the divider of some
/// horizontal split: we pick the innermost `right` split whose divider sits at
/// the pane's right edge and convert the column delta into that split's ratio
/// space. `None` when the pane or such a split can't be found, or the pane is
/// already at the target.
pub fn resize_plan(
    layout_json: &str,
    pane_id: &str,
    term_cols_now: u16,
    term_cols_target: u16,
) -> Option<ResizeStep> {
    let msg = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)).ok()?;
    let layout = &msg.result.layout;
    let pane_rect = layout
        .panes
        .iter()
        .find(|p| p.pane_id.as_deref() == Some(pane_id))?
        .rect
        .as_ref()?;
    // The pane rect can be a couple of columns wider than the terminal inside
    // it (pane chrome); express the target in rect space.
    let chrome = pane_rect.width - i64::from(term_cols_now);
    let target_rect_w = i64::from(term_cols_target) + chrome.max(0);

    let divider_x = pane_rect.x + pane_rect.width;
    let split = layout
        .splits
        .iter()
        .filter(|s| s.direction.as_deref() == Some("right"))
        .filter_map(|s| Some((s.rect.as_ref()?, s.ratio?)))
        .filter(|(rect, ratio)| {
            let split_divider = rect.x + (f64::from(rect.width as i32) * ratio).round() as i64;
            rect.x <= pane_rect.x && (split_divider - divider_x).abs() <= 2 && rect.width > 0
        })
        .min_by_key(|(rect, _)| rect.width)?;

    let delta = (target_rect_w - pane_rect.width) as f64 / split.0.width as f64;
    if delta.abs() < 0.005 {
        return None;
    }
    Some(ResizeStep {
        direction: if delta > 0.0 { "right" } else { "left" },
        amount: delta.abs(),
    })
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
    fn decision_opens_when_no_explorer_in_tab() {
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p9","label":"Explorer","tab_id":"w1:t2"}}"#
        ));
        assert_eq!(launch_decision(&json), "OPEN", "other-tab Explorer is ignored");
    }

    #[test]
    fn decision_focuses_unfocused_explorer_in_same_tab() {
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","label":"Explorer","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision(&json), "FOCUS w1:p2");
    }

    #[test]
    fn decision_closes_when_explorer_is_focused() {
        let json = pane_list(
            r#"{"pane_id":"w1:p2","label":"Explorer","tab_id":"w1:t1","focused":true}"#,
        );
        assert_eq!(launch_decision(&json), "CLOSE w1:p2");
    }

    #[test]
    fn decision_degrades_to_open_on_garbage_or_unsafe_ids() {
        assert_eq!(launch_decision("not json"), "OPEN");
        assert_eq!(launch_decision(&pane_list(r#"{"pane_id":"w1:p1"}"#)), "OPEN");
        let json = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"--evil","label":"Explorer","tab_id":"w1:t1"}}"#
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
            layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":90,"height":50}}"#)
        );
        assert_eq!(open_plan(&layout_json), "w1:p1\t0.36");
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
            r#"{"pane_id":"w1:p2","rect":{"x":119,"y":1,"width":90,"height":70}},
               {"pane_id":"w1:p3","rect":{"x":29,"y":36,"width":90,"height":35}},
               {"pane_id":"w1:p1","rect":{"x":29,"y":1,"width":90,"height":35}}"#,
        );
        let plan = open_plan(&json);
        let (id, ratio) = plan.split_once('\t').unwrap();
        assert_eq!(id, "w1:p1");
        assert_eq!(ratio, "0.36"); // 32 / 90
    }

    #[test]
    fn open_plan_clamps_ratio() {
        let wide = layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":400,"height":50}}"#);
        assert_eq!(open_plan(&wide), "w1:p1\t0.15");
        let narrow = layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":40,"height":50}}"#);
        assert_eq!(open_plan(&narrow), "w1:p1\t0.50");
    }

    fn layout_with_splits(panes: &str, splits: &str) -> String {
        format!(
            r#"{{"id":"cli:pane:layout","result":{{"layout":{{"panes":[{panes}],"splits":[{splits}]}}}}}}"#
        )
    }

    #[test]
    fn resize_plan_converts_columns_to_split_ratio_delta() {
        // Explorer: 32 rect cols (30 terminal cols) at the left of a 160-col split.
        let json = layout_with_splits(
            r#"{"pane_id":"w1:p2","rect":{"x":0,"y":0,"width":32,"height":50}},
               {"pane_id":"w1:p1","rect":{"x":32,"y":0,"width":128,"height":50}}"#,
            r#"{"direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        // Collapse 30 → 4 terminal cols: rect 32 → 6, delta -26/160.
        let step = resize_plan(&json, "w1:p2", 30, 4).unwrap();
        assert_eq!(step.direction, "left");
        assert!((step.amount - 26.0 / 160.0).abs() < 1e-9);
        // Expand 30 → 40: rect 32 → 42, delta +10/160.
        let step = resize_plan(&json, "w1:p2", 30, 40).unwrap();
        assert_eq!(step.direction, "right");
        assert!((step.amount - 10.0 / 160.0).abs() < 1e-9);
    }

    #[test]
    fn resize_plan_picks_innermost_matching_split() {
        // Nested: root split (divider elsewhere) plus the inner split whose
        // divider is at the explorer's right edge.
        let json = layout_with_splits(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":20,"height":50}}"#,
            r#"{"direction":"right","ratio":0.5,"rect":{"x":0,"y":0,"width":200,"height":50}},
               {"direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":100,"height":50}}"#,
        );
        let step = resize_plan(&json, "e", 18, 40).unwrap();
        // delta computed against the inner 100-col split: +22/100.
        assert!((step.amount - 22.0 / 100.0).abs() < 1e-9);
    }

    #[test]
    fn resize_plan_returns_none_when_unresizable_or_at_target() {
        assert!(resize_plan("not json", "e", 30, 4).is_none());
        // No split with a divider at the pane's edge (e.g. the only pane).
        let solo = layout_with_splits(r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":100,"height":50}}"#, "");
        assert!(resize_plan(&solo, "e", 98, 30).is_none());
        // Already at the target.
        let json = layout_with_splits(
            r#"{"pane_id":"e","rect":{"x":0,"y":0,"width":32,"height":50}}"#,
            r#"{"direction":"right","ratio":0.2,"rect":{"x":0,"y":0,"width":160,"height":50}}"#,
        );
        assert!(resize_plan(&json, "e", 30, 30).is_none());
    }

    #[test]
    fn open_plan_is_empty_on_failure() {
        assert_eq!(open_plan("not json"), "");
        assert_eq!(open_plan(&layout("")), "");
        let unsafe_id = layout(r#"{"pane_id":"--x","rect":{"x":0,"y":0,"width":90,"height":50}}"#);
        assert_eq!(open_plan(&unsafe_id), "");
    }
}
