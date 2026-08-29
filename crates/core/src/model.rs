//! The vocabulary the whole pipeline shares.
//!
//! These types are plain data with no behaviour that touches the outside world,
//! so both frontends can hold them, serialise them, and let the user edit them
//! before anything is executed. That last point is the reason the identification
//! result is a *value* rather than a side effect: the GUI has to be able to show
//! "I think this is Parks and Recreation season 7" and let you say no.

use crate::lang::{Language, LanguageSet};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A duration in milliseconds.
///
/// Milliseconds rather than `std::time::Duration` because every source we
/// have (ffprobe, MakeMKV, SRT) is decimal seconds or a timestamp, and round
/// tripping through floats is where duration comparisons start drifting.
pub type Millis = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
    Other,
}

impl TrackKind {
    /// The letter ffmpeg uses in stream specifiers like `0:a:1`.
    pub fn spec(self) -> &'static str {
        match self {
            TrackKind::Video => "v",
            TrackKind::Audio => "a",
            TrackKind::Subtitle => "s",
            TrackKind::Other => "d",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub kind: TrackKind,
    /// Index among streams of the same kind - what `0:a:2` means.
    pub index: usize,
    pub codec: String,
    /// Language tag as the file spells it, which may be any ISO 639 variant.
    pub language: String,
    pub channels: u32,
    pub title: Option<String>,
    pub default: bool,
}

impl Track {
    /// A bitmap subtitle has to be OCR'd to become useful, and forces a
    /// burn-in re-encode on any client that selects it.
    pub fn is_bitmap_subtitle(&self) -> bool {
        self.kind == TrackKind::Subtitle
            && matches!(self.codec.as_str(), "dvd_subtitle" | "hdmv_pgs_subtitle")
    }

    /// Commentary tracks are removed by default: they are rarely wanted and
    /// they clutter the track picker on every player.
    pub fn is_commentary(&self) -> bool {
        self.title
            .as_deref()
            .map(|t| {
                let t = t.to_ascii_lowercase();
                t.contains("commentary") || t.contains("director")
            })
            .unwrap_or(false)
    }
}

/// One title as it exists on the disc, before ripping.
///
/// MakeMKV can enumerate this without reading the whole disc, which is what
/// lets us identify a disc before spending forty minutes ripping it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscTitle {
    /// MakeMKV's title id, needed to ask for this one specifically.
    pub id: u32,
    pub duration: Millis,
    pub chapter_count: usize,
    /// Chapter durations, when the scanner can give them before ripping.
    ///
    /// MakeMKV reports only a count, but ffmpeg's `dvdvideo` demuxer reports
    /// the durations - and those are what decomposes a play-all. Knowing them
    /// in advance means the play-all itself never has to be read.
    #[serde(default)]
    pub chapters: Vec<Millis>,
    pub size_bytes: u64,
    /// The filename MakeMKV will give it, e.g. `title_t03.mkv`.
    pub output_name: String,
    pub tracks: Vec<Track>,
}

/// What a scan of the drive found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiscScan {
    pub drive: Drive,
    /// The volume label, e.g. `PARKS_AND_RECREATION_S7D1`. Often the single
    /// strongest clue about what the disc is.
    pub label: String,
    pub titles: Vec<DiscTitle>,
}

impl DiscScan {
    /// Every language on the disc, in the order first encountered.
    ///
    /// This is what the rip settings offer to choose from: showing the user the
    /// languages that are actually there beats a text field they have to guess
    /// the spelling for.
    pub fn languages(&self, kind: TrackKind) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for t in &self.titles {
            for track in t.tracks.iter().filter(|x| x.kind == kind) {
                if !out.contains(&track.language) {
                    out.push(track.language.clone());
                }
            }
        }
        out
    }

    /// Audio and subtitle languages together, which is what the filter covers.
    pub fn all_languages(&self) -> Vec<String> {
        let mut out = self.languages(TrackKind::Audio);
        for l in self.languages(TrackKind::Subtitle) {
            if !out.contains(&l) {
                out.push(l);
            }
        }
        out
    }

    /// Durations of the titles worth considering, sorted - a rough fingerprint
    /// that survives being ripped and re-probed.
    pub fn duration_fingerprint(&self, min: Millis) -> Vec<Millis> {
        let mut v: Vec<Millis> =
            self.titles.iter().map(|t| t.duration).filter(|d| *d >= min).collect();
        v.sort_unstable();
        v
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Drive {
    /// What MakeMKV calls it, e.g. `disc:0`.
    pub id: String,
    /// Device node, e.g. `/dev/sr0`.
    pub device: String,
    /// Human name, e.g. `HL-DT-ST BD-RE`.
    pub name: String,
    /// Volume label when a disc is loaded.
    pub disc_label: Option<String>,
}

impl Drive {
    pub fn has_disc(&self) -> bool {
        self.disc_label.is_some()
    }
}

/// What the disc turned out to be.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Media {
    Series {
        title: String,
        year: Option<u32>,
        season: u32,
        /// Catalogue id, so episode details can be fetched later.
        provider_id: Option<String>,
    },
    Movie {
        title: String,
        year: Option<u32>,
        provider_id: Option<String>,
    },
}

