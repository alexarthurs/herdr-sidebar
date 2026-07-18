//! Sideways text for the collapsed sliver: terminals can't rotate font glyphs,
//! so each letter is drawn as a small 5×7 pixel bitmap, rotated 90° clockwise
//! (reads top-to-bottom, letter tops facing right, like VS Code's vertical
//! labels), and rendered as braille-pattern characters (2×4 pixels per cell —
//! well covered by Cascadia-family fonts).

/// 5×7 bitmaps for the letters the sliver needs, rows top-to-bottom.
fn glyph(ch: char) -> Option<[&'static str; 7]> {
    let rows = match ch.to_ascii_uppercase() {
        'E' => ["#####", "#....", "#....", "####.", "#....", "#....", "#####"],
        'X' => ["#...#", "#...#", ".#.#.", "..#..", ".#.#.", "#...#", "#...#"],
        'P' => ["####.", "#...#", "#...#", "####.", "#....", "#....", "#...."],
        'L' => ["#....", "#....", "#....", "#....", "#....", "#....", "#####"],
        'O' => [".###.", "#...#", "#...#", "#...#", "#...#", "#...#", ".###."],
        'R' => ["####.", "#...#", "#...#", "####.", "#.#..", "#..#.", "#...#"],
        _ => return None,
    };
    Some(rows)
}

/// Pixel grid for `text` rotated 90° clockwise: each letter becomes 7 wide ×
/// 5 tall, letters stacked top-to-bottom with a one-pixel gap.
fn rotated_pixels(text: &str) -> Vec<[bool; 7]> {
    let mut grid: Vec<[bool; 7]> = Vec::new();
    for ch in text.chars() {
        let Some(rows) = glyph(ch) else { continue };
        if !grid.is_empty() {
            grid.push([false; 7]);
        }
        // 90° CW: rotated[x][H-1-y] = original[y][x]  (5 wide × 7 tall → 7 × 5).
        for x in 0..5 {
            let mut line = [false; 7];
            for (y, row) in rows.iter().enumerate() {
                line[6 - y] = row.as_bytes()[x] == b'#';
            }
            grid.push(line);
        }
    }
    grid
}

/// Braille dot bit for pixel (x, y) within a 2×4 cell.
fn braille_bit(x: usize, y: usize) -> u32 {
    match (x, y) {
        (0, 0) => 0x01,
        (0, 1) => 0x02,
        (0, 2) => 0x04,
        (0, 3) => 0x40,
        (1, 0) => 0x08,
        (1, 1) => 0x10,
        (1, 2) => 0x20,
        _ => 0x80,
    }
}

/// `text`, rotated 90° clockwise, as lines of braille characters (4 cells
/// wide), truncated to `max_lines`.
pub fn lines(text: &str, max_lines: usize) -> Vec<String> {
    let pixels = rotated_pixels(text);
    let mut out = Vec::new();
    for band in pixels.chunks(4) {
        if out.len() >= max_lines {
            break;
        }
        let mut line = String::new();
        for cell_x in 0..4 {
            let mut bits = 0u32;
            for (y, row) in band.iter().enumerate() {
                for x in 0..2 {
                    let px = cell_x * 2 + x;
                    if px < 7 && row[px] {
                        bits |= braille_bit(x, y);
                    }
                }
            }
            line.push(char::from_u32(0x2800 + bits).unwrap_or(' '));
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotated_l_reads_sideways() {
        // L rotated 90° CW = a bar along the top-left with the foot at the top:
        // pixel row 0 (the foot column of the original) must be fully set.
        let px = rotated_pixels("L");
        assert_eq!(px.len(), 5);
        assert!(px[0].iter().all(|&p| p), "top row is the original left spine");
        assert_eq!(px[1], [true, false, false, false, false, false, false]);
    }

    #[test]
    fn letters_stack_with_gaps() {
        assert_eq!(rotated_pixels("EX").len(), 5 + 1 + 5);
        assert_eq!(rotated_pixels("EXPLORER").len(), 8 * 5 + 7);
    }

    #[test]
    fn lines_are_braille_and_truncate() {
        let all = lines("EXPLORER", 100);
        assert_eq!(all.len(), 47_usize.div_ceil(4));
        assert!(all.iter().all(|l| l.chars().count() == 4));
        assert!(
            all.iter()
                .flat_map(|l| l.chars())
                .all(|c| ('\u{2800}'..='\u{28ff}').contains(&c))
        );
        assert_eq!(lines("EXPLORER", 3).len(), 3);
        assert!(lines("", 10).is_empty());
    }
}
