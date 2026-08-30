//! Deciding how to encode a title, and building the command that does it.
//!
//! HandBrake is a wrapper around the same libx264 this uses; encoding a title
//! both ways with matching settings produces byte-identical x264 parameter
//! strings. What HandBrake actually supplies is the preprocessing decisions,
//! which `analyze` now makes explicitly. What is left is mapping and muxing,
//! and that is what this module plans.
//!
//! The shell version needed three ffmpeg passes per episode: transcode, then
//! mux in the recognised subtitles, then write the tags. Two of those passes
//! existed only because the subtitles were recognised from the *transcode*
//! rather than from the rip. Recognising from the rip instead means the SRTs
//! exist before encoding starts, so they can be extra inputs to the one pass
//! that was always necessary.
//!
//! Everything here is pure: [`plan`] turns state into a [`TranscodePlan`], and
//! [`TranscodePlan::command`] turns that into argv. Nothing runs. That is what
//! lets a test assert that a specific `-map` is present, which is how the
//! silently-dropped subtitle track and the off-by-one `-disposition` index
//! would both have been caught before they reached a file.

pub mod analyze;
pub mod mp4;

use crate::host::Command;
use crate::lang::LanguageSet;
use crate::media::MediaInfo;
use crate::model::{Container, JobSettings, Quality, Tags, TrackKind};
use analyze::VideoAnalysis;
use std::path::{Path, PathBuf};

/// A recognised subtitle file to be muxed in.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleInput {
    pub path: PathBuf,
    /// ISO 639-2 code written into the output track.
    pub language: String,
}

/// Which of the source's tracks survive.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TrackSelection {
    /// Audio streams to keep, as `0:a:N` indices, in output order.
    pub audio: Vec<usize>,
    /// Bitmap subtitle streams to carry through, as `0:s:N` indices.
    pub bitmaps: Vec<usize>,
    /// Audio dropped for being commentary, for reporting.
    pub dropped_commentary: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscodePlan {
    pub input: PathBuf,
    pub output: PathBuf,
    pub video: Quality,
    pub audio: Quality,
    pub container: Container,
    pub selection: TrackSelection,
    pub subtitles: Vec<SubtitleInput>,
    /// Video filter chain, already ordered.
    pub filters: Vec<String>,
    pub frame_rate: Option<String>,
    pub dual_audio: bool,
    pub tags: Tags,
}

impl TranscodePlan {
    /// Number of audio streams in the output, counting the stereo fallback.
    pub fn audio_count(&self) -> usize {
        self.selection.audio.len() + usize::from(self.dual_audio)
    }

