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

    /// The same for many images at once, answered in the order given.
    ///
    /// Worth having separately because the cost here is almost entirely the
    /// process, not the reading: a table for a disc took a couple of minutes
    /// as eight hundred runs of Tesseract and takes a few seconds as a dozen.
    ///
    /// The default is the honest one-at-a-time loop, so a reader only
    /// implements this if it can actually do better.
    fn read_lines(&self, pngs: &[Vec<u8>]) -> Result<Vec<String>> {
        pngs.iter().map(|p| self.read_line(p)).collect()
    }
}

/// Split what a reader answered for several images back into one each.
///
/// Tesseract ends every page with a form feed except, sometimes, the last.
/// Anything other than exactly one page per image means the pages cannot be
/// matched to the images that produced them - and a page matched to the wrong
/// image votes the wrong labels onto the wrong shapes, into a table that is
/// then reused by every disc of that release. So this refuses rather than
/// guesses, and the caller falls back to reading them one at a time.
fn pages(out: &str, wanted: usize) -> Option<Vec<String>> {
    let mut parts: Vec<&str> = out.split('\u{c}').collect();
    if parts.len() == wanted + 1 && parts.last().is_some_and(|p| p.trim().is_empty()) {
        parts.pop();
    }
    (parts.len() == wanted)
        .then(|| parts.into_iter().map(|p| plausible(p.trim_matches('\n').trim())).collect())
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
    fn read_lines(&self, pngs: &[Vec<u8>]) -> Result<Vec<String>> {
        if pngs.is_empty() {
            return Ok(Vec::new());
        }
        match self.read_together(pngs) {
            Ok(text) => Ok(text),
            // One at a time still works and is only slow. Falling back beats
            // losing the disc's lettering over a batch that would not line up.
            Err(_) => pngs.iter().map(|p| self.read_line(p)).collect(),
        }
    }

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

impl Tesseract<'_> {
    /// Everything in one run, which is where the time goes.
    fn read_together(&self, pngs: &[Vec<u8>]) -> Result<Vec<String>> {
        let mut written = Vec::with_capacity(pngs.len());
        let mut list = String::new();
        for (i, png) in pngs.iter().enumerate() {
            let name = self.scratch.join(format!("batch-{i:05}.png"));
            std::fs::write(&name, png)
                .map_err(|e| crate::Error(format!("{}: {e}", name.display())))?;
            list.push_str(&name.to_string_lossy());
            list.push('\n');
            written.push(name);
        }
        let list_path = self.scratch.join("batch.txt");
        let outcome = std::fs::write(&list_path, list.as_bytes())
            .map_err(|e| crate::Error(format!("{}: {e}", list_path.display())))
            .and_then(|()| {
                self.runner.run(
                    &Command::new("tesseract")
                        .arg(list_path.to_string_lossy().into_owned())
                        .args(["-", "--psm", "7", "-l", &self.language]),
                )
            });
        for p in written.iter().chain(std::iter::once(&list_path)) {
            let _ = std::fs::remove_file(p);
        }
        let out = outcome?;
        // A page it could not read stops the whole run rather than being
        // skipped, so this is where that shows up.
        pages(&out.stdout, pngs.len()).ok_or_else(|| {
            crate::Error("the reader answered with the wrong number of pages".into())
        })
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

/// The traineddata installed, e.g. `["eng", "swe"]`.
///
/// Asked rather than assumed: reading a Swedish track with the English data
/// labels every a-ring as an a, and a table is reused by every disc of that
/// release afterwards, so a wrong label here is not a wrong episode but a
/// wrong season. Where the track's own language is missing, English still
/// reads most of the alphabet and the letters it cannot are left blank.
pub fn languages(runner: &dyn Runner) -> Vec<String> {
    let Ok(out) = runner.run(&Command::new("tesseract").arg("--list-langs")) else {
        return Vec::new();
    };
    out.stdout
        .lines()
        .chain(out.stderr.lines())
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains(' ') && !l.ends_with(':'))
        .map(str::to_string)
        .collect()
}

/// Which traineddata to read a track of this language with.
///
/// Its own or none. Falling back to English looked generous and was not: read
/// with English data, Frozen's Icelandic track voted `d` and `o` for one shape
/// and `p` and `b` for another, and those votes went into the table the
/// English and Swedish tracks share - so a language nobody could read did not
/// merely fail, it took the two that had worked down with it. A track whose
/// language is not installed keeps its bitmaps, which is a subtitle, where a
/// table taught nonsense is wrong for every disc of the release.
pub fn data_for(installed: &[String], language: &str) -> Option<String> {
    installed.iter().find(|l| *l == language).cloned()
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
    fn a_track_is_only_read_with_its_own_language() {
        let installed = vec!["eng".to_string(), "swe".to_string()];
        assert_eq!(data_for(&installed, "swe").as_deref(), Some("swe"));
        // Icelandic is not installed, and English is not a substitute: read
        // that way it votes d and o for one shape and p and b for another,
        // into a table the English and Swedish tracks share.
        assert_eq!(data_for(&installed, "isl"), None);
        assert_eq!(data_for(&[], "eng"), None);
    }

    #[test]
    fn the_language_list_is_read_past_the_line_that_introduces_it() {
        // tesseract prints "List of available languages in ...:" first, and
        // taking that as a language name asks it for traineddata called "List".
        let out = "List of available languages in \"/x/tessdata\" (3):\neng\nosd\nswe\n";
        let langs: Vec<&str> = out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.contains(' ') && !l.ends_with(':'))
            .collect();
        assert_eq!(langs, ["eng", "osd", "swe"]);
    }

    #[test]
    fn pages_come_back_one_per_image() {
        // Tesseract ends every page with a form feed, except sometimes the
        // last one, so both shapes have to be read the same way.
        let with = "one\n\u{c}two\n\u{c}three\n\u{c}";
        let without = "one\n\u{c}two\n\u{c}three\n";
        for out in [with, without] {
            assert_eq!(
                pages(out, 3).as_deref(),
                Some(&["one".to_string(), "two".into(), "three".into()][..])
            );
        }
    }

    #[test]
    fn a_page_that_read_as_nothing_still_holds_its_place() {
        // A blank line in the middle must not shift every answer after it onto
        // the wrong image.
        let out = "one\n\u{c}\n\u{c}three\n";
        assert_eq!(
            pages(out, 3).as_deref(),
            Some(&["one".to_string(), String::new(), "three".into()][..])
        );
    }

    #[test]
    fn the_wrong_number_of_pages_is_refused_rather_than_lined_up_anyway() {
        // The failure this prevents is silent and permanent: a page matched to
        // the wrong image votes the wrong labels onto the wrong shapes, into a
        // table every later disc of that release is then decoded with.
        assert_eq!(pages("one\n\u{c}two\n", 3), None);
        assert_eq!(pages("", 2), None);
    }

    #[test]
    fn a_batch_is_read_with_the_same_rewrites_as_a_single_line() {
        assert_eq!(pages("| mean\n\u{c}", 1).as_deref(), Some(&["I mean".to_string()][..]));
    }

    #[test]
    fn a_reader_with_no_batching_of_its_own_still_answers_in_order() {
        let r = FakeReader::new(&["first", "second", "third"]);
        let pngs = vec![vec![1u8], vec![2], vec![3]];
        assert_eq!(r.read_lines(&pngs).unwrap(), ["first", "second", "third"]);
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
