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

use crate::disc::DiscKind;
use crate::lang::{self, LanguageSet};
use crate::model::{Container, Quality};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Resolve one of the XDG base directories, ours appended.
///
/// Pure, taking what the environment said rather than reading it, because these
/// are process-global and a test that sets one races every other test.
pub fn xdg_dir(explicit: Option<PathBuf>, home: Option<PathBuf>, fallback: &str) -> PathBuf {
    explicit
        .or_else(|| home.map(|h| h.join(fallback)))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("riplika")
}

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
    pub include_extended_cuts: bool,
    /// Read each title twice for exact chapter marks. Off: it doubles the read.
    #[serde(default)]
    pub accurate_chapters: bool,
    pub include_extras: bool,
    pub drop_commentary: bool,
    /// Where finished video goes. Historically the only one, hence the name.
    pub output_dir: Option<PathBuf>,
    /// Where finished music goes.
    #[serde(default)]
    pub music_dir: Option<PathBuf>,
    /// Where game images go.
    #[serde(default)]
    pub games_dir: Option<PathBuf>,
    pub rip_dir: Option<PathBuf>,
    pub glyph_table: Option<PathBuf>,
    pub words_dir: Option<PathBuf>,
    /// How episode filenames are built.
    pub episode_template: String,
    /// What a ripped CD is written as.
    #[serde(default)]
    pub music_format: AudioFormat,
    /// The drive's audio read offset, in samples.
    ///
    /// A property of the drive model, not of any disc: every drive returns
    /// audio displaced by a fixed amount, and a rip that does not correct for
    /// it plays perfectly and matches nothing. Zero until somebody says
    /// otherwise, because a wrong correction is worse than none.
    #[serde(default)]
    pub read_offset: i32,
    /// Where Redump datfiles are kept, for naming and verifying game dumps.
    #[serde(default)]
    pub dat_dir: Option<PathBuf>,
    /// How track filenames are built.
    #[serde(default = "music_template_default")]
    pub music_template: String,
    /// How hard to work on a ripped CD.
    ///
    /// Only means anything for a lossy format; see
    /// [`AudioFormat::quality_applies`].
    #[serde(default = "music_quality_default")]
    pub music_quality: Quality,
}

fn music_quality_default() -> Quality {
    Quality::High
}

fn music_template_default() -> String {
    crate::naming::DEFAULT_TRACK_TEMPLATE.to_string()
}

/// What a ripped CD is written as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AudioFormat {
    /// Lossless, and about half the size of the raw disc. The default because
    /// it is the only one a later re-encode can start from without having
    /// already thrown something away.
    #[default]
    Flac,
    /// Lossy, and plays on anything.
    Mp3,
}

impl AudioFormat {
    pub fn extension(self) -> &'static str {
        match self {
            AudioFormat::Flac => "flac",
            AudioFormat::Mp3 => "mp3",
        }
    }

    /// What ffmpeg calls the encoder.
    pub fn encoder(self) -> &'static str {
        match self {
            AudioFormat::Flac => "flac",
            AudioFormat::Mp3 => "libmp3lame",
        }
    }

    /// Does a quality setting change anything here?
    ///
    /// For FLAC it does not. Every compression level decodes to bit-identical
    /// audio; only the file size and the encode time move, by a few percent.
    /// So the setting is not merely ignored - the control for it is switched
    /// off, because a control that visibly changes nothing is worse than no
    /// control at all.
    pub fn quality_applies(self) -> bool {
        matches!(self, AudioFormat::Mp3)
    }
}

/// FLAC's compression level.
///
/// Chosen once rather than exposed: it is lossless at every level, and 8 is
/// the usual archival choice - a few percent smaller than the default, and no
/// slower to decode.
pub const FLAC_COMPRESSION: u32 = 8;

impl Default for Preferences {
    fn default() -> Self {
        Preferences {
            // English first is a deliberate default rather than an accident:
            // most discs carry it, and it gives the subtitle recogniser a
            // language it has the best wordlist for.
            preferred_languages: vec!["eng".into()],
            // Off: it doubles how long a disc takes to read, and buys chapter
            // marks that are already within two seconds without it.
            accurate_chapters: false,
            // On by default, but inert unless MakeMKV is installed. Defaulting
            // it off would mean a disc the free reader cannot read simply
            // fails, with the fix buried in a dialog.
            makemkv_fallback: true,
            video: Quality::Medium,
            audio: Quality::High,
            container: Container::Mp4,
            dual_audio: false,
            keep_bitmap_subs: false,
            include_extended_cuts: true,
            include_extras: true,
            drop_commentary: true,
            output_dir: None,
            music_dir: None,
            games_dir: None,
            rip_dir: None,
            glyph_table: None,
            words_dir: None,
            episode_template: crate::naming::DEFAULT_EPISODE_TEMPLATE.to_string(),
            // Lossless by default: a CD rip is the only copy of that disc most
            // people will ever make, and a lossy one cannot be improved later.
            music_format: AudioFormat::Flac,
            music_quality: music_quality_default(),
            music_template: music_template_default(),
            dat_dir: None,
            read_offset: 0,
        }
    }
}

