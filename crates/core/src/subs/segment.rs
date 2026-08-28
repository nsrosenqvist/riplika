//! Turn a subtitle bitmap into lines of individual glyph bitmaps.
//!
//! DVD subtitles are a rendered bitmap font, so each character is pixel
//! identical everywhere it appears. Segmenting into glyphs lets us look them up
//! by exact match instead of guessing with statistical OCR.

use crate::subs::vobsub::Spu;

#[derive(Debug, Clone)]
pub struct Glyph {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Row-major bitmap, one byte per pixel, 1 = ink.
    pub bits: Vec<u8>,
}

impl Glyph {
    pub fn key(&self) -> String {
        // FNV-1a over dimensions and pixels; collisions are not a concern at
        // the scale of a few hundred distinct glyphs.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |b: u8| {
            h ^= b as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        };
        mix(self.w as u8);
        mix((self.w >> 8) as u8);
        mix(self.h as u8);
        mix((self.h >> 8) as u8);
        for &b in &self.bits {
            mix(b);
        }
        format!("{h:016x}")
    }
}

#[derive(Debug, Clone)]
pub struct Line {
    pub glyphs: Vec<Glyph>,
    pub top: i32,
    pub bottom: i32,
}

impl Line {
    pub fn height(&self) -> i32 {
        (self.bottom - self.top + 1).max(1)
    }
}

#[derive(Debug, Clone)]
pub struct SegOpts {
    /// Components smaller than this many pixels are treated as noise.
    pub min_ink: usize,
    /// A gap wider than this fraction of the line height becomes a space.
    pub space_ratio: f32,
    /// Vertical gap below this fraction of line height can join a mark to its
    /// stem (the dot of an i, the two dots of a colon). Line height shrinks on
    /// all-caps lines, so this needs slack or colons split into two periods.
    pub mark_gap_ratio: f32,
}

impl Default for SegOpts {
    fn default() -> Self {
        Self {
            min_ink: 3,
            space_ratio: 0.28,
            mark_gap_ratio: 0.80,
        }
    }
}

struct Comp {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    cells: Vec<(i32, i32)>,
}

fn components(mask: &[u8], w: usize, h: usize, min_ink: usize) -> Vec<Comp> {
    let mut seen = vec![false; w * h];
    let mut out = Vec::new();
    let mut stack: Vec<usize> = Vec::new();

    for start in 0..w * h {
        if mask[start] == 0 || seen[start] {
            continue;
        }
        stack.clear();
        stack.push(start);
        seen[start] = true;
        let mut cells = Vec::new();
        let (mut x0, mut y0) = ((start % w) as i32, (start / w) as i32);
        let (mut x1, mut y1) = (x0, y0);

        while let Some(i) = stack.pop() {
            let (cx, cy) = ((i % w) as i32, (i / w) as i32);
            cells.push((cx, cy));
            x0 = x0.min(cx);
            x1 = x1.max(cx);
            y0 = y0.min(cy);
            y1 = y1.max(cy);
            for dy in -1i32..=1 {
                for dx in -1i32..=1 {
                    let (nx, ny) = (cx + dx, cy + dy);
                    if nx < 0 || ny < 0 || nx >= w as i32 || ny >= h as i32 {
                        continue;
                    }
                    let j = ny as usize * w + nx as usize;
                    if mask[j] != 0 && !seen[j] {
                        seen[j] = true;
                        stack.push(j);
                    }
                }
            }
        }
        if cells.len() >= min_ink {
            out.push(Comp {
                x0,
                y0,
                x1,
                y1,
                cells,
            });
        }
    }
    out
}

fn to_glyph(c: &Comp) -> Glyph {
    let w = c.x1 - c.x0 + 1;
    let h = c.y1 - c.y0 + 1;
    let mut bits = vec![0u8; (w * h) as usize];
    for &(cx, cy) in &c.cells {
        bits[((cy - c.y0) * w + (cx - c.x0)) as usize] = 1;
    }
    Glyph {
        x: c.x0,
        y: c.y0,
        w,
        h,
        bits,
    }
}

fn merge(a: &Glyph, b: &Glyph) -> Glyph {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.w).max(b.x + b.w) - 1;
    let y1 = (a.y + a.h).max(b.y + b.h) - 1;
    let w = x1 - x0 + 1;
    let h = y1 - y0 + 1;
    let mut bits = vec![0u8; (w * h) as usize];
    for g in [a, b] {
        for gy in 0..g.h {
            for gx in 0..g.w {
                if g.bits[(gy * g.w + gx) as usize] != 0 {
                    let px = g.x + gx - x0;
                    let py = g.y + gy - y0;
                    bits[(py * w + px) as usize] = 1;
                }
            }
        }
    }
    Glyph {
        x: x0,
        y: y0,
        w,
        h,
        bits,
    }
}

