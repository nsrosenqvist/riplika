//! Languages, and matching the many spellings of them that discs use.
//!
//! A DVD may tag the same language `ger` or `deu`, a user will type `german`,
//! and ffmpeg will print whichever the container holds. ISO 639-2 has two sets
//! of codes for twenty-odd languages - a bibliographic one from the English
//! name and a terminological one from the native name - and authoring tools
//! disagree about which to write, so a match has to accept both.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A language as we resolved it, with every code that could name it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Language {
    /// Canonical ISO 639-2/T code, the one we write into output files.
    pub code: String,
    /// English name, for display. Falls back to the code when unknown.
    pub name: String,
    aliases: Vec<String>,
}

impl Language {
    /// The two-letter code this language also goes by, if it has one.
    ///
    /// Spelling dictionaries are filed under it - `sv_SE.dic`, `en_GB.dic` -
    /// while every code written into an output file here is the three-letter
    /// one, so something has to bridge them.
    pub fn short(&self) -> Option<&str> {
        self.aliases.iter().find(|a| a.chars().count() == 2).map(String::as_str)
    }

    /// Does a stream tagged `tag` hold this language?
    pub fn matches(&self, tag: &str) -> bool {
        let t = tag.trim().trim_end_matches(',').to_ascii_lowercase();
        self.aliases.contains(&t)
    }
}

impl fmt::Display for Language {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

/// name, canonical code, then every alias including the code itself.
const TABLE: &[(&str, &str, &[&str])] = &[
    ("English", "eng", &["eng", "en"]),
    ("Swedish", "swe", &["swe", "sv"]),
    ("Norwegian", "nor", &["nor", "nb", "nn", "no"]),
    ("Danish", "dan", &["dan", "da"]),
    ("Finnish", "fin", &["fin", "fi"]),
    ("Icelandic", "isl", &["isl", "ice", "is"]),
    ("German", "deu", &["deu", "ger", "de"]),
    ("French", "fra", &["fra", "fre", "fr"]),
    ("Spanish", "spa", &["spa", "es"]),
    ("Portuguese", "por", &["por", "pt"]),
    ("Italian", "ita", &["ita", "it"]),
    ("Dutch", "nld", &["nld", "dut", "nl"]),
    ("Polish", "pol", &["pol", "pl"]),
    ("Czech", "ces", &["ces", "cze", "cs"]),
    ("Greek", "ell", &["ell", "gre", "el"]),
    ("Russian", "rus", &["rus", "ru"]),
    ("Turkish", "tur", &["tur", "tr"]),
    ("Arabic", "ara", &["ara", "ar"]),
    ("Hebrew", "heb", &["heb", "he"]),
    ("Hindi", "hin", &["hin", "hi"]),
    ("Japanese", "jpn", &["jpn", "ja"]),
    ("Korean", "kor", &["kor", "ko"]),
    ("Chinese", "zho", &["zho", "chi", "zh", "cmn", "yue"]),
    ("Undetermined", "und", &["und", ""]),
];

/// Resolve a name or code. Unknown input becomes a language that matches only
/// itself, so an obscure code still works as a filter.
pub fn parse(input: &str) -> Language {
    let key = input.trim().to_ascii_lowercase();
    for (name, code, aliases) in TABLE {
        if key == name.to_ascii_lowercase() || aliases.contains(&key.as_str()) {
            return Language {
                code: (*code).into(),
                name: (*name).into(),
                aliases: aliases.iter().map(|s| (*s).to_string()).collect(),
            };
        }
    }
    Language { code: key.clone(), name: input.trim().into(), aliases: vec![key] }
}

/// Every language we know, for populating a GUI picker.
pub fn all() -> Vec<Language> {
    TABLE.iter().map(|(n, _, _)| parse(n)).collect()
}

/// What the user asked to keep.
///
/// Two distinct things, and conflating them is a trap: "I did not say" and "I
/// said none" look the same as an empty list but must not behave the same. A
/// command line with no `--languages` has no preference; a window where every
/// language has been unticked has a very definite one, and answering it by
/// keeping everything is the opposite of what unticking looks like it does.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum LanguageSet {
    /// No preference: keep whatever the disc carries.
    #[default]
    Everything,
    /// Exactly these, in this order. Empty means none.
    Only(Vec<Language>),
}

impl LanguageSet {
    /// Parse a `english,swedish` style list. Nothing said means no preference.
    pub fn parse(spec: &str) -> Self {
        let wanted: Vec<Language> =
            spec.split([',', ';']).map(str::trim).filter(|s| !s.is_empty()).map(parse).collect();
        if wanted.is_empty() { LanguageSet::Everything } else { LanguageSet::Only(wanted) }
    }

    /// The languages named, if any were.
    pub fn wanted(&self) -> &[Language] {
        match self {
            LanguageSet::Everything => &[],
            LanguageSet::Only(v) => v,
        }
    }

    /// Was no preference expressed at all?
    pub fn is_everything(&self) -> bool {
        matches!(self, LanguageSet::Everything)
    }

