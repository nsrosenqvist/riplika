//! Measuring the picture before deciding how to encode it.
//!
//! These are the decisions HandBrake makes for you and never shows you. Making
//! them explicit means they can be wrong in visible ways rather than invisible
//! ones - and one of them was: a naive `fieldmatch,decimate` on this material
//! threw away one real frame in five (24,772 kept out of 30,964). The output
//! still claimed 23.976 and still had the right duration, so nothing looked
//! wrong until the frames were counted. The cause was soft telecine: MakeMKV
//! keeps the pulldown flags and ffmpeg honours them, so the decoder was
//! *already* producing 23.976 progressive film and decimation removed real
//! frames rather than duplicates.
//!
//! The lesson is in `measure_decoded_fps`: ask the decoder what it produces,
//! never trust what the container declares.

use crate::Result;
use crate::host::{Command, Runner};
use crate::media::MediaInfo;
use std::collections::HashMap;
use std::path::Path;

/// Seconds of video sampled when measuring. Long enough for the 5-frame
/// telecine cadence to be unambiguous, short enough to stay cheap.
const SAMPLE_SECONDS: u32 = 20;

#[derive(Debug, Clone, PartialEq)]
pub struct VideoAnalysis {
    /// Frames per second the decoder actually emits.
    pub decoded_fps: f64,
    /// True only when the duplicates telecine leaves are really present.
    pub telecined: bool,
    /// `crop=W:H:X:Y`, when there are black bars worth removing.
    pub crop: Option<String>,
    /// Pixel aspect, snapped to a standard DVD ratio.
    pub sample_aspect: String,
}

/// Count frames over a short sample, optionally through a filter.
pub fn frame_count_command(path: &Path, filter: Option<&str>) -> Command {
    let mut c = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "info", "-t", &SAMPLE_SECONDS.to_string(), "-i"])
        .path(path);
    if let Some(f) = filter {
        c = c.args(["-vf", f]);
    }
    c.args(["-an", "-f", "null", "-"])
}

/// Sample the middle of the film, not the opening: studio logos and fades are
/// letterboxed differently from the feature and would give a useless crop.
pub fn crop_command(path: &Path) -> Command {
    Command::new("ffmpeg")
        .args(["-nostdin", "-v", "info", "-ss", "300", "-t", "60", "-i"])
        .path(path)
        .args(["-vf", "cropdetect=24:2:0", "-frames:v", "400", "-an", "-f", "null", "-"])
}

/// Frame count from a finished run, from whichever stream carried it.
///
/// ffmpeg writes progress to stderr, but wrappers and older builds put it on
/// stdout; reading only one silently yields zero frames, which then reads as
/// "not telecined" no matter what the source is.
pub fn frames_of(out: &crate::host::Output) -> u64 {
    parse_frame_count(&out.stderr).or_else(|| parse_frame_count(&out.stdout)).unwrap_or(0)
}

/// Pull the final `frame=  1234` out of ffmpeg's progress output.
pub fn parse_frame_count(output: &str) -> Option<u64> {
    let mut last = None;
    for line in output.lines() {
        // progress is rewritten with \r, so one "line" holds many updates
        for part in line.split('\r') {
            if let Some(rest) = part.trim_start().strip_prefix("frame=") {
                let n: String =
                    rest.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(v) = n.parse::<u64>() {
                    last = Some(v);
                }
            }
        }
    }
    last
}

/// The crop cropdetect suggested most often.
pub fn parse_crop(output: &str) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for line in output.lines() {
        for part in line.split_whitespace() {
            if part.starts_with("crop=") && part.matches(':').count() == 3 {
                *counts.entry(part).or_default() += 1;
            }
        }
    }
    // ties broken by the string so the result is deterministic across runs
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(c, _)| c.to_string())
        .filter(|c| c != "crop=0:0:0:0")
}

/// Is a crop worth applying?
///
/// Cropping a couple of pixels costs a re-encode of the whole frame and can
/// push the dimensions off the macroblock grid, for no visible gain.
pub fn crop_is_worthwhile(crop: &str, width: u32, height: u32) -> bool {
    let nums: Vec<u32> =
        crop.trim_start_matches("crop=").split(':').filter_map(|n| n.parse().ok()).collect();
    let [w, h, ..] = nums[..] else { return false };
    (width.saturating_sub(w)) >= 4 || (height.saturating_sub(h)) >= 4
}

