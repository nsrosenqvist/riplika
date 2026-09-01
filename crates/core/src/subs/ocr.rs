//! Reading a line of subtitle the slow, uncertain way, once per release.
//!
//! This is not how subtitles are decoded. Decoding is an exact lookup of
//! shapes that have already been labelled, and it stays that way. This is how a
//! table gets its labels in the first place, on a disc nobody has built one
//! for: read a sample of lines, let the votes settle what each shape is, and
//! then never do it again for that font.
//!
//! Measured on Parks and Recreation, against the same disc's hand-labelled
//! table: of 120 rendered lines, **every one** agreed with the segmentation on
//! how many characters it held - which is what the vote requires - and after
//! reading `|` as `I`, 6 characters in 1,839 differed. Those are the `I`/`l`
//! collisions and the quote shapes, which the vote records as ambiguity classes
//! rather than as labels, and which context resolves at decode time.

use crate::Result;
use crate::host::{Command, Runner};
use crate::subs::picture;
use crate::subs::segment::Line;
use std::path::Path;

/// Something that can read a rendered line of text.
pub trait Reader: Send + Sync {
    /// The text on this image, or an empty string if it will not say.
    ///
    /// An empty answer is not an error: a line it cannot read simply casts no
    /// votes, and the shapes on it wait for a line that can.
    fn read_line(&self, png: &[u8]) -> Result<String>;
}

/// How much bigger to draw a line before reading it.
///
/// DVD subtitles are rendered at about a twenty-pixel cap height and a reader
/// trained on scanned print wants roughly double that.
pub const ZOOM: usize = 3;

/// Characters a subtitle font does not contain.
///
/// A bar is the one that matters. Tesseract reads a capital I as `|` about
/// three times in four, which on its own is 15 of the 21 character errors in
/// the measurement above; no DVD subtitle has ever contained a pipe, so this
/// costs nothing and is not a guess. Everything else it gets wrong - `I` for
/// `l`, a curly quote for a straight one - is a real collision between real
/// characters and is left for the vote to settle.
fn plausible(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '|' => 'I',
            // Typographic quotes, which Tesseract picks between run to run for
            // the same shape. The disagreement is what costs: the votes split,
            // the shape settles on nothing, and it comes out as a placeholder.
            // Subtitles are written with straight quotes, and a straight quote
            // where the disc drew a curly one is a difference nobody reading
            // an episode will notice - where a blank is.
            '\u{2018}' | '\u{2019}' | '\u{2032}' => '\'',
            '\u{201c}' | '\u{201d}' | '\u{2033}' => '"',
            other => other,
        })
        .collect()
}

/// Tesseract, run once per sampled line.
pub struct Tesseract<'a> {
    pub runner: &'a dyn Runner,
    /// Where the rendered lines go while they are being read.
    pub scratch: &'a Path,
    /// The traineddata to use, e.g. `eng`.
    pub language: String,
}

impl Reader for Tesseract<'_> {
    fn read_line(&self, png: &[u8]) -> Result<String> {
        if png.is_empty() {
            return Ok(String::new());
        }
        // Named for the content, so two threads reading different lines cannot
        // collide and a repeat run reuses the same name rather than filling the
        // directory.
        let name = self.scratch.join(format!("line-{:016x}.png", fnv(png)));
        std::fs::write(&name, png).map_err(|e| crate::Error(format!("{}: {e}", name.display())))?;
        let out = self.runner.run(
            // psm 7: this image is one line of text. Told that, it stops
            // looking for a page layout that is not there.
            &Command::new("tesseract").arg(name.to_string_lossy().into_owned()).args([
                "-",
                "--psm",
                "7",
                "-l",
                &self.language,
            ]),
        );
        let _ = std::fs::remove_file(&name);
        let out = out?;
        Ok(plausible(out.stdout.lines().next().unwrap_or_default().trim()))
    }
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    h
}

/// Read one segmented line, by drawing it and handing it over.
pub fn read(reader: &dyn Reader, line: &Line) -> Result<String> {
    let png = picture::line_png(line, ZOOM);
    // A line with no glyphs draws no picture, and handing a reader a blank
    // page spends a process to be told nothing. The guard belongs here, where
    // every reader passes, rather than in each of them.
    if png.is_empty() {
        return Ok(String::new());
    }
    reader.read_line(&png)
}

/// Whether a reader is installed at all.
///
/// Checked before a disc is offered the treatment, so the answer is "this
/// machine cannot label a new font" rather than a failure forty minutes in.
pub fn available(runner: &dyn Runner) -> bool {
    runner.run(&Command::new("tesseract").arg("--version")).map(|o| o.ok()).unwrap_or(false)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A reader that answers from a script, and remembers what it was shown.
    pub struct FakeReader {
        pub answers: Mutex<Vec<String>>,
        pub seen: Mutex<Vec<usize>>,
    }

    impl FakeReader {
        pub fn new(answers: &[&str]) -> FakeReader {
            FakeReader {
                answers: Mutex::new(answers.iter().rev().map(|s| s.to_string()).collect()),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl Reader for FakeReader {
        fn read_line(&self, png: &[u8]) -> Result<String> {
            self.seen.lock().unwrap().push(png.len());
            Ok(self.answers.lock().unwrap().pop().unwrap_or_default())
        }
    }

    #[test]
    fn a_bar_is_read_as_a_capital_i() {
        // Tesseract answers "| mean, there were rumors" for three capital I in
        // four, and no subtitle font has a pipe in it.
        assert_eq!(plausible("| mean, there were rumors"), "I mean, there were rumors");
    }

    #[test]
    fn curly_quotes_are_read_as_the_straight_ones_subtitles_use() {
        // Tesseract picks between " and \u{2018} for the same shape from one line to
        // the next. Split votes settle on nothing and the shape comes out as a
        // placeholder, which is worse than either answer.
        assert_eq!(plausible("\u{201c}Hello,\u{201d} he said"), "\"Hello,\" he said");
        assert_eq!(plausible("it\u{2019}s"), "it's");
    }

    #[test]
    fn a_real_collision_is_left_for_the_vote() {
        // I and l are genuinely the same shape in many faces. Rewriting one to
        // the other here would hide the collision the table exists to record.
        assert_eq!(plausible("Iowa"), "Iowa");
        assert_eq!(plausible("lowa"), "lowa");
    }

    #[test]
    fn an_empty_picture_is_not_handed_to_a_reader() {
        // A line with no glyphs on it would spend a process to be told nothing.
        let r = FakeReader::new(&["something"]);
        let t = crate::subs::segment::Line { glyphs: Vec::new(), top: 0, bottom: 0 };
        assert_eq!(read(&r, &t).unwrap(), "");
        assert!(r.seen.lock().unwrap().is_empty());
    }
}
