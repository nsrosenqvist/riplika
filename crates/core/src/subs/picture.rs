//! A segmented line of subtitle, drawn back out as an image.
//!
//! Drawn from the glyphs rather than from the original bitmap on purpose. The
//! glyphs are what was segmented, so whatever reads this image is looking at
//! exactly the shapes that will be labelled - no anti-aliased edge, no shadow,
//! no palette entry that turned out to be background. The reader gets the
//! cleanest possible rendering of the thing in question, and its answer lines
//! up with the segmentation by construction.

use crate::subs::segment::Line;

/// White paper, black ink, and a margin.
///
/// Tesseract is trained on scanned pages: text with room around it, dark on
/// light, and larger than a DVD's twenty-pixel cap height. All three are free
/// to give it here, and it reads far better for them.
pub const MARGIN: usize = 12;

/// Draw one line as a PNG.
///
/// `zoom` scales the glyphs up. DVD subtitles are rendered small and a reader
/// trained on print does noticeably better with more pixels to look at.
pub fn line_png(line: &Line, zoom: usize) -> Vec<u8> {
    let zoom = zoom.max(1);
    let Some(bounds) = extent(line) else {
        return Vec::new();
    };
    let (left, top, right, bottom) = bounds;
    let w = (right - left + 1).max(1) as usize * zoom + MARGIN * 2;
    let h = (bottom - top + 1).max(1) as usize * zoom + MARGIN * 2;

    let mut raw = vec![255u8; w * h];
    for g in &line.glyphs {
        for gy in 0..g.h as usize {
            for gx in 0..g.w as usize {
                if g.bits.get(gy * g.w as usize + gx).copied().unwrap_or(0) == 0 {
                    continue;
                }
                let px = ((g.x - left) as usize + gx) * zoom + MARGIN;
                let py = ((g.y - top) as usize + gy) * zoom + MARGIN;
                for dy in 0..zoom {
                    for dx in 0..zoom {
                        if let Some(p) = raw.get_mut((py + dy) * w + px + dx) {
                            *p = 0;
                        }
                    }
                }
            }
        }
    }
    encode(&raw, w, h)
}

/// The box the line's glyphs occupy, in the subtitle's own coordinates.
fn extent(line: &Line) -> Option<(i32, i32, i32, i32)> {
    let mut it = line.glyphs.iter();
    let first = it.next()?;
    let mut bounds = (first.x, first.y, first.x + first.w - 1, first.y + first.h - 1);
    for g in it {
        bounds.0 = bounds.0.min(g.x);
        bounds.1 = bounds.1.min(g.y);
        bounds.2 = bounds.2.max(g.x + g.w - 1);
        bounds.3 = bounds.3.max(g.y + g.h - 1);
    }
    Some(bounds)
}

fn encode(raw: &[u8], w: usize, h: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, w as u32, h as u32);
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        if let Ok(mut writer) = enc.write_header() {
            let _ = writer.write_image_data(raw);
        }
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subs::segment::Glyph;

    /// A two-pixel-wide upright stroke, at some offset.
    fn stroke(x: i32, y: i32) -> Glyph {
        Glyph { x, y, w: 2, h: 3, bits: vec![1, 1, 1, 1, 1, 1] }
    }

    fn line(glyphs: Vec<Glyph>) -> Line {
        let top = glyphs.iter().map(|g| g.y).min().unwrap_or(0);
        let bottom = glyphs.iter().map(|g| g.y + g.h - 1).max().unwrap_or(0);
        Line { glyphs, top, bottom }
    }

    #[test]
    fn a_line_is_drawn_at_the_size_its_glyphs_ask_for() {
        // 2px + 4px gap + 2px wide, 3px tall, doubled, plus a margin each side
        let png = line_png(&line(vec![stroke(10, 5), stroke(16, 5)]), 2);
        let d = png::Decoder::new(std::io::Cursor::new(&png));
        let reader = d.read_info().expect("a png came out");
        let info = reader.info();
        assert_eq!(info.width as usize, 8 * 2 + MARGIN * 2);
        assert_eq!(info.height as usize, 3 * 2 + MARGIN * 2);
    }

    #[test]
    fn the_ink_is_dark_and_the_paper_is_light() {
        // A reader trained on scanned pages wants it that way round, and a
        // line drawn white-on-black reads as an empty page.
        let png = line_png(&line(vec![stroke(0, 0)]), 1);
        let d = png::Decoder::new(std::io::Cursor::new(&png));
        let mut reader = d.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        let w = info.width as usize;
        assert_eq!(buf[0], 255, "the corner is paper");
        assert_eq!(buf[MARGIN * w + MARGIN], 0, "the first ink pixel is ink");
    }

    #[test]
    fn a_line_with_nothing_on_it_is_no_picture_rather_than_a_blank_one() {
        // Handing a reader an empty page would spend a process to be told
        // nothing, and the caller cannot tell that from a failure.
        assert!(line_png(&line(Vec::new()), 2).is_empty());
    }

    #[test]
    fn glyphs_keep_the_gaps_between_them() {
        // The spacing is what says where the words are, and a reader that sees
        // them run together answers with one long word.
        let wide = line_png(&line(vec![stroke(0, 0), stroke(40, 0)]), 1);
        let tight = line_png(&line(vec![stroke(0, 0), stroke(4, 0)]), 1);
        let width = |p: &[u8]| {
            png::Decoder::new(std::io::Cursor::new(p.to_vec()))
                .read_info()
                .map(|r| r.info().width)
                .unwrap_or(0)
        };
        assert!(width(&wide) > width(&tight));
    }
}
