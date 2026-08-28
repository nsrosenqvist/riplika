//! Reading DVDs with nothing but free software.
//!
//! MakeMKV is excellent and it is also proprietary, so it is worth knowing what
//! it is actually needed for. For DVD: nothing. `libdvdread`, `libdvdnav` and
//! `libdvdcss` handle the structure and the CSS, and ffmpeg exposes all three
//! through its `dvdvideo` demuxer - which we already depend on. So the whole
//! DVD path is free software, with no extra dependency beyond an ffmpeg built
//! `--enable-libdvdnav --enable-libdvdread`.
//!
//! Blu-ray is a different matter and this module does not pretend otherwise:
//! `libbluray` reads the structure but decryption needs `libaacs`, which ships
//! no keys, and `libbdplus`, which needs conversion tables. See
//! [`super::makemkv`], which is still the right tool there.
//!
//! One thing this gains over MakeMKV: chapter *durations* come back from the
//! scan, before anything is ripped. That is what play-all decomposition needs,
//! so the disc can be sorted out in advance and the play-all title - two and a
//! half hours of redundant reading on a single Parks and Recreation disc -
//! never read at all.

use crate::host::{Command, Runner};
use crate::media::parse_probe;
use crate::model::{DiscScan, DiscTitle, Drive, Millis, TrackKind};
use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// Highest DVD title number the format allows.
const MAX_TITLE: u32 = 99;

/// Titles shorter than this are menu stubs and first-play jumps.
const MIN_TITLE_MS: Millis = 5_000;

/// ISO 9660 puts its primary volume descriptor in sector 16.
const PVD_OFFSET: u64 = 16 * 2048;

/// Read the volume label out of an ISO 9660 primary volume descriptor.
///
/// The label is the one clue a DVD gives about what it is, and it costs a
/// single 2 KB read - no library, no disc spin-up beyond what is already
/// happening.
pub fn volume_label(pvd: &[u8]) -> Option<String> {
    // type 1, then the magic, or this is not a primary volume descriptor
    if pvd.len() < 72 || pvd[0] != 1 || &pvd[1..6] != b"CD001" {
        return None;
    }
    let raw = &pvd[40..72];
    let label: String = raw
        .iter()
        .map(|b| *b as char)
        .collect::<String>()
        .trim_end_matches(['\0', ' '])
        .trim()
        .to_string();
    if label.is_empty() { None } else { Some(label) }
}

fn read_volume_label(device: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(device).ok()?;
    f.seek(SeekFrom::Start(PVD_OFFSET)).ok()?;
    let mut buf = [0u8; 2048];
    f.read_exact(&mut buf).ok()?;
    volume_label(&buf)
}

/// libdvdcss key-exchange method.
///
/// Its default falls back to cracking the title keys by brute force, which on
/// this disc failed for exactly the VTSs holding the episodes - the scan came
/// back with the extras and nothing else, which looks like a disc with no
/// episodes on it rather than like a decryption failure. `key` does the proper
/// player-key exchange with the drive and is both faster and more reliable.
pub const DVDCSS_METHOD: &str = "key";

/// Probe one title. Everything we need is in ffprobe's normal JSON.
pub fn probe_command(device: &Path, title: u32) -> Command {
    Command::new("ffprobe")
        .env("DVDCSS_METHOD", DVDCSS_METHOD)
        .args([
            "-v", "error",
            "-f", "dvdvideo",
            "-title", &title.to_string(),
            "-i",
        ])
        .path(device)
        .args([
            "-print_format", "json",
            "-show_format", "-show_streams", "-show_chapters",
        ])
}

/// Rip one title straight to Matroska.
///
/// `-c copy` throughout: the streams are lifted off the disc untouched, so this
/// is a transfer rather than a transcode and the encoder settings still apply
/// later. Matroska because MP4 cannot hold VobSub, and the bitmaps have to
/// survive long enough to be recognised.
pub fn rip_command(device: &Path, title: u32, dest: &Path) -> Command {
    Command::new("ffmpeg")
        .env("DVDCSS_METHOD", DVDCSS_METHOD)
        .args([
            "-nostdin", "-y",
            // progress on stdout in a form that is parseable rather than pretty
            "-progress", "pipe:1", "-v", "error",
            "-f", "dvdvideo",
            // Accurate chapter marks are worth a second read: they are what
            // play-all decomposition matches on.
            "-preindex", "true",
            "-title", &title.to_string(),
            "-i",
        ])
        .path(device)
        .args(["-map", "0", "-c", "copy"])
        .path(dest)
}

