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
    Language {
        code: key.clone(),
        name: input.trim().into(),
        aliases: vec![key],
    }
}

/// Every language we know, for populating a GUI picker.
pub fn all() -> Vec<Language> {
    TABLE.iter().map(|(n, _, _)| parse(n)).collect()
}

/// The languages a user asked to keep, **in preference order**.
///
/// Order is meaningful, not cosmetic: the first language listed ends up first
/// in the output file and gets the default flag, so `swedish,english` really
/// does make a player open on Swedish.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LanguageSet(pub Vec<Language>);

impl LanguageSet {
    /// Parse a `english,swedish` style list. Empty means "keep everything".
    pub fn parse(spec: &str) -> Self {
        LanguageSet(
            spec.split([',', ';'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(parse)
                .collect(),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Pick which of `tags` to keep, returning indices in preference order.
    ///
    /// An empty set keeps everything as-is. A tag is used at most once, so two
    /// English tracks stay in their original relative order rather than the
    /// first one being chosen twice.
    pub fn select(&self, tags: &[String]) -> Vec<usize> {
        if self.is_empty() {
            return (0..tags.len()).collect();
        }
        let mut keep = Vec::new();
        for want in &self.0 {
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
    /// A file with no audio is broken, so an audio filter that matches nothing
    /// keeps everything. Missing subtitles are survivable and a wrong-language
    /// subtitle is worse than none, so that filter is honoured exactly.
    pub fn select_with_fallback(&self, tags: &[String], kind: crate::model::TrackKind) -> Vec<usize> {
        let keep = self.select(tags);
        if keep.is_empty() && kind == crate::model::TrackKind::Audio {
            return (0..tags.len()).collect();
        }
        keep
    }
}

impl fmt::Display for LanguageSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.0.iter().map(|l| l.name.as_str()).collect();
        f.write_str(&names.join(", "))
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
    fn empty_set_keeps_everything() {
        let set = LanguageSet::parse("");
        assert_eq!(set.select(&tags(&["eng", "swe"])), vec![0, 1]);
    }

    #[test]
    fn audio_falls_back_but_subtitles_do_not() {
        let set = LanguageSet::parse("japanese");
        let t = tags(&["eng", "swe"]);
        // no audio at all is a broken file, so keep what there is
        assert_eq!(set.select_with_fallback(&t, TrackKind::Audio), vec![0, 1]);
        // a subtitle in a language you cannot read is worse than none
        assert!(set.select_with_fallback(&t, TrackKind::Subtitle).is_empty());
    }

    #[test]
    fn separators_and_spacing_are_forgiving() {
        assert_eq!(
            LanguageSet::parse(" english ; swedish , ").0.len(),
            2
        );
    }
}
