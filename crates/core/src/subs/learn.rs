//! Teaching a glyph table what its shapes say.
//!
//! Two things can tell it. A set of subtitles already trusted for this disc,
//! which is how the shipped table was built and is exact. Or a reader looking
//! at the shapes, which is how a disc nobody has a table for gets one, and is
//! not. The difference is only where the text comes from. Everything past that
//! is the same machinery - the structural check, the votes, the per-glyph
//! spacing - and lives here rather than in whichever front end asked.
//!
//! The structural check is what makes an unreliable source usable. A line only
//! votes when it agrees with the segmentation on how many characters it holds,
//! so a misread line is discarded whole instead of teaching the table a
//! nonsense. What survives is then a majority across every instance of a
//! shape, and a shape whose votes will not agree becomes an ambiguity class
//! rather than a wrong label.

use crate::subs::segment::{self, Line};
use crate::subs::table::Table;
use std::collections::BTreeMap;

/// How a pass over a stream went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Lesson {
    /// Subtitle events segmented.
    pub events: usize,
    /// Glyph instances seen, labelled or not.
    pub instances: usize,
    /// Cues whose text lined up with the shapes, so it could vote.
    pub aligned: usize,
    /// Cues whose text did not line up and was therefore ignored.
    pub skipped: usize,
    /// Votes cast.
    pub votes: usize,
}

/// What settling the votes decided.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settled {
    /// Shapes given a label.
    pub labelled: usize,
    /// Shapes the font draws identically, recorded as `"l|I"`.
    pub ambiguous: usize,
    /// Shapes the votes would not settle. These stay blank.
    pub undecided: usize,
    /// Shapes that learned their own spacing threshold.
    pub spacing: usize,
}

/// Votes and spacing observations, accumulated across a stream.
#[derive(Default)]
pub struct Teacher {
    pub lesson: Lesson,
    /// Per glyph: gaps seen with no space after, and gaps seen with one.
    gaps: BTreeMap<usize, (Vec<i32>, Vec<i32>)>,
}

impl Teacher {
    pub fn new() -> Teacher {
        Teacher::default()
    }

    /// Take in one cue's shapes, and its text if anything knows it.
    ///
    /// The shapes go into the table either way - a table of shapes with no
    /// labels is still worth having, since it is what the review page shows.
    pub fn read(&mut self, table: &mut Table, lines: &[Line], text: Option<&str>) {
        self.lesson.events += 1;
        let idxs: Vec<Vec<usize>> =
            lines.iter().map(|l| l.glyphs.iter().map(|g| table.observe(g)).collect()).collect();
        let linegaps: Vec<Vec<i32>> = lines.iter().map(segment::gaps).collect();
        self.lesson.instances += idxs.iter().map(|v| v.len()).sum::<usize>();

        let Some(text) = text else {
            return;
        };
        // Vote only where the shapes and the text agree exactly on structure.
        // A guess here would poison the table, and unlike a wrong recognition a
        // wrong label is wrong for every future disc that uses this table.
        let rlines: Vec<&str> = text.lines().collect();
        let fits = rlines.len() == idxs.len()
            && rlines
                .iter()
                .zip(&idxs)
                .all(|(r, gs)| r.chars().filter(|c| !c.is_whitespace()).count() == gs.len());
        if !fits {
            self.lesson.skipped += 1;
            return;
        }
        self.lesson.aligned += 1;

        for ((r, gs), lg) in rlines.iter().zip(&idxs).zip(&linegaps) {
            // Walk the text including its spaces, so this learns not only what
            // each shape is but whether a space follows it.
            let mut k = 0usize;
            let mut space_pending = false;
            for c in r.chars() {
                if c.is_whitespace() {
                    space_pending = true;
                    continue;
                }
                if k >= gs.len() {
                    break;
                }
                table.vote(gs[k], &c.to_string());
                self.lesson.votes += 1;
                if k > 0
                    && let Some(&g) = lg.get(k - 1)
                {
                    let e = self.gaps.entry(gs[k - 1]).or_default();
                    if space_pending { e.1.push(g) } else { e.0.push(g) }
                }
                space_pending = false;
                k += 1;
            }
        }
    }