/// Turn one probed title into a [`DiscTitle`].
///
/// Returns `None` for the menu stubs and zero-length entries that DVD title
/// numbering is littered with.
pub fn parse_title(json: &str, title: u32) -> Option<DiscTitle> {
    let info = parse_probe(json).ok()?;
    if info.duration < MIN_TITLE_MS {
        return None;
    }
    // A title with no video is a navigation artefact, not content.
    if info.tracks_of(TrackKind::Video).is_empty() {
        return None;
    }
    Some(DiscTitle {
        id: title,
        duration: info.duration,
        chapter_count: info.chapters.len(),
        chapters: info.chapter_durations(),
        size_bytes: 0,
        output_name: format!("title_t{title:02}.mkv"),
        tracks: info.tracks,
    })
}

/// Optical drives, from the kernel rather than from a helper program.
pub fn optical_devices() -> Vec<PathBuf> {
    let mut out = Vec::new();
    // /sys/block/sr* is the authoritative list; /dev/sr* only exists once udev
    // has caught up, but is what we actually open.
    if let Ok(entries) = std::fs::read_dir("/sys/block") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("sr") {
                out.push(PathBuf::from("/dev").join(&name));
            }
        }
    }
    out.sort();
    out
}

fn model_of(device: &Path) -> String {
    let name = device.file_name().unwrap_or_default().to_string_lossy();
    let read = |what: &str| {
        std::fs::read_to_string(format!("/sys/block/{name}/device/{what}"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let (vendor, model) = (read("vendor"), read("model"));
    let full = format!("{vendor} {model}").trim().to_string();
    if full.is_empty() { "Optical drive".into() } else { full }
}

/// Signs that a scan cannot be trusted.
///
/// This matters more than it looks. When libdvdcss cannot decrypt a VTS, the
/// scan does not fail - it returns the titles it *could* read, which on a TV
/// box set means the extras and none of the episodes. That is indistinguishable
/// from a disc that genuinely has no episodes on it, and it is exactly the kind
/// of silent, plausible wrong answer this codebase exists to avoid. So the
/// stderr that would otherwise be discarded is inspected, and any sign of
/// trouble is carried out to the caller.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanHealth {
    /// VTS files libdvdcss could not decrypt.
    pub css_failures: Vec<String>,
    /// Sectors the drive could not read: scratches, or deliberate obfuscation.
    pub read_errors: usize,
    /// Titles the disc's own table declares.
    pub declared: usize,
    /// Titles that actually decoded.
    pub decoded: usize,
}

impl ScanHealth {
    pub fn is_trustworthy(&self) -> bool {
        self.css_failures.is_empty() && self.read_errors == 0
    }

    /// What to tell the user when it is not.
    pub fn complaint(&self) -> String {
        let mut parts = Vec::new();
        if !self.css_failures.is_empty() {
            parts.push(format!(
                "libdvdcss could not decrypt {} of the disc ({}){}",
                if self.css_failures.len() == 1 { "a part" } else { "parts" },
                self.css_failures.join(", "),
                " - titles in those parts are missing from this scan"
            ));
        }
        if self.read_errors > 0 {
            parts.push(format!("{} unreadable sectors", self.read_errors));
        }
        parts.join("; ")
    }
}

/// Look for trouble in a probe's stderr.
///
/// A title that is simply empty also fails, so "Invalid data" alone means
/// nothing; these two messages are specific to decryption and to the disc
/// itself being unreadable.
pub fn inspect_stderr(stderr: &str, health: &mut ScanHealth) {
    for line in stderr.lines() {
        if let Some(rest) = line.split("Error cracking CSS key for ").nth(1) {
            let file = rest.split_whitespace().next().unwrap_or(rest).to_string();
            if !health.css_failures.contains(&file) {
                health.css_failures.push(file);
            }
        }
        let l = line.to_ascii_lowercase();
        if l.contains("cannot read from device")
            || l.contains("error reading nav packet")
            || l.contains("read error")
        {
            health.read_errors += 1;
        }
    }
}

/// A [`Ripper`](super::Ripper) that needs no proprietary software.
pub struct DvdVideo<'a> {
    pub runner: &'a dyn Runner,
    /// Highest title number to look at. Lower is faster; the default is the
    /// format maximum.
    pub max_title: u32,
}

impl<'a> DvdVideo<'a> {
    pub fn new(runner: &'a dyn Runner) -> Self {
        DvdVideo {
            runner,
            max_title: MAX_TITLE,
        }
    }
}


impl DvdVideo<'_> {
    /// Scan, and say whether the result can be trusted.
    ///
    /// Aborts as soon as decryption fails rather than pressing on: continuing
    /// would spend several minutes assembling a title list that is missing
    /// whole video title sets, and the caller almost certainly wants to hand
    /// the disc to MakeMKV instead.
    pub fn scan_checked(&self, drive: &Drive) -> Result<(DiscScan, ScanHealth)> {
        let device = PathBuf::from(&drive.device);
        let mut health = ScanHealth::default();

        // Ask the disc how many titles it has. Guessing the end of the range is
        // not safe: a disc can have content at 2-19 and again at 39-58, so any
        // stop-after-N-empties rule quits in the hole and returns a season with
        // no episodes in it - which looks exactly like a disc that has none.
        let numbers: Vec<u32> = match super::iso::device_reader(&device)
            .and_then(|mut r| super::iso::title_table(&mut r))
        {
            Ok(table) => table.iter().map(|t| t.number).collect(),
            Err(_) => (1..=self.max_title).collect(),
        };
        health.declared = numbers.len();

        let mut titles = Vec::new();
        for n in numbers {
            let out = self.runner.run(&probe_command(&device, n))?;
            inspect_stderr(&out.stderr, &mut health);
            if !health.is_trustworthy() {
                // No point spending minutes on a list we already know is short.
                return Ok((
                    DiscScan {
                        drive: drive.clone(),
                        label: drive.disc_label.clone().unwrap_or_default(),
                        titles,
                    },
                    health,
                ));
            }
            if let Some(t) = out.ok().then(|| parse_title(&out.stdout, n)).flatten() {
                titles.push(t);
            }
        }
        health.decoded = titles.len();

        if titles.is_empty() {
            return Err(Error(format!(
                "no titles on {} - is there a DVD in the drive?",
                drive.device
            )));
        }
        Ok((
            DiscScan {
                drive: drive.clone(),
                label: drive.disc_label.clone().unwrap_or_default(),
                titles,
            },
            health,
        ))
    }
}

