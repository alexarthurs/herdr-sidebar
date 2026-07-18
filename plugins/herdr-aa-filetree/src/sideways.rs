//! Sideways text for the collapsed sliver: terminals can't rotate font glyphs,
//! so each letter is drawn as a small 4×5 pixel bitmap, rotated 90°
//! counter-clockwise (reads bottom-to-top, like a chart's y-axis label), and
//! rendered with half-block characters (▀▄█ — one pixel per cell horizontally,
//! two vertically). Solid blocks, not braille dots, and a rotated letter spans
//! exactly two rows with a blank row between letters, so glyphs never smear
//! across cell boundaries.

/// 4×5 bitmaps for the letters the sliver needs, rows top-to-bottom.
fn glyph(ch: char) -> Option<[&'static str; 5]> {
    let rows = match ch.to_ascii_uppercase() {
        'E' => ["####", "#...", "###.", "#...", "####"],
        'X' => ["#..#", "#..#", ".##.", "#..#", "#..#"],
        'P' => ["###.", "#..#", "###.", "#...", "#..."],
        'L' => ["#...", "#...", "#...", "#...", "####"],
        'O' => [".##.", "#..#", "#..#", "#..#", ".##."],
        'R' => ["###.", "#..#", "###.", "#.#.", "#..#"],
        _ => return None,
    };
    Some(rows)
}

/// Pixel grid for `text` rotated 90° counter-clockwise: each letter becomes
/// 5 wide × 4 tall. Rotated-CCW text reads bottom-to-top, so the LAST letter
/// is stacked first (topmost), with a two-pixel (one-row) gap between letters.
fn rotated_pixels(text: &str) -> Vec<[bool; 5]> {
    let mut grid: Vec<[bool; 5]> = Vec::new();
    for ch in text.chars().rev() {
        let Some(rows) = glyph(ch) else { continue };
        if !grid.is_empty() {
            grid.push([false; 5]);
            grid.push([false; 5]);
        }
        // 90° CCW: new[r][c] = old[c][W-1-r]  (4 wide × 5 tall → 5 × 4).
        for r in 0..4 {
            let mut line = [false; 5];
            for (c, item) in line.iter_mut().enumerate() {
                *item = rows[c].as_bytes()[3 - r] == b'#';
            }
            grid.push(line);
        }
    }
    grid
}

/// `text`, rotated 90° counter-clockwise, as lines of half-block characters
/// (5 cells wide), truncated to `max_lines`.
pub fn lines(text: &str, max_lines: usize) -> Vec<String> {
    let pixels = rotated_pixels(text);
    let mut out = Vec::new();
    let empty = [false; 5];
    for band in pixels.chunks(2) {
        if out.len() >= max_lines {
            break;
        }
        let top = band[0];
        let bottom = *band.get(1).unwrap_or(&empty);
        let line = (0..5)
            .map(|x| match (top[x], bottom[x]) {
                (false, false) => ' ',
                (true, false) => '▀',
                (false, true) => '▄',
                (true, true) => '█',
            })
            .collect();
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotated_l_reads_sideways_ccw() {
        // L rotated 90° CCW: the foot becomes the right column, the spine the
        // bottom row.
        let px = rotated_pixels("L");
        assert_eq!(px.len(), 4);
        assert_eq!(px[0], [false, false, false, false, true]);
        assert!(px[3].iter().all(|&p| p), "bottom row is the original spine");
    }

    #[test]
    fn reads_bottom_to_top() {
        // "EX": X must be the TOP glyph (rotated-CCW text reads upward), and a
        // full blank row separates the letters.
        let px = rotated_pixels("EX");
        assert_eq!(px[0], [true, true, false, true, true]); // X right edge up top
        assert_eq!(px.len(), 4 + 2 + 4);
        assert_eq!(px[4], [false; 5]);
        assert_eq!(px[5], [false; 5]);
    }

    #[test]
    fn lines_are_half_blocks_and_truncate() {
        let all = lines("EXPLORER", 100);
        assert_eq!(all.len(), (8_usize * 4 + 7 * 2).div_ceil(2));
        assert!(all.iter().all(|l| l.chars().count() == 5));
        assert!(
            all.iter()
                .flat_map(|l| l.chars())
                .all(|c| matches!(c, ' ' | '▀' | '▄' | '█'))
        );
        assert_eq!(lines("EXPLORER", 4).len(), 4);
        assert!(lines("", 10).is_empty());
    }
}