/// Group components into text lines by vertical overlap.
fn group_lines(mut comps: Vec<Glyph>) -> Vec<Line> {
    comps.sort_by_key(|g| (g.y, g.x));
    let mut lines: Vec<Line> = Vec::new();

    for g in comps {
        let gt = g.y;
        let gb = g.y + g.h - 1;
        let mut placed = false;
        for l in lines.iter_mut() {
            let overlap = (gb.min(l.bottom) - gt.max(l.top) + 1).max(0);
            let smaller = (gb - gt + 1).min(l.bottom - l.top + 1).max(1);
            if overlap * 100 / smaller >= 35 {
                l.top = l.top.min(gt);
                l.bottom = l.bottom.max(gb);
                l.glyphs.push(g.clone());
                placed = true;
                break;
            }
        }
        if !placed {
            lines.push(Line {
                top: gt,
                bottom: gb,
                glyphs: vec![g],
            });
        }
    }

    for l in lines.iter_mut() {
        l.glyphs.sort_by_key(|g| g.x);
    }
    lines.sort_by_key(|l| l.top);
    lines
}

/// Join marks to the glyph they belong to: the dot of an i or j, the point of a
/// ? or !, the two dots of a colon. Without this the dot of an 'i' detaches and
/// the character reads as something else.
fn merge_marks(line: &mut Line, opts: &SegOpts) {
    let lh = line.height() as f32;
    let max_gap = (lh * opts.mark_gap_ratio) as i32;
    let mut changed = true;

    while changed {
        changed = false;
        'outer: for i in 0..line.glyphs.len() {
            for j in 0..line.glyphs.len() {
                if i == j {
                    continue;
                }
                let (a, b) = (&line.glyphs[i], &line.glyphs[j]);
                // Horizontal overlap, or near-aligned centres: an italic face
                // shifts the dot of an i to the right of its stem, so plain
                // overlap is not enough to catch it.
                let ol = ((a.x + a.w).min(b.x + b.w) - a.x.max(b.x)).max(0);
                let narrow = a.w.min(b.w).max(1);
                let (top, bot) = if a.y < b.y { (a, b) } else { (b, a) };
                let gap = bot.y - (top.y + top.h);

                // An italic face shears marks sideways: the two dots of a colon
                // can miss each other entirely in x. Allow an offset that grows
                // with the vertical distance, but only between narrow strokes -
                // otherwise a period would glue itself to the letter beside it.
                let widest = a.w.max(b.w);
                let both_narrow = widest as f32 <= lh * 0.35;
                let dy = (bot.y - top.y).abs();
                let tol = if both_narrow {
                    (widest as f32 * 0.6 + dy as f32 * 0.35) as i32
                } else {
                    (widest as f32 * 0.5) as i32
                };
                let ca = a.x * 2 + a.w;
                let cb = b.x * 2 + b.w;
                let centred = (ca - cb).abs() / 2 <= tol;
                if ol * 100 / narrow < 55 && !centred {
                    continue;
                }
                if gap < 0 || gap > max_gap {
                    continue;
                }
                // only merge when at least one part is small - a mark, not a letter
                let small = top.h.min(bot.h) as f32 <= lh * 0.42;
                if !small {
                    continue;
                }
                let m = merge(a, b);
                let (lo, hi) = (i.min(j), i.max(j));
                line.glyphs.remove(hi);
                line.glyphs.remove(lo);
                line.glyphs.push(m);
                line.glyphs.sort_by_key(|g| g.x);
                changed = true;
                break 'outer;
            }
        }
    }
}

/// Attach a diacritic that was left behind when its twin merged.
///
/// An umlaut is two dots. Once the first has joined its letter, the second no
/// longer sits *above* anything - it overlaps the merged glyph - so the
/// stacking test in `merge_marks` rejects it and it survives as a stray period.
/// A ring (a-ring) is a single mark and never hits this.
fn merge_diacritics(line: &mut Line) {
    let lh = line.height() as f32;
    loop {
        let mut found = None;
        'outer: for i in 0..line.glyphs.len() {
            for j in 0..line.glyphs.len() {
                if i == j {
                    continue;
                }
                let (s, b) = (&line.glyphs[i], &line.glyphs[j]);
                // one must be a mark, the other a letter-sized body
                if s.h as f32 > lh * 0.30 || (b.h as f32) < lh * 0.5 {
                    continue;
                }
                // the mark must sit within the body's horizontal extent
                let ol = ((s.x + s.w).min(b.x + b.w) - s.x.max(b.x)).max(0);
                if ol * 100 / s.w.max(1) < 70 {
                    continue;
                }
                // ... and in its upper reaches, where a diacritic belongs
                if s.y + s.h > b.y + (b.h as f32 * 0.4) as i32 {
                    continue;
                }
                found = Some((i, j));
                break 'outer;
            }
        }
        let Some((i, j)) = found else { break };
        let m = merge(&line.glyphs[i], &line.glyphs[j]);
        let (lo, hi) = (i.min(j), i.max(j));
        line.glyphs.remove(hi);
        line.glyphs.remove(lo);
        line.glyphs.push(m);
        line.glyphs.sort_by_key(|g| g.x);
    }
}