    /// Pick which of `tags` to keep, returning indices in preference order.
    ///
    /// An empty set keeps everything as-is. A tag is used at most once, so two
    /// English tracks stay in their original relative order rather than the
    /// first one being chosen twice.
    pub fn select(&self, tags: &[String]) -> Vec<usize> {
        let LanguageSet::Only(wanted) = self else {
            return (0..tags.len()).collect();
        };
        let mut keep = Vec::new();
        for want in wanted {
            for (i, tag) in tags.iter().enumerate() {
                if !keep.contains(&i) && want.matches(tag) {
                    keep.push(i);
                }
            }
        }
        keep
    }

    /// Which tracks to keep, with the fallbacks each stream type needs.
    ///
    /// Subtitles are honoured exactly, including down to none: a subtitle in a
    /// language you cannot read is worse than no subtitle.
    ///
    /// Audio cannot be, because a video with no sound is a broken rip rather
    /// than a spare one. When nothing matches, the disc's first track is kept -
    /// one track, not all of them, so asking for Japanese on a disc that has
    /// none does not silently hand back Spanish as well as English.
    pub fn select_with_fallback(
        &self,
        tags: &[String],
        kind: crate::model::TrackKind,
    ) -> Vec<usize> {
        let keep = self.select(tags);
        if keep.is_empty() && kind == crate::model::TrackKind::Audio && !tags.is_empty() {
            return vec![0];
        }
        keep
    }
}

impl fmt::Display for LanguageSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LanguageSet::Everything => f.write_str("every language"),
            LanguageSet::Only(v) if v.is_empty() => f.write_str("none"),
            LanguageSet::Only(v) => {
                let names: Vec<&str> = v.iter().map(|l| l.name.as_str()).collect();
                f.write_str(&names.join(", "))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TrackKind;

    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn both_iso_variants_match() {
        // a disc may write either; we cannot control which
        for t in ["ger", "deu", "de", "German", "GERMAN"] {
            assert_eq!(parse(t).code, "deu", "{t}");
        }
        for t in ["ice", "isl", "is"] {
            assert_eq!(parse(t).code, "isl", "{t}");
        }
        assert!(parse("french").matches("fre"));
        assert!(parse("fra").matches("fre"));
    }

    #[test]
    fn ffprobe_trailing_comma_still_matches() {
        // `-show_entries stream_tags=language -of csv=p=0` emits "eng,"
        assert!(parse("english").matches("eng,"));
        assert!(parse("english").matches(" eng "));
    }

    #[test]
    fn unknown_code_matches_only_itself() {
        let l = parse("xyz");
        assert!(l.matches("xyz"));
        assert!(!l.matches("eng"));
    }

    #[test]
    fn selection_follows_request_order_not_file_order() {
        let set = LanguageSet::parse("swedish,english");
        // file has English first; asking for Swedish first must reorder
        assert_eq!(set.select(&tags(&["eng", "swe", "fin"])), vec![1, 0]);
    }

    #[test]
    fn duplicate_language_tracks_keep_relative_order() {
        let set = LanguageSet::parse("english");
        assert_eq!(set.select(&tags(&["eng", "spa", "eng"])), vec![0, 2]);
    }

    #[test]
    fn saying_nothing_keeps_everything() {
        // a command line with no --languages has no preference
        let set = LanguageSet::parse("");
        assert!(set.is_everything());
        assert_eq!(set.select(&tags(&["eng", "swe"])), vec![0, 1]);
    }

    #[test]
    fn saying_none_is_not_the_same_as_saying_nothing() {
        // The trap this replaces: both were an empty list, so unticking every
        // language in the window kept every language - the opposite of what it
        // looks like it does.
        let none = LanguageSet::Only(Vec::new());
        assert!(!none.is_everything());
        assert!(none.select(&tags(&["eng", "swe"])).is_empty());
    }

    #[test]
    fn choosing_no_language_leaves_no_subtitles_but_is_not_silent() {
        // a video with no sound is a broken rip rather than a spare one
        let none = LanguageSet::Only(Vec::new());
        let t = tags(&["eng", "swe"]);
        assert!(none.select_with_fallback(&t, TrackKind::Subtitle).is_empty());
        assert_eq!(none.select_with_fallback(&t, TrackKind::Audio), vec![0]);
    }

    #[test]
    fn audio_falls_back_but_subtitles_do_not() {
        let set = LanguageSet::parse("japanese");
        let t = tags(&["eng", "swe"]);
        // one track, not all of them: asking for Japanese on a disc that has
        // none should not quietly hand back Spanish as well as English
        assert_eq!(set.select_with_fallback(&t, TrackKind::Audio), vec![0]);
        // a subtitle in a language you cannot read is worse than none
        assert!(set.select_with_fallback(&t, TrackKind::Subtitle).is_empty());
    }

    #[test]
    fn separators_and_spacing_are_forgiving() {
        assert_eq!(LanguageSet::parse(" english ; swedish , ").wanted().len(), 2);
    }
}
