//! Resolving glyphs that are genuinely ambiguous in the source bitmap.
//!
//! Some characters are drawn identically by the subtitle font - in this DVD's
//! face, capital I and lowercase l are the same 3x21 bar. No amount of image
//! matching can separate them, because the distinction is not in the picture.
//! The only recoverable signal is context, so we resolve per word using a
//! wordlist plus a few structural rules.

use super::recognize::Word;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub const DEFAULT_WORDLIST: &str = "/usr/share/dict/cracklib-small";

/// Where a desktop keeps its spelling dictionaries.
///
/// Worth looking in rather than shipping our own: the GNOME runtime this is
/// packaged against carries them for a hundred languages already, so a Flatpak
/// that bundled wordlists would be carrying a second copy of something in the
/// sandbox with it - and every one it did not think to bundle would be a
/// language whose ambiguous glyphs fall back to structural rules.
const DICTIONARIES: [&str; 2] = ["/usr/share/hunspell", "/usr/share/myspell"];

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

    /// Every wordlist worth reading for this language.
    ///
    /// A file if one was named, else the one for this language in the folder
    /// that was named, else whatever the desktop has. All the variants of a
    /// language together, because the only question ever asked of these is
    /// whether something is a word: `colour` and `color` are both answers we
    /// want yes to, and a dialect that lacks one is not evidence against it.
    pub fn wordlists(path: Option<&Path>, code: &str) -> Vec<PathBuf> {
        if let Some(p) = path {
            if p.is_dir() {
                let named = p.join(format!("{code}.txt"));
                if named.exists() {
                    return vec![named];
                }
            } else if p.exists() {
                return vec![p.to_path_buf()];
            }
        }
        Self::installed_for(&DICTIONARIES, code)
    }

    /// The desktop's own dictionaries for this language, if it has any.
    ///
    /// Matched on the two-letter code, so `sv` finds sv.dic, sv_SE and sv_FI
    /// and nothing else - a mismatched wordlist is worse than none, and it is
    /// the language that must match, not the region.
    pub fn installed_for(dirs: &[&str], code: &str) -> Vec<PathBuf> {
        let language = crate::lang::parse(code);
        let Some(short) = language.short().map(str::to_ascii_lowercase) else {
            return Vec::new();
        };
        let mut found: Vec<PathBuf> = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().is_none_or(|x| x != "dic") {
                    continue;
                }
                let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                let base = stem.split('_').next().unwrap_or(stem).to_ascii_lowercase();
                if base == short {
                    found.push(p);
                }
            }
            if !found.is_empty() {
                // One directory's worth. The two are usually the same files.
                break;
            }
        }
        found.sort();
        found
    }

    /// The wordlist for this language, given either the file or the folder
    /// they live in.
    ///
    /// Both are handed around: preferences hold the folder, and `--words`
    /// takes a file. Passing the folder where a file was wanted read as "no
    /// wordlist", and the note that followed asked the reader to pass the very
    /// thing they had passed - while every ambiguous glyph on the disc fell
    /// back to structural rules in silence.
    pub fn wordlist(path: Option<&Path>, code: &str) -> Option<PathBuf> {
        Self::wordlists(path, code).into_iter().next()
    }

    pub fn load_lang(path: Option<&Path>, lang: &str) -> Resolver {
        let english = lang.is_empty() || lang.to_lowercase().starts_with("en");
        // Fall back to the built-in list only for English. A mismatched wordlist
        // is worse than none: "vii" and "alia" are English words while "vil" and
        // "alla" are not, so scoring Icelandic against English turns "ég vil"
        // into "ég viI".
        let code = crate::lang::parse(lang).code;
        let mut files = Self::wordlists(path, &code);
        if files.is_empty() && english {
            files.push(DEFAULT_WORDLIST.into());
        }
        Resolver::load_from(&files, english)
    }

    /// Read a set of wordlists into one.
    pub fn load_from(files: &[PathBuf], english: bool) -> Resolver {
        let mut words = HashSet::new();
        for file in files {
            let Ok(s) = std::fs::read_to_string(file) else {
                continue;
            };
            for line in s.lines() {
                // A hunspell .dic writes "word/FLAGS" and opens with a count.
                // Taking the part before the slash reads both that and a plain
                // list, and the count falls out for not being letters.
                let w = line.split('/').next().unwrap_or("").trim().to_lowercase();
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
                "i", "a", "is", "it", "if", "in", "ill", "island", "all", "will", "well", "tell",
                "look", "like", "little", "last", "left", "life", "line", "list", "live", "long",
                "love", "let", "less", "later",
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
                        fixed.extend(std::iter::repeat_n(true, t.chars().count()));
                    }
                    Slot::Ambiguous(v) => {
                        let pick = k % v.len();
                        k /= v.len();
                        out.push_str(&v[pick]);
                        fixed.extend(std::iter::repeat_n(false, v[pick].chars().count()));
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
            if best.as_ref().is_none_or(|(b, _)| sc > *b) {
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
            for (j, ch) in chars.iter().enumerate().take(i).skip(start) {
                if ch.is_alphabetic() && fixed.get(j).copied().unwrap_or(true) {
                    fixed_alpha += 1;
                    if ch.is_uppercase() {
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
                    let upper: Option<&String> =
                        v.iter().find(|x| x.chars().next().is_some_and(|c| c.is_uppercase()));
                    let lower: Option<&String> =
                        v.iter().find(|x| x.chars().next().is_some_and(|c| c.is_lowercase()));
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
            if l_ok && r_ok && best.is_none_or(|(g, _)| gap > g) {
                best = Some((gap, i));
            }
        }
        best.map(|(_, i)| (i, ()))
    }

    /// Resolve a whole line, preserving the spaces already decided by spacing.
    pub fn resolve_line(&self, words: &[Word]) -> String {
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
            at_start = word.trim_end_matches(['"', '\'', ')']).ends_with(['.', '!', '?']);
            out.push(word);
        }
        out.join(" ")
    }
}

#[cfg(test)]
mod wordlist_tests {
    use super::*;

    #[test]
    fn a_folder_of_wordlists_finds_the_one_for_this_language() {
        // Preferences hold the folder and `--words` takes a file, so both
        // arrive here. Reading the folder as a file meant no wordlist at all,
        // in silence, and every ambiguous glyph on the disc fell back to
        // structural rules - "Inte" came out as "lnte" with a 119,591-word
        // Swedish list sitting unread beside it.
        let dir = crate::subs::source::temp_dir("words").expect("a temp dir");
        std::fs::write(dir.0.join("swe.txt"), "inte\nchans\n").unwrap();

        let from_folder = Resolver::wordlist(Some(&dir.0), "swe").expect("found in the folder");
        assert!(from_folder.ends_with("swe.txt"));
        let named = Resolver::wordlist(Some(&from_folder), "swe").expect("named directly");
        assert_eq!(named, from_folder);

        assert!(Resolver::load_lang(Some(&from_folder), "swe").is_word("inte"));
    }

    #[test]
    fn a_language_with_no_list_of_its_own_gets_nothing_rather_than_the_wrong_one() {
        // A mismatched wordlist is worse than none: "vil" and "alla" are not
        // English words, so scoring Icelandic against English turns "ég vil"
        // into "ég viI".
        //
        // Asked of a folder this test wrote, not of the machine's own: what a
        // developer happens to have installed must not decide whether a test
        // passes.
        let dir = crate::subs::source::temp_dir("hunspell").expect("a temp dir");
        std::fs::write(dir.0.join("en_GB.dic"), "1\nthe\n").unwrap();
        let d = dir.0.to_string_lossy().into_owned();
        assert!(Resolver::installed_for(&[&d], "isl").is_empty());
        assert!(!Resolver::installed_for(&[&d], "eng").is_empty());
    }

    #[test]
    fn the_desktops_own_dictionaries_stand_in_when_nothing_was_installed() {
        // The Flatpak ships no wordlists, and the runtime it is built against
        // carries them for a hundred languages. Without this every ambiguous
        // glyph on every disc fell back to structural rules on a fresh
        // install, including English.
        let dir = crate::subs::source::temp_dir("hunspell").expect("a temp dir");
        for name in ["sv.dic", "sv_SE.dic", "sv_FI.dic", "en_GB.dic", "svenska.dic"] {
            std::fs::write(dir.0.join(name), "1\ninte/AB\n").unwrap();
        }
        let d = dir.0.to_string_lossy().into_owned();
        let found = Resolver::installed_for(&[&d], "swe");
        let names: Vec<String> =
            found.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        // every variant of Swedish, and nothing that merely starts with sv
        assert_eq!(names, ["sv.dic", "sv_FI.dic", "sv_SE.dic"]);
    }

    #[test]
    fn a_dictionary_is_read_past_its_affix_flags_and_its_count() {
        // A hunspell .dic opens with the number of entries and writes each as
        // "word/FLAGS". Read as a plain list, the flags make every line a
        // non-word and the dictionary comes out empty.
        let dir = crate::subs::source::temp_dir("hunspell").expect("a temp dir");
        std::fs::write(dir.0.join("sv.dic"), "153714\ninte/AB\nchans\n-/-06XY\n").unwrap();
        let d = dir.0.to_string_lossy().into_owned();
        let r = Resolver::load_from(&Resolver::installed_for(&[&d], "swe"), false);
        assert!(r.is_word("inte"), "the flags were taken as part of the word");
        assert!(r.is_word("chans"));
        assert!(!r.is_word("153714"), "the count is not a word");
    }

    #[test]
    fn a_named_wordlist_still_wins_over_the_desktops() {
        // Somebody who built a list for this disc means it to be used.
        let dir = crate::subs::source::temp_dir("words").expect("a temp dir");
        std::fs::write(dir.0.join("swe.txt"), "inte\n").unwrap();
        let got = Resolver::wordlists(Some(&dir.0), "swe");
        assert_eq!(got.len(), 1);
        assert!(got[0].ends_with("swe.txt"));
    }

    #[test]
    fn nothing_in_means_nothing_out() {
        assert_eq!(Resolver::wordlist(None, "eng"), None);
        assert_eq!(Resolver::wordlist(Some(Path::new("/nope/eng.txt")), "eng"), None);
    }
}