/// Where finished files go for a disc of this kind, when nothing is configured.
///
/// A music library and a video library are not the same folder and nothing
/// downstream treats them alike - a media server scans them separately, and an
/// album filed under Videos is an album it will try to read as a film. Games
/// are not a library at all; they are images waiting to be checked against a
/// datfile.
///
/// Here rather than in each front end because there are three of them and
/// three copies of "music goes in Music" is three chances to disagree - which
/// is what had happened: the command line knew, and the window put a CD in
/// Videos.
pub fn default_output_dir(library: Library) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join(match library {
        Library::Music => "Music",
        Library::Games => "Games",
        Library::Video => "Videos",
    })
}

/// Which of the three collections a disc's output belongs to.
///
/// Named rather than taken from `DiscKind` at every call site, because the
/// command line knows it is ripping a CD without having a `DiscKind` in its
/// hand, and asking the drive again to find out what it already knows would
/// be a read for nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Library {
    Video,
    Music,
    Games,
}

impl Library {
    /// Where a disc of this kind goes. Video for anything not yet looked at:
    /// it is the common case and the one the rest of the window assumes.
    pub fn of(kind: Option<&DiscKind>) -> Library {
        match kind {
            Some(DiscKind::Audio(_)) => Library::Music,
            Some(DiscKind::Data(_)) => Library::Games,
            _ => Library::Video,
        }
    }
}

impl Preferences {
    /// Where a disc of this kind should be written.
    ///
    /// A configured folder is a choice and is honoured whatever is in the
    /// drive; without one, the kind decides.
    pub fn output_for(&self, library: Library) -> PathBuf {
        self.configured(library).unwrap_or_else(|| default_output_dir(library))
    }

    /// The folder chosen for this library, if one was.
    ///
    /// One setting cannot serve three libraries. It used to, and choosing a
    /// folder while a CD was in the drive therefore sent every later game
    /// image to the music folder as well - which is not a preference anybody
    /// expressed.
    pub fn configured(&self, library: Library) -> Option<PathBuf> {
        match library {
            Library::Video => self.output_dir.clone(),
            Library::Music => self.music_dir.clone(),
            Library::Games => self.games_dir.clone(),
        }
    }

    /// Remember a folder for one library, leaving the others alone.
    pub fn set_output_for(&mut self, library: Library, dir: PathBuf) {
        match library {
            Library::Video => self.output_dir = Some(dir),
            Library::Music => self.music_dir = Some(dir),
            Library::Games => self.games_dir = Some(dir),
        }
    }

    /// Is MakeMKV installed?
    pub fn makemkv_available() -> bool {
        crate::host::which(MAKEMKV).is_some()
    }

    /// Should we actually use it? Wanting it is not enough.
    pub fn use_makemkv(&self) -> bool {
        self.makemkv_fallback && Self::makemkv_available()
    }

