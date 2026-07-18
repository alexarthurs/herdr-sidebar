//! Sideways text for the collapsed sliver: terminals can't rotate font glyphs,
//! so each letter is drawn as a 3×5 pixel bitmap, rotated 90° counter-clockwise
//! (reads bottom-to-top, like a chart's y-axis label), and rendered with
//! sextant characters (🬀… — 2×3 pixels per cell, Symbols for Legacy Computing;
//! covered by the Cascadia family). A rotated letter is 5×3 pixels — exactly
//! one sextant row — so the whole label is one row per letter, three cells
//! wide: solid, compact, no dots.

/// 3×5 bitmaps for the letters the sliver needs, rows top-to-bottom.
fn glyph(ch: char) -> Option<[&'static str; 5]> {
    let rows = match ch.to_ascii_uppercase() {
        'E' => ["###", "#..", "###", "#..", "###"],
        'X' => ["#.#", "#.#", ".#.", "#.#", "#.#"],
        'P' => ["###", "#.#", "###", "#..", "#.."],
        'L' => ["#..", "#..", "#..", "#..", "###"],
        'O' => ["###", "#.#", "#.#", "#.#", "###"],
        'R' => ["###", "#.#", "##.", "#.#", "#.#"],
        _ => return None,
    };
    Some(rows)
}

/// Sextant character for a 6-bit (2 wide × 3 tall) pixel pattern. The sextant
/// block omits patterns already encoded as Block Elements.
fn sextant(bits: u32) -> char {
    match bits {
        0 => ' ',
        21 => '▌',
        42 => '▐',
        63 => '█',
        _ => {
            let skipped = (bits > 21) as u32 + (bits > 42) as u32;
            char::from_u32(0x1FB00 + bits - 1 - skipped).unwrap_or(' ')
        }
    }
}

/// `text`, rotated 90° counter-clockwise, one sextant row per letter (3 cells
/// wide), truncated to `max_lines`. Rotated-CCW text reads bottom-to-top, so
/// the LAST letter comes first (topmost).
pub fn lines(text: &str, max_lines: usize) -> Vec<String> {
    let mut out = Vec::new();
    for ch in text.chars().rev() {
        if out.len() >= max_lines {
            break;
        }
        let Some(rows) = glyph(ch) else { continue };
        // 90° CCW: px[r][c] = rows[c][2 - r]  (3 wide × 5 tall → 5 × 3).
        let px = |r: usize, c: usize| rows[c].as_bytes()[2 - r] == b'#';
        let mut line = String::new();
        for cell_x in 0..3 {
            let mut bits = 0u32;
            for y in 0..3 {
                for dx in 0..2 {
                    let x = cell_x * 2 + dx;
                    if x < 5 && px(y, x) {
                        bits |= 1 << (y * 2 + dx);
                    }
                }
            }
            line.push(sextant(bits));
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_row_per_letter_reading_bottom_to_top() {
        let all = lines("EXPLORER", 100);
        assert_eq!(all.len(), 8);
        assert!(all.iter().all(|l| l.chars().count() == 3));
        // Reversed stack is R,E,R,O,L,P,X,E top-to-bottom: the two E rows
        // (indices 1 and 7) and the two R rows (0 and 2) render identically.
        assert_eq!(all[1], all[7], "both E rows render identically");
        assert_eq!(all[0], all[2], "both R rows render identically");
        assert_eq!(lines("EXPLORER", 3).len(), 3);
        assert!(lines("", 10).is_empty());
    }

    #[test]
    fn sextant_encoding_matches_block_element_gaps() {
        assert_eq!(sextant(0), ' ');
        assert_eq!(sextant(21), '▌');
        assert_eq!(sextant(42), '▐');
        assert_eq!(sextant(63), '█');
        assert_eq!(sextant(1), '\u{1FB00}');
        assert_eq!(sextant(22), '\u{1FB14}'); // one past the ▌ gap
        assert_eq!(sextant(62), '\u{1FB3B}'); // last sextant before █
    }

    #[test]
    fn glyphs_are_solid_marks() {
        for l in lines("EXPLORER", 100) {
            assert!(
                l.chars().any(|c| c != ' '),
                "every letter row has visible pixels"
            );
            assert!(l.chars().all(|c| {
                c == ' '
                    || c == '▌'
                    || c == '▐'
                    || c == '█'
                    || ('\u{1fb00}'..='\u{1fb3b}').contains(&c)
            }));
        }
    }
}
