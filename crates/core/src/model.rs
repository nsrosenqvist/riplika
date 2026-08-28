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
        let mut v: Vec<Millis> = self
            .titles
            .iter()
            .map(|t| t.duration)
            .filter(|d| *d >= min)
            .collect();
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
    pub reasons: Vec<String>,
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
    pub drop_commentary: bool,
    /// Where to look for `<code>.txt` wordlists used in subtitle recognition.
    pub words_dir: Option<PathBuf>,
    /// Glyph table for subtitle recognition; built on the fly when absent.
    pub glyph_table: Option<PathBuf>,
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
            drop_commentary: true,
            words_dir: None,
            glyph_table: None,
        }
    }
}

/// What one ripped title becomes in the library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Episode { season: u32, number: u32 },
    /// A longer cut of an episode, filed under `extras/`.
    ExtendedCut { season: u32, number: u32 },
    Feature,
    Extra,
    /// A title that just replays others back to back; never written out.
    PlayAll,
}

impl Role {
    pub fn is_output(&self) -> bool {
        !matches!(self, Role::PlayAll)
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
        assert_eq!(
            Role::ExtendedCut { season: 2, number: 1 }.subdirectory(),
            Some("extras")
        );
        assert_eq!(Role::Episode { season: 2, number: 1 }.subdirectory(), None);
    }

    #[test]
    fn quality_parses_the_names_a_user_types() {
        assert_eq!(Quality::parse("HIGH"), Some(Quality::High));
        assert_eq!(Quality::parse(" med "), Some(Quality::Medium));
        assert_eq!(Quality::parse("lossless"), None);
    }
}