    /// How many votes a shape has so far.
    ///
    /// Used to decide whether a cue is worth reading: one holding only shapes
    /// that are already well attested teaches nothing, and reading it costs a
    /// process.
    pub fn thin(table: &Table, lines: &[Line], enough: u64) -> bool {
        lines.iter().flat_map(|l| &l.glyphs).any(|g| {
            table.get(&g.key()).map(|e| e.votes.values().sum::<u64>() < enough).unwrap_or(true)
        })
    }

    /// Turn the observations into thresholds and settle the votes.
    pub fn settle(self, table: &mut Table, min_agreement: f32) -> Settled {
        // Midway between the typical within-word gap and the typical gap that
        // carried a space. Per glyph, because the tail of an f or a y eats into
        // the gap after it and one global number turns "if you" into "ifyou".
        let mut spacing = 0;
        for (gi, (mut no, mut yes)) in self.gaps {
            if no.len() < 6 || yes.len() < 6 {
                continue;
            }
            no.sort_unstable();
            yes.sort_unstable();
            let lo = no[no.len() * 3 / 4];
            let hi = yes[yes.len() / 4];
            if hi > lo
                && let Some(g) = table.glyphs.get_mut(gi)
            {
                g.gap = Some((lo + hi + 1) / 2);
                spacing += 1;
            }
        }
        let (labelled, ambiguous, undecided) = table.apply_votes(min_agreement);
        table.reindex();
        Settled { labelled, ambiguous, undecided, spacing }
    }
}

/// How much reading to do before calling a table built.
#[derive(Debug, Clone, Copy)]
pub struct Effort {
    /// Votes a shape needs before cues holding only it are skipped.
    ///
    /// A shape needs enough for a runner-up to be visible as a collision
    /// rather than as noise, which `apply_votes` puts at ten.
    pub enough: u64,
    /// The most cues to read, however thin the table still is.
    ///
    /// A bound on the time rather than on the quality: a disc whose rare
    /// shapes only appear in the last reel is not worth another ten minutes of
    /// processes, and what stays unlabelled is reported rather than guessed.
    pub cues: usize,
    /// The share of votes a shape needs before it is called labelled.
    pub agreement: f32,
}

impl Default for Effort {
    fn default() -> Self {
        Effort { enough: 24, cues: 400, agreement: 0.9 }
    }
}

/// One subtitle stream, as the reader needs to see it.
pub struct Stream<'a> {
    pub events: &'a [crate::subs::vobsub::Event],
    pub palette: &'a [[u8; 3]],
    pub opts: &'a segment::SegOpts,
}