impl Media {
    pub fn title(&self) -> &str {
        match self {
            Media::Series { title, .. } | Media::Movie { title, .. } => title,
        }
    }

    pub fn year(&self) -> Option<u32> {
        match self {
            Media::Series { year, .. } | Media::Movie { year, .. } => *year,
        }
    }

    /// The work itself, without the season.
    ///
    /// A catalogue search returns *shows*; which season a disc holds is a
    /// separate question about the disc. Showing a season beside a search hit
    /// implies the search found one, and then contradicts whatever the user
    /// sets the season to.
    pub fn describe_work(&self) -> String {
        match self {
            Media::Series { title, year, .. } | Media::Movie { title, year, .. } => match year {
                Some(y) => format!("{title} ({y})"),
                None => title.clone(),
            },
        }
    }

    /// The same work, for a given season.
    pub fn with_season(&self, season: u32) -> Media {
        match self {
            Media::Series { title, year, provider_id, .. } => Media::Series {
                title: title.clone(),
                year: *year,
                season,
                provider_id: provider_id.clone(),
            },
            other => other.clone(),
        }
    }

    pub fn season(&self) -> Option<u32> {
        match self {
            Media::Series { season, .. } => Some(*season),
            Media::Movie { .. } => None,
        }
    }

    /// One-line description for a GUI row.
    pub fn describe(&self) -> String {
        match self {
            Media::Series { title, season, year, .. } => match year {
                Some(y) => format!("{title} ({y}) - season {season}"),
                None => format!("{title} - season {season}"),
            },
            Media::Movie { title, year, .. } => match year {
                Some(y) => format!("{title} ({y})"),
                None => title.clone(),
            },
        }
    }
}

/// One episode from a catalogue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Episode {
    pub season: u32,
    pub number: u32,
    pub title: String,
    /// ISO date, as the tag wants it.
    pub air_date: Option<String>,
    pub runtime_minutes: Option<u32>,
}

/// A guess at what the disc is, with the evidence for it.
///
/// Confidence and reasons are both shown to the user rather than being used to
/// silently pick: the GUI puts the top candidate up with "why" underneath, so a
/// wrong guess is obvious rather than mysterious.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub media: Media,
    /// 0.0 to 1.0.
    pub confidence: f32,
    /// Why we think this disc is this work.
    pub reasons: Vec<String>,
    /// What the work is: broadcaster, kind, years. Separate from the reasons,
    /// because "who made it" and "why we think it is on this disc" are
    /// different questions and only one of them is useful when picking between
    /// nine shows with the same person's name in the title.
    pub detail: Option<String>,
}

/// How hard to work on the picture and the sound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    High,
    Medium,
    Low,
}

impl Quality {
    /// DVD is 720x480 MPEG-2 at 4-6 Mb/s, so the useful range is narrow: by
    /// CRF 18 the encode is transparent against the source and further bits
    /// mostly track MPEG-2's own noise, while below CRF 23 SD detail goes fast.
    pub fn crf(self) -> u32 {
        match self {
            Quality::High => 18,   // ~1.5 Mb/s, ~240 MB per 21-minute episode
            Quality::Medium => 20, // ~1.0 Mb/s, ~170 MB - the sweet spot
            Quality::Low => 23,    // ~0.7 Mb/s, ~107 MB
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Quality::High => "high",
            Quality::Medium => "medium",
            Quality::Low => "low",
        }
    }

    pub fn parse(s: &str) -> Option<Quality> {
        match s.trim().to_ascii_lowercase().as_str() {
            "high" => Some(Quality::High),
            "medium" | "med" => Some(Quality::Medium),
            "low" => Some(Quality::Low),
            _ => None,
        }
    }
}

/// Container for the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Container {
    /// Plays everywhere, but carries no per-track titles and only `mov_text`
    /// subtitles.
    Mp4,
    /// Carries everything, including per-track titles and full tag targets.
    Mkv,
}