    /// The ffmpeg invocation. Pure - this is the thing tests assert on.
    pub fn command(&self) -> Command {
        let mut c = Command::new("ffmpeg").args(["-nostdin", "-v", "error", "-y", "-i"]);
        c = c.path(&self.input);

        // Recognised subtitles are extra inputs, so input N+1 is subtitle N.
        for s in &self.subtitles {
            c = c.arg("-i").path(&s.path);
        }

        c = c.args(["-map", "0:v:0"]);
        for i in &self.selection.audio {
            c = c.args(["-map", &format!("0:a:{i}")]);
        }
        // The fallback is derived from whichever track ended up first, so it
        // follows the language preference rather than the file's own order.
        if self.dual_audio {
            let first = self.selection.audio.first().copied().unwrap_or(0);
            c = c.args(["-map", &format!("0:a:{first}")]);
        }
        for (n, _) in self.subtitles.iter().enumerate() {
            c = c.args(["-map", &format!("{}:0", n + 1)]);
        }
        for i in &self.selection.bitmaps {
            c = c.args(["-map", &format!("0:s:{i}")]);
        }
        // Chapters are worth keeping and data streams are not: a DVD rip
        // carries a `bin_data` stream that no player wants and that broke a
        // blanket `-map 0`.
        c = c.args(["-dn", "-map_chapters", "0"]);

        c = c.args(["-c:v", "libx264", "-crf", &self.video.crf().to_string()]);
        c = c.args(["-preset", "medium", "-profile:v", "high", "-level", "4.0"]);
        if !self.filters.is_empty() {
            c = c.args(["-vf", &self.filters.join(",")]);
        }
        if let Some(r) = &self.frame_rate {
            c = c.args(["-fps_mode", "cfr", "-r", r]);
        }

        // Audio. "high" keeps the original AC3 untouched: no downmix, no second
        // lossy generation. The cost is browsers, which cannot decode AC3 at
        // all - hence --dual-audio.
        match self.audio {
            Quality::High => c = c.args(["-c:a", "copy"]),
            Quality::Medium => c = c.args(["-c:a", "aac", "-b:a", "160k", "-ac", "2"]),
            Quality::Low => c = c.args(["-c:a", "aac", "-b:a", "96k", "-ac", "2"]),
        }
        if self.dual_audio {
            let n = self.selection.audio.len();
            c = c.args([
                &format!("-c:a:{n}"),
                "aac",
                &format!("-b:a:{n}"),
                "160k",
                &format!("-ac:a:{n}"),
                "2",
            ]);
        }

        for (n, s) in self.subtitles.iter().enumerate() {
            c = c.args([&format!("-c:s:{n}"), self.container.format().text_subtitle_codec()]);
            c = c.args([&format!("-metadata:s:s:{n}"), &format!("language={}", s.language)]);
        }
        let text = self.subtitles.len();
        for (n, _) in self.selection.bitmaps.iter().enumerate() {
            c = c.args([&format!("-c:s:{}", text + n), "copy"]);
        }

        // Make the language preference real. ffmpeg copies the source's
        // disposition across, so without this "swedish,english" would put
        // Swedish first and still leave English flagged default - and players
        // go by the flag, not the order.
        for i in 0..self.audio_count() {
            c = c.args([
                format!("-disposition:a:{i}"),
                if i == 0 { "default" } else { "0" }.to_string(),
            ]);
        }
        for i in 0..(text + self.selection.bitmaps.len()) {
            // Only a *text* track is ever made default. Defaulting to a bitmap
            // makes the server burn it into the picture and re-encode, which is
            // the problem subtitle recognition exists to avoid.
            let on = i == 0 && text > 0;
            c = c.args([
                format!("-disposition:s:{i}"),
                if on { "default" } else { "0" }.to_string(),
            ]);
        }

        let format = self.container.format();
        for (k, v) in self.tags.pairs() {
            if format.mux_carries(k) {
                c = c.args(["-metadata", &format!("{k}={v}")]);
            }
        }

        c = c.args(format.mux_options().iter().copied());
        // Say the format rather than leaving ffmpeg to infer it from the name.
        // The file being written is a ".part", so there is nothing in the name
        // to infer from, and ffmpeg's answer is to refuse the output entirely.
        c = c.args(["-f", format.muxer()]);
        c.path(&self.output)
    }
}

impl Tags {
    /// Tag names ffmpeg understands, skipping the ones we have nothing for.
    pub fn pairs(&self) -> Vec<(&'static str, String)> {
        let mut v = Vec::new();
        if let Some(x) = &self.title {
            v.push(("title", x.clone()));
        }
        if let Some(x) = &self.show {
            v.push(("show", x.clone()));
        }
        if let Some(x) = self.season_number {
            v.push(("season_number", x.to_string()));
        }
        if let Some(x) = self.episode_sort {
            v.push(("episode_sort", x.to_string()));
        }
        if let Some(x) = &self.episode_id {
            v.push(("episode_id", x.clone()));
        }
        if let Some(x) = &self.date {
            v.push(("date", x.clone()));
        }
        if let Some(x) = self.media_type {
            v.push(("media_type", x.to_string()));
        }
        v
    }
}