/// Build or extend a table by reading the stream's own lines.
///
/// Only cues holding a shape that is still thin are read: a film has a
/// thousand cues and the alphabet is done with in the first few dozen, so
/// reading all of them would spend an hour of processes to learn nothing.
pub fn from_reader(
    reader: &dyn crate::subs::ocr::Reader,
    stream: Stream<'_>,
    table: &mut Table,
    how: Effort,
    progress: &mut dyn FnMut(f32),
) -> crate::Result<Settled> {
    let Stream { events, palette, opts } = stream;
    let (effort, min_agreement) = (how, how.agreement);
    let mut teacher = Teacher::new();
    let mut read = 0usize;
    for (n, ev) in events.iter().enumerate() {
        let lines = segment::segment(&ev.spu, palette, opts);
        if lines.iter().all(|l| l.glyphs.is_empty()) {
            continue;
        }
        if read >= effort.cues || !Teacher::thin(table, &lines, effort.enough) {
            // Still taken in, so the table holds every shape on the disc even
            // where nothing read it. An unlabelled shape is a blank in the
            // output; a missing one is a shape the next disc has to discover.
            teacher.read(table, &lines, None);
            continue;
        }
        let mut text = String::new();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                text.push('\n');
            }
            text.push_str(&crate::subs::ocr::read(reader, line)?);
        }
        teacher.read(table, &lines, Some(&text));
        read += 1;
        progress((read as f32 / effort.cues as f32).min(1.0));
        let _ = n;
    }
    progress(1.0);
    Ok(teacher.settle(table, min_agreement))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subs::segment::Glyph;

    /// A glyph whose shape is decided by one byte, so tests can spell.
    fn g(shape: u8, x: i32) -> Glyph {
        Glyph { x, y: 0, w: 2, h: 2, bits: vec![1, shape, shape, 1] }
    }

    /// A line of shapes, spaced so `gaps` sees the wide ones as word breaks.
    fn line(spec: &[(u8, i32)]) -> Line {
        Line { glyphs: spec.iter().map(|(s, x)| g(*s, *x)).collect(), top: 0, bottom: 1 }
    }

    #[test]
    fn text_that_lines_up_teaches_every_shape_on_it() {
        let mut t = Table::default();
        let mut teacher = Teacher::new();
        teacher.read(&mut t, &[line(&[(1, 0), (2, 4), (3, 8)])], Some("cat"));
        assert_eq!(teacher.lesson.aligned, 1);
        assert_eq!(teacher.lesson.votes, 3);
        let settled = teacher.settle(&mut t, 0.9);
        assert_eq!(settled.labelled, 3);
    }

    #[test]
    fn text_that_does_not_line_up_teaches_nothing_at_all() {
        // The whole line is discarded, not the character that disagreed: once
        // the counts differ there is no telling which shape is which, and a
        // label guessed here is wrong for every disc that reuses the table.
        let mut t = Table::default();
        let mut teacher = Teacher::new();
        teacher.read(&mut t, &[line(&[(1, 0), (2, 4), (3, 8)])], Some("cart"));
        assert_eq!(teacher.lesson.aligned, 0);
        assert_eq!(teacher.lesson.skipped, 1);
        assert_eq!(teacher.lesson.votes, 0);
        // but the shapes are still in the table, for the review page
        assert_eq!(t.glyphs.len(), 3);
    }

    #[test]
    fn a_shape_the_readers_disagree_about_is_not_given_a_label() {
        // I and l are the same shape in many faces. Recording one of them would
        // be wrong half the time; recording the collision lets context settle
        // it at decode time.
        let mut t = Table::default();
        let mut teacher = Teacher::new();
        for i in 0..60 {
            let text = if i % 2 == 0 { "I" } else { "l" };
            teacher.read(&mut t, &[line(&[(7, 0)])], Some(text));
        }
        let settled = teacher.settle(&mut t, 0.9);
        assert_eq!(settled.labelled, 0);
        assert_eq!(settled.ambiguous, 1);
        assert_eq!(t.glyphs[0].text.as_deref(), Some("I|l"));
    }

    #[test]
    fn a_reader_that_is_wrong_now_and_then_is_outvoted() {
        // The measured rate is 6 characters in 1,839. This is that, exaggerated
        // ten times over, and the shape still comes out right.
        let mut t = Table::default();
        let mut teacher = Teacher::new();
        for i in 0..100 {
            let text = if i % 30 == 0 { "e" } else { "a" };
            teacher.read(&mut t, &[line(&[(9, 0)])], Some(text));
        }
        let settled = teacher.settle(&mut t, 0.9);
        assert_eq!(settled.labelled, 1);
        assert_eq!(t.glyphs[0].text.as_deref(), Some("a"));
    }

    #[test]
    fn shapes_are_taken_in_even_when_nothing_knows_what_they_say() {
        // An unlabelled table is what the review page is for, and what a second
        // pass with a reader extends.
        let mut t = Table::default();
        let mut teacher = Teacher::new();
        teacher.read(&mut t, &[line(&[(1, 0), (2, 4)])], None);
        assert_eq!(t.glyphs.len(), 2);
        assert_eq!(teacher.lesson.instances, 2);
        assert_eq!(teacher.lesson.votes, 0);
    }

    #[test]
    fn a_cue_of_shapes_already_well_attested_is_not_worth_reading() {
        // Reading every cue on a film would spend an hour of processes to learn
        // nothing after the first few minutes.
        let mut t = Table::default();
        let mut teacher = Teacher::new();
        let l = line(&[(1, 0), (2, 4)]);
        assert!(Teacher::thin(&t, std::slice::from_ref(&l), 10), "an empty table knows nothing");
        for _ in 0..12 {
            teacher.read(&mut t, std::slice::from_ref(&l), Some("ab"));
        }
        assert!(!Teacher::thin(&t, std::slice::from_ref(&l), 10));
        // a cue holding one shape nobody has seen is still worth reading
        let fresh = line(&[(1, 0), (5, 4)]);
        assert!(Teacher::thin(&t, std::slice::from_ref(&fresh), 10));
    }
}