impl Container {
    pub fn extension(self) -> &'static str {
        match self {
            Container::Mp4 => "mp4",
            Container::Mkv => "mkv",
        }
    }

    /// The text subtitle codec this container wants.
    pub fn text_subtitle_codec(self) -> &'static str {
        match self {
            Container::Mp4 => "mov_text",
            Container::Mkv => "srt",
        }
    }
}

/// Everything the user chose, and everything we worked out, ready to execute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JobSettings {
    pub output_dir: PathBuf,
    pub video: Quality,
    pub audio: Quality,
    pub container: Container,
    pub languages: LanguageSet,
    /// Add an AAC stereo track beside untouched original audio, so browser
    /// clients have something they can decode without server-side transcoding.
    pub dual_audio: bool,
    /// Keep VobSub bitmaps even after they have been recognised.
    pub keep_bitmap_subs: bool,
    /// Produce the longer cuts of episodes some discs carry.
    ///
    /// Spotting one means comparing pictures, which needs the file, so this
    /// also decides whether episode-length titles nobody claimed are read at
    /// all - and on a television disc those are the expensive ones.
    pub include_extended_cuts: bool,
    /// Produce the bonus material: featurettes, deleted scenes, gag reels.
    ///
    /// A season disc can carry thirty of these against seven episodes. They are
    /// most of the titles and a good deal of the reading.
    pub include_extras: bool,
    pub drop_commentary: bool,
    /// Where to look for `<code>.txt` wordlists used in subtitle recognition.
    pub words_dir: Option<PathBuf>,
    /// Glyph table for subtitle recognition; built on the fly when absent.
    pub glyph_table: Option<PathBuf>,
    /// How episode filenames are built. `None` uses the default.
    pub episode_template: Option<String>,
}

impl Default for JobSettings {
    fn default() -> Self {
        JobSettings {
            output_dir: PathBuf::from("."),
            video: Quality::Medium,
            audio: Quality::High,
            container: Container::Mp4,
            languages: LanguageSet::default(),
            dual_audio: false,
            keep_bitmap_subs: false,
            include_extended_cuts: true,
            include_extras: true,
            drop_commentary: true,
            words_dir: None,
            glyph_table: None,
            episode_template: None,
        }
    }
}

/// What one ripped title becomes in the library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Episode {
        season: u32,
        number: u32,
    },
    /// A longer cut of an episode, filed under `extras/`.
    ExtendedCut {
        season: u32,
        number: u32,
    },
    Feature,
    Extra,
    /// A title that just replays others back to back; never written out.
    PlayAll,
}

impl Role {
    pub fn is_output(&self) -> bool {
        !matches!(self, Role::PlayAll)
    }

    /// Is this worth producing, given what was asked for?
    pub fn wanted(&self, settings: &JobSettings) -> bool {
        match self {
            Role::PlayAll => false,
            Role::Episode { .. } | Role::Feature => true,
            Role::ExtendedCut { .. } => settings.include_extended_cuts,
            Role::Extra => settings.include_extras,
        }
    }

    /// Extras live in a subdirectory so a media server does not try to file
    /// them as episodes.
    pub fn subdirectory(&self) -> Option<&'static str> {
        match self {
            Role::ExtendedCut { .. } | Role::Extra => Some("extras"),
            _ => None,
        }
    }
}

/// What a disc turned out to hold, counted by role.
///
/// The counting lives here rather than in a front end because there are three
/// consumers of it - the window, the command line, and the log written for the
/// run - and three copies of "which roles count as an episode" is three
/// chances for them to disagree about the same disc.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub episodes: usize,
    pub features: usize,
    pub extended_cuts: usize,
    pub extras: usize,
    /// Not produced: the same video again, reached through a play-all title.
    pub play_alls: usize,
}

impl Plan {
    pub fn of(items: &[Item]) -> Self {
        let n = |f: fn(&Role) -> bool| items.iter().filter(|i| f(&i.role)).count();
        Self {
            episodes: n(|r| matches!(r, Role::Episode { .. })),
            features: n(|r| matches!(r, Role::Feature)),
            extended_cuts: n(|r| matches!(r, Role::ExtendedCut { .. })),
            extras: n(|r| matches!(r, Role::Extra)),
            play_alls: n(|r| matches!(r, Role::PlayAll)),
        }
    }