/// Pair up the two marks of a double quote.
///
/// The font draws `"` as two separate strokes, so component analysis sees two
/// apostrophes. Joining them keeps the invariant that one glyph is one
/// character, which is what makes exact lookup work.
fn merge_quotes(line: &mut Line) {
    let lh = line.height() as f32;
    loop {
        let mut found = None;
        for i in 0..line.glyphs.len().saturating_sub(1) {
            let (a, b) = (&line.glyphs[i], &line.glyphs[i + 1]);
            let small = (a.h as f32) <= lh * 0.45 && (b.h as f32) <= lh * 0.45;
            let high = (a.y as f32) < line.top as f32 + lh * 0.4
                && (b.y as f32) < line.top as f32 + lh * 0.4;
            let aligned = (a.y - b.y).abs() <= 2 && (a.h - b.h).abs() <= 2;
            let gap = b.x - (a.x + a.w);
            let close = gap >= 0 && gap <= ((a.w.max(b.w) as f32) * 0.9) as i32;
            if small && high && aligned && close {
                found = Some(i);
                break;
            }
        }
        let Some(i) = found else { break };
        let m = merge(&line.glyphs[i], &line.glyphs[i + 1]);
        line.glyphs.remove(i + 1);
        line.glyphs[i] = m;
    }
}

/// Fold a line that is nothing but marks into the line it belongs to.
///
/// On a line with no ascenders - "is coming over" - the dots of the i's are the
/// tallest thing present, so they group as a line of their own and the stems
/// are left reading as `1`. Real line spacing is several times the dot-to-stem
/// gap, so the two cases separate cleanly.
fn coalesce_mark_lines(lines: &mut Vec<Line>) {
    let mut i = 0;
    while i < lines.len() {
        let h = lines[i].height();
        let dist_next = lines
            .get(i + 1)
            .map(|n| (n.top - lines[i].bottom - 1).max(0));
        let dist_prev = if i > 0 {
            Some((lines[i].top - lines[i - 1].bottom - 1).max(0))
        } else {
            None
        };
        let small_vs = |other: &Line| h * 10 <= other.height() * 4;
        let ok_next = matches!((lines.get(i + 1), dist_next), (Some(n), Some(d))
            if small_vs(n) && d * 4 <= n.height());
        let ok_prev = i > 0
            && matches!(dist_prev, Some(d) if small_vs(&lines[i - 1]) && d * 4 <= lines[i - 1].height());

        let target = match (ok_next, ok_prev) {
            (true, true) => {
                if dist_next.unwrap_or(i32::MAX) <= dist_prev.unwrap_or(i32::MAX) {
                    Some(i + 1)
                } else {
                    Some(i - 1)
                }
            }
            (true, false) => Some(i + 1),
            (false, true) => Some(i - 1),
            _ => None,
        };
        let Some(t) = target else {
            i += 1;
            continue;
        };
        let marks = lines.remove(i);
        let t = if t > i { t - 1 } else { t };
        lines[t].top = lines[t].top.min(marks.top);
        lines[t].bottom = lines[t].bottom.max(marks.bottom);
        lines[t].glyphs.extend(marks.glyphs);
        lines[t].glyphs.sort_by_key(|g| g.x);
        i = 0; // bounds moved; rescan
    }
}

pub fn segment(spu: &Spu, palette: &[[u8; 3]], opts: &SegOpts) -> Vec<Line> {
    let Some(mask) = spu.ink_mask(palette) else {
        return Vec::new();
    };
    let comps = components(&mask, spu.w, spu.h, opts.min_ink);
    let glyphs: Vec<Glyph> = comps.iter().map(to_glyph).collect();
    let mut lines = group_lines(glyphs);
    coalesce_mark_lines(&mut lines);
    for l in lines.iter_mut() {
        merge_marks(l, opts);
        merge_diacritics(l);
        merge_quotes(l);
    }
    lines.retain(|l| !l.glyphs.is_empty());
    lines
}

/// Horizontal gap from each glyph to the next on the same line.
pub fn gaps(line: &Line) -> Vec<i32> {
    line.glyphs
        .windows(2)
        .map(|w| w[1].x - (w[0].x + w[0].w))
        .collect()
}

/// Gap in pixels above which a space is inserted on this line.
pub fn space_threshold(line: &Line, opts: &SegOpts) -> i32 {
    ((line.height() as f32 * opts.space_ratio) as i32).max(2)
}
