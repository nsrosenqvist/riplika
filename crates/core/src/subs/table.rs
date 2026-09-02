//! The glyph table: the exact-match lookup that replaces statistical OCR.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

use crate::subs::segment::Glyph;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub key: String,
    pub w: i32,
    pub h: i32,
    /// Base64 of one byte per pixel, 1 = ink.
    pub bits: String,
    /// The character(s) this glyph represents. `None` until labelled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// How often the glyph was seen while building the table.
    #[serde(default)]
    pub count: u64,
    /// Label votes gathered during bootstrap, highest first when saved.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub votes: BTreeMap<String, u64>,
    /// Gap after this glyph above which a space follows.
    ///
    /// Learned per glyph because letters overhang differently: the tail of an
    /// `f` or `y` eats into the gap, so one global number splits `if you` into
    /// `ifyou` while leaving other pairs correct.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap: Option<i32>,
}

impl Entry {
    pub fn bitmap(&self) -> Vec<u8> {
        B64.decode(self.bits.as_bytes()).unwrap_or_default()
    }

    /// Fraction of votes that agree with the chosen label, 1.0 when unanimous.
    pub fn agreement(&self) -> f32 {
        let total: u64 = self.votes.values().sum();
        if total == 0 {
            return 0.0;
        }
        let top = self.votes.values().copied().max().unwrap_or(0);
        top as f32 / total as f32
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Table {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub source: String,
    pub glyphs: Vec<Entry>,
    #[serde(skip)]
    index: BTreeMap<String, usize>,
}

fn one() -> u32 {
    1
}

impl Table {
    pub fn load(path: &Path) -> Result<Table, String> {
        let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut t: Table =
            serde_json::from_str(&s).map_err(|e| format!("{}: {e}", path.display()))?;
        t.reindex();
        Ok(t)
    }

    /// The same as `load`, for callers that read through the `Fs` port.
    ///
    /// The index is not stored - it is derived from the glyphs - so a table
    /// deserialised without rebuilding it looks empty to every lookup, which
    /// reads as "this table knows nothing about this disc".
    pub fn from_bytes(bytes: &[u8]) -> Result<Table, String> {
        let mut t: Table = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        t.reindex();
        Ok(t)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut t = self.clone();
        // most-seen first keeps the review sheet in a useful order
        t.glyphs.sort_by(|a, b| b.count.cmp(&a.count).then(a.key.cmp(&b.key)));
        let s = serde_json::to_string_pretty(&t).map_err(|e| e.to_string())?;
        std::fs::write(path, s).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn reindex(&mut self) {
        self.index = self.glyphs.iter().enumerate().map(|(i, g)| (g.key.clone(), i)).collect();
    }

    pub fn get(&self, key: &str) -> Option<&Entry> {
        self.index.get(key).map(|&i| &self.glyphs[i])
    }

    pub fn observe(&mut self, g: &Glyph) -> usize {
        let key = g.key();
        if let Some(&i) = self.index.get(&key) {
            self.glyphs[i].count += 1;
            return i;
        }
        let e = Entry {
            key: key.clone(),
            w: g.w,
            h: g.h,
            bits: B64.encode(&g.bits),
            text: None,
            count: 1,
            votes: BTreeMap::new(),
            gap: None,
        };
        self.glyphs.push(e);
        let i = self.glyphs.len() - 1;
        self.index.insert(key, i);
        i
    }

    pub fn vote(&mut self, idx: usize, label: &str) {
        *self.glyphs[idx].votes.entry(label.to_string()).or_insert(0) += 1;
    }
}

/// How sure the votes have to be before a shape is called labelled.
///
/// Two sets of numbers, because two very different things cast votes.
/// Subtitles already trusted for this disc are exact, so anything short of
/// near-unanimity means the font genuinely draws two characters alike. A
/// reader looking at the shapes is right about 99 characters in 100 and
/// wrong in a scatter, so demanding the same unanimity throws away shapes
/// it was overwhelmingly right about - The Lion King's `l` had 85% of 273
/// votes and was left blank, losing 754 instances of the commonest letter
/// on the disc to placeholders.
#[derive(Debug, Clone, Copy)]
pub struct Settling {
    /// Share of the votes the top label needs to be adopted outright.
    pub agreement: f32,
    /// Share the top two together need before the shape is called a
    /// collision rather than a mess.
    pub covered: f32,
    /// Votes the runner-up needs before it counts as a real second reading
    /// rather than noise.
    pub second: u64,
    /// And as a share, so a long run does not make noise look like signal.
    pub second_share: f32,
    /// Treat a runner-up the font is known to draw alike as a collision,
    /// however comfortably the top label leads.
    ///
    /// Vote share alone cannot tell these apart. The Lion King's English `l`
    /// took 85% with `/` behind it, and its Swedish vertical stroke took 86%
    /// with `I` behind it - the first is one letter read badly and wants the
    /// label, the second is two letters the font draws identically and wants
    /// the class. What separates them is not how big the runner-up is but
    /// what it is.
    pub collisions: bool,
}

/// Characters a subtitle face is apt to draw with the same shape.
///
/// Only the vertical strokes, which is the collision this project has actually
/// met - `l|I` is the case the resolver's structural rules are written for.
/// Kept deliberately short: every pair added here is a shape that stops being
/// decided by the votes, so a wrong entry costs a letter on every disc.
fn drawn_alike(a: &str, b: &str) -> bool {
    const STROKES: [&str; 4] = ["l", "I", "i", "1"];
    a != b && STROKES.contains(&a) && STROKES.contains(&b)
}

/// A letter with its accent taken off, for comparing one to another.
///
/// The shapes are genuinely different - the accent is drawn - so this is not a
/// collision the way `l` and `I` are, and a shape the reader is sure about
/// keeps its label. It only settles the ones it was unsure of, where the whole
/// disagreement was over whether there is an accent on it at all.
fn bare(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
        'è' | 'é' | 'ê' | 'ë' | 'ē' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ý' | 'ÿ' => 'y',
        'ñ' => 'n',
        'ç' => 'c',
        other => other,
    }
}

/// The same letter, disagreeing only about its accent.
fn the_same_letter(a: &str, b: &str) -> bool {
    let strip = |s: &str| -> Option<char> {
        let mut it = s.chars();
        let c = it.next()?;
        it.next().is_none().then(|| bare(c.to_ascii_lowercase()))
    };
    a != b && strip(a).is_some() && strip(a) == strip(b)
}

impl Settling {
    /// For votes from subtitles already trusted for this disc.
    pub fn from_a_reference(agreement: f32) -> Settling {
        Settling { agreement, covered: 0.97, second: 10, second_share: 0.08, collisions: false }
    }

    /// For votes from something reading the shapes.
    pub fn from_a_reader() -> Settling {
        Settling { agreement: 0.80, covered: 0.70, second: 8, second_share: 0.05, collisions: true }
    }
}

impl Table {
    /// Adopt the majority vote as each glyph's label.
    ///
    /// When two labels each hold a real share of the votes the glyph is not
    /// mislabelled - the font simply draws both characters identically (capital
    /// I and lowercase l, most often). Record it as an ambiguity class
    /// `"l|I"`, most frequent first, and let context resolve it later.
    pub fn apply_votes(&mut self, min_agreement: f32) -> (usize, usize, usize) {
        self.settle_votes(Settling::from_a_reference(min_agreement))
    }

    /// The same, with the thresholds said out loud.
    pub fn settle_votes(&mut self, how: Settling) -> (usize, usize, usize) {
        let min_agreement = how.agreement;
        let (mut set, mut ambiguous, mut skipped) = (0, 0, 0);
        for g in self.glyphs.iter_mut() {
            if g.votes.is_empty() {
                continue;
            }
            let total: u64 = g.votes.values().sum();
            let mut ranked: Vec<(&String, &u64)> = g.votes.iter().collect();
            ranked.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
            let top = *ranked[0].1;

            // Asked before the majority, because a shape two letters share is
            // not settled by one of them being commoner in the sample.
            let collision = how.collisions
                && ranked.len() >= 2
                && drawn_alike(ranked[0].0, ranked[1].0)
                && *ranked[1].1 >= 5
                && (*ranked[1].1 as f32 / total as f32) >= 0.05;
            if collision {
                g.text = Some(format!("{}|{}", ranked[0].0, ranked[1].0));
                ambiguous += 1;
                continue;
            }
            if (top as f32 / total as f32) >= min_agreement {
                g.text = Some(ranked[0].0.clone());
                set += 1;
                continue;
            }
            // Not sure enough to call it, and the disagreement is only about
            // whether the letter has an accent on it. Better a class context
            // can settle than a placeholder: The Lion King's disc read é five
            // times and e three, which is 62% and under any sensible bar, and
            // "jättebuffé" came out as "jättebuff" and a box. Two votes is
            // enough here where eight is demanded elsewhere, because a letter
            // with an accent turns up a handful of times in a whole film - the
            // shape had eight instances on that disc, and every one was read.
            if how.collisions
                && ranked.len() >= 2
                && the_same_letter(ranked[0].0, ranked[1].0)
                && *ranked[1].1 >= 2
                && (top + *ranked[1].1) as f32 / total as f32 >= 0.9
            {
                g.text = Some(format!("{}|{}", ranked[0].0, ranked[1].0));
                ambiguous += 1;
                continue;
            }
            // Two strong candidates that between them explain the glyph. Demand
            // real support for the runner-up: a handful of stray votes is noise
            // in the bootstrap source, not a genuine collision.
            if ranked.len() >= 2 {
                let second = *ranked[1].1;
                let covered = (top + second) as f32 / total as f32;
                if covered >= how.covered
                    && second >= how.second
                    && (second as f32 / total as f32) >= how.second_share
                {
                    g.text = Some(format!("{}|{}", ranked[0].0, ranked[1].0));
                    ambiguous += 1;
                    continue;
                }
            }
            skipped += 1;
        }
        (set, ambiguous, skipped)
    }

    pub fn unlabelled(&self) -> usize {
        self.glyphs.iter().filter(|g| g.text.is_none()).count()
    }

    pub fn labelled(&self) -> usize {
        self.glyphs.iter().filter(|g| g.text.is_some()).count()
    }
}

#[cfg(test)]
mod settling_tests {
    use super::*;
    use crate::subs::segment::Glyph;

    fn shape(n: u8) -> Glyph {
        Glyph { x: 0, y: 0, w: 2, h: 2, bits: vec![1, n, n, 1] }
    }

    /// A shape carrying the votes The Lion King's lowercase l actually got.
    fn the_lion_kings_l() -> Table {
        let mut t = Table::default();
        let i = t.observe(&shape(1));
        for (label, n) in [("l", 232), ("/", 16), ("f", 15), ("i", 10)] {
            for _ in 0..n {
                t.vote(i, label);
            }
        }
        t
    }

    #[test]
    fn a_reader_that_was_right_five_times_in_six_is_believed() {
        // 232 of 273 votes for l, and it came out blank - so 754 instances of
        // the commonest letter on the disc were drawn as placeholders.
        let mut t = the_lion_kings_l();
        let (labelled, _, _) = t.settle_votes(Settling::from_a_reader());
        assert_eq!(labelled, 1);
        assert_eq!(t.glyphs[0].text.as_deref(), Some("l"));
    }

    #[test]
    fn the_same_votes_from_a_trusted_reference_are_a_disagreement() {
        // A reference is exact, so five readings in six agreeing means the
        // reference disagrees with itself, which is not a label.
        let mut t = the_lion_kings_l();
        let (labelled, _, undecided) = t.settle_votes(Settling::from_a_reference(0.9));
        assert_eq!(labelled, 0);
        assert_eq!(undecided, 1);
    }

    #[test]
    fn a_stroke_two_letters_share_is_a_collision_however_far_ahead_one_is() {
        // The Lion King's Swedish vertical stroke: l 113, I 18. Labelled l, it
        // wrote BIOGRAFI as BlOGRAFl. The share is no different from the
        // English l that deserves its label - what differs is that the runner
        // up here is a letter the face draws the same way.
        let mut t = Table::default();
        let i = t.observe(&shape(3));
        for (label, n) in [("l", 113), ("I", 18)] {
            for _ in 0..n {
                t.vote(i, label);
            }
        }
        let (labelled, ambiguous, _) = t.settle_votes(Settling::from_a_reader());
        assert_eq!((labelled, ambiguous), (0, 1));
        assert_eq!(t.glyphs[0].text.as_deref(), Some("l|I"));
    }

    #[test]
    fn a_runner_up_that_is_nonsense_does_not_make_a_collision() {
        // The English l: l 232, / 16. A face does not draw l and a solidus
        // alike, so this is one letter read badly and it keeps its label.
        let mut t = the_lion_kings_l();
        let (labelled, ambiguous, _) = t.settle_votes(Settling::from_a_reader());
        assert_eq!((labelled, ambiguous), (1, 0));
        assert_eq!(t.glyphs[0].text.as_deref(), Some("l"));
    }

    #[test]
    fn an_accent_the_reader_was_unsure_of_beats_a_placeholder() {
        // Cloudy with a Chance of Meatballs: the é shape appears eight times in
        // the whole film and was read é five times and e three. 62% is under
        // any sensible bar and three votes is under the runner-up floor, so it
        // came out blank and "jättebuffé" was drawn as "jättebuff" and a box.
        let mut t = Table::default();
        let i = t.observe(&shape(6));
        for (label, n) in [("é", 5), ("e", 3)] {
            for _ in 0..n {
                t.vote(i, label);
            }
        }
        let (labelled, ambiguous, _) = t.settle_votes(Settling::from_a_reader());
        assert_eq!((labelled, ambiguous), (0, 1));
        assert_eq!(t.glyphs[0].text.as_deref(), Some("é|e"));
    }

    #[test]
    fn a_letter_the_reader_was_sure_of_keeps_its_label() {
        // An accent is drawn, so these really are different shapes. A shape
        // read as e ninety times in a hundred is an e, and turning that into a
        // class would hand the resolver a decision it does not need to make.
        let mut t = Table::default();
        let i = t.observe(&shape(7));
        for (label, n) in [("e", 90), ("é", 10)] {
            for _ in 0..n {
                t.vote(i, label);
            }
        }
        let (labelled, ambiguous, _) = t.settle_votes(Settling::from_a_reader());
        assert_eq!((labelled, ambiguous), (1, 0));
        assert_eq!(t.glyphs[0].text.as_deref(), Some("e"));
    }

    #[test]
    fn two_different_letters_are_not_rescued_this_way() {
        // The rule is for one letter argued about, not for a shape nobody
        // could read: c and o are not the same letter with the accent in
        // question, and a class of them would be a guess dressed as evidence.
        let mut t = Table::default();
        let i = t.observe(&shape(8));
        for (label, n) in [("c", 5), ("o", 3)] {
            for _ in 0..n {
                t.vote(i, label);
            }
        }
        let (labelled, ambiguous, undecided) = t.settle_votes(Settling::from_a_reader());
        assert_eq!((labelled, ambiguous, undecided), (0, 0, 1));
    }

    #[test]
    fn a_stray_reading_of_a_lookalike_is_still_only_a_stray_reading() {
        // i 424 against I 8 is not a collision, it is a reader slipping twice
        // in a hundred. Treating it as one would put a class on every stroke
        // on the disc and leave the resolver to guess at all of them.
        let mut t = Table::default();
        let i = t.observe(&shape(4));
        for (label, n) in [("i", 424), ("I", 8)] {
            for _ in 0..n {
                t.vote(i, label);
            }
        }
        let (labelled, ambiguous, _) = t.settle_votes(Settling::from_a_reader());
        assert_eq!((labelled, ambiguous), (1, 0));
        assert_eq!(t.glyphs[0].text.as_deref(), Some("i"));
    }

    /// Votes the two sources are meant to read differently: a clear majority
    /// for `l`, with `I` behind it at the smallest share that still counts.
    fn mostly_l_with_some_i() -> Table {
        let mut t = Table::default();
        let i = t.observe(&shape(5));
        for (label, n) in [("l", 190), ("I", 10)] {
            for _ in 0..n {
                t.vote(i, label);
            }
        }
        t
    }

    #[test]
    fn a_trusted_reference_still_takes_a_clear_majority_as_the_label() {
        // The shipped table was built this way and is the best there is. A
        // reference is exact: 95% for l means l, and the ten readings of I are
        // ten places the reference itself says I.
        let mut t = mostly_l_with_some_i();
        let (labelled, ambiguous, _) = t.settle_votes(Settling::from_a_reference(0.9));
        assert_eq!((labelled, ambiguous), (1, 0));
        assert_eq!(t.glyphs[0].text.as_deref(), Some("l"));
    }

    #[test]
    fn a_reader_reads_the_same_votes_as_a_shape_both_letters_use() {
        // The same numbers, and the opposite answer, because a reader saying I
        // ten times about a shape it usually calls l is the face drawing them
        // alike rather than the reader slipping.
        let mut t = mostly_l_with_some_i();
        let (labelled, ambiguous, _) = t.settle_votes(Settling::from_a_reader());
        assert_eq!((labelled, ambiguous), (0, 1));
        assert_eq!(t.glyphs[0].text.as_deref(), Some("l|I"));
    }

    #[test]
    fn a_shape_that_two_characters_share_is_a_collision_not_a_label() {
        // I at 28 and i at 19 of 64 is not one letter read badly. It is two
        // letters the font draws alike, and context has to settle it.
        let mut t = Table::default();
        let i = t.observe(&shape(2));
        for (label, n) in [("I", 28), ("i", 19), ("f", 8), ("!", 4)] {
            for _ in 0..n {
                t.vote(i, label);
            }
        }
        let (labelled, ambiguous, _) = t.settle_votes(Settling::from_a_reader());
        assert_eq!((labelled, ambiguous), (0, 1));
        assert_eq!(t.glyphs[0].text.as_deref(), Some("I|i"));
    }
}
