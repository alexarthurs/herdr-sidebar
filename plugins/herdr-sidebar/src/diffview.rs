//! VS Code-style diff rendering: dual line-number gutters, syntax-highlighted
//! code over red/green row tints, and a darker word-level tint on the changed
//! segment of paired lines. Input is plain `git diff` output (no ANSI) — the
//! parsing is ours, so the look is too.

use std::collections::HashMap;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::syntax::LineHighlighter;
use crate::ui::palette;

// Row tints live in `ui::Palette` (`diff_*`) so the light theme can swap them
// for GitHub-light tints without a second copy of this renderer.

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
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("diff --git") {
            in_hunk = false;
        }
        if (!in_hunk && (line.starts_with("--- ") || line.starts_with("+++ ")))
            || line.starts_with("diff --git")
            || line.starts_with("index ")
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
            in_hunk = true;
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
                && old_chars[old_chars.len() - 1 - suffix]
                    == new_chars[new_chars.len() - 1 - suffix]
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

/// A parsed diff line with its code already highlighted and word-tinted,
/// shared by the unified and side-by-side layouts.
enum Styled {
    Hunk,
    Plain(String),
    Ctx {
        old: usize,
        new: usize,
        old_spans: Vec<Span<'static>>,
        new_spans: Vec<Span<'static>>,
    },
    Del(usize, Vec<Span<'static>>),
    Add(usize, Vec<Span<'static>>),
}

/// Parse and highlight `diff`, returning the lines and the width of the
/// widest line number.
fn styled(rel: &str, diff: &str) -> (Vec<Styled>, usize) {
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

    let colors = palette();
    let out = evs
        .iter()
        .enumerate()
        .map(|(idx, ev)| match ev {
            Ev::Hunk => Styled::Hunk,
            Ev::Plain(t) => Styled::Plain(t.clone()),
            Ev::Ctx(o, n, t) => Styled::Ctx {
                old: *o,
                new: *n,
                old_spans: old_hl.line(t),
                new_spans: new_hl.line(t),
            },
            Ev::Del(o, t) => {
                let mut spans = old_hl.line(t);
                if let Some(&range) = ranges.get(&idx) {
                    spans = overlay_bg(spans, range, colors.diff_del_word_bg);
                }
                Styled::Del(*o, spans)
            }
            Ev::Add(n, t) => {
                let mut spans = new_hl.line(t);
                if let Some(&range) = ranges.get(&idx) {
                    spans = overlay_bg(spans, range, colors.diff_add_word_bg);
                }
                Styled::Add(*n, spans)
            }
        })
        .collect();
    (out, w)
}

/// Render a unified diff for `rel` (its extension picks the grammar) into
/// display lines: `old new ±` gutters, tinted rows, highlighted code. Each
/// diff line is one display line, in order.
pub fn render(rel: &str, diff: &str) -> Vec<Line<'static>> {
    let (evs, w) = styled(rel, diff);

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

    let colors = palette();
    evs.into_iter()
        .map(|ev| match ev {
            Styled::Hunk => Line::from(Span::styled(
                format!("{}⋯", " ".repeat(w * 2 + 2)),
                Style::default().dim(),
            )),
            Styled::Plain(t) => Line::from(Span::styled(t, Style::default().dim())),
            Styled::Ctx {
                old,
                new,
                new_spans,
                ..
            } => {
                let mut all = gutter(Some(old), Some(new), " ", Color::Reset);
                all.extend(new_spans);
                Line::from(all)
            }
            Styled::Del(o, spans) => {
                let mut all = gutter(Some(o), None, "-", colors.diff_del_mark);
                all.extend(spans);
                Line::from(all).style(Style::default().bg(colors.diff_del_bg))
            }
            Styled::Add(n, spans) => {
                let mut all = gutter(None, Some(n), "+", colors.diff_add_mark);
                all.extend(spans);
                Line::from(all).style(Style::default().bg(colors.diff_add_bg))
            }
        })
        .collect()
}

/// One side of a side-by-side row: a line number, its `-`/`+` mark, the
/// code, and the side's row tint.
struct Half {
    no: usize,
    mark: &'static str,
    mark_fg: Color,
    spans: Vec<Span<'static>>,
    bg: Option<Color>,
}

/// Render `diff` side by side for a `width`-wide pane: old lines on the
/// left, new on the right, deletions paired with the additions that follow
/// them. Each row is tagged with the [`render`] line it starts at, so a
/// viewer can keep its place when the layout toggles. Long lines wrap
/// within their column (or are clipped when `wrap` is off), and every row
/// is exactly `width` columns.
pub fn render_split(
    rel: &str,
    diff: &str,
    width: usize,
    wrap: bool,
) -> Vec<(usize, Line<'static>)> {
    let (evs, w) = styled(rel, diff);
    let colors = palette();
    let left_w = width.saturating_sub(1) / 2;
    let right_w = width.saturating_sub(1) - left_w;
    let full_row = |spans: Vec<Span<'static>>| Line::from(fit(spans, width, None));

    let mut rows = Vec::new();
    let pair =
        |rows: &mut Vec<(usize, Line<'static>)>,
         src: usize,
         left: Option<Half>,
         right: Option<Half>| {
            let l = half_rows(left.as_ref(), w, left_w, wrap);
            let r = half_rows(right.as_ref(), w, right_w, wrap);
            for i in 0..l.len().max(r.len()).max(1) {
                let mut spans = l
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| fit(Vec::new(), left_w, left.as_ref().and_then(|h| h.bg)));
                spans.push(Span::styled("│", Style::default().dim()));
                spans.extend(r.get(i).cloned().unwrap_or_else(|| {
                    fit(Vec::new(), right_w, right.as_ref().and_then(|h| h.bg))
                }));
                rows.push((src, Line::from(spans)));
            }
        };
    let del = |no: usize, spans: &[Span<'static>]| Half {
        no,
        mark: "-",
        mark_fg: colors.diff_del_mark,
        spans: spans.to_vec(),
        bg: Some(colors.diff_del_bg),
    };
    let add = |no: usize, spans: &[Span<'static>]| Half {
        no,
        mark: "+",
        mark_fg: colors.diff_add_mark,
        spans: spans.to_vec(),
        bg: Some(colors.diff_add_bg),
    };

    let mut i = 0;
    while i < evs.len() {
        match &evs[i] {
            Styled::Hunk => {
                let marker = format!("{}⋯", " ".repeat(w + 3));
                rows.push((
                    i,
                    full_row(vec![Span::styled(marker, Style::default().dim())]),
                ));
                i += 1;
            }
            Styled::Plain(t) => {
                let line = Line::from(Span::styled(t.clone(), Style::default().dim()));
                let mut pieces = crate::wrap::wrap_line(&line, width.max(1));
                if !wrap {
                    pieces.truncate(1);
                }
                for piece in pieces {
                    rows.push((i, full_row(piece.spans)));
                }
                i += 1;
            }
            Styled::Ctx {
                old,
                new,
                old_spans,
                new_spans,
            } => {
                let ctx = |no: usize, spans: &[Span<'static>]| Half {
                    no,
                    mark: " ",
                    mark_fg: Color::Reset,
                    spans: spans.to_vec(),
                    bg: None,
                };
                pair(
                    &mut rows,
                    i,
                    Some(ctx(*old, old_spans)),
                    Some(ctx(*new, new_spans)),
                );
                i += 1;
            }
            Styled::Del(..) | Styled::Add(..) => {
                let del_start = i;
                while matches!(evs.get(i), Some(Styled::Del(..))) {
                    i += 1;
                }
                let add_start = i;
                while matches!(evs.get(i), Some(Styled::Add(..))) {
                    i += 1;
                }
                let (dels, adds) = (add_start - del_start, i - add_start);
                for k in 0..dels.max(adds) {
                    let left = match evs.get(del_start + k) {
                        Some(Styled::Del(no, spans)) if k < dels => Some(del(*no, spans)),
                        _ => None,
                    };
                    let right = match evs.get(add_start + k) {
                        Some(Styled::Add(no, spans)) if k < adds => Some(add(*no, spans)),
                        _ => None,
                    };
                    let src = if k < dels {
                        del_start + k
                    } else {
                        add_start + k
                    };
                    pair(&mut rows, src, left, right);
                }
            }
        }
    }
    rows
}

/// Lay one side out in a `col`-wide column: the gutter on the first row,
/// blank gutters on continuations, each row padded to the column.
fn half_rows(half: Option<&Half>, w: usize, col: usize, wrap: bool) -> Vec<Vec<Span<'static>>> {
    let Some(h) = half else {
        return Vec::new();
    };
    let gutter_w = w + 3;
    let code = Line::from(h.spans.clone());
    let mut pieces = crate::wrap::wrap_line(&code, col.saturating_sub(gutter_w).max(1));
    if !wrap {
        pieces.truncate(1);
    }
    pieces
        .into_iter()
        .enumerate()
        .map(|(i, piece)| {
            let mut spans = if i == 0 {
                vec![
                    Span::styled(format!("{:>w$} ", h.no), Style::default().dim()),
                    Span::styled(format!("{} ", h.mark), Style::default().fg(h.mark_fg)),
                ]
            } else {
                vec![Span::raw(" ".repeat(gutter_w))]
            };
            spans.extend(piece.spans);
            fit(spans, col, h.bg)
        })
        .collect()
}

