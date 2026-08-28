//! Resolving glyphs that are genuinely ambiguous in the source bitmap.
//!
//! Some characters are drawn identically by the subtitle font - in this DVD's
//! face, capital I and lowercase l are the same 3x21 bar. No amount of image
//! matching can separate them, because the distinction is not in the picture.
//! The only recoverable signal is context, so we resolve per word using a
//! wordlist plus a few structural rules.

use std::collections::HashSet;
use std::path::Path;

pub const DEFAULT_WORDLIST: &str = "/usr/share/dict/cracklib-small";

/// One output position: either settled, or a set of candidates in descending
/// order of how often each was seen.
#[derive(Debug, Clone)]
pub enum Slot {
    Fixed(String),
    Ambiguous(Vec<String>),
}

pub struct Resolver {
    words: HashSet<String>,
    /// Whether the English-specific rules apply.
    ///
    /// They are worth a lot for English and actively wrong elsewhere: the
    /// standalone pronoun "I" is capitalised in English, whereas the Swedish
    /// preposition "i" is not, so the same rule would capitalise every one.
    english: bool,
}

impl Resolver {
    pub fn load(path: Option<&Path>) -> Resolver {
        Resolver::load_lang(path, "en")
    }

    pub fn load_lang(path: Option<&Path>, lang: &str) -> Resolver {
        let english = lang.is_empty() || lang.to_lowercase().starts_with("en");
        // Fall back to the built-in list only for English. A mismatched wordlist
        // is worse than none: "vii" and "alia" are English words while "vil" and
        // "alla" are not, so scoring Icelandic against English turns "ég vil"
        // into "ég viI".
        let p = match (path, english) {
            (Some(p), _) => Some(p.to_path_buf()),
            (None, true) => Some(DEFAULT_WORDLIST.into()),
            (None, false) => None,
        };
        let mut words = HashSet::new();
        if let Some(s) = p.and_then(|p| std::fs::read_to_string(&p).ok()) {
            for line in s.lines() {
                let w = line.trim().to_lowercase();
                // keep accented forms - they matter for every language but English
                if w.chars().count() > 1 && w.chars().all(|c| c.is_alphabetic()) {
                    words.insert(w);
                }
            }
        }
        if english {
            // High-frequency words a password list may lack, and the forms that
            // matter most for the I/l pair.
            for w in [
                "i", "a", "is", "it", "if", "in", "ill", "island", "all", "will",
                "well", "tell", "look", "like", "little", "last", "left", "life",
                "line", "list", "live", "long", "love", "let", "less", "later",
            ] {
                words.insert(w.to_string());
            }
        }
        Resolver { words, english }
    }

    pub fn has_wordlist(&self) -> bool {
        !self.words.is_empty()
    }

    pub fn is_word(&self, w: &str) -> bool {
        self.words.contains(&w.to_lowercase())
    }

    /// Resolve one whitespace-delimited word.
    pub fn resolve_word(&self, slots: &[Slot]) -> String {
        self.resolve_word_at(slots, false)
    }

