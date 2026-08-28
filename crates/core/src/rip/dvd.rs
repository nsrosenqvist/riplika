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

/// libdvdcss decryption methods, in the order worth trying.
///
/// These are not alternatives to each other so much as answers to different
/// problems, which is why trying them in turn recovers discs that any single
/// one fails on:
///
/// - `key` asks the drive to do the CSS handshake. Fast and reliable, but an
///   RPC-2 drive refuses it when the disc's region does not match the one
///   region the drive is set to.
/// - `disc` cracks the disc key without the drive's help, so region does not
///   come into it - this is the one that reads a Region 1 disc in a drive set
///   to Region 2.
/// - `title` cracks each title key from the encrypted data itself. Slowest, and
///   the last resort.
///
/// The default is a partial version of this that starts at `title`, and on the
/// Parks and Recreation disc it failed for exactly the video title sets holding
/// the episodes - producing a scan with the extras and nothing else, which is
/// indistinguishable from a disc that has no episodes on it.
pub const CSS_METHODS: &[&str] = &["key", "disc", "title"];

/// The method to use when nothing has told us otherwise.
pub const DVDCSS_METHOD: &str = "key";

/// Probe one title. Everything we need is in ffprobe's normal JSON.
pub fn probe_command_with(device: &Path, title: u32, method: &str) -> Command {
    Command::new("ffprobe")
        .env("DVDCSS_METHOD", method)
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

pub fn probe_command(device: &Path, title: u32) -> Command {
    probe_command_with(device, title, DVDCSS_METHOD)
}

/// Rip one title straight to Matroska.
///
/// `-c copy` throughout: the streams are lifted off the disc untouched, so this
/// is a transfer rather than a transcode and the encoder settings still apply
/// later. Matroska because MP4 cannot hold VobSub, and the bitmaps have to
/// survive long enough to be recognised.
pub fn rip_command_with(
    device: &Path,
    title: u32,
    dest: &Path,
    method: &str,
    chapters: Option<(u32, u32)>,
) -> Command {
    let mut c = Command::new("ffmpeg")
        .env("DVDCSS_METHOD", method)
        .args([
            "-nostdin", "-y",
            // progress on stdout in a form that is parseable rather than pretty
            "-progress", "pipe:1", "-v", "error",
            "-f", "dvdvideo",
            // Accurate chapter marks are worth a second read: they are what
            // play-all decomposition matches on.
            "-preindex", "true",
            "-title", &title.to_string(),
        ]);
    // Reading a chapter range is how a title with one damaged chapter is
    // salvaged: the rest of it is still perfectly good.
    if let Some((first, last)) = chapters {
        c = c.args(["-chapter_start", &first.to_string()]);
        c = c.args(["-chapter_end", &last.to_string()]);
    }
    c.arg("-i")
        .path(device)
        .args(["-map", "0", "-c", "copy"])
        .path(dest)
}

pub fn rip_command(device: &Path, title: u32, dest: &Path) -> Command {
    rip_command_with(device, title, dest, DVDCSS_METHOD, None)
}

/// How far below the scanned duration a rip may fall and still be whole.
///
/// A read error does not usually make ffmpeg fail - it makes it stop early and
/// exit cleanly, leaving a file that plays and is simply missing its ending.
/// Nothing downstream would notice, so the length is checked against what the
/// scan said the title was.
pub const SHORT_RIP_TOLERANCE: f32 = 0.02;

/// Is this rip suspiciously short?
pub fn is_short(expected: Millis, actual: Millis) -> bool {
    if expected == 0 {
        return false;
    }
    let missing = expected.saturating_sub(actual) as f32 / expected as f32;
    missing > SHORT_RIP_TOLERANCE
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
    /// The libdvdcss method this attempt used.
    pub method: String,
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

/// What to suggest when a title cannot be read at all.
///
/// Deliberately a suggestion rather than an automatic escalation. A sector
/// rescue can take hours and wants the disc cleaned first, so it is a decision
/// rather than something to start on someone's behalf - but they should not
/// have to go and find out that it exists.
pub fn rescue_advice(device: &Path, title: u32) -> String {
    let vts = super::iso::device_reader(device)
        .and_then(|mut r| super::iso::title_table(&mut r))
        .ok()
        .and_then(|table| table.iter().find(|t| t.number == title).map(|t| t.vts));
    match vts {
        Some(vts) => format!(
            "Try recovering it: riplika rescue {} disc.iso --vts {vts}",
            device.display()
        ),
        None => format!(
            "Try recovering it: riplika rescue {} disc.iso",
            device.display()
        ),
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
    pub fn scan_checked(
        &self,
        drive: &Drive,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<(DiscScan, ScanHealth)> {
        // Try each decryption method in turn. They answer different problems -
        // a region-locked drive refuses the handshake, a cracking-resistant
        // disc defeats the brute force - so a disc that one cannot read is
        // often trivial for the next.
        let mut last: Option<(DiscScan, ScanHealth)> = None;
        for method in CSS_METHODS {
            let attempt = self.scan_with(drive, method, progress)?;
            if attempt.1.is_trustworthy() {
                return Ok(attempt);
            }
            if last.as_ref().is_none_or(|(s, _)| attempt.0.titles.len() > s.titles.len()) {
                last = Some(attempt);
            }
        }
        // Nothing worked cleanly; hand back the best of a bad set, with the
        // complaint attached so the caller can fall back or say so.
        last.ok_or_else(|| Error("no decryption method could read the disc".into()))
    }

    /// One scan attempt with one decryption method.
    pub fn scan_with(
        &self,
        drive: &Drive,
        method: &str,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<(DiscScan, ScanHealth)> {
        let device = PathBuf::from(&drive.device);
        let mut health = ScanHealth {
            method: method.to_string(),
            ..ScanHealth::default()
        };

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
        let total = health.declared.max(1) as f32;
        for (done, n) in numbers.into_iter().enumerate() {
            // Each probe is a real fraction of the work, and there are up to 99
            // of them, so this is genuine progress rather than a guess.
            progress(
                done as f32 / total,
                Some(&format!("title {n} of {}", health.declared)),
            );
            let out = self.runner.run(&probe_command_with(&device, n, method))?;
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
        progress(1.0, None);

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

impl DvdVideo<'_> {
    /// How long a ripped file actually turned out to be.
    fn measure(&self, path: &Path) -> Millis {
        self.runner
            .run(&crate::media::probe_command(path))
            .ok()
            .and_then(|o| crate::media::parse_probe(&o.stdout).ok())
            .map(|i| i.duration)
            .unwrap_or(0)
    }

    /// Read one title, working around what can be worked around.
    ///
    /// Three things go wrong on a DVD and each has a different answer. The read
    /// can fail outright, which is often transient and worth simply repeating.
    /// The decryption can fail, which another method may not. And the read can
    /// stop early on a damaged sector, which is the dangerous one: ffmpeg exits
    /// cleanly and leaves a file that plays and is merely missing its ending.
    fn rip_one(
        &self,
        device: &Path,
        title: &DiscTitle,
        dest: &Path,
        report: &mut dyn FnMut(f32, &str),
    ) -> Result<()> {
        let mut trouble = Vec::new();

        for (attempt, method) in CSS_METHODS.iter().enumerate() {
            if attempt > 0 {
                report(0.0, &format!("retrying title {} with method {method}", title.id));
            }
            let cmd = rip_command_with(device, title.id, dest, method, None);
            let out = self.runner.stream(&cmd, &mut |line| {
                if let Some(us) = line.strip_prefix("out_time_us=")
                    && let Ok(us) = us.trim().parse::<u64>()
                {
                    let done = (us / 1000) as f32 / title.duration.max(1) as f32;
                    report(done.clamp(0.0, 1.0), &format!("title {}", title.id));
                }
            })?;

            if !out.ok() || !dest.exists() {
                trouble.push(format!("{method}: {}", out.last_error()));
                continue;
            }
            let got = self.measure(dest);
            if !is_short(title.duration, got) {
                return Ok(());
            }
            // A short file is not a failure ffmpeg reported, so say so plainly
            // rather than accepting it.
            trouble.push(format!(
                "{method}: stopped at {}s of {}s",
                got / 1000,
                title.duration / 1000
            ));
        }

        // Every method stopped early. Take the title a chapter at a time: one
        // damaged chapter should cost that chapter, not the whole episode.
        report(0.0, &format!("title {} is damaged; salvaging by chapter", title.id));
        let (parts, lost) = self.salvage(device, title, dest, report)?;
        if parts.is_empty() {
            return Err(Error(format!(
                "unreadable ({}). {}",
                trouble.join("; "),
                rescue_advice(device, title.id)
            )));
        }
        self.concatenate(&parts, dest)?;
        for p in &parts {
            let _ = std::fs::remove_file(p);
        }
        if !lost.is_empty() {
            report(
                1.0,
                &format!(
                    "title {} recovered without chapter{} {}",
                    title.id,
                    if lost.len() == 1 { "" } else { "s" },
                    lost.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
                ),
            );
        }
        Ok(())
    }

    /// Read a title one chapter at a time, keeping whatever survives.
    fn salvage(
        &self,
        device: &Path,
        title: &DiscTitle,
        dest: &Path,
        report: &mut dyn FnMut(f32, &str),
    ) -> Result<(Vec<PathBuf>, Vec<u32>)> {
        let count = title.chapter_count.max(1) as u32;
        let mut parts = Vec::new();
        let mut lost = Vec::new();
        for chapter in 1..=count {
            let part = dest.with_extension(format!("ch{chapter:02}.mkv"));
            let cmd = rip_command_with(
                device,
                title.id,
                &part,
                DVDCSS_METHOD,
                Some((chapter, chapter)),
            );
            let out = self.runner.run(&cmd)?;
            if out.ok() && part.exists() && self.measure(&part) > 0 {
                parts.push(part);
            } else {
                let _ = std::fs::remove_file(&part);
                lost.push(chapter);
            }
            report(chapter as f32 / count as f32, &format!("title {} chapter {chapter}", title.id));
        }
        Ok((parts, lost))
    }

    /// Join salvaged chapters back into one file.
    fn concatenate(&self, parts: &[PathBuf], dest: &Path) -> Result<()> {
        let list = dest.with_extension("parts.txt");
        let body: String = parts
            .iter()
            .map(|p| format!("file '{}'\n", p.display()))
            .collect();
        std::fs::write(&list, body).map_err(|e| Error(format!("{}: {e}", list.display())))?;
        let cmd = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y", "-f", "concat", "-safe", "0", "-i"])
            .path(&list)
            .args(["-map", "0", "-c", "copy"])
            .path(dest);
        let result = self.runner.require(&cmd);
        let _ = std::fs::remove_file(&list);
        result.map(|_| ())
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

    fn scan(
        &self,
        drive: &Drive,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<DiscScan> {
        let (scan, health) = self.scan_checked(drive, progress)?;
        // Used on its own there is no fallback to hand the disc to, so an
        // untrustworthy scan has to be an error. Returning it would hand back a
        // disc with its episodes missing and nothing to say they were ever
        // there - which is the failure this health check exists to prevent, and
        // it was only being applied when a fallback happened to be configured.
        if !health.is_trustworthy() {
            return Err(Error(format!(
                "{}. MakeMKV works around this - use --reader auto.",
                health.complaint()
            )));
        }
        Ok(scan)
    }

    fn rip(
        &self,
        drive: &Drive,
        titles: &[DiscTitle],
        dest: &Path,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<super::RipOutcome> {
        std::fs::create_dir_all(dest).map_err(|e| Error(format!("{}: {e}", dest.display())))?;
        let device = PathBuf::from(&drive.device);
        let mut outcome = super::RipOutcome::default();

        // Weighted by running time, not by title count. A disc holds a
        // three-hour play-all beside a fifteen-second stub, so counting titles
        // makes the bar leap and stall and any estimate from it useless.
        let total: f64 = titles.iter().map(|t| t.duration.max(1) as f64).sum();
        let mut done: f64 = 0.0;

        for title in titles {
            let out_path = dest.join(&title.output_name);
            let base = (done / total) as f32;
            let span = (title.duration.max(1) as f64 / total) as f32;
            done += title.duration.max(1) as f64;

            // Stopping is not damage. Without this, cancelling a rip records
            // every remaining title as unreadable and keeps going through them.
            if self.runner.cancelled() {
                return Err(Error("cancelled".into()));
            }
            let mut report = |p: f32, m: &str| progress(base + p * span, Some(m));
            match self.rip_one(&device, title, &out_path, &mut report) {
                Ok(()) => outcome.written.push(out_path),
                // A disc is mostly menus and transitions, and one of them being
                // unreadable is not a reason to abandon the episodes. The
                // failure is carried out so the caller can say so.
                Err(_) if self.runner.cancelled() => {
                    let _ = std::fs::remove_file(&out_path);
                    return Err(Error("cancelled".into()));
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&out_path);
                    outcome.failed.push((title.id, e.0));
                }
            }
        }
        progress(1.0, None);
        Ok(outcome)
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
        let scan = d.scan(&drive(), &mut |_, _| {}).unwrap();
        assert_eq!(
            scan.titles.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![2, 41, 42, 43, 44, 45, 46, 47]
        );
    }

    #[test]
    fn an_empty_drive_is_an_error_not_an_empty_scan() {
        let r = FakeRunner::new();
        let d = DvdVideo { runner: &r, max_title: 12 };
        assert!(d.scan(&drive(), &mut |_, _| {}).unwrap_err().0.contains("no titles"));
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

    /// A device that cannot exist, so the ISO title table read falls back and
    /// the test does not quietly depend on what is in the real drive.
    fn drive() -> Drive {
        Drive {
            id: "/dev/riplika-no-such-device".into(),
            device: "/dev/riplika-no-such-device".into(),
            name: "d".into(),
            disc_label: Some("X".into()),
        }
    }

    #[test]
    fn ripping_reports_progress_as_it_goes() {
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
        let title = parse_title(EPISODE, 9).unwrap();
        let mut seen = Vec::new();
        let dir = std::env::temp_dir().join("riplika-dvd-progress-test");
        // no file is ever produced, so this ends in failure - but only after
        // reporting, which is what is under test
        let _ = d.rip(&drive(), &[title], &dir, &mut |p, _| seen.push(p));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(seen.contains(&0.0));
        assert!(seen.iter().any(|p| (*p - 0.5).abs() < 0.01), "{seen:?}");
    }

    #[test]
    fn progress_may_go_backwards_when_a_read_is_retried() {
        // it is honestly re-reading, and saying so beats a bar that pretends
        struct Failing;
        impl Runner for Failing {
            fn run(&self, _: &Command) -> Result<crate::host::Output> {
                Ok(crate::host::Output::default())
            }
            fn stream(&self, _: &Command, on: &mut dyn FnMut(&str)) -> Result<crate::host::Output> {
                on("out_time_us=644500000");
                Ok(crate::host::Output::default())
            }
        }
        let d = DvdVideo { runner: &Failing, max_title: 1 };
        let title = parse_title(EPISODE, 9).unwrap();
        let mut messages = Vec::new();
        let dir = std::env::temp_dir().join("riplika-dvd-retry-test");
        let _ = d.rip(&drive(), &[title], &dir, &mut |_, m| {
            if let Some(m) = m {
                messages.push(m.to_string());
            }
        });
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            messages.iter().any(|m| m.contains("retrying")),
            "should have said it was retrying: {messages:?}"
        );
        assert!(
            messages.iter().any(|m| m.contains("salvaging by chapter")),
            "should have fallen back to chapter salvage: {messages:?}"
        );
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
            id: "/dev/riplika-no-such-device".into(),
            device: "/dev/riplika-no-such-device".into(),
            name: "drive".into(),
            disc_label: Some("DISC".into()),
        }
    }

    #[test]
    fn scanning_stops_as_soon_as_decryption_fails() {
        // pressing on would spend minutes building a list we know is short
        let r = FakeRunner::new().fail("ffprobe", CSS_FAILURE);
        let d = DvdVideo { runner: &r, max_title: 58 };
        let (_, health) = d.scan_checked(&drive(), &mut |_, _| {}).unwrap();
        assert!(!health.is_trustworthy());
        // one probe per decryption method: each attempt is abandoned the moment
        // it fails, but a method that fails is not the end - the next one
        // answers a different problem
        assert_eq!(r.calls().len(), CSS_METHODS.len());
    }

    #[test]
    fn the_plain_scan_still_succeeds_on_a_healthy_disc() {
        let r = FakeRunner::new().on("-title 1 ", super::tests::EPISODE);
        let d = DvdVideo { runner: &r, max_title: 3 };
        assert_eq!(d.scan(&drive(), &mut |_, _| {}).unwrap().titles.len(), 1);
    }
}

#[cfg(test)]
mod tolerance_tests {
    use super::*;
    use crate::host::{FakeRunner, Output};
    use crate::rip::Ripper;
    use std::sync::Mutex;

    /// A device that cannot exist, so the ISO title table read falls back and
    /// the test does not quietly depend on what is in the real drive.
    fn drive() -> Drive {
        Drive {
            id: "/dev/riplika-no-such-device".into(),
            device: "/dev/riplika-no-such-device".into(),
            name: "d".into(),
            disc_label: Some("X".into()),
        }
    }

    const REGION_REFUSED: &str =
        "libdvdcss: Could not get disc key\nlibdvdnav: Error cracking CSS key for /VIDEO_TS/VTS_01_1.VOB";

    #[test]
    fn a_method_that_fails_is_not_the_end_of_the_disc() {
        // `key` is refused by an RPC-2 drive on a region mismatch; `disc`
        // cracks without the drive's help and does not care about region
        let r = FakeRunner::new()
            .fail("DVDCSS_METHOD=key", REGION_REFUSED)
            .on("DVDCSS_METHOD=disc", tests::EPISODE);
        let mut d = DvdVideo::new(&r);
        d.max_title = 1;
        let (scan, health) = d.scan_checked(&drive(), &mut |_, _| {}).unwrap();
        assert!(health.is_trustworthy());
        assert_eq!(health.method, "disc");
        assert_eq!(scan.titles.len(), 1);
    }

    #[test]
    fn the_best_attempt_is_returned_when_none_of_them_are_clean() {
        let r = FakeRunner::new().fail("ffprobe", REGION_REFUSED);
        let mut d = DvdVideo::new(&r);
        d.max_title = 1;
        let (_, health) = d.scan_checked(&drive(), &mut |_, _| {}).unwrap();
        assert!(!health.is_trustworthy());
        // and it tried everything before giving up
        assert_eq!(r.calls().len(), CSS_METHODS.len());
    }

    #[test]
    fn a_rip_that_stops_early_is_caught_rather_than_accepted() {
        // The dangerous case: ffmpeg hits a damaged sector, stops, and exits
        // zero. The file plays and is simply missing its ending, so nothing
        // downstream would ever notice.
        assert!(is_short(1_289_000, 600_000));
        assert!(is_short(1_289_000, 1_200_000));
        // but normal rounding is not damage
        assert!(!is_short(1_289_000, 1_289_000));
        assert!(!is_short(1_289_000, 1_280_000));
        // and a title of unknown length cannot be judged
        assert!(!is_short(0, 0));
    }

    #[test]
    fn a_chapter_range_is_asked_for_when_salvaging() {
        let c = rip_command_with(
            Path::new("/dev/sr0"),
            41,
            Path::new("/rip/t41.ch03.mkv"),
            "key",
            Some((3, 3)),
        );
        assert_eq!(c.value_of("-chapter_start"), Some("3"));
        assert_eq!(c.value_of("-chapter_end"), Some("3"));
        assert_eq!(c.value_of("-title"), Some("41"));
        // a whole-title read asks for no range at all
        let whole = rip_command_with(Path::new("/dev/sr0"), 41, Path::new("/x.mkv"), "key", None);
        assert!(!whole.has("-chapter_start"));
    }

    /// A drive that fails whole-title reads but manages individual chapters,
    /// except chapter 3 - one scratch in the middle of an episode.
    struct Scratched {
        calls: Mutex<Vec<Command>>,
        good: Mutex<Vec<PathBuf>>,
    }

    impl Runner for Scratched {
        fn run(&self, cmd: &Command) -> Result<Output> {
            self.calls.lock().unwrap().push(cmd.clone());
            // probing a produced part reports a plausible length
            if cmd.program == "ffprobe" && !cmd.has("dvdvideo") {
                return Ok(Output {
                    status: 0,
                    stdout: r#"{"streams":[],"chapters":[],"format":{"duration":"250.0"}}"#.into(),
                    stderr: String::new(),
                });
            }
            if let Some(range) = cmd.value_of("-chapter_start") {
                let dest = PathBuf::from(cmd.args.last().unwrap());
                if range == "3" {
                    return Ok(Output { status: 1, stdout: String::new(), stderr: "read error".into() });
                }
                std::fs::create_dir_all(dest.parent().unwrap()).ok();
                std::fs::write(&dest, b"part").ok();
                self.good.lock().unwrap().push(dest);
                return Ok(Output::default());
            }
            Ok(Output::default())
        }

        fn stream(&self, cmd: &Command, _: &mut dyn FnMut(&str)) -> Result<Output> {
            // whole-title reads always stop early, producing nothing
            self.calls.lock().unwrap().push(cmd.clone());
            Ok(Output::default())
        }
    }

    #[test]
    fn one_damaged_chapter_costs_that_chapter_not_the_episode() {
        let runner = Scratched {
            calls: Mutex::new(Vec::new()),
            good: Mutex::new(Vec::new()),
        };
        let d = DvdVideo::new(&runner);
        let mut title = parse_title(tests::EPISODE, 41).unwrap();
        title.chapter_count = 5;
        let dir = std::env::temp_dir().join(format!("riplika-salvage-{}", std::process::id()));
        let mut messages = Vec::new();
        let result = d.rip(&drive(), &[title], &dir, &mut |_, m| {
            if let Some(m) = m {
                messages.push(m.to_string());
            }
        });
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_ok(), "{result:?}");
        // four of the five chapters were kept, and it said which one was not
        assert!(
            messages.iter().any(|m| m.contains("without chapter 3")),
            "{messages:?}"
        );
        let calls = runner.calls.lock().unwrap();
        assert!(calls.iter().any(|c| c.has("concat")), "should have rejoined the parts");
    }

    #[test]
    fn a_title_that_cannot_be_read_is_reported_not_written() {
        let r = FakeRunner::new().fail("ffmpeg", "Input/output error");
        let d = DvdVideo::new(&r);
        let title = parse_title(tests::EPISODE, 41).unwrap();
        let dir = std::env::temp_dir().join("riplika-unreadable-test");
        let outcome = d.rip(&drive(), &[title], &dir, &mut |_, _| {}).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(outcome.written.is_empty());
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, 41);
        assert!(outcome.failed[0].1.contains("unreadable"), "{:?}", outcome.failed[0]);
        // and it says what to try, so the failure is actionable
        assert!(outcome.failed[0].1.contains("riplika rescue"));
    }

    #[test]
    fn one_unreadable_title_does_not_abandon_the_rest_of_the_disc() {
        // A disc is mostly menus, transitions and extras. Aborting on the first
        // one that will not read cost a 47-title disc after two titles, and the
        // episodes were still to come.
        struct OneBadTitle;
        impl Runner for OneBadTitle {
            fn run(&self, cmd: &Command) -> Result<crate::host::Output> {
                // probing a produced file: claim the expected length
                if cmd.program == "ffprobe" && !cmd.has("dvdvideo") {
                    return Ok(crate::host::Output {
                        status: 0,
                        stdout: r#"{"streams":[],"chapters":[],"format":{"duration":"1289.0"}}"#.into(),
                        stderr: String::new(),
                    });
                }
                Ok(crate::host::Output::default())
            }
            fn stream(&self, cmd: &Command, _: &mut dyn FnMut(&str)) -> Result<crate::host::Output> {
                let dest = PathBuf::from(cmd.args.last().unwrap());
                if cmd.value_of("-title") == Some("3") {
                    return Ok(crate::host::Output {
                        status: 1,
                        stdout: String::new(),
                        stderr: "Invalid data found".into(),
                    });
                }
                std::fs::create_dir_all(dest.parent().unwrap()).ok();
                std::fs::write(&dest, b"x").ok();
                Ok(crate::host::Output::default())
            }
        }

        let d = DvdVideo::new(&OneBadTitle);
        let titles: Vec<DiscTitle> = [2u32, 3, 4, 5]
            .iter()
            .map(|n| {
                let mut t = parse_title(tests::EPISODE, *n).unwrap();
                t.chapter_count = 1;
                t
            })
            .collect();
        let dir = std::env::temp_dir().join(format!("riplika-partial-{}", std::process::id()));
        let outcome = d.rip(&drive(), &titles, &dir, &mut |_, _| {}).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(outcome.written.len(), 3, "the readable titles must survive");
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(outcome.failed[0].0, 3);
        assert!(!outcome.is_complete());
    }
}

#[cfg(test)]
mod advice_tests {
    use super::*;

    #[test]
    fn a_title_that_cannot_be_read_points_at_the_rescue_command() {
        // the rescue takes hours and wants the disc cleaned first, so it is a
        // suggestion rather than something started on the user's behalf - but
        // they should not have to discover it exists
        let advice = rescue_advice(Path::new("/dev/riplika-no-such-device"), 41);
        assert!(advice.contains("riplika rescue"), "{advice}");
        assert!(advice.contains("/dev/riplika-no-such-device"), "{advice}");
    }
}

#[cfg(test)]
mod scan_progress_tests {
    use super::*;
    use crate::host::FakeRunner;
    use crate::rip::Ripper;

    fn drive() -> Drive {
        Drive {
            id: "/dev/riplika-no-such-device".into(),
            device: "/dev/riplika-no-such-device".into(),
            name: "d".into(),
            disc_label: Some("DISC".into()),
        }
    }

    #[test]
    fn a_scan_reports_where_it_has_got_to() {
        // A scan probes each title in turn and takes minutes on a full disc.
        // Reporting nothing leaves a progress bar that never moves, which reads
        // as a hung application.
        let mut r = FakeRunner::new();
        for n in 1..=8 {
            r = r.on(&format!("-title {n} "), tests::EPISODE);
        }
        let d = DvdVideo { runner: &r, max_title: 8 };
        let mut seen: Vec<f32> = Vec::new();
        d.scan(&drive(), &mut |f, _| seen.push(f)).unwrap();

        assert!(seen.len() > 1, "only {} report(s)", seen.len());
        assert_eq!(seen.first().copied(), Some(0.0));
        assert_eq!(seen.last().copied(), Some(1.0), "must finish at 100%");
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "went backwards: {seen:?}");
    }

    #[test]
    fn progress_names_the_title_being_probed() {
        let r = FakeRunner::new().on("-title 1 ", tests::EPISODE);
        let d = DvdVideo { runner: &r, max_title: 3 };
        let mut messages: Vec<String> = Vec::new();
        d.scan(&drive(), &mut |_, m| {
            if let Some(m) = m {
                messages.push(m.to_string());
            }
        })
        .unwrap();
        assert!(messages.iter().any(|m| m.contains("title 1")), "{messages:?}");
    }
}

#[cfg(test)]
mod cancel_tests {
    use super::*;
    use crate::host::Output;
    use crate::rip::Ripper;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    fn drive() -> Drive {
        Drive {
            id: "/dev/riplika-no-such-device".into(),
            device: "/dev/riplika-no-such-device".into(),
            name: "d".into(),
            disc_label: Some("X".into()),
        }
    }

    /// A runner that is cancelled after the first title, as pressing Cancel does.
    struct CancelledAfterOne {
        stopped: AtomicBool,
        attempts: AtomicUsize,
    }

    impl Runner for CancelledAfterOne {
        fn cancelled(&self) -> bool {
            self.stopped.load(Ordering::SeqCst)
        }
        fn run(&self, _: &Command) -> Result<Output> {
            if self.cancelled() {
                return Err(Error("cancelled".into()));
            }
            Ok(Output::default())
        }
        fn stream(&self, _: &Command, _: &mut dyn FnMut(&str)) -> Result<Output> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            self.stopped.store(true, Ordering::SeqCst);
            Err(Error("cancelled".into()))
        }
    }

    #[test]
    fn cancelling_stops_rather_than_marking_everything_unreadable() {
        // Treating a cancelled command as a bad title recorded every remaining
        // title as damaged and kept going through all of them - twenty
        // warnings and a "nothing could be read from the disc" for a rip the
        // user had simply stopped.
        let runner = CancelledAfterOne {
            stopped: AtomicBool::new(false),
            attempts: AtomicUsize::new(0),
        };
        let d = DvdVideo::new(&runner);
        let titles: Vec<DiscTitle> = (2..=20)
            .map(|n| parse_title(tests::EPISODE, n).unwrap())
            .collect();
        let dir = std::env::temp_dir().join(format!("riplika-cancel-{}", std::process::id()));
        let result = d.rip(&drive(), &titles, &dir, &mut |_, _| {});
        let _ = std::fs::remove_dir_all(&dir);

        let e = result.unwrap_err();
        assert_eq!(e.0, "cancelled", "cancelling is not a disc fault");
        // and it stopped rather than working through the other eighteen
        assert!(runner.attempts.load(Ordering::SeqCst) <= 2, "kept trying after cancel");
    }

    #[test]
    fn a_cancelled_runner_reports_itself_as_such() {
        let cancel = crate::host::Cancel::new();
        let runner = crate::host::RealRunner::new(cancel.clone());
        assert!(!runner.cancelled());
        cancel.cancel();
        assert!(runner.cancelled());
    }
}

#[cfg(test)]
mod health_enforcement_tests {
    use super::*;
    use crate::host::FakeRunner;
    use crate::rip::Ripper;

    const CSS_FAILURE: &str =
        "libdvdnav: Error cracking CSS key for /VIDEO_TS/VTS_06_1.VOB (0x000651ea)";

    fn drive() -> Drive {
        Drive {
            id: "/dev/riplika-no-such-device".into(),
            device: "/dev/riplika-no-such-device".into(),
            name: "d".into(),
            disc_label: Some("DISC".into()),
        }
    }

    #[test]
    fn a_partial_scan_is_refused_rather_than_returned() {
        // Used on its own there is nothing to fall back to, so handing back
        // what could be read means handing back a disc with its episodes
        // missing and nothing to say they existed. On a real disc that came out
        // as twenty-three short extras and no episodes at all.
        let r = FakeRunner::new().fail("ffprobe", CSS_FAILURE);
        let d = DvdVideo { runner: &r, max_title: 8 };
        let e = d.scan(&drive(), &mut |_, _| {}).unwrap_err();
        assert!(e.0.contains("decrypt"), "{}", e.0);
        assert!(e.0.contains("--reader auto"), "must say what to do: {}", e.0);
    }

    #[test]
    fn a_clean_scan_still_comes_back_normally() {
        let r = FakeRunner::new().on("-title 1 ", tests::EPISODE);
        let d = DvdVideo { runner: &r, max_title: 3 };
        assert_eq!(d.scan(&drive(), &mut |_, _| {}).unwrap().titles.len(), 1);
    }
}