/// Choose which source tracks to keep.
pub fn select_tracks(info: &MediaInfo, settings: &JobSettings) -> TrackSelection {
    let audio_tracks = info.tracks_of(TrackKind::Audio);
    let commentary: Vec<usize> = audio_tracks
        .iter()
        .filter(|t| settings.drop_commentary && t.is_commentary())
        .map(|t| t.index)
        .collect();

    // Filter by language over the tracks that survived the commentary rule, so
    // a commentary track can never be the one English track that is kept.
    let keepable: Vec<&&crate::model::Track> =
        audio_tracks.iter().filter(|t| !commentary.contains(&t.index)).collect();
    let tags: Vec<String> = keepable.iter().map(|t| t.language.clone()).collect();
    let audio: Vec<usize> = settings
        .languages
        .select_with_fallback(&tags, TrackKind::Audio)
        .into_iter()
        .map(|i| keepable[i].index)
        .collect();

    let sub_tracks = info.tracks_of(TrackKind::Subtitle);
    let sub_tags: Vec<String> = sub_tracks.iter().map(|t| t.language.clone()).collect();
    let wanted = settings.languages.select_with_fallback(&sub_tags, TrackKind::Subtitle);
    let bitmaps: Vec<usize> = if settings.keep_bitmap_subs {
        wanted
            .iter()
            .map(|i| sub_tracks[*i].index)
            .filter(|i| sub_tracks[*i].is_bitmap_subtitle())
            .collect()
    } else {
        Vec::new()
    };

    TrackSelection { audio, bitmaps, dropped_commentary: commentary }
}

/// Which subtitle streams to recognise, in the user's preferred order.
pub fn subtitles_to_recognise(info: &MediaInfo, languages: &LanguageSet) -> Vec<usize> {
    let subs = info.tracks_of(TrackKind::Subtitle);
    let tags: Vec<String> = subs.iter().map(|t| t.language.clone()).collect();
    languages
        .select_with_fallback(&tags, TrackKind::Subtitle)
        .into_iter()
        .filter(|i| subs[*i].is_bitmap_subtitle())
        .collect()
}