    /// The summary, in English, as the log and the command line both print it.
    ///
    /// English on purpose. The window says this in the reader's language, but a
    /// log that changes language with its reader is one that cannot be searched
    /// or usefully pasted into a bug report.
    pub fn lines(&self) -> Vec<String> {
        let plural = |n: usize, noun: &str| format!("{n} {noun}{}", if n == 1 { "" } else { "s" });
        let mut parts = Vec::new();
        for (n, noun) in [
            (self.episodes, "episode"),
            (self.features, "feature"),
            (self.extended_cuts, "extended cut"),
            (self.extras, "extra"),
        ] {
            if n > 0 {
                parts.push(plural(n, noun));
            }
        }
        let mut lines = Vec::new();
        if !parts.is_empty() {
            lines.push(format!("holds {}", parts.join(", ")));
        }
        if self.play_alls > 0 {
            lines.push(format!(
                "skipping {} - the same video again",
                plural(self.play_alls, "play-all title")
            ));
        }
        lines
    }

    /// Nothing it can name. Play-all titles do not count: they are what is
    /// being skipped, not what is being made.
    pub fn holds_nothing(&self) -> bool {
        self.episodes == 0 && self.features == 0 && self.extended_cuts == 0 && self.extras == 0
    }
}

/// One title, what we decided it is, and what it should be called.
///
/// This is the structure the GUI lets you edit before pressing go: change a
/// role, fix a title, reorder two episodes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// Path to the ripped file.
    pub source: PathBuf,
    pub role: Role,
    /// Display title, e.g. `Pawnee Zoo`.
    pub title: String,
    pub air_date: Option<String>,
    pub duration: Millis,
    /// Filled in once planned; the final path under the output directory.
    pub destination: Option<PathBuf>,
}

/// Metadata written into the output file.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Tags {
    pub title: Option<String>,
    pub show: Option<String>,
    pub season_number: Option<u32>,
    pub episode_sort: Option<u32>,
    pub episode_id: Option<String>,
    pub date: Option<String>,
    /// Apple's `media_type`: 10 is a TV show, 9 a movie. Jellyfin and Plex both
    /// read it, and without it an MP4 episode can be filed as a movie.
    pub media_type: Option<u32>,
}

/// A language's subtitles, once recognised.
#[derive(Debug, Clone, PartialEq)]
pub struct RecognisedSubtitle {
    pub language: Language,
    /// Index among the file's subtitle streams.
    pub stream: usize,
    pub srt_path: PathBuf,
    pub cues: usize,
    pub unknown_glyphs: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(role: Role) -> Item {
        Item {
            source: PathBuf::from("/rip/a.mkv"),
            role,
            title: String::new(),
            air_date: None,
            duration: 0,
            destination: None,
        }
    }

    #[test]
    fn a_plan_counts_each_role_apart() {
        let items = vec![
            with(Role::Episode { season: 1, number: 1 }),
            with(Role::Episode { season: 1, number: 2 }),
            with(Role::Feature),
            with(Role::ExtendedCut { season: 1, number: 1 }),
            with(Role::Extra),
            with(Role::Extra),
            with(Role::PlayAll),
        ];
        let p = Plan::of(&items);
        assert_eq!(
            (p.episodes, p.features, p.extended_cuts, p.extras, p.play_alls),
            (2, 1, 1, 2, 1)
        );
    }

    #[test]
    fn a_disc_of_only_play_alls_holds_nothing() {
        // they are what is being skipped, not what is being made
        assert!(Plan::of(&[with(Role::PlayAll)]).holds_nothing());
        assert!(Plan::of(&[]).holds_nothing());
        assert!(!Plan::of(&[with(Role::Extra)]).holds_nothing());
    }

    #[test]
    fn one_of_something_is_written_as_one() {
        // the same mistake the window made: a count formatted into a noun
        // spelled for the other case
        let one = Plan { episodes: 1, features: 1, extended_cuts: 1, extras: 1, play_alls: 1 };
        assert_eq!(one.lines()[0], "holds 1 episode, 1 feature, 1 extended cut, 1 extra");
        assert_eq!(one.lines()[1], "skipping 1 play-all title - the same video again");
    }

    #[test]
    fn more_than_one_is_written_as_many() {
        let many = Plan { episodes: 7, features: 0, extended_cuts: 0, extras: 23, play_alls: 2 };
        assert_eq!(many.lines()[0], "holds 7 episodes, 23 extras");
        assert_eq!(many.lines()[1], "skipping 2 play-all titles - the same video again");
    }

    #[test]
    fn a_plan_with_nothing_in_it_says_nothing() {
        // an empty line in a log reads as something having gone wrong
        assert!(Plan::default().lines().is_empty());
    }

