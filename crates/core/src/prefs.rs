//! Settings that outlive a single disc.
//!
//! Kept in a plain JSON file rather than GSettings. GSettings needs a schema
//! compiled into a system directory, which means the application cannot be run
//! from a build directory without installing it first - a poor trade for a
//! dozen values.
//!
//! Everything here has a working default, and an unreadable or half-written
//! file falls back to those defaults rather than refusing to start. Preferences
//! are a convenience; losing them should cost a re-tick, not a launch.

use crate::lang::{self, LanguageSet};
use crate::model::{Container, Quality};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The name of the tool needed for Blu-ray, and for working around discs the
/// free reader cannot manage.
pub const MAKEMKV: &str = "makemkvcon";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    /// Languages worth keeping, most wanted first.
    ///
    /// Stored as codes so the file is stable regardless of how the name was
    /// spelled when it was chosen.
    pub preferred_languages: Vec<String>,
    /// Hand a disc to MakeMKV when the free reader cannot read it.
    ///
    /// Only meaningful when MakeMKV is actually installed; see
    /// [`Preferences::makemkv_available`].
    pub makemkv_fallback: bool,
    pub video: Quality,
    pub audio: Quality,
    pub container: Container,
    pub dual_audio: bool,
    pub keep_bitmap_subs: bool,
    pub drop_commentary: bool,
    pub output_dir: Option<PathBuf>,
    pub rip_dir: Option<PathBuf>,
    pub glyph_table: Option<PathBuf>,
    pub words_dir: Option<PathBuf>,
}

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            // English first is a deliberate default rather than an accident:
            // most discs carry it, and it gives the subtitle recogniser a
            // language it has the best wordlist for.
            preferred_languages: vec!["eng".into()],
            // On by default, but inert unless MakeMKV is installed. Defaulting
            // it off would mean a disc the free reader cannot read simply
            // fails, with the fix buried in a dialog.
            makemkv_fallback: true,
            video: Quality::Medium,
            audio: Quality::High,
            container: Container::Mp4,
            dual_audio: false,
            keep_bitmap_subs: false,
            drop_commentary: true,
            output_dir: None,
            rip_dir: None,
            glyph_table: None,
            words_dir: None,
        }
    }
}

impl Preferences {
    /// Is MakeMKV installed?
    pub fn makemkv_available() -> bool {
        crate::host::which(MAKEMKV).is_some()
    }

    /// Should we actually use it? Wanting it is not enough.
    pub fn use_makemkv(&self) -> bool {
        self.makemkv_fallback && Self::makemkv_available()
    }

    pub fn languages(&self) -> LanguageSet {
        LanguageSet(self.preferred_languages.iter().map(|c| lang::parse(c)).collect())
    }

    /// Which of `available` to tick, in preference order, then the rest.
    ///
    /// The disc decides what can be offered and the preferences decide what
    /// starts ticked; a language the user cares about that is not on the disc
    /// simply does not appear.
    pub fn preselect(&self, available: &[String]) -> Vec<(String, bool)> {
        let wanted = self.languages();
        let mut out: Vec<(String, bool)> = Vec::new();
        // preferred ones first, in preference order
        for want in &wanted.0 {
            for a in available {
                if want.matches(a) && !out.iter().any(|(x, _)| x == a) {
                    out.push((a.clone(), true));
                }
            }
        }
        for a in available {
            if !out.iter().any(|(x, _)| x == a) {
                out.push((a.clone(), false));
            }
        }
        out
    }

    /// Turn preferences plus a chosen set of languages into job settings.
    pub fn to_settings(&self, output_dir: PathBuf, languages: LanguageSet) -> crate::model::JobSettings {
        crate::model::JobSettings {
            output_dir,
            video: self.video,
            audio: self.audio,
            container: self.container,
            languages,
            dual_audio: self.dual_audio,
            keep_bitmap_subs: self.keep_bitmap_subs,
            drop_commentary: self.drop_commentary,
            words_dir: self.words_dir.clone(),
            glyph_table: self.glyph_table.clone(),
        }
    }