/// Decide whether a 29.97 source is telecined, from the ratio of frames a
/// decimating filter kept.
///
/// True telecine is 4 frames out of every 5, so a ratio near 0.8. Anything
/// else means leave it alone, and in particular the 1.0 you get from soft
/// telecine that the decoder has already resolved to 23.976.
pub fn looks_telecined(plain_frames: u64, decimated_frames: u64) -> bool {
    if plain_frames == 0 {
        return false;
    }
    let ratio = decimated_frames as f64 / plain_frames as f64;
    (ratio - 0.8).abs() < 0.03
}

/// The constant frame rate to pin, given what the decoder produces.
///
/// Pinning matters because soft telecine reaches the encoder as 23.976 unique
/// frames on a 29.97 timestamp grid, every fifth held for two ticks. Muxed as
/// is, that is a variable-rate file that merely averages 23.976, which some
/// players handle badly.
pub fn pick_frame_rate(decoded_fps: f64) -> Option<&'static str> {
    let near = |target: f64| (decoded_fps - target).abs() < 0.05;
    if near(23.976) || near(24.0) {
        Some("24000/1001")
    } else if near(29.97) || near(30.0) {
        Some("30000/1001")
    } else if near(25.0) {
        Some("25")
    } else {
        None
    }
}

/// Snap a measured aspect to the ratios DVD actually uses.
///
/// Passing a source's odd ratio straight through makes ffmpeg approximate it -
/// `853/720` came back as `77/65` - and the picture drifts. The four values
/// below are the only ones a DVD can legitimately have.
pub fn snap_sar(raw: Option<&str>) -> String {
    let Some(raw) = raw else { return "1/1".into() };
    let raw = raw.replace(':', "/");
    let Some((a, b)) = raw.split_once('/') else { return "1/1".into() };
    let (Ok(a), Ok(b)) = (a.trim().parse::<f64>(), b.trim().parse::<f64>()) else {
        return "1/1".into();
    };
    if b == 0.0 || a == 0.0 {
        return "1/1".into();
    }
    let v = a / b;
    for (name, target) in [
        ("32/27", 32.0 / 27.0), // 4:3 NTSC
        ("8/9", 8.0 / 9.0),     // 4:3 PAL
        ("64/45", 64.0 / 45.0), // 16:9 NTSC
        ("16/11", 16.0 / 11.0), // 16:9 PAL
        ("1/1", 1.0),
    ] {
        if (v - target).abs() / target < 0.01 {
            return name.into();
        }
    }
    format!("{}/{}", a as i64, b as i64)
}

/// Measure what the decoder produces, rather than believing the container.
pub fn measure_decoded_fps(runner: &dyn Runner, path: &Path) -> Result<f64> {
    let out = runner.run(&frame_count_command(path, None))?;
    Ok(frames_of(&out) as f64 / SAMPLE_SECONDS as f64)
}

