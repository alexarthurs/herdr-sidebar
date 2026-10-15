//! Collapsed "rail" mode: « shrinks the sidebar pane to a slim strip of
//! activity icons plus a » expand button, like herdr's own left sidebar.
//!
//! The state is GLOBAL (one marker file in the run dir): collapsing in any
//! tab collapses every live sidebar (each polls the marker), and a sidebar
//! that spawns later — a new tab, a view switch, a redeploy, the ensure hook
//! — comes back collapsed. herdr clamps split ratios to 0.1–0.9, so the rail is
//! as narrow as herdr allows (~10% of the tab), not a fixed column count.

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::icons::IconTheme;
use crate::ui::{activity_button_style, activity_icons, draw_activity_caps, gear_icon, palette};

/// Key the `toggle-collapse` action injects into the sidebar pane (the
/// activities already use F9–F12).
pub const TOGGLE_KEY: &str = "f8";

/// Asked-for rail width in terminal columns. Deliberately tiny: herdr clamps
/// the split ratio, which yields its minimum pane width.
const RAIL_COLS: u16 = 1;

pub fn dir() -> PathBuf {
    crate::rundir::dir("collapsed")
}

/// Marker name inside `dir()`; not a tab id, so the ensure sweep must skip
/// this directory.
const GLOBAL: &str = "all";

/// Whether the sidebars are collapsed (in every tab).
pub fn is_collapsed() -> bool {
    crate::snooze::is_set(&dir(), GLOBAL)
}

/// How often a live sidebar polls the shared marker.
const SYNC_EVERY: Duration = Duration::from_millis(300);

/// One rail button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RailHit {
    Explorer,
    Search,
    SourceControl,
    QuickOpen,
    Settings,
    Expand,
}

/// How long after startup a collapsed rail re-asserts its width: the
/// launcher opens a 50/50 split and resizes it AFTER our process starts.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(5);

pub struct Rail {
    pub collapsed: bool,
    zones: Vec<(Rect, RailHit)>,
    born: Instant,
    last_sync: Instant,
}

impl Rail {
    /// Restore the shared collapsed state for `pane_id`, shrinking the pane
    /// if it was spawned at full width while collapsed.
    pub fn load(pane_id: Option<&str>) -> Self {
        let collapsed = pane_id.is_some() && is_collapsed();
        if let (true, Some(pane)) = (collapsed, pane_id) {
            let cols = crossterm::terminal::size().map_or(RAIL_COLS, |(w, _)| w);
            let dock_right = crate::state::load_state().dock_right;
            resize(pane, cols, RAIL_COLS, dock_right, false);
        }
        Self {
            collapsed,
            zones: Vec::new(),
            born: Instant::now(),
            last_sync: Instant::now(),
        }
    }

    /// Call on every pane resize: shortly after startup, a collapsed rail
    /// shrinks back if the launcher's own resize widened it.
    pub fn settle(&self, pane_id: &str, cols: u16, dock_right: bool) {
        if self.collapsed && self.born.elapsed() < SETTLE {
            resize(pane_id, cols, RAIL_COLS, dock_right, false);
        }
    }

    /// Collapse or expand `pane_id`: persist the shared marker, then resize.
    /// `cols` is the pane's current width; `expanded_cols` the preferred
    /// sidebar width to restore.
    pub fn set(
        &mut self,
        pane_id: &str,
        on: bool,
        cols: u16,
        expanded_cols: u16,
        dock_right: bool,
    ) -> Result<(), String> {
        let marker = if on {
            crate::snooze::set(&dir(), GLOBAL)
        } else {
            crate::snooze::clear(&dir(), GLOBAL)
        };
        marker.map_err(|e| e.to_string())?;
        self.apply(pane_id, on, cols, expanded_cols, dock_right);
        Ok(())
    }

    /// Call every tick: follow a collapse/expand made in another tab.
    pub fn sync(&mut self, pane_id: &str, cols: u16, expanded_cols: u16, dock_right: bool) {
        if self.last_sync.elapsed() < SYNC_EVERY {
            return;
        }
        self.last_sync = Instant::now();
        let on = is_collapsed();
        if on != self.collapsed {
            self.apply(pane_id, on, cols, expanded_cols, dock_right);
        }
    }

    fn apply(&mut self, pane_id: &str, on: bool, cols: u16, expanded_cols: u16, dock_right: bool) {
        self.collapsed = on;
        if on {
            resize(pane_id, cols, RAIL_COLS, dock_right, false);
        } else {
            resize(pane_id, cols, expanded_cols, dock_right, true);
        }
    }