    /// `$XDG_CONFIG_HOME/riplika/preferences.json`.
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("riplika").join("preferences.json")
    }

    pub fn load_from(path: &Path) -> Preferences {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn load() -> Preferences {
        Self::load_from(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> crate::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| crate::Error(format!("{}: {e}", dir.display())))?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| crate::Error(format!("preferences: {e}")))?;
        // Write and rename, so an interrupted save cannot leave a truncated
        // file that then fails to parse and silently resets everything.
        let tmp = path.with_extension("json.part");
        std::fs::write(&tmp, json).map_err(|e| crate::Error(format!("{}: {e}", tmp.display())))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| crate::Error(format!("{}: {e}", path.display())))?;
        Ok(())
    }

    pub fn save(&self) -> crate::Result<()> {
        self.save_to(&Self::path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("riplika-prefs-{}-{name}", std::process::id()))
    }

    #[test]
    fn defaults_are_usable_without_a_file() {
        let p = Preferences::default();
        assert_eq!(p.preferred_languages, vec!["eng"]);
        assert!(p.makemkv_fallback);
        assert_eq!(p.video, Quality::Medium);
        assert!(p.drop_commentary);
    }

    #[test]
    fn preferences_round_trip_through_a_file() {
        let path = tmp("roundtrip.json");
        let p = Preferences {
            preferred_languages: vec!["swe".into(), "eng".into()],
            video: Quality::High,
            makemkv_fallback: false,
            ..Preferences::default()
        };
        p.save_to(&path).unwrap();
        assert_eq!(Preferences::load_from(&path), p);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_or_corrupt_file_falls_back_to_defaults() {
        // losing preferences should cost a re-tick, not a launch
        assert_eq!(Preferences::load_from(Path::new("/nonexistent/x.json")), Preferences::default());
        let path = tmp("corrupt.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        assert_eq!(Preferences::load_from(&path), Preferences::default());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_missing_newer_fields_still_loads() {
        // an older preferences file must not stop the application
        let path = tmp("partial.json");
        std::fs::write(&path, r#"{"preferred_languages":["swe"]}"#).unwrap();
        let p = Preferences::load_from(&path);
        assert_eq!(p.preferred_languages, vec!["swe"]);
        assert_eq!(p.video, Quality::Medium); // default filled in
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn saving_leaves_no_partial_file_behind() {
        let path = tmp("atomic.json");
        Preferences::default().save_to(&path).unwrap();
        assert!(!path.with_extension("json.part").exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wanting_makemkv_is_not_enough_to_use_it() {
        let mut p = Preferences { makemkv_fallback: false, ..Preferences::default() };
        assert!(!p.use_makemkv());
        // and with it wanted, the answer still depends on the machine
        p.makemkv_fallback = true;
        assert_eq!(p.use_makemkv(), Preferences::makemkv_available());
    }

    fn langs(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn preferred_languages_are_ticked_and_ordered_first() {
        let p = Preferences {
            preferred_languages: vec!["swe".into(), "eng".into()],
            ..Preferences::default()
        };
        // the disc offers these, in this order
        let got = p.preselect(&langs(&["eng", "spa", "swe"]));
        assert_eq!(
            got,
            vec![
                ("swe".to_string(), true),
                ("eng".to_string(), true),
                ("spa".to_string(), false),
            ]
        );
    }

    #[test]
    fn a_preferred_language_not_on_the_disc_simply_does_not_appear() {
        let p = Preferences {
            preferred_languages: vec!["jpn".into(), "eng".into()],
            ..Preferences::default()
        };
        let got = p.preselect(&langs(&["eng", "spa"]));
        assert_eq!(got, vec![("eng".to_string(), true), ("spa".to_string(), false)]);
    }

    #[test]
    fn everything_on_the_disc_is_offered_even_when_unwanted() {
        // the preference decides what starts ticked, not what can be chosen
        let p = Preferences::default();
        let got = p.preselect(&langs(&["eng", "spa", "fin", "isl"]));
        assert_eq!(got.len(), 4);
        assert_eq!(got.iter().filter(|(_, on)| *on).count(), 1);
    }

    #[test]
    fn the_iso_variants_a_disc_might_use_still_match_a_preference() {
        let p = Preferences {
            preferred_languages: vec!["german".into()],
            ..Preferences::default()
        };
        // the disc writes the other variant
        assert_eq!(p.preselect(&langs(&["ger"])), vec![("ger".to_string(), true)]);
    }

    #[test]
    fn settings_carry_the_preferences_through() {
        let p = Preferences {
            video: Quality::Low,
            keep_bitmap_subs: true,
            ..Preferences::default()
        };
        let s = p.to_settings(PathBuf::from("/media"), LanguageSet::parse("english"));
        assert_eq!(s.video, Quality::Low);
        assert!(s.keep_bitmap_subs);
        assert_eq!(s.output_dir, PathBuf::from("/media"));
    }

    #[test]
    fn the_config_path_follows_the_xdg_variable() {
        // set for this test only; the value is read at call time
        unsafe { std::env::set_var("XDG_CONFIG_HOME", "/tmp/xdg-test") };
        assert_eq!(Preferences::path(), PathBuf::from("/tmp/xdg-test/riplika/preferences.json"));
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }
}