impl super::Ripper for DvdVideo<'_> {
    fn drives(&self) -> Result<Vec<Drive>> {
        Ok(optical_devices()
            .into_iter()
            .map(|device| Drive {
                id: device.to_string_lossy().into_owned(),
                name: model_of(&device),
                disc_label: read_volume_label(&device),
                device: device.to_string_lossy().into_owned(),
            })
            .collect())
    }

    fn scan(&self, drive: &Drive) -> Result<DiscScan> {
        self.scan_checked(drive).map(|(s, _)| s)
    }

    fn rip(
        &self,
        drive: &Drive,
        titles: &[DiscTitle],
        dest: &Path,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<Vec<PathBuf>> {
        std::fs::create_dir_all(dest).map_err(|e| Error(format!("{}: {e}", dest.display())))?;
        let device = PathBuf::from(&drive.device);
        let mut written = Vec::new();

        for (n, title) in titles.iter().enumerate() {
            let out_path = dest.join(&title.output_name);
            let base = n as f32 / titles.len() as f32;
            let span = 1.0 / titles.len() as f32;
            let label = format!("title {}", title.id);

            let cmd = rip_command(&device, title.id, &out_path);
            let out = self.runner.stream(&cmd, &mut |line| {
                if let Some(us) = line.strip_prefix("out_time_us=")
                    && let Ok(us) = us.trim().parse::<u64>() {
                        let done = (us / 1000) as f32 / title.duration.max(1) as f32;
                        progress(base + done.clamp(0.0, 1.0) * span, Some(&label));
                    }
            })?;

            if !out.ok() {
                return Err(Error(format!(
                    "title {}: {}",
                    title.id,
                    out.last_error()
                )));
            }
            // ffmpeg can exit zero having written nothing readable
            if !out_path.exists() {
                return Err(Error(format!(
                    "title {} produced no file at {}",
                    title.id,
                    out_path.display()
                )));
            }
            written.push(out_path);
        }
        progress(1.0, None);
        Ok(written)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::host::FakeRunner;
    use crate::rip::Ripper;

    fn pvd(label: &str) -> Vec<u8> {
        let mut v = vec![0u8; 2048];
        v[0] = 1;
        v[1..6].copy_from_slice(b"CD001");
        let bytes = label.as_bytes();
        v[40..40 + bytes.len()].copy_from_slice(bytes);
        for b in v.iter_mut().take(72).skip(40 + bytes.len()) {
            *b = b' ';
        }
        v
    }

    #[test]
    fn the_volume_label_comes_out_of_the_descriptor() {
        // read straight off the real disc, this is what it looks like
        assert_eq!(
            volume_label(&pvd("PARKS_AND_RECREATION")).as_deref(),
            Some("PARKS_AND_RECREATION")
        );
    }

    #[test]
    fn padding_is_stripped_but_inner_spaces_are_kept() {
        assert_eq!(volume_label(&pvd("THE BIG LEBOWSKI")).as_deref(), Some("THE BIG LEBOWSKI"));
    }

    #[test]
    fn a_blank_or_wrong_descriptor_is_none_rather_than_an_empty_name() {
        assert_eq!(volume_label(&pvd("")), None);
        let mut bad = pvd("X");
        bad[1] = b'X'; // magic no longer CD001
        assert_eq!(volume_label(&bad), None);
        assert_eq!(volume_label(&[0u8; 10]), None);
    }

    pub const EPISODE: &str = r#"{
      "streams":[
        {"codec_type":"video","codec_name":"mpeg2video","width":720,"height":480,
         "sample_aspect_ratio":"32:27","avg_frame_rate":"30000/1001"},
        {"codec_type":"audio","codec_name":"ac3","channels":6,"tags":{"language":"eng"}},
        {"codec_type":"subtitle","codec_name":"dvd_subtitle","tags":{"language":"eng"}},
        {"codec_type":"subtitle","codec_name":"dvd_subtitle","tags":{"language":"spa"}}
      ],
      "chapters":[
        {"start_time":"0.000000","end_time":"223.170000"},
        {"start_time":"223.170000","end_time":"301.700000"}
      ],
      "format":{"duration":"1289.000000"}
    }"#;

    /// A menu stub: the demuxer answers, but there is nothing there.
    const STUB: &str = r#"{"streams":[],"chapters":[],"format":{"duration":"0.000000"}}"#;

    #[test]
    fn a_title_carries_its_chapter_durations_before_anything_is_ripped() {
        // this is the whole point: MakeMKV's scan gives a chapter *count*, and
        // play-all decomposition needs the durations
        let t = parse_title(EPISODE, 9).unwrap();
        assert_eq!(t.id, 9);
        assert_eq!(t.duration, 1_289_000);
        assert_eq!(t.chapters, vec![223_170, 78_530]);
        assert_eq!(t.chapter_count, 2);
        assert_eq!(t.output_name, "title_t09.mkv");
    }

    #[test]
    fn tracks_and_languages_survive_the_probe() {
        let t = parse_title(EPISODE, 9).unwrap();
        let subs: Vec<&str> = t
            .tracks
            .iter()
            .filter(|x| x.kind == TrackKind::Subtitle)
            .map(|x| x.language.as_str())
            .collect();
        assert_eq!(subs, vec!["eng", "spa"]);
        assert!(t.tracks.iter().any(|x| x.kind == TrackKind::Audio && x.channels == 6));
    }

    #[test]
    fn menu_stubs_are_skipped() {
        // DVD title numbering is littered with these
        assert!(parse_title(STUB, 1).is_none());
        assert!(parse_title("not json", 1).is_none());
    }

    #[test]
    fn a_title_with_no_video_is_not_content() {
        let audio_only = r#"{"streams":[{"codec_type":"audio","codec_name":"ac3"}],
                             "chapters":[],"format":{"duration":"600.0"}}"#;
        assert!(parse_title(audio_only, 3).is_none());
    }

    #[test]
    fn a_long_gap_in_the_numbering_does_not_truncate_the_scan() {
        // The real disc: content at 2, then nothing until 41. Any
        // stop-after-N-empties rule returns a season with no episodes in it.
        let mut r = FakeRunner::new().on("-title 2 ", EPISODE);
        for n in 41..=47 {
            r = r.on(&format!("-title {n} "), EPISODE);
        }
        let d = DvdVideo { runner: &r, max_title: 58 };
        let drive = Drive {
            id: "/dev/sr0".into(),
            device: "/dev/sr0".into(),
            name: "drive".into(),
            disc_label: Some("DISC".into()),
        };
        let scan = d.scan(&drive).unwrap();
        assert_eq!(
            scan.titles.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![2, 41, 42, 43, 44, 45, 46, 47]
        );
    }

    #[test]
    fn an_empty_drive_is_an_error_not_an_empty_scan() {
        let r = FakeRunner::new();
        let d = DvdVideo { runner: &r, max_title: 12 };
        let drive = Drive {
            id: "/dev/sr0".into(),
            device: "/dev/sr0".into(),
            name: "drive".into(),
            disc_label: None,
        };
        assert!(d.scan(&drive).unwrap_err().0.contains("no titles"));
    }

    #[test]
    fn the_rip_copies_streams_rather_than_re_encoding() {
        let c = rip_command(Path::new("/dev/sr0"), 9, Path::new("/rip/t09.mkv"));
        assert_eq!(c.value_of("-f"), Some("dvdvideo"));
        assert_eq!(c.value_of("-title"), Some("9"));
        assert_eq!(c.value_of("-c"), Some("copy"));
        // accurate chapter marks are what decomposition matches on
        assert_eq!(c.value_of("-preindex"), Some("true"));
        assert!(c.has("-nostdin"));
        assert_eq!(c.args.last().unwrap(), "/rip/t09.mkv");
    }

    #[test]
    fn both_commands_ask_for_the_working_key_exchange_method() {
        // the default method silently returns a disc with its episodes missing
        assert_eq!(
            probe_command(Path::new("/dev/sr0"), 9).env,
            vec![("DVDCSS_METHOD".to_string(), "key".to_string())]
        );
        assert_eq!(
            rip_command(Path::new("/dev/sr0"), 9, Path::new("/x.mkv")).env,
            vec![("DVDCSS_METHOD".to_string(), "key".to_string())]
        );
    }

    #[test]
    fn matroska_is_the_rip_container_because_mp4_cannot_hold_vobsub() {
        let c = rip_command(Path::new("/dev/sr0"), 1, Path::new("/rip/t01.mkv"));
        assert!(c.args.last().unwrap().ends_with(".mkv"));
    }

    #[test]
    fn ripping_reports_progress_from_ffmpeg_and_ends_at_one() {
        struct Progress;
        impl Runner for Progress {
            fn run(&self, _: &Command) -> Result<crate::host::Output> {
                Ok(crate::host::Output::default())
            }
            fn stream(
                &self,
                _: &Command,
                on_line: &mut dyn FnMut(&str),
            ) -> Result<crate::host::Output> {
                for l in ["out_time_us=0", "out_time_us=644500000", "out_time_us=1289000000"] {
                    on_line(l);
                }
                Ok(crate::host::Output::default())
            }
        }
        let d = DvdVideo { runner: &Progress, max_title: 1 };
        let drive = Drive {
            id: "/dev/sr0".into(),
            device: "/dev/sr0".into(),
            name: "d".into(),
            disc_label: Some("X".into()),
        };
        let title = parse_title(EPISODE, 9).unwrap();
        let mut seen = Vec::new();
        // the output file will not exist, so this fails - but only after
        // reporting, which is what we are checking
        let dir = std::env::temp_dir().join("riplika-dvd-test");
        let _ = d.rip(&drive, &[title], &dir, &mut |p, _| seen.push(p));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen.first().is_some_and(|p| *p == 0.0), "{seen:?}");
        assert!(seen.iter().any(|p| (*p - 0.5).abs() < 0.01), "{seen:?}");
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
    }
}

