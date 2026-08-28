//! What is inside a media file, according to ffprobe.
//!
//! The probe is one JSON call rather than the half-dozen CSV calls the shell
//! version made. CSV was the source of a recurring class of bug: a field that
//! is empty produces a bare comma, a field containing a comma produces an extra
//! one, and either way the columns shift silently. JSON says which field is
//! which.

use crate::host::{Command, Runner};
use crate::model::{Millis, Track, TrackKind};
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chapter {
    pub start: Millis,
    pub end: Millis,
}

impl Chapter {
    pub fn duration(&self) -> Millis {
        self.end.saturating_sub(self.start)
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MediaInfo {
    pub duration: Millis,
    pub chapters: Vec<Chapter>,
    pub tracks: Vec<Track>,
    /// Sample aspect ratio as the file states it, e.g. `32:27`.
    pub sample_aspect: Option<String>,
    pub width: u32,
    pub height: u32,
    /// Frame rate the container advertises. What the *decoder* produces can
    /// differ - see `transcode::analyze`.
    pub declared_fps: f64,
}

impl MediaInfo {
    pub fn tracks_of(&self, kind: TrackKind) -> Vec<&Track> {
        self.tracks.iter().filter(|t| t.kind == kind).collect()
    }

    /// Language tags of one stream type, positionally - the input every
    /// language filter works from.
    pub fn language_tags(&self, kind: TrackKind) -> Vec<String> {
        self.tracks_of(kind)
            .iter()
            .map(|t| t.language.clone())
            .collect()
    }

    pub fn chapter_durations(&self) -> Vec<Millis> {
        self.chapters.iter().map(Chapter::duration).collect()
    }
}

/// Reads media files. A trait so the pipeline can be exercised over invented
/// discs rather than real ones.
pub trait Prober: Send + Sync {
    fn probe(&self, path: &Path) -> Result<MediaInfo>;
}

pub struct FfProbe<'a>(pub &'a dyn Runner);

impl Prober for FfProbe<'_> {
    fn probe(&self, path: &Path) -> Result<MediaInfo> {
        let cmd = probe_command(path);
        let out = self.0.require(&cmd)?;
        parse_probe(&out.stdout)
    }
}

/// One call, everything we need.
pub fn probe_command(path: &Path) -> Command {
    Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-show_chapters",
        ])
        .path(path)
}

fn seconds_to_ms(v: &serde_json::Value) -> Millis {
    let s = match v {
        serde_json::Value::String(s) => s.parse::<f64>().unwrap_or(0.0),
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    };
    (s * 1000.0).round().max(0.0) as Millis
}

/// Parse an `avg_frame_rate` style `30000/1001` fraction.
pub fn parse_fps(s: &str) -> f64 {
    let (a, b) = s.split_once('/').unwrap_or((s, "1"));
    let (a, b) = (a.parse::<f64>().unwrap_or(0.0), b.parse::<f64>().unwrap_or(1.0));
    if b == 0.0 { 0.0 } else { a / b }
}

