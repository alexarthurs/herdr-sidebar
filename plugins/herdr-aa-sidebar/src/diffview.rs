//! VS Code-style diff rendering: dual line-number gutters, syntax-highlighted
//! code over red/green row tints, and a darker word-level tint on the changed
//! segment of paired lines. Input is plain `git diff` output (no ANSI) — the
//! parsing is ours, so the look is too.

use std::collections::HashMap;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::syntax::LineHighlighter;

/// Row tints (VS Code dark diff editor vibes).
const DEL_BG: Color = Color::Rgb(0x42, 0x22, 0x26);
const DEL_WORD_BG: Color = Color::Rgb(0x6f, 0x30, 0x36);
const ADD_BG: Color = Color::Rgb(0x20, 0x39, 0x28);
const ADD_WORD_BG: Color = Color::Rgb(0x35, 0x59, 0x3d);
const DEL_MARK: Color = Color::Rgb(0xd1, 0x6d, 0x76);
const ADD_MARK: Color = Color::Rgb(0x8c, 0xc9, 0x8f);

/// One parsed diff line, before rendering.
#[derive(Debug, PartialEq, Eq)]
enum Ev {
    /// A new hunk begins (rendered as a dim separator between hunks).
    Hunk,
    /// Unchanged: (old line no, new line no, text).
    Ctx(usize, usize, String),
    Del(usize, String),
    Add(usize, String),
    /// Anything unparsed worth keeping ("Binary files … differ").
    Plain(String),
}

fn parse_events(diff: &str) -> Vec<Ev> {
    let mut evs = Vec::new();
    let mut old_no = 0usize;
    let mut new_no = 0usize;
    let mut seen_hunk = false;
    for line in diff.lines() {
        if line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("similarity")
            || line.starts_with("rename ")
            || line.starts_with("copy ")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with('\\')
        {
            continue;
        }
        if line.starts_with("@@") {
            for tok in line.split_whitespace() {
                let (sign, rest) = match tok.split_at_checked(1) {
                    Some(pair) => pair,
                    None => continue,
                };
                let start = rest.split(',').next().unwrap_or("");
                if let Ok(n) = start.parse::<usize>() {
                    match sign {
                        "-" => old_no = n,
                        "+" => new_no = n,
                        _ => {}
                    }
                }
            }
            if seen_hunk {
                evs.push(Ev::Hunk);
            }
            seen_hunk = true;
            continue;
        }
        if let Some(t) = line.strip_prefix('-') {
            evs.push(Ev::Del(old_no, t.to_string()));
            old_no += 1;
        } else if let Some(t) = line.strip_prefix('+') {
            evs.push(Ev::Add(new_no, t.to_string()));
            new_no += 1;
        } else if let Some(t) = line.strip_prefix(' ') {
            evs.push(Ev::Ctx(old_no, new_no, t.to_string()));
            old_no += 1;
            new_no += 1;
        } else if line.is_empty() {
            // Some tools trim the leading space off blank context lines.
            evs.push(Ev::Ctx(old_no, new_no, String::new()));
            old_no += 1;
            new_no += 1;
        } else {
            evs.push(Ev::Plain(line.to_string()));
        }
    }
    evs
}

/// Char range (start, end) of the differing middle of a changed line,
/// keyed by event index — deletions paired 1:1 with the additions that
/// immediately follow them, VS Code style.
fn word_ranges(evs: &[Ev]) -> HashMap<usize, (usize, usize)> {
    let mut ranges = HashMap::new();
    let mut i = 0;
    while i < evs.len() {
        let del_start = i;
        while matches!(evs.get(i), Some(Ev::Del(..))) {
            i += 1;
        }
        let add_start = i;
        while matches!(evs.get(i), Some(Ev::Add(..))) {
            i += 1;
        }
        if del_start == add_start || add_start == i {
            if i == del_start {
                i += 1;
            }
            continue;
        }
        for k in 0..(add_start - del_start).min(i - add_start) {
            let (Some(Ev::Del(_, old)), Some(Ev::Add(_, new))) =
                (evs.get(del_start + k), evs.get(add_start + k))
            else {
                continue;
            };
            let old_chars: Vec<char> = old.chars().collect();
            let new_chars: Vec<char> = new.chars().collect();
            let mut prefix = 0;
            while prefix < old_chars.len()
                && prefix < new_chars.len()
                && old_chars[prefix] == new_chars[prefix]
            {
                prefix += 1;
            }
            let mut suffix = 0;
            while suffix < old_chars.len().saturating_sub(prefix)
                && suffix < new_chars.len().saturating_sub(prefix)
                && old_chars[old_chars.len() - 1 - suffix] == new_chars[new_chars.len() - 1 - suffix]
            {
                suffix += 1;
            }
            // Only worth tinting when the lines genuinely share material.
            if prefix + suffix == 0 {
                continue;
            }
            ranges.insert(del_start + k, (prefix, old_chars.len() - suffix));
            ranges.insert(add_start + k, (prefix, new_chars.len() - suffix));
        }
    }
    ranges
}