#[cfg(test)]
mod health_tests {
    use super::*;
    use crate::host::FakeRunner;
    use crate::rip::Ripper;

    const CSS_FAILURE: &str = "\
[dvdvideo @ 0x1] libdvdnav: Error cracking CSS key for /VIDEO_TS/VTS_06_1.VOB (0x000651ea)
[dvdvideo @ 0x1] libdvdnav: Error cracking CSS key for /VIDEO_TS/VTS_07_1.VOB (0x00065214)
/dev/sr0: Invalid data found when processing input";

    #[test]
    fn a_decryption_failure_is_noticed_rather_than_silently_shortening_the_list() {
        // this is the dangerous case: the scan "succeeds" with the extras and
        // none of the episodes, which looks like a disc that simply has none
        let mut h = ScanHealth::default();
        inspect_stderr(CSS_FAILURE, &mut h);
        assert!(!h.is_trustworthy());
        assert_eq!(h.css_failures.len(), 2);
        assert!(h.complaint().contains("VTS_06_1.VOB"), "{}", h.complaint());
    }

    #[test]
    fn the_same_vts_failing_twice_is_reported_once() {
        let mut h = ScanHealth::default();
        inspect_stderr(CSS_FAILURE, &mut h);
        inspect_stderr(CSS_FAILURE, &mut h);
        assert_eq!(h.css_failures.len(), 2);
    }