/// Pad `spans` with spaces to exactly `col` columns (clipping anything
/// wider) and give every span without its own background the row tint.
fn fit(spans: Vec<Span<'static>>, col: usize, bg: Option<Color>) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let width = span.width();
        if used + width > col {
            let mut clipped = String::new();
            for c in span.content.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
                if used + cw > col {
                    break;
                }
                used += cw;
                clipped.push(c);
            }
            out.push(Span::styled(clipped, span.style));
            break;
        }
        used += width;
        out.push(span);
    }
    if used < col {
        out.push(Span::raw(" ".repeat(col - used)));
    }
    if let Some(bg) = bg {
        for span in &mut out {
            if span.style.bg.is_none() {
                span.style = span.style.bg(bg);
            }
        }
    }
    out
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
        let colors = palette();
        let lines = render("app.ts", DIFF);
        assert_eq!(lines.len(), 5);
        // Context row: both numbers, no tint.
        assert!(lines[0].to_string().starts_with(" 1  1"));
        assert_eq!(lines[0].style.bg, None);
        // Deletion: old number only, red row tint.
        assert!(lines[1].to_string().contains('-'));
        assert_eq!(lines[1].style.bg, Some(colors.diff_del_bg));
        // Addition: new number only, green row tint.
        assert_eq!(lines[2].style.bg, Some(colors.diff_add_bg));
        // The paired del/add carry a darker word-level tint on the middle.
        let word_tinted = lines[1]
            .spans
            .iter()
            .any(|s| s.style.bg == Some(colors.diff_del_word_bg));
        assert!(word_tinted, "expected word-level tint on the deletion");
        // The unpaired trailing addition has no word tint.
        let plain_add = lines[3]
            .spans
            .iter()
            .all(|s| s.style.bg != Some(colors.diff_add_word_bg));
        assert!(plain_add);
    }

    #[test]
    fn deleted_line_starting_with_dashes_does_not_desync_gutters() {
        // A deleted SQL/Lua-style "-- comment" line looks exactly like a
        // "--- a/file" header if the parser isn't hunk-aware.
        let diff = concat!(
            "diff --git a/q.sql b/q.sql\n",
            "index 111..222 100644\n",
            "--- a/q.sql\n",
            "+++ b/q.sql\n",
            "@@ -1,3 +1,2 @@\n",
            " SELECT 1;\n",
            "--- trailing comment\n",
            " SELECT 2;\n",
        );
        let lines = render("q.sql", diff);
        assert_eq!(lines.len(), 3, "the deleted comment line must render");
        assert!(lines[0].to_string().starts_with(" 1  1"));
        assert!(lines[1].to_string().contains("-- trailing comment"));
        // Old-side numbering must not skip: line 3 is old line 3, not 2.
        assert!(lines[2].to_string().starts_with(" 3  2"));
    }

    /// The plain text of each half of a side-by-side row, split at the `│`.
    fn halves(line: &Line<'_>) -> (String, String) {
        let text = line.to_string();
        let (l, r) = text
            .split_once('│')
            .expect("a side-by-side row has a divider");
        (l.trim_end().to_string(), r.trim_end().to_string())
    }

    #[test]
    fn side_by_side_pairs_old_and_new_lines() {
        let colors = palette();
        let rows = render_split("app.ts", DIFF, 100, true);
        // ctx, the del/add pair, the leftover add, ctx.
        assert_eq!(rows.len(), 4);
        let (l, r) = halves(&rows[0].1);
        assert!(l.starts_with(" 1   import"), "{l:?}");
        assert!(r.starts_with(" 1   import"), "{r:?}");
        let (l, r) = halves(&rows[1].1);
        assert!(
            l.starts_with(" 2 - console.log(\"scm-playground up\");"),
            "{l:?}"
        );
        assert!(r.starts_with(" 2 + console.log(\"scm oh yeah\");"), "{r:?}");
        // Each half carries its own tint, and the word-level tints survive.
        let bgs: Vec<_> = rows[1].1.spans.iter().map(|s| s.style.bg).collect();
        assert!(bgs.contains(&Some(colors.diff_del_bg)));
        assert!(bgs.contains(&Some(colors.diff_add_bg)));
        assert!(bgs.contains(&Some(colors.diff_del_word_bg)));
        assert!(bgs.contains(&Some(colors.diff_add_word_bg)));
        // An addition with no deletion to pair with leaves the old side empty.
        let (l, r) = halves(&rows[2].1);
        assert_eq!(l, "");
        assert!(r.starts_with(" 3 + // TODO: debounce input"), "{r:?}");
        let (l, r) = halves(&rows[3].1);
        assert!(l.starts_with(" 3   search"), "{l:?}");
        assert!(r.starts_with(" 4   search"), "{r:?}");
    }

    #[test]
    fn side_by_side_rows_fill_the_width_and_map_to_unified_lines() {
        let rows = render_split("app.ts", DIFF, 80, true);
        for (_, row) in &rows {
            assert_eq!(row.width(), 80, "{:?}", row.to_string());
        }
        // Tagged with the unified line each row starts at, so toggling the
        // layout keeps the reader's place.
        let srcs: Vec<usize> = rows.iter().map(|(src, _)| *src).collect();
        assert_eq!(srcs, vec![0, 1, 3, 4]);
    }

    #[test]
    fn side_by_side_wraps_within_each_column() {
        let long = format!(
            "@@ -1,1 +1,1 @@\n-{}\n+short\n",
            "word ".repeat(20).trim_end()
        );
        let rows = render_split("x.txt", &long, 60, true);
        assert!(rows.len() > 1, "a long old line wraps onto more rows");
        for (src, row) in &rows[1..] {
            // The first hunk has no marker row, so the pair is line 0.
            assert_eq!(*src, 0, "continuations belong to the same pair");
            assert_eq!(row.width(), 60);
            let (l, r) = halves(row);
            assert!(
                l.starts_with("    "),
                "continuations leave the gutter blank: {l:?}"
            );
            assert_eq!(r, "", "the short new line does not repeat");
        }
        let unwrapped = render_split("x.txt", &long, 60, false);
        assert_eq!(unwrapped.len(), 1, "one clipped row");
        assert_eq!(unwrapped[0].1.width(), 60);
    }

    #[test]
    fn side_by_side_keeps_hunk_markers_and_plain_lines_full_width() {
        let two_hunks = "@@ -1,1 +1,1 @@\n ctx\n@@ -9,1 +9,1 @@\n ctx2\n";
        let rows = render_split("x.rs", two_hunks, 40, true);
        assert_eq!(rows.len(), 3);
        assert!(rows[1].1.to_string().contains('⋯'));
        let bin = render_split(
            "x.bin",
            "Binary files a/x.bin and b/x.bin differ\n",
            40,
            true,
        );
        assert_eq!(bin.len(), 1);
        assert!(bin[0].1.to_string().starts_with("Binary files"));
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