/// Re-slice spans so `range` (in chars) carries `bg` — the word-level tint.
fn overlay_bg(spans: Vec<Span<'static>>, range: (usize, usize), bg: Color) -> Vec<Span<'static>> {
    let (start, end) = range;
    if start >= end {
        return spans;
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    for span in spans {
        let chars: Vec<char> = span.content.chars().collect();
        let len = chars.len();
        let (a, b) = (start.clamp(pos, pos + len), end.clamp(pos, pos + len));
        if a == b || a >= pos + len {
            out.push(span);
        } else {
            let style = span.style;
            let before: String = chars[..a - pos].iter().collect();
            let middle: String = chars[a - pos..b - pos].iter().collect();
            let after: String = chars[b - pos..].iter().collect();
            if !before.is_empty() {
                out.push(Span::styled(before, style));
            }
            out.push(Span::styled(middle, style.bg(bg)));
            if !after.is_empty() {
                out.push(Span::styled(after, style));
            }
        }
        pos += len;
    }
    out
}

/// Render a unified diff for `rel` (its extension picks the grammar) into
/// display lines: `old new ±` gutters, tinted rows, highlighted code.
pub fn render(rel: &str, diff: &str) -> Vec<Line<'static>> {
    let evs = parse_events(diff);
    let ranges = word_ranges(&evs);

    let max_no = evs
        .iter()
        .map(|e| match e {
            Ev::Ctx(o, n, _) => (*o).max(*n),
            Ev::Del(o, _) => *o,
            Ev::Add(n, _) => *n,
            _ => 0,
        })
        .max()
        .unwrap_or(0);
    let w = max_no.to_string().len().max(2);

    // Two stateful highlighters approximate the old and new file contexts,
    // so multi-line constructs mostly survive (the delta/bat trick).
    let mut old_hl = LineHighlighter::new(rel);
    let mut new_hl = LineHighlighter::new(rel);

    let gutter = |o: Option<usize>, n: Option<usize>, mark: &str, mark_fg: Color| {
        let fmt = |v: Option<usize>| match v {
            Some(v) => format!("{v:>w$}"),
            None => " ".repeat(w),
        };
        vec![
            Span::styled(format!("{} {} ", fmt(o), fmt(n)), Style::default().dim()),
            Span::styled(format!("{mark} "), Style::default().fg(mark_fg)),
        ]
    };

    let mut lines = Vec::new();
    for (idx, ev) in evs.iter().enumerate() {
        match ev {
            Ev::Hunk => lines.push(Line::from(Span::styled(
                format!("{}⋯", " ".repeat(w * 2 + 2)),
                Style::default().dim(),
            ))),
            Ev::Plain(t) => lines.push(Line::from(Span::styled(
                t.clone(),
                Style::default().dim(),
            ))),
            Ev::Ctx(o, n, t) => {
                old_hl.line(t);
                let spans = new_hl.line(t);
                let mut all = gutter(Some(*o), Some(*n), " ", Color::Reset);
                all.extend(spans);
                lines.push(Line::from(all));
            }
            Ev::Del(o, t) => {
                let mut spans = old_hl.line(t);
                if let Some(&range) = ranges.get(&idx) {
                    spans = overlay_bg(spans, range, DEL_WORD_BG);
                }
                let mut all = gutter(Some(*o), None, "-", DEL_MARK);
                all.extend(spans);
                lines.push(Line::from(all).style(Style::default().bg(DEL_BG)));
            }
            Ev::Add(n, t) => {
                let mut spans = new_hl.line(t);
                if let Some(&range) = ranges.get(&idx) {
                    spans = overlay_bg(spans, range, ADD_WORD_BG);
                }
                let mut all = gutter(None, Some(*n), "+", ADD_MARK);
                all.extend(spans);
                lines.push(Line::from(all).style(Style::default().bg(ADD_BG)));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: no `\` line continuations — they would eat the leading space
    // that marks context lines.
    const DIFF: &str = concat!(
        "diff --git a/src/app.ts b/src/app.ts\n",
        "index 111..222 100644\n",
        "--- a/src/app.ts\n",
        "+++ b/src/app.ts\n",
        "@@ -1,4 +1,5 @@\n",
        " import { search } from \"./search\";\n",
        "-console.log(\"scm-playground up\");\n",
        "+console.log(\"scm oh yeah\");\n",
        "+// TODO: debounce input\n",
        " search(\"hello\");\n",
    );

    #[test]
    fn diff_parses_gutters_tints_and_word_ranges() {
        let lines = render("app.ts", DIFF);
        assert_eq!(lines.len(), 5);
        // Context row: both numbers, no tint.
        assert!(lines[0].to_string().starts_with(" 1  1"));
        assert_eq!(lines[0].style.bg, None);
        // Deletion: old number only, red row tint.
        assert!(lines[1].to_string().contains('-'));
        assert_eq!(lines[1].style.bg, Some(DEL_BG));
        // Addition: new number only, green row tint.
        assert_eq!(lines[2].style.bg, Some(ADD_BG));
        // The paired del/add carry a darker word-level tint on the middle.
        let word_tinted = lines[1]
            .spans
            .iter()
            .any(|s| s.style.bg == Some(DEL_WORD_BG));
        assert!(word_tinted, "expected word-level tint on the deletion");
        // The unpaired trailing addition has no word tint.
        let plain_add = lines[3]
            .spans
            .iter()
            .all(|s| s.style.bg != Some(ADD_WORD_BG));
        assert!(plain_add);
    }

    #[test]
    fn hunk_boundaries_and_binary_lines_survive() {
        let two_hunks = "@@ -1,1 +1,1 @@\n ctx\n@@ -9,1 +9,1 @@\n ctx2\n";
        let lines = render("x.rs", two_hunks);
        assert_eq!(lines.len(), 3);
        assert!(lines[1].to_string().contains('⋯'));
        let bin = render("x.bin", "Binary files a/x.bin and b/x.bin differ\n");
        assert_eq!(bin.len(), 1);
    }
}