    #[test]
    fn an_ordinary_empty_title_is_not_treated_as_damage() {
        // every disc has menu stubs that fail this way; treating them as
        // trouble would send every scan to the fallback
        let mut h = ScanHealth::default();
        inspect_stderr(
            "[dvdvideo @ 0x1] Title 20, PGC 17 looks empty (may consist of padding cells)\n\
             /dev/sr0: Invalid data found when processing input",
            &mut h,
        );
        assert!(h.is_trustworthy());
    }

    #[test]
    fn unreadable_sectors_are_counted() {
        let mut h = ScanHealth::default();
        inspect_stderr("libdvdread: Cannot read from device\nsomething read error here", &mut h);
        assert_eq!(h.read_errors, 2);
        assert!(!h.is_trustworthy());
        assert!(h.complaint().contains("unreadable"), "{}", h.complaint());
    }

    #[test]
    fn a_clean_scan_is_trustworthy_and_says_nothing() {
        let h = ScanHealth::default();
        assert!(h.is_trustworthy());
        assert_eq!(h.complaint(), "");
    }

    fn drive() -> Drive {
        Drive {
            id: "/dev/sr0".into(),
            device: "/dev/sr0".into(),
            name: "drive".into(),
            disc_label: Some("DISC".into()),
        }
    }

    #[test]
    fn scanning_stops_as_soon_as_decryption_fails() {
        // pressing on would spend minutes building a list we know is short
        let r = FakeRunner::new().fail("ffprobe", CSS_FAILURE);
        let d = DvdVideo { runner: &r, max_title: 58 };
        let (_, health) = d.scan_checked(&drive()).unwrap();
        assert!(!health.is_trustworthy());
        assert_eq!(r.calls().len(), 1, "should have abandoned after the first probe");
    }

    #[test]
    fn the_plain_scan_still_succeeds_on_a_healthy_disc() {
        let r = FakeRunner::new().on("-title 1 ", super::tests::EPISODE);
        let d = DvdVideo { runner: &r, max_title: 3 };
        assert_eq!(d.scan(&drive()).unwrap().titles.len(), 1);
    }
}