    /// `sentence_start` says this word opens a line or follows `.`, `!` or `?`.
    ///
    /// That is strong evidence for an ambiguous first letter: the word must be
    /// capitalised, and a capital L is a different glyph, so the bar can only be
    /// a capital I. Without it, and without a wordlist, Finnish "Iskekää" comes
    /// out as "lskekää".
    pub fn resolve_word_at(&self, slots: &[Slot], sentence_start: bool) -> String {
        let amb: Vec<usize> = slots
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s, Slot::Ambiguous(_)))
            .map(|(i, _)| i)
            .collect();

        if amb.is_empty() {
            return slots
                .iter()
                .map(|s| match s {
                    Slot::Fixed(t) => t.as_str(),
                    Slot::Ambiguous(v) => v[0].as_str(),
                })
                .collect();
        }

        // Enumerate combinations, but keep it bounded - long ambiguous runs are
        // better served by the structural fallback than by 2^n candidates.
        let combos: usize = amb
            .iter()
            .map(|&i| match &slots[i] {
                Slot::Ambiguous(v) => v.len(),
                _ => 1,
            })
            .product();
        if combos > 64 {
            return self.fallback(slots, sentence_start);
        }

        let mut best: Option<(i32, String)> = None;
        for n in 0..combos {
            let mut k = n;
            let mut out = String::new();
            // which characters are settled - only those may vote on letter case
            let mut fixed = Vec::new();
            for s in slots.iter() {
                match s {
                    Slot::Fixed(t) => {
                        out.push_str(t);
                        fixed.extend(std::iter::repeat(true).take(t.chars().count()));
                    }
                    Slot::Ambiguous(v) => {
                        let pick = k % v.len();
                        k /= v.len();
                        out.push_str(&v[pick]);
                        fixed.extend(std::iter::repeat(false).take(v[pick].chars().count()));
                    }
                }
            }
            let mut sc = self.score(&out, &fixed);
            if sentence_start {
                match out.chars().next() {
                    Some(c) if c.is_uppercase() => sc += 45,
                    Some(c) if c.is_lowercase() => sc -= 45,
                    _ => {}
                }
            }
            if best.as_ref().map_or(true, |(b, _)| sc > *b) {
                best = Some((sc, out));
            }
        }
        best.map(|(_, s)| s).unwrap_or_else(|| self.fallback(slots, sentence_start))
    }

    fn score(&self, cand: &str, fixed: &[bool]) -> i32 {
        // Score each alphabetic run separately. A cue like "That's...(LAUGHING)"
        // arrives as one token because nothing separates it by a space, and
        // judging it whole would hide both the dictionary hit and the fact that
        // the second half is an acronym-style all-caps word.
        let chars: Vec<char> = cand.chars().collect();
        let mut sc = 0;
        let mut i = 0;
        while i < chars.len() {
            if !(chars[i].is_alphabetic() || chars[i] == '\'') {
                i += 1;
                continue;
            }
            let start = i;
            while i < chars.len() && (chars[i].is_alphabetic() || chars[i] == '\'') {
                i += 1;
            }
            let run: String = chars[start..i].iter().collect();
            let core = run.trim_matches('\'');
            if core.is_empty() {
                continue;
            }
            if self.english && (core == "I" || matches!(core, "I'm" | "I'll" | "I've" | "I'd")) {
                sc += 140;
                continue;
            }
            let base = core.split('\'').next().unwrap_or("");
            if base.len() > 1 && self.is_word(base) {
                sc += 100;
            }
            // only a distinct form is worth extra - otherwise a plain word
            // scores twice and outweighs the case evidence
            if core.len() > 1 && core != base && self.is_word(core) {
                sc += 60;
            }
            // Decide "this word is shouted" from settled letters only. Letting
            // the candidate's own guesses vote turns "We'll" into "We'II".
            let mut fixed_alpha = 0;
            let mut fixed_upper = 0;
            for j in start..i {
                if chars[j].is_alphabetic() && fixed.get(j).copied().unwrap_or(true) {
                    fixed_alpha += 1;
                    if chars[j].is_uppercase() {
                        fixed_upper += 1;
                    }
                }
            }
            let all_caps = fixed_alpha >= 2 && fixed_upper == fixed_alpha;
            for (k, c) in chars[start..i].iter().enumerate() {
                if !c.is_alphabetic() {
                    continue;
                }
                if all_caps {
                    // Inside a shouted word or an acronym, case is the strongest
                    // signal there is - stronger than the wordlist, which would
                    // otherwise turn "IOW" into "lOW" on the strength of "low".
                    if c.is_uppercase() {
                        sc += 25;
                    } else {
                        sc -= 100;
                    }
                } else if c.is_uppercase() && k > 0 {
                    sc -= 35;
                }
            }
        }
        sc
    }

    /// Structural rules for when the wordlist offers no opinion.
    fn fallback(&self, slots: &[Slot], sentence_start: bool) -> String {
        let n_alpha = slots.len();
        let mut out = String::new();
        for (i, s) in slots.iter().enumerate() {
            match s {
                Slot::Fixed(t) => out.push_str(t),
                Slot::Ambiguous(v) => {
                    let upper: Option<&String> = v.iter().find(|x| {
                        x.chars().next().map_or(false, |c| c.is_uppercase())
                    });
                    let lower: Option<&String> = v.iter().find(|x| {
                        x.chars().next().map_or(false, |c| c.is_lowercase())
                    });
                    // In English a short word starting with the bar is almost
                    // always "I"/"It"/"If". Other languages have no such rule,
                    // so prefer the lowercase reading there.
                    let want_upper =
                        (i == 0 && sentence_start) || (self.english && i == 0 && n_alpha <= 2);
                    let pick = if want_upper { upper.or(lower) } else { lower.or(upper) };
                    out.push_str(pick.unwrap_or(&v[0]));
                }
            }
        }
        out
    }

    /// Recover a space that fell just under the threshold.
    ///
    /// A narrow glyph like `I` leaves little room before the next letter, so
    /// "I just" can come through as "Ijust". Only split where the geometry
    /// already showed a near-miss gap - splitting on the dictionary alone would
    /// happily turn "Pawnee" into "Paw nee".
    fn split_near_miss(&self, slots: &[Slot], gaps: &[(i32, i32)]) -> Option<(usize, ())> {
        let text: String = slots
            .iter()
            .map(|s| match s {
                Slot::Fixed(t) => t.clone(),
                Slot::Ambiguous(v) => v[0].clone(),
            })
            .collect();
        let core: String = text.chars().filter(|c| c.is_alphabetic()).collect();
        if core.len() < 4 || self.is_word(&core) {
            return None;
        }
        let mut best: Option<(i32, usize)> = None;
        for (i, &(gap, thr)) in gaps.iter().enumerate() {
            // within two pixels of being called a space
            if thr <= 0 || gap * 10 < (thr * 10 - 20) {
                continue;
            }
            let left: String = slots[..=i]
                .iter()
                .map(|s| match s {
                    Slot::Fixed(t) => t.clone(),
                    Slot::Ambiguous(v) => v[0].clone(),
                })
                .collect();
            let right: String = slots[i + 1..]
                .iter()
                .map(|s| match s {
                    Slot::Fixed(t) => t.clone(),
                    Slot::Ambiguous(v) => v[0].clone(),
                })
                .collect();
            // never split across an apostrophe: "you're" is one word, and both
            // halves happen to look like words on their own
            if left.ends_with('\'') || right.starts_with('\'') {
                continue;
            }
            let lc: String = left.chars().filter(|c| c.is_alphabetic()).collect();
            let rc: String = right.chars().filter(|c| c.is_alphabetic()).collect();
            let l_ok = (lc.len() == 1 && matches!(lc.as_str(), "I" | "l" | "a" | "A"))
                || (lc.len() >= 2 && self.is_word(&lc));
            let r_ok = rc.len() >= 2 && self.is_word(&rc);
            if l_ok && r_ok && best.map_or(true, |(g, _)| gap > g) {
                best = Some((gap, i));
            }
        }
        best.map(|(_, i)| (i, ()))
    }

    /// Resolve a whole line, preserving the spaces already decided by spacing.
    pub fn resolve_line(&self, words: &[(Vec<Slot>, Vec<(i32, i32)>)]) -> String {
        let mut out: Vec<String> = Vec::new();
        let mut at_start = true; // the first word on a line opens a sentence
        for (slots, gaps) in words {
            let word = match self.split_near_miss(slots, gaps) {
                Some((i, _)) => format!(
                    "{} {}",
                    self.resolve_word_at(&slots[..=i], at_start),
                    self.resolve_word_at(&slots[i + 1..], false)
                ),
                None => self.resolve_word_at(slots, at_start),
            };
            at_start = word
                .trim_end_matches(|c: char| c == '"' || c == '\'' || c == ')')
                .ends_with(['.', '!', '?']);
            out.push(word);
        }
        out.join(" ")
    }
}