pub fn parse_probe(json: &str) -> Result<MediaInfo> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| Error(format!("ffprobe output: {e}")))?;

    let mut info = MediaInfo {
        duration: v
            .get("format")
            .and_then(|f| f.get("duration"))
            .map(seconds_to_ms)
            .unwrap_or(0),
        ..MediaInfo::default()
    };

    for c in v.get("chapters").and_then(|c| c.as_array()).unwrap_or(&vec![]) {
        // ffprobe gives both a rational tick count and a seconds string; the
        // string is the one that does not need the time base.
        let start = c.get("start_time").map(seconds_to_ms).unwrap_or(0);
        let end = c.get("end_time").map(seconds_to_ms).unwrap_or(0);
        info.chapters.push(Chapter { start, end });
    }

    // ffmpeg's `0:a:2` counts within a stream type, so each type is numbered
    // from zero independently of the file's overall stream order.
    let mut per_kind = [0usize; 4];
    for s in v.get("streams").and_then(|s| s.as_array()).unwrap_or(&vec![]) {
        let codec_type = s.get("codec_type").and_then(|x| x.as_str()).unwrap_or("");
        let kind = match codec_type {
            "video" => TrackKind::Video,
            "audio" => TrackKind::Audio,
            "subtitle" => TrackKind::Subtitle,
            _ => TrackKind::Other,
        };
        let slot = match kind {
            TrackKind::Video => 0,
            TrackKind::Audio => 1,
            TrackKind::Subtitle => 2,
            TrackKind::Other => 3,
        };
        let index = per_kind[slot];
        per_kind[slot] += 1;

        let tags = s.get("tags");
        let tag = |k: &str| {
            tags.and_then(|t| t.get(k))
                .and_then(|x| x.as_str())
                .map(str::to_string)
        };

        if kind == TrackKind::Video && index == 0 {
            info.width = s.get("width").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            info.height = s.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            info.sample_aspect = s
                .get("sample_aspect_ratio")
                .and_then(|x| x.as_str())
                .filter(|x| *x != "0:1" && !x.is_empty())
                .map(str::to_string);
            info.declared_fps = s
                .get("avg_frame_rate")
                .and_then(|x| x.as_str())
                .map(parse_fps)
                .unwrap_or(0.0);
        }

        info.tracks.push(Track {
            kind,
            index,
            codec: s
                .get("codec_name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            // An untagged track is `und`, not a missing value: the filter has
            // to be able to match it, and players show it as Undetermined.
            language: tag("language").unwrap_or_else(|| "und".into()),
            channels: s.get("channels").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            title: tag("title"),
            default: s
                .get("disposition")
                .and_then(|d| d.get("default"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0)
                == 1,
        });
    }

    Ok(info)
}

/// A prober that answers from a table, for tests.
#[derive(Debug, Default)]
pub struct FakeProber(pub std::sync::Mutex<Vec<(std::path::PathBuf, MediaInfo)>>);

impl FakeProber {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with(self, path: impl Into<std::path::PathBuf>, info: MediaInfo) -> Self {
        self.0.lock().unwrap().push((path.into(), info));
        self
    }
}

impl Prober for FakeProber {
    fn probe(&self, path: &Path) -> Result<MediaInfo> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, i)| i.clone())
            .ok_or_else(|| Error(format!("{}: no fake probe", path.display())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "streams": [
        {"codec_type":"video","codec_name":"mpeg2video","width":720,"height":480,
         "sample_aspect_ratio":"32:27","avg_frame_rate":"30000/1001",
         "disposition":{"default":1}},
        {"codec_type":"audio","codec_name":"ac3","channels":6,
         "tags":{"language":"eng"},"disposition":{"default":1}},
        {"codec_type":"audio","codec_name":"ac3","channels":2,
         "tags":{"language":"eng","title":"Feature Commentary"},"disposition":{"default":0}},
        {"codec_type":"audio","codec_name":"ac3","channels":6,
         "tags":{"language":"spa"},"disposition":{"default":0}},
        {"codec_type":"subtitle","codec_name":"dvd_subtitle",
         "tags":{"language":"eng"},"disposition":{"default":0}},
        {"codec_type":"subtitle","codec_name":"dvd_subtitle",
         "tags":{"language":"spa"},"disposition":{"default":0}},
        {"codec_type":"data","codec_name":"bin_data"}
      ],
      "chapters": [
        {"start_time":"0.000000","end_time":"60.500000"},
        {"start_time":"60.500000","end_time":"180.000000"}
      ],
      "format": {"duration":"1274.933000"}
    }"#;

    #[test]
    fn indices_are_counted_per_stream_type() {
        // ffmpeg's 0:a:1 means the second *audio* stream, not the second stream
        let i = parse_probe(SAMPLE).unwrap();
        let audio = i.tracks_of(TrackKind::Audio);
        assert_eq!(audio.iter().map(|t| t.index).collect::<Vec<_>>(), vec![0, 1, 2]);
        let subs = i.tracks_of(TrackKind::Subtitle);
        assert_eq!(subs.iter().map(|t| t.index).collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(i.tracks_of(TrackKind::Video)[0].index, 0);
    }

    #[test]
    fn durations_survive_the_float_round_trip() {
        let i = parse_probe(SAMPLE).unwrap();
        assert_eq!(i.duration, 1_274_933);
        assert_eq!(i.chapter_durations(), vec![60_500, 119_500]);
    }

    #[test]
    fn untagged_tracks_become_und_rather_than_missing() {
        let i = parse_probe(
            r#"{"streams":[{"codec_type":"audio","codec_name":"ac3"}],"format":{}}"#,
        )
        .unwrap();
        assert_eq!(i.tracks[0].language, "und");
    }

    #[test]
    fn a_zero_aspect_ratio_is_treated_as_absent() {
        let i = parse_probe(
            r#"{"streams":[{"codec_type":"video","sample_aspect_ratio":"0:1"}],"format":{}}"#,
        )
        .unwrap();
        assert_eq!(i.sample_aspect, None);
    }

    #[test]
    fn video_details_come_from_the_first_video_stream() {
        let i = parse_probe(SAMPLE).unwrap();
        assert_eq!((i.width, i.height), (720, 480));
        assert_eq!(i.sample_aspect.as_deref(), Some("32:27"));
        assert!((i.declared_fps - 29.97).abs() < 0.01);
    }

    #[test]
    fn data_streams_are_kept_but_marked_other() {
        let i = parse_probe(SAMPLE).unwrap();
        // `bin_data` is why output mapping is explicit rather than `-map 0`
        assert_eq!(i.tracks_of(TrackKind::Other).len(), 1);
    }

    #[test]
    fn commentary_and_language_come_through_the_tags() {
        let i = parse_probe(SAMPLE).unwrap();
        let audio = i.tracks_of(TrackKind::Audio);
        assert!(audio[1].is_commentary());
        assert_eq!(i.language_tags(TrackKind::Audio), vec!["eng", "eng", "spa"]);
    }

    #[test]
    fn fps_fractions_parse() {
        assert!((parse_fps("24000/1001") - 23.976).abs() < 0.001);
        assert_eq!(parse_fps("25/1"), 25.0);
        assert_eq!(parse_fps("0/0"), 0.0);
    }

    #[test]
    fn garbage_is_an_error_not_a_silent_empty_file() {
        assert!(parse_probe("not json").is_err());
    }
}