    #[test]
    fn bitmap_detection_covers_dvd_and_bluray() {
        let sub = |codec: &str| Track {
            kind: TrackKind::Subtitle,
            index: 0,
            codec: codec.into(),
            language: "eng".into(),
            channels: 0,
            title: None,
            default: false,
        };
        assert!(sub("dvd_subtitle").is_bitmap_subtitle());
        assert!(sub("hdmv_pgs_subtitle").is_bitmap_subtitle());
        assert!(!sub("mov_text").is_bitmap_subtitle());
        assert!(!sub("subrip").is_bitmap_subtitle());
    }

    #[test]
    fn commentary_is_recognised_however_it_is_spelt() {
        let named = |t: &str| Track {
            kind: TrackKind::Audio,
            index: 0,
            codec: "ac3".into(),
            language: "eng".into(),
            channels: 2,
            title: Some(t.into()),
            default: false,
        };
        assert!(named("Commentary").is_commentary());
        assert!(named("Feature Commentary with the cast").is_commentary());
        assert!(named("Director's commentary").is_commentary());
        assert!(!named("English 5.1").is_commentary());
    }

    #[test]
    fn play_all_titles_are_never_written_out() {
        assert!(!Role::PlayAll.is_output());
        assert!(Role::Episode { season: 1, number: 1 }.is_output());
    }

    #[test]
    fn extras_are_filed_in_a_subdirectory() {
        assert_eq!(Role::Extra.subdirectory(), Some("extras"));
        assert_eq!(Role::ExtendedCut { season: 2, number: 1 }.subdirectory(), Some("extras"));
        assert_eq!(Role::Episode { season: 2, number: 1 }.subdirectory(), None);
    }

    #[test]
    fn quality_parses_the_names_a_user_types() {
        assert_eq!(Quality::parse("HIGH"), Some(Quality::High));
        assert_eq!(Quality::parse(" med "), Some(Quality::Medium));
        assert_eq!(Quality::parse("lossless"), None);
    }
}

#[cfg(test)]
mod season_tests {
    use super::*;

    fn show() -> Media {
        Media::Series {
            title: "Parks and Recreation".into(),
            year: Some(2009),
            season: 1,
            provider_id: Some("1633".into()),
        }
    }

    #[test]
    fn a_search_hit_is_described_without_a_season() {
        // the search found a show; which season the disc holds is a separate
        // question, and implying otherwise contradicts whatever season is set
        assert_eq!(show().describe_work(), "Parks and Recreation (2009)");
    }

    #[test]
    fn the_season_can_be_changed_without_a_new_search() {
        let s6 = show().with_season(6);
        assert_eq!(s6.season(), Some(6));
        assert_eq!(s6.title(), "Parks and Recreation");
        // the catalogue id survives, so episode titles still resolve
        assert_eq!(s6.provider_id().as_deref(), Some("1633"));
    }

    #[test]
    fn a_film_has_no_season_to_set() {
        let m = Media::Movie { title: "Lebowski".into(), year: Some(1998), provider_id: None };
        assert_eq!(m.season(), None);
        assert_eq!(m.with_season(6), m, "setting a season on a film changes nothing");
    }
}

#[cfg(test)]
mod wanted_tests {
    use super::*;

    fn settings(extended: bool, extras: bool) -> JobSettings {
        JobSettings {
            include_extended_cuts: extended,
            include_extras: extras,
            ..JobSettings::default()
        }
    }

    #[test]
    fn episodes_and_features_are_always_taken() {
        // there is no configuration in which the thing you put the disc in for
        // is skipped
        for s in [settings(false, false), settings(true, true)] {
            assert!(Role::Episode { season: 6, number: 1 }.wanted(&s));
            assert!(Role::Feature.wanted(&s));
        }
    }

    #[test]
    fn a_play_all_is_never_taken_however_it_is_configured() {
        for s in [settings(false, false), settings(true, true)] {
            assert!(!Role::PlayAll.wanted(&s));
        }
    }

    #[test]
    fn the_two_switches_are_independent() {
        let cut = Role::ExtendedCut { season: 6, number: 1 };
        assert!(cut.wanted(&settings(true, false)));
        assert!(!cut.wanted(&settings(false, true)));
        assert!(Role::Extra.wanted(&settings(false, true)));
        assert!(!Role::Extra.wanted(&settings(true, false)));
    }

    #[test]
    fn wanting_neither_leaves_only_the_programme() {
        let s = settings(false, false);
        let roles = [
            Role::Episode { season: 6, number: 1 },
            Role::ExtendedCut { season: 6, number: 1 },
            Role::Extra,
            Role::PlayAll,
        ];
        let kept: Vec<&Role> = roles.iter().filter(|r| r.wanted(&s)).collect();
        assert_eq!(kept.len(), 1);
        assert!(matches!(kept[0], Role::Episode { .. }));
    }
}