/// Build the plan for one title.
///
/// `failed` names subtitle streams recognition could not handle. Their bitmaps
/// are carried through whatever the settings say: dropping one loses that
/// language entirely, which is far worse than the redundancy we are removing.
#[allow(clippy::too_many_arguments)]
pub fn plan(
    input: &Path,
    output: &Path,
    info: &MediaInfo,
    analysis: &VideoAnalysis,
    settings: &JobSettings,
    subtitles: Vec<SubtitleInput>,
    failed: &[usize],
    tags: Tags,
) -> TranscodePlan {
    let mut selection = select_tracks(info, settings);
    let sub_tracks = info.tracks_of(TrackKind::Subtitle);
    for i in failed {
        if let Some(t) = sub_tracks.get(*i)
            && t.is_bitmap_subtitle()
            && !selection.bitmaps.contains(&t.index)
        {
            selection.bitmaps.push(t.index);
        }
    }
    selection.bitmaps.sort_unstable();

    let mut filters = Vec::new();
    if analysis.telecined {
        filters.push("fieldmatch".to_string());
        filters.push("decimate".to_string());
    }
    if let Some(c) = &analysis.crop {
        filters.push(c.clone());
    }
    filters.push(format!("setsar={}", analysis.sample_aspect));

    TranscodePlan {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        video: settings.video,
        audio: settings.audio,
        container: settings.container,
        selection,
        subtitles,
        filters,
        frame_rate: analyze::pick_frame_rate(analysis.decoded_fps).map(str::to_string),
        // Re-encoding audio and then adding a second lossy copy of it would be
        // two generations of loss for no gain, so the fallback only makes sense
        // beside untouched original audio.
        dual_audio: settings.dual_audio && settings.audio == Quality::High,
        tags,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Track;

    fn track(kind: TrackKind, index: usize, codec: &str, lang: &str, title: Option<&str>) -> Track {
        Track {
            kind,
            index,
            codec: codec.into(),
            language: lang.into(),
            channels: if kind == TrackKind::Audio { 6 } else { 0 },
            title: title.map(str::to_string),
            default: index == 0,
        }
    }

    /// A typical Parks and Recreation disc: 5.1 English, a commentary, Spanish,
    /// and bitmap subtitles for both languages.
    fn disc() -> MediaInfo {
        MediaInfo {
            duration: 1_274_933,
            width: 720,
            height: 480,
            sample_aspect: Some("32:27".into()),
            declared_fps: 29.97,
            tracks: vec![
                track(TrackKind::Video, 0, "mpeg2video", "und", None),
                track(TrackKind::Audio, 0, "ac3", "eng", None),
                track(TrackKind::Audio, 1, "ac3", "eng", Some("Feature Commentary")),
                track(TrackKind::Audio, 2, "ac3", "spa", None),
                track(TrackKind::Subtitle, 0, "dvd_subtitle", "eng", None),
                track(TrackKind::Subtitle, 1, "dvd_subtitle", "spa", None),
                track(TrackKind::Other, 0, "bin_data", "und", None),
            ],
            ..MediaInfo::default()
        }
    }

    fn analysis() -> VideoAnalysis {
        VideoAnalysis {
            decoded_fps: 23.976,
            telecined: false,
            crop: None,
            sample_aspect: "32/27".into(),
        }
    }

    fn settings() -> JobSettings {
        JobSettings::default()
    }

    #[test]
    fn the_output_format_is_stated_and_not_left_to_the_file_name() {
        // The bug this replaces produced nothing at all. Files are written to
        // a ".part" path while being made, ffmpeg picks its muxer from the
        // extension, and ".part" is not one it knows - so it refused every
        // output with "Invalid argument" and the whole run wrote zero files.
        //
        // Nothing caught it because every test here drives a FakeRunner, which
        // will happily accept a command real ffmpeg rejects. So the check has
        // to be that the format is said out loud.
        let mut s = settings();
        s.container = Container::Mp4;
        assert_eq!(build(&s, vec![], &[]).value_of("-f"), Some("mp4"));
        s.container = Container::Mkv;
        assert_eq!(build(&s, vec![], &[]).value_of("-f"), Some("matroska"));
    }

    fn build(s: &JobSettings, subs: Vec<SubtitleInput>, failed: &[usize]) -> Command {
        plan(
            Path::new("/rip/t00.mkv"),
            Path::new("/out/ep.mp4"),
            &disc(),
            &analysis(),
            s,
            subs,
            failed,
            Tags::default(),
        )
        .command()
    }

    fn srt(lang: &str) -> SubtitleInput {
        SubtitleInput { path: PathBuf::from(format!("/tmp/{lang}.srt")), language: lang.into() }
    }

    #[test]
    fn commentary_is_dropped_by_default() {
        let sel = select_tracks(&disc(), &settings());
        assert_eq!(sel.audio, vec![0, 2]);
        assert_eq!(sel.dropped_commentary, vec![1]);
    }

    #[test]
    fn commentary_is_kept_when_asked_for() {
        let mut s = settings();
        s.drop_commentary = false;
        assert_eq!(select_tracks(&disc(), &s).audio, vec![0, 1, 2]);
    }

    #[test]
    fn a_language_filter_never_selects_the_commentary_track() {
        // both English tracks are "eng"; the filter must not pick the wrong one
        let mut s = settings();
        s.languages = LanguageSet::parse("english");
        assert_eq!(select_tracks(&disc(), &s).audio, vec![0]);
    }

    #[test]
    fn language_order_reorders_the_output() {
        let mut s = settings();
        s.languages = LanguageSet::parse("spanish,english");
        let sel = select_tracks(&disc(), &s);
        assert_eq!(sel.audio, vec![2, 0]);
    }

    #[test]
    fn bitmaps_are_dropped_unless_asked_for() {
        assert!(select_tracks(&disc(), &settings()).bitmaps.is_empty());
        let mut s = settings();
        s.keep_bitmap_subs = true;
        assert_eq!(select_tracks(&disc(), &s).bitmaps, vec![0, 1]);
    }

    #[test]
    fn a_track_recognition_failed_on_keeps_its_bitmap_regardless() {
        // dropping it would lose that language entirely, not just its text form
        let c = build(&settings(), vec![srt("eng")], &[1]);
        assert!(c.values_of("-map").contains(&"0:s:1"), "{}", c.display());
    }

    #[test]
    fn the_data_stream_is_excluded_rather_than_mapped_wholesale() {
        // `-map 0` accumulated a duplicate bin_data stream on every pass
        let c = build(&settings(), vec![srt("eng")], &[]);
        assert!(c.has("-dn"));
        assert!(!c.values_of("-map").contains(&"0"));
    }

    #[test]
    fn recognised_subtitles_are_mapped_from_their_own_inputs() {
        // the bug this replaces: `-c copy` without a subtitle -map wrote a file
        // that looked right and had no subtitles in it
        let c = build(&settings(), vec![srt("eng"), srt("spa")], &[]);
        let maps = c.values_of("-map");
        assert_eq!(maps, vec!["0:v:0", "0:a:0", "0:a:2", "1:0", "2:0"]);
        assert_eq!(c.value_of("-c:s:0"), Some("mov_text"));
        assert_eq!(c.value_of("-metadata:s:s:1"), Some("language=spa"));
    }

    #[test]
    fn the_first_text_subtitle_is_default_and_the_rest_are_not() {
        // English subtitles enabled by default is a deliberate preference here,
        // not an accident of what the disc happened to flag
        let c = build(&settings(), vec![srt("eng"), srt("spa")], &[]);
        assert_eq!(c.value_of("-disposition:s:0"), Some("default"));
        assert_eq!(c.value_of("-disposition:s:1"), Some("0"));
    }

    #[test]
    fn a_bitmap_is_never_made_the_default_subtitle() {
        // a default bitmap makes the server burn it in and re-encode, which is
        // the whole problem recognition exists to avoid
        let mut s = settings();
        s.keep_bitmap_subs = true;
        let c = build(&s, vec![], &[]);
        assert_eq!(c.value_of("-disposition:s:0"), Some("0"));
    }

    #[test]
    fn dispositions_cover_every_output_stream_exactly_once() {
        let mut s = settings();
        s.keep_bitmap_subs = true;
        s.dual_audio = true;
        let p = plan(
            Path::new("/i.mkv"),
            Path::new("/o.mp4"),
            &disc(),
            &analysis(),
            &s,
            vec![srt("eng")],
            &[],
            Tags::default(),
        );
        let c = p.command();
        // 2 audio + 1 fallback, then 1 text + 2 bitmap subtitles
        for i in 0..3 {
            assert!(c.has(&format!("-disposition:a:{i}")), "audio {i}");
        }
        assert!(!c.has("-disposition:a:3"));
        for i in 0..3 {
            assert!(c.has(&format!("-disposition:s:{i}")), "sub {i}");
        }
        assert!(!c.has("-disposition:s:3"));
    }

    #[test]
    fn the_stereo_fallback_follows_the_preferred_track_not_the_first_in_the_file() {
        let mut s = settings();
        s.dual_audio = true;
        s.languages = LanguageSet::parse("spanish,english");
        let c = build(&s, vec![], &[]);
        // Spanish is 0:a:2 and is now first, so the fallback is derived from it
        assert_eq!(c.values_of("-map"), vec!["0:v:0", "0:a:2", "0:a:0", "0:a:2"]);
        assert_eq!(c.value_of("-c:a:2"), Some("aac"));
    }

    #[test]
    fn dual_audio_is_ignored_when_the_audio_is_already_re_encoded() {
        // adding a lossy copy of a lossy copy buys nothing
        let mut s = settings();
        s.dual_audio = true;
        s.audio = Quality::Medium;
        let p = plan(
            Path::new("/i.mkv"),
            Path::new("/o.mp4"),
            &disc(),
            &analysis(),
            &s,
            vec![],
            &[],
            Tags::default(),
        );
        assert!(!p.dual_audio);
    }

    #[test]
    fn audio_tiers_map_to_the_intended_codecs() {
        for (q, expect) in
            [(Quality::High, "copy"), (Quality::Medium, "aac"), (Quality::Low, "aac")]
        {
            let mut s = settings();
            s.audio = q;
            assert_eq!(build(&s, vec![], &[]).value_of("-c:a"), Some(expect));
        }
        let mut s = settings();
        s.audio = Quality::Low;
        assert_eq!(build(&s, vec![], &[]).value_of("-b:a"), Some("96k"));
    }

    #[test]
    fn video_tiers_map_to_the_intended_crf() {
        for (q, crf) in [(Quality::High, "18"), (Quality::Medium, "20"), (Quality::Low, "23")] {
            let mut s = settings();
            s.video = q;
            assert_eq!(build(&s, vec![], &[]).value_of("-crf"), Some(crf));
        }
    }

    #[test]
    fn the_filter_chain_is_ordered_ivtc_then_crop_then_aspect() {
        let a = VideoAnalysis {
            decoded_fps: 23.976,
            telecined: true,
            crop: Some("crop=720:352:0:64".into()),
            sample_aspect: "32/27".into(),
        };
        let p = plan(
            Path::new("/i.mkv"),
            Path::new("/o.mp4"),
            &disc(),
            &a,
            &settings(),
            vec![],
            &[],
            Tags::default(),
        );
        assert_eq!(
            p.command().value_of("-vf"),
            Some("fieldmatch,decimate,crop=720:352:0:64,setsar=32/27")
        );
        // undoing telecine always lands on film rate
        assert_eq!(p.command().value_of("-r"), Some("24000/1001"));
        assert_eq!(p.command().value_of("-fps_mode"), Some("cfr"));
    }

    #[test]
    fn the_frame_rate_is_pinned_so_the_output_is_not_variable() {
        let c = build(&settings(), vec![], &[]);
        assert_eq!(c.value_of("-fps_mode"), Some("cfr"));
        assert_eq!(c.value_of("-r"), Some("24000/1001"));
    }

    #[test]
    fn faststart_is_mp4_only() {
        assert!(build(&settings(), vec![], &[]).has("+faststart"));
        let mut s = settings();
        s.container = Container::Mkv;
        let c = build(&s, vec![srt("eng")], &[]);
        assert!(!c.has("+faststart"));
        // and MKV takes real SRT rather than MP4's cut-down text codec
        assert_eq!(c.value_of("-c:s:0"), Some("srt"));
    }

    #[test]
    fn chapters_are_carried_over() {
        assert_eq!(build(&settings(), vec![], &[]).value_of("-map_chapters"), Some("0"));
    }

    #[test]
    fn tags_are_written_in_the_same_pass() {
        let t = Tags {
            title: Some("Pawnee Zoo".into()),
            show: Some("Parks and Recreation".into()),
            season_number: Some(2),
            episode_sort: Some(1),
            media_type: Some(10),
            ..Tags::default()
        };
        let p = plan(
            Path::new("/i.mkv"),
            Path::new("/o.mp4"),
            &disc(),
            &analysis(),
            &settings(),
            vec![],
            &[],
            t,
        );
        let c = p.command();
        let meta = c.values_of("-metadata");
        assert!(meta.contains(&"title=Pawnee Zoo"));
        assert!(meta.contains(&"show=Parks and Recreation"));
        assert!(meta.contains(&"media_type=10"));
        // nothing empty gets written
        assert!(!meta.iter().any(|m| m.ends_with('=')));
    }

    #[test]
    fn ffmpeg_is_never_given_our_stdin() {
        // one loop, eight episodes, seven silently skipped
        assert!(build(&settings(), vec![], &[]).has("-nostdin"));
    }

    #[test]
    fn only_bitmap_subtitles_are_sent_for_recognition() {
        let mut info = disc();
        info.tracks.push(track(TrackKind::Subtitle, 2, "mov_text", "eng", None));
        // an already-recognised text track would be pointless to OCR
        assert_eq!(subtitles_to_recognise(&info, &LanguageSet::default()), vec![0, 1]);
        assert_eq!(subtitles_to_recognise(&info, &LanguageSet::parse("spanish")), vec![1]);
    }
}