    pub fn languages(&self) -> LanguageSet {
        LanguageSet::Only(self.preferred_languages.iter().map(|c| lang::parse(c)).collect())
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
        for want in wanted.wanted() {
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
    pub fn to_settings(
        &self,
        output_dir: PathBuf,
        languages: LanguageSet,
    ) -> crate::model::JobSettings {
        crate::model::JobSettings {
            output_dir,
            video: self.video,
            audio: self.audio,
            container: self.container,
            music_format: self.music_format,
            music_quality: self.music_quality,
            music_template: Some(self.music_template.clone()).filter(|t| !t.trim().is_empty()),
            accurate_chapters: self.accurate_chapters,
            languages,
            dual_audio: self.dual_audio,
            keep_bitmap_subs: self.keep_bitmap_subs,
            include_extended_cuts: self.include_extended_cuts,
            include_extras: self.include_extras,
            drop_commentary: self.drop_commentary,
            words_dir: self.words_dir(),
            glyph_table: self.glyph_table(),
            episode_template: Some(self.episode_template.clone()).filter(|t| !t.trim().is_empty()),
        }
    }

    /// `$XDG_DATA_HOME/riplika` - the glyph table and the wordlists.
    ///
    /// Application data, not settings: built once by `riplika build`, then used
    /// without being thought about. There is no reason for anyone to point at
    /// them by hand, so they are not offered as a choice.
    pub fn data_dir() -> PathBuf {
        xdg_dir(
            std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
            ".local/share",
        )
    }

    /// `$XDG_CACHE_HOME/riplika` - where a rip lands before it is encoded.
    ///
    /// Cache, because it can be thrown away and made again from the disc. Not
    /// the system temporary directory: that is a tmpfs on most desktops, held
    /// in RAM, and a disc is eight gigabytes.
    pub fn cache_dir() -> PathBuf {
        xdg_dir(
            std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
            ".cache",
        )
    }

    /// Where subtitle recognition looks, unless told otherwise.
    pub fn default_glyph_table() -> PathBuf {
        Self::data_dir().join("glyphs.json")
    }

    pub fn default_words_dir() -> PathBuf {
        Self::data_dir().join("words")
    }

    /// Where datfiles go when nobody has said otherwise.
    pub fn default_dat_dir() -> PathBuf {
        Self::data_dir().join("dats")
    }

    pub fn default_rip_dir() -> PathBuf {
        Self::cache_dir().join("rip")
    }

    /// The glyph table to use: what was configured, or the standard place.
    pub fn glyph_table(&self) -> Option<PathBuf> {
        self.glyph_table
            .clone()
            .or_else(|| Some(Self::default_glyph_table()).filter(|p| p.exists()))
    }

    pub fn words_dir(&self) -> Option<PathBuf> {
        self.words_dir.clone().or_else(|| Some(Self::default_words_dir()).filter(|p| p.is_dir()))
    }

    pub fn dat_dir(&self) -> Option<PathBuf> {
        self.dat_dir.clone().or_else(|| Some(Self::default_dat_dir()).filter(|p| p.is_dir()))
    }

    pub fn rip_dir(&self) -> PathBuf {
        self.rip_dir.clone().unwrap_or_else(Self::default_rip_dir)
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
        assert_eq!(
            Preferences::load_from(Path::new("/nonexistent/x.json")),
            Preferences::default()
        );
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
            vec![("swe".to_string(), true), ("eng".to_string(), true), ("spa".to_string(), false),]
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
        let p =
            Preferences { preferred_languages: vec!["german".into()], ..Preferences::default() };
        // the disc writes the other variant
        assert_eq!(p.preselect(&langs(&["ger"])), vec![("ger".to_string(), true)]);
    }

    #[test]
    fn settings_carry_the_preferences_through() {
        let p =
            Preferences { video: Quality::Low, keep_bitmap_subs: true, ..Preferences::default() };
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

#[cfg(test)]
mod xdg_tests {
    use super::*;

    #[test]
    fn the_xdg_variable_is_used_when_it_is_set() {
        assert_eq!(
            xdg_dir(Some(PathBuf::from("/x/data")), None, ".local/share"),
            PathBuf::from("/x/data/riplika")
        );
    }

    #[test]
    fn without_it_the_standard_place_under_home_is_used() {
        assert_eq!(
            xdg_dir(None, Some(PathBuf::from("/home/someone")), ".local/share"),
            PathBuf::from("/home/someone/.local/share/riplika")
        );
        assert_eq!(
            xdg_dir(None, Some(PathBuf::from("/home/someone")), ".cache"),
            PathBuf::from("/home/someone/.cache/riplika")
        );
    }

    #[test]
    fn a_rip_does_not_land_in_the_system_temporary_directory() {
        // On most desktops that is a tmpfs held in RAM, and a disc is eight
        // gigabytes. Filling it takes the whole session down with it.
        let rip = Preferences::default().rip_dir();
        assert!(!rip.starts_with("/tmp/"), "{}", rip.display());
        assert!(rip.to_string_lossy().contains("riplika"));
    }

    #[test]
    fn the_three_directories_are_distinct() {
        // settings, data and scratch have different lifetimes: one is backed
        // up, one is rebuilt, one is thrown away
        let config = Preferences::path();
        let data = Preferences::data_dir();
        let cache = Preferences::cache_dir();
        assert_ne!(data, cache);
        assert!(!config.starts_with(&cache));
        assert!(!data.starts_with(&cache));
    }

    #[test]
    fn an_explicit_choice_still_wins() {
        // someone whose home is small should be able to rip elsewhere
        let p = Preferences {
            rip_dir: Some(PathBuf::from("/mnt/big/rip")),
            glyph_table: Some(PathBuf::from("/somewhere/mine.json")),
            ..Preferences::default()
        };
        assert_eq!(p.rip_dir(), PathBuf::from("/mnt/big/rip"));
        assert_eq!(p.glyph_table(), Some(PathBuf::from("/somewhere/mine.json")));
    }

    #[test]
    fn an_absent_glyph_table_is_absent_rather_than_a_path_to_nothing() {
        // the pipeline warns and keeps the bitmaps; it must not be handed a
        // path that does not exist and told to read it
        let p = Preferences { glyph_table: None, words_dir: None, ..Preferences::default() };
        // whichever way this machine is set up, a missing file is None
        if !Preferences::default_glyph_table().exists() {
            assert_eq!(p.glyph_table(), None);
        }
        if !Preferences::default_words_dir().is_dir() {
            assert_eq!(p.words_dir(), None);
        }
    }
}

#[cfg(test)]
mod music_format_tests {
    use super::*;

    #[test]
    fn a_quality_setting_only_means_something_for_a_lossy_format() {
        // FLAC decodes to bit-identical audio at every compression level, so
        // there is nothing for a tier to decide.
        assert!(!AudioFormat::Flac.quality_applies());
        assert!(AudioFormat::Mp3.quality_applies());
    }

    #[test]
    fn each_format_names_its_own_encoder_and_extension() {
        assert_eq!(AudioFormat::Flac.extension(), "flac");
        assert_eq!(AudioFormat::Flac.encoder(), "flac");
        assert_eq!(AudioFormat::Mp3.extension(), "mp3");
        assert_eq!(AudioFormat::Mp3.encoder(), "libmp3lame");
    }

    #[test]
    fn the_tiers_run_from_best_to_worst_without_repeating_themselves() {
        let levels: Vec<u32> =
            [Quality::High, Quality::Medium, Quality::Low].iter().map(|q| q.lame_vbr()).collect();
        assert_eq!(levels, vec![0, 2, 5], "V-numbers count upwards as quality falls");
    }

    #[test]
    fn lossless_is_the_default_because_a_lossy_rip_cannot_be_improved_later() {
        assert_eq!(Preferences::default().music_format, AudioFormat::Flac);
    }

    #[test]
    fn settings_saved_before_music_existed_still_load() {
        // The fields were added after people already had a settings file, and
        // one missing them must not be read as a corrupt one.
        let mut v: serde_json::Value =
            serde_json::to_value(Preferences::default()).expect("preferences serialise");
        let map = v.as_object_mut().expect("preferences are an object");
        map.remove("music_format");
        map.remove("music_quality");
        let back: Preferences = serde_json::from_value(v).expect("an older file still loads");
        assert_eq!(back.music_format, AudioFormat::Flac);
        assert_eq!(back.music_quality, Quality::High);
    }

    #[test]
    fn each_kind_of_disc_has_a_library_of_its_own() {
        // A CD went to Videos, where a media server reads it as a film.
        let toc = crate::disc::Toc { tracks: Vec::new(), leadout: 0 };
        assert_eq!(Library::of(Some(&DiscKind::Audio(toc.clone()))), Library::Music);
        assert_eq!(Library::of(Some(&DiscKind::Data(None))), Library::Games);
        assert_eq!(Library::of(Some(&DiscKind::DvdVideo)), Library::Video);
        assert_eq!(Library::of(Some(&DiscKind::BluRay)), Library::Video);
    }

    #[test]
    fn a_disc_nobody_has_looked_at_yet_is_assumed_to_be_video() {
        // The common case, and what the rest of the window is laid out for.
        assert_eq!(Library::of(None), Library::Video);
        assert_eq!(Library::of(Some(&DiscKind::Empty)), Library::Video);
    }

    #[test]
    fn the_defaults_are_three_different_folders() {
        let dirs = [Library::Video, Library::Music, Library::Games].map(default_output_dir);
        assert!(dirs[0].ends_with("Videos"), "{:?}", dirs[0]);
        assert!(dirs[1].ends_with("Music"), "{:?}", dirs[1]);
        assert!(dirs[2].ends_with("Games"), "{:?}", dirs[2]);
    }

    #[test]
    fn a_folder_chosen_for_one_library_does_not_move_the_others() {
        // Choosing a folder while a CD was in the drive used to set the one
        // folder there was, so every later game image went to the music
        // library too - a preference nobody expressed.
        let mut p = Preferences::default();
        p.set_output_for(Library::Music, PathBuf::from("/media/albums"));
        assert_eq!(p.output_for(Library::Music), PathBuf::from("/media/albums"));
        assert!(p.output_for(Library::Games).ends_with("Games"));
        assert!(p.output_for(Library::Video).ends_with("Videos"));
    }

    #[test]
    fn a_folder_chosen_for_a_library_is_used_for_it() {
        let mut p = Preferences::default();
        for library in [Library::Video, Library::Music, Library::Games] {
            p.set_output_for(library, PathBuf::from("/media/x"));
            assert_eq!(p.output_for(library), PathBuf::from("/media/x"));
        }
    }
}