/// Everything the encoder needs to know about the picture.
pub fn analyze(runner: &dyn Runner, path: &Path, info: &MediaInfo) -> Result<VideoAnalysis> {
    let decoded_fps = measure_decoded_fps(runner, path)?;

    // Only 29.97 out of the decoder can still be hiding telecine; anything
    // else is either already film or genuinely video.
    let telecined = if (decoded_fps - 29.97).abs() < 0.6 {
        let plain = frames_of(&runner.run(&frame_count_command(path, None))?);
        let decimated =
            frames_of(&runner.run(&frame_count_command(path, Some("fieldmatch,decimate")))?);
        looks_telecined(plain, decimated)
    } else {
        false
    };

    let out = runner.run(&crop_command(path))?;
    let crop = parse_crop(&out.stderr)
        .or_else(|| parse_crop(&out.stdout))
        .filter(|c| crop_is_worthwhile(c, info.width, info.height));

    Ok(VideoAnalysis {
        // Undoing telecine yields film rate whatever we measured before
        decoded_fps: if telecined { 23.976 } else { decoded_fps },
        telecined,
        crop,
        sample_aspect: snap_sar(info.sample_aspect.as_deref()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_frame_count_wins_across_carriage_returns() {
        let out = "frame=  100 fps=50\rframe=  479 fps=48 q=-0.0\rframe=  599 fps=49\n";
        assert_eq!(parse_frame_count(out), Some(599));
    }

    #[test]
    fn no_progress_output_is_none_not_zero() {
        assert_eq!(parse_frame_count("Stream mapping:\n  Stream #0:0\n"), None);
    }

    #[test]
    fn soft_telecine_is_left_alone() {
        // the bug: decoder already gives 23.976, so decimate would drop real
        // frames. Equal counts mean nothing to remove.
        assert!(!looks_telecined(479, 479));
    }

    #[test]
    fn hard_telecine_is_detected_by_the_four_in_five_ratio() {
        assert!(looks_telecined(600, 480));
        assert!(looks_telecined(30_964, 24_772)); // the real measurement
    }

    #[test]
    fn a_partial_reduction_is_not_telecine() {
        assert!(!looks_telecined(600, 570));
        assert!(!looks_telecined(0, 0));
    }

    #[test]
    fn frame_rates_snap_to_the_broadcast_fractions() {
        assert_eq!(pick_frame_rate(23.976), Some("24000/1001"));
        assert_eq!(pick_frame_rate(29.97), Some("30000/1001"));
        assert_eq!(pick_frame_rate(25.0), Some("25"));
        assert_eq!(pick_frame_rate(0.0), None);
        assert_eq!(pick_frame_rate(48.0), None);
    }

    #[test]
    fn odd_aspects_snap_rather_than_being_approximated() {
        // 853/720 became 77/65 when passed through, drifting the picture
        assert_eq!(snap_sar(Some("853/720")), "32/27");
        assert_eq!(snap_sar(Some("32:27")), "32/27");
        assert_eq!(snap_sar(Some("64/45")), "64/45");
        assert_eq!(snap_sar(Some("8/9")), "8/9");
    }

    #[test]
    fn a_missing_or_broken_aspect_is_square() {
        assert_eq!(snap_sar(None), "1/1");
        assert_eq!(snap_sar(Some("0/1")), "1/1");
        assert_eq!(snap_sar(Some("nonsense")), "1/1");
    }

    #[test]
    fn an_unrecognised_aspect_is_passed_through_intact() {
        assert_eq!(snap_sar(Some("3/2")), "3/2");
    }

    #[test]
    fn the_most_frequent_crop_wins() {
        let out = "crop=720:480:0:0\ncrop=720:352:0:64\ncrop=720:352:0:64\ncrop=720:356:0:62\n";
        assert_eq!(parse_crop(out).as_deref(), Some("crop=720:352:0:64"));
    }

    #[test]
    fn a_full_frame_crop_is_not_worth_a_filter() {
        assert!(!crop_is_worthwhile("crop=720:480:0:0", 720, 480));
        assert!(!crop_is_worthwhile("crop=720:478:0:1", 720, 480));
        assert!(crop_is_worthwhile("crop=720:352:0:64", 720, 480));
    }

    #[test]
    fn measurement_samples_a_fixed_window() {
        let c = frame_count_command(Path::new("/x.mkv"), Some("fieldmatch,decimate"));
        assert_eq!(c.value_of("-t"), Some("20"));
        assert_eq!(c.value_of("-vf"), Some("fieldmatch,decimate"));
        // measuring must never wait on stdin
        assert!(c.has("-nostdin"));
    }

    #[test]
    fn crop_detection_skips_the_opening_titles() {
        let c = crop_command(Path::new("/x.mkv"));
        assert_eq!(c.value_of("-ss"), Some("300"));
    }

    #[test]
    fn analysis_reports_film_rate_once_telecine_is_undone() {
        use crate::host::FakeRunner;
        let r = FakeRunner::new()
            .on("-i /x.mkv -an", "frame=  599")
            .on("fieldmatch,decimate", "frame=  479")
            .on("cropdetect", "crop=720:352:0:64");
        // FakeRunner answers on stdout; analyze falls back to it
        let info = MediaInfo { width: 720, height: 480, ..MediaInfo::default() };
        let a = analyze(&r, Path::new("/x.mkv"), &info).unwrap();
        assert!(a.telecined);
        assert!((a.decoded_fps - 23.976).abs() < 0.001);
        assert_eq!(a.crop.as_deref(), Some("crop=720:352:0:64"));
    }
}