    /// Draw the rail over the whole pane: activity icons (`active`
    /// highlighted; outside unified mode only the pane's own view), then
    /// quick-open/settings and » at the bottom.
    /// Chips use the expanded activity bar's look (3-row capped highlight).
    pub fn draw(
        &mut self,
        frame: &mut Frame,
        theme: IconTheme,
        active: RailHit,
        merged: bool,
        mouse_pos: Option<(u16, u16)>,
    ) {
        let area = frame.area();
        self.zones.clear();
        if area.height < 4 || area.width == 0 {
            return;
        }
        let bottom = area.y + area.height - 1;
        // Bottom rows: buttons (bottom - 2), spacer, » (bottom).
        let last_free = bottom.saturating_sub(4);
        let (exp, search, git) = activity_icons(theme);
        // Icon row of each chip; the caps sit one row above and below.
        let mut y = area.y + 1;
        for (hit, icon) in [
            (RailHit::Explorer, exp),
            (RailHit::Search, search),
            (RailHit::SourceControl, git),
        ] {
            if !merged && hit != active {
                continue;
            }
            if y + 1 > last_free {
                break;
            }
            self.chips(
                frame,
                area,
                y,
                &[(hit, icon)],
                theme,
                active,
                mouse_pos,
                true,
            );
            y += 3;
        }

        self.chips(
            frame,
            area,
            bottom - 2,
            &[
                (RailHit::QuickOpen, quick_open_icon(theme)),
                (RailHit::Settings, gear_icon(theme)),
            ],
            theme,
            active,
            mouse_pos,
            false,
        );
        // The whole bottom row expands: easy to hit on a narrow strip.
        let row = Rect::new(area.x, bottom, area.width, 1);
        let hovered = mouse_pos.is_some_and(|(x, y)| hits(row, x, y));
        let mut glyph = "»".bold().fg(palette().header_accent);
        if hovered {
            glyph = glyph.bg(palette().activity_hover_bg);
        }
        frame.render_widget(
            Paragraph::new(Line::from(glyph)).alignment(Alignment::Center),
            row,
        );
        self.zones.push((row, RailHit::Expand));
    }

    /// Draw a centered row of icon chips at `y`, recording their hit zones;
    /// returns the first chip's rect. With `caps`, the active/hovered chip
    /// gets the activity bar's half-block caps above and below, and the hit
    /// zone covers all three rows.
    #[allow(clippy::too_many_arguments)]
    fn chips(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        y: u16,
        chips: &[(RailHit, &str)],
        theme: IconTheme,
        active: RailHit,
        mouse_pos: Option<(u16, u16)>,
        caps: bool,
    ) -> Rect {
        // FA glyphs render two cells wide in the non-Mono Nerd Font; the
        // emoji already are. Pad so every chip is the same width (same rule
        // as the expanded activity bar).
        let slack = if theme == IconTheme::Material {
            " "
        } else {
            ""
        };
        let texts: Vec<String> = chips
            .iter()
            .map(|(_, icon)| format!(" {icon}{slack} "))
            .collect();
        let total: u16 = texts
            .iter()
            .map(|t| Span::raw(t.as_str()).width() as u16)
            .sum();
        let mut x = area.x + area.width.saturating_sub(total) / 2;
        let mut first = Rect::default();
        for ((hit, _), text) in chips.iter().zip(texts) {
            let w = (Span::raw(text.as_str()).width() as u16)
                .min((area.x + area.width).saturating_sub(x));
            let rect = Rect::new(x, y, w, 1);
            let zone = if caps {
                Rect::new(x, y.saturating_sub(1), w, 3)
            } else {
                rect
            };
            let is_active = caps && *hit == active;
            let hovered = mouse_pos.is_some_and(|(mx, my)| hits(zone, mx, my));
            if caps && (is_active || hovered) {
                let color = if is_active {
                    palette().selection_bg
                } else {
                    palette().activity_hover_bg
                };
                draw_activity_caps(frame, (x, x + w), y.saturating_sub(1), y + 1, color);
            }
            frame.render_widget(
                Paragraph::new(Span::styled(
                    text,
                    activity_button_style(is_active, hovered),
                )),
                rect,
            );
            self.zones.push((zone, *hit));
            if first == Rect::default() {
                first = rect;
            }
            x += w;
        }
        first
    }

    /// The rail button under a pane-local click, if any.
    pub fn hit(&self, column: u16, row: u16) -> Option<RailHit> {
        self.zones
            .iter()
            .find(|(rect, _)| hits(*rect, column, row))
            .map(|&(_, hit)| hit)
    }
}

/// Theme-matched quick-open (file picker) glyph.
fn quick_open_icon(theme: IconTheme) -> &'static str {
    match theme {
        IconTheme::Material => "\u{f15b}",
        IconTheme::Emoji => "📄",
    }
}

fn hits(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

/// Resize `pane_id` from `cols` toward `target` columns. `preferred` uses the
/// sidebar's usual share clamp (restoring); otherwise the target is passed
/// through and herdr's own ratio clamp decides (collapsing).
fn resize(pane_id: &str, cols: u16, target: u16, dock_right: bool, preferred: bool) {
    let Ok(layout) =
        crate::ipc::call_text("pane.layout", serde_json::json!({ "pane_id": pane_id }))
    else {
        return;
    };
    let step = if preferred {
        crate::launch::preferred_resize_plan(&layout, pane_id, cols, target, dock_right)
    } else {
        crate::launch::resize_plan(&layout, pane_id, cols, target, dock_right)
    };
    let Some(step) = step else { return };
    let _ = crate::ipc::call_text(
        "pane.resize",
        serde_json::json!({
            "pane_id": pane_id,
            "direction": step.direction,
            "amount": step.amount,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_finds_buttons_by_rect() {
        let rail = Rail {
            collapsed: true,
            born: Instant::now(),
            last_sync: Instant::now(),
            zones: vec![
                (Rect::new(4, 1, 4, 1), RailHit::Explorer),
                (Rect::new(0, 9, 12, 1), RailHit::Expand),
            ],
        };
        assert_eq!(rail.hit(5, 1), Some(RailHit::Explorer));
        assert_eq!(rail.hit(3, 1), None);
        assert_eq!(rail.hit(0, 9), Some(RailHit::Expand));
        assert_eq!(rail.hit(11, 9), Some(RailHit::Expand));
        assert_eq!(rail.hit(5, 2), None);
    }
}
