//! Turning segmented glyphs back into text via the table.

use crate::subs::resolve::{Resolver, Slot};
use crate::subs::segment::Line;
use crate::subs::table::Table;

pub struct Recognized {
    pub text: String,
    pub unknown: Vec<String>,
}

/// Word gaps depend on the font size, which is fixed per disc but not known in
/// advance, so measure it from the file.
///
/// The gap distribution is bimodal - tight gaps inside a word, wider ones
/// between words - but it has a long tail from dialogue dashes and split
/// layouts. Clip that tail, then take an Otsu split of the histogram; plain
/// k-means gets dragged into the tail and swallows every space on the disc.
pub fn estimate_space_gap(all: &[Vec<Line>], fallback: i32) -> i32 {
    let mut gaps: Vec<i32> = Vec::new();
    let mut heights: Vec<i32> = Vec::new();
    for lines in all {
        for l in lines {
            heights.push(l.height());
            for w in l.glyphs.windows(2) {
                let g = w[1].x - (w[0].x + w[0].w);
                if g >= 0 {
                    gaps.push(g);
                }
            }
        }
    }
    if gaps.len() < 50 {
        return fallback;
    }
    gaps.sort_unstable();
    heights.sort_unstable();
    let lh = heights[heights.len() / 2].max(1);

    // discard the top 2% - those are layout gaps, not spaces
    let clip = gaps[(gaps.len() as f32 * 0.98) as usize % gaps.len()].max(4);
    let mut hist = vec![0u32; clip as usize + 1];
    for &g in &gaps {
        hist[g.min(clip) as usize] += 1;
    }

    let total: u32 = hist.iter().sum();
    let sum_all: f64 = hist
        .iter()
        .enumerate()
        .map(|(i, &c)| i as f64 * c as f64)
        .sum();
    let (mut w0, mut sum0) = (0u32, 0f64);
    let (mut best_t, mut best_var) = (fallback, -1f64);
    for t in 0..hist.len() {
        w0 += hist[t];
        sum0 += t as f64 * hist[t] as f64;
        let w1 = total - w0;
        if w0 == 0 || w1 == 0 {
            continue;
        }
        let m0 = sum0 / w0 as f64;
        let m1 = (sum_all - sum0) / w1 as f64;
        let var = w0 as f64 * w1 as f64 * (m0 - m1) * (m0 - m1);
        if var > best_var {
            best_var = var;
            best_t = t as i32 + 1;
        }
    }

    // a space is a fraction of the line height - refuse anything absurd
    let lo = (lh as f32 * 0.10).round().max(2.0) as i32;
    let hi = (lh as f32 * 0.55).round() as i32;
    best_t.clamp(lo, hi.max(lo))
}

/// Build the slot sequence for one subtitle, splitting into words on gaps.
type Word = (Vec<Slot>, Vec<(i32, i32)>);

fn to_words(
    line: &Line,
    table: &Table,
    space_gap: i32,
    placeholder: char,
    unknown: &mut Vec<String>,
) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut cur: Vec<Slot> = Vec::new();
    let mut curgaps: Vec<(i32, i32)> = Vec::new();
    let mut prev_right: Option<i32> = None;

    let mut prev_gap: Option<i32> = None;
    for g in &line.glyphs {
        if let Some(pr) = prev_right {
            // the threshold belongs to the glyph on the left of the gap
            let thr = prev_gap.unwrap_or(space_gap);
            let gap = g.x - pr;
            if gap >= thr && !cur.is_empty() {
                words.push((std::mem::take(&mut cur), std::mem::take(&mut curgaps)));
            } else if !cur.is_empty() {
                curgaps.push((gap, thr));
            }
        }
        let key = g.key();
        prev_gap = table.get(&key).and_then(|e| e.gap);
        match table.get(&key).and_then(|e| e.text.clone()) {
            Some(t) if t.contains('|') => {
                cur.push(Slot::Ambiguous(t.split('|').map(str::to_string).collect()))
            }
            Some(t) => cur.push(Slot::Fixed(t)),
            None => {
                unknown.push(key);
                cur.push(Slot::Fixed(placeholder.to_string()));
            }
        }
        prev_right = Some(g.x + g.w);
    }
    if !cur.is_empty() {
        words.push((cur, curgaps));
    }
    words
}

pub fn lines_to_text(
    lines: &[Line],
    table: &Table,
    resolver: &Resolver,
    space_gap: i32,
    placeholder: char,
) -> Recognized {
    let mut out: Vec<String> = Vec::new();
    let mut unknown = Vec::new();

    for line in lines {
        let words = to_words(line, table, space_gap, placeholder, &mut unknown);
        let s = resolver.resolve_line(&words);
        let s = s.trim().to_string();
        if !s.is_empty() {
            out.push(s);
        }
    }

    Recognized {
        text: out.join("\n"),
        unknown,
    }
}
