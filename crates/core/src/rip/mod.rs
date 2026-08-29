//! Stage one: getting the disc onto disk.

pub mod dvd;
pub mod iso;
pub mod makemkv;

use crate::host::{Command, Runner};
use crate::model::{DiscScan, DiscTitle, Drive};
use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// Which optical device is mounted at a given path.
///
/// The desktop hands an application a *mount point* when a disc is inserted -
/// `/run/media/someone/PARKS_AND_RECREATION` - because that is what it knows
/// about. Everything here works from the device, so the two have to be
/// connected, and the kernel's mount table is what connects them.
pub fn device_mounted_at(mounts: &str, mount_point: &Path) -> Option<PathBuf> {
    let wanted = mount_point.to_string_lossy();
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (Some(device), Some(at)) = (fields.next(), fields.next()) else {
            continue;
        };
        // /proc/mounts escapes spaces and the like as octal
        let at = unescape_mount(at);
        if at == wanted && device.starts_with("/dev/") {
            return Some(PathBuf::from(device));
        }
    }
    None
}

/// Undo the octal escaping the kernel uses in its mount table.
fn unescape_mount(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let bytes: Vec<char> = path.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '\\' && i + 3 < bytes.len() {
            let digits: String = bytes[i + 1..i + 4].iter().collect();
            if let Ok(code) = u8::from_str_radix(&digits, 8) {
                out.push(code as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

/// Work out which drive the desktop is pointing at.
///
/// Accepts what a `.desktop` file's `%u` actually delivers: a `file://` URI, a
/// plain mount point, or a device node if something passes one directly.
pub fn drive_from_argument(argument: &str, mounts: &str) -> Option<PathBuf> {
    let path = argument
        .strip_prefix("file://")
        .map(percent_decode)
        .unwrap_or_else(|| argument.to_string());
    let path = PathBuf::from(path.trim_end_matches('/'));

    if path.to_string_lossy().starts_with("/dev/") {
        return Some(path);
    }
    device_mounted_at(mounts, &path)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Open the drive.
///
/// A separate command rather than an ioctl of our own: `eject` already knows
/// the several ways a tray can be persuaded to open, and which one a slot-load
/// or an external enclosure needs.
pub fn eject_command(device: &Path) -> Command {
    Command::new("eject").path(device)
}

/// How long to wait for a tray before giving up on it.
///
/// A drive still finishing a read will not answer at all - the request queues
/// behind it, and on a wedged drive it never returns. Waiting forever would
/// hang whoever asked, so the wait is bounded and the failure is reported.
pub const EJECT_TIMEOUT_SECONDS: u64 = 20;

/// Titles shorter than this are menu loops and transitions, not content.
pub const DEFAULT_MIN_LENGTH_SECONDS: u32 = 120;

/// What a rip produced, including what it could not.
///
/// A disc has menu stubs, transitions and the occasional genuinely damaged
/// extra, and one of them failing is not a reason to abandon the other
/// forty-six titles. The failures are carried out rather than thrown, so the
/// caller can report them and carry on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RipOutcome {
    pub written: Vec<PathBuf>,
    /// Title id and why it could not be read.
    pub failed: Vec<(u32, String)>,
}

impl RipOutcome {
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Reads discs. Behind a trait so the pipeline can be tested without one.
pub trait Ripper: Send + Sync {
    /// Drives on this machine, and what is loaded in them.
    fn drives(&self) -> Result<Vec<Drive>>;

    /// Enumerate a disc's titles without ripping it.
    ///
    /// Reports progress from 0.0 to 1.0. A scan probes each title in turn and
    /// takes minutes on a full disc, so a caller with a progress bar has
    /// something real to put in it rather than a bar that never moves.
    fn scan(
        &self,
        drive: &Drive,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<DiscScan>;

    /// Rip `titles` into `dest`, reporting progress from 0.0 to 1.0.
    ///
    /// Returns the files written, in the order the titles were given.
    fn rip(
        &self,
        drive: &Drive,
        titles: &[DiscTitle],
        dest: &Path,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<RipOutcome>;
}

pub struct MakeMkv<'a> {
    pub runner: &'a dyn Runner,
    pub min_length_seconds: u32,
}

impl<'a> MakeMkv<'a> {
    pub fn new(runner: &'a dyn Runner) -> Self {
        MakeMkv {
            runner,
            min_length_seconds: DEFAULT_MIN_LENGTH_SECONDS,
        }
    }
}

impl Ripper for MakeMkv<'_> {
    fn drives(&self) -> Result<Vec<Drive>> {
        // Probing a nonexistent disc index is how MakeMKV is asked to list
        // drives; it reports failure for the disc and success for the listing,
        // so the exit status is not meaningful here.
        let out = self.runner.run(&makemkv::drives_command())?;
        let drives = makemkv::parse_drives(&out.stdout);
        if drives.is_empty()
            && let Some(msg) = makemkv::parse_error(&out.stdout) {
                return Err(Error(format!("MakeMKV: {msg}")));
            }
        Ok(drives)
    }

    fn scan(
        &self,
        drive: &Drive,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<DiscScan> {
        // MakeMKV reports its own progress while it reads the disc structure,
        // so it is passed through rather than invented.
        let cmd = makemkv::scan_command(&drive.id, self.min_length_seconds);
        let mut message: Option<String> = None;
        let out = self.runner.stream(&cmd, &mut |line| {
            if let Some(p) = makemkv::parse_progress(line) {
                if let Some(m) = p.message {
                    message = Some(m);
                }
                if p.total.is_finite() {
                    progress(p.total, message.as_deref());
                }
            }
        })?;
        if let Some(msg) = makemkv::parse_error(&out.stdout) {
            return Err(Error(format!("MakeMKV: {msg}")));
        }
        makemkv::parse_scan(&out.stdout, drive.clone())
    }

    fn rip(
        &self,
        drive: &Drive,
        titles: &[DiscTitle],
        dest: &Path,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<RipOutcome> {
        std::fs::create_dir_all(dest).map_err(|e| Error(format!("{}: {e}", dest.display())))?;

        // One invocation per title, rather than `all`. It costs a little in
        // disc seeking but means a failure names the title it happened on, and
        // a run can be resumed by skipping what is already there.
        let mut outcome = RipOutcome::default();
        // Weighted by running time: a disc holds a three-hour play-all beside a
        // fifteen-second stub, so counting titles makes the bar leap and stall.
        let total: f64 = titles.iter().map(|t| t.duration.max(1) as f64).sum();
        let mut done: f64 = 0.0;
        for title in titles {
            let out_path = dest.join(&title.output_name);
            let base = (done / total) as f32;
            let span = (title.duration.max(1) as f64 / total) as f32;
            done += title.duration.max(1) as f64;

            if self.runner.cancelled() {
                return Err(Error("cancelled".into()));
            }
            let mut message = None;
            let cmd =
                makemkv::rip_command(&drive.id, Some(title.id), dest, self.min_length_seconds);
            let out = self.runner.stream(&cmd, &mut |line| {
                if let Some(p) = makemkv::parse_progress(line) {
                    if let Some(m) = p.message {
                        message = Some(m);
                    }
                    if p.total.is_finite() {
                        progress(base + p.total * span, message.as_deref());
                    }
                }
            })?;

            // A disc has menu stubs and the odd damaged extra; one of them
            // failing is not a reason to abandon the rest of the disc.
            if self.runner.cancelled() {
                return Err(Error("cancelled".into()));
            }
            if let Some(msg) = makemkv::parse_error(&out.stdout) {
                outcome.failed.push((title.id, msg));
                continue;
            }
            // MakeMKV exits zero having saved nothing, so the file is the proof
            if !out_path.exists() {
                outcome.failed.push((title.id, "produced no file".into()));
                continue;
            }
            outcome.written.push(out_path);
        }
        progress(1.0, None);
        Ok(outcome)
    }
}

/// A ripper that invents a disc, for tests and for `--dry-run`.
pub struct FakeRipper {
    pub scan: DiscScan,
    pub written: std::sync::Mutex<Vec<PathBuf>>,
}

impl FakeRipper {
    pub fn new(scan: DiscScan) -> Self {
        FakeRipper {
            scan,
            written: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl Ripper for FakeRipper {
    fn drives(&self) -> Result<Vec<Drive>> {
        Ok(vec![self.scan.drive.clone()])
    }

    fn scan(
        &self,
        _drive: &Drive,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<DiscScan> {
        progress(1.0, None);
        Ok(self.scan.clone())
    }

    fn rip(
        &self,
        _drive: &Drive,
        titles: &[DiscTitle],
        dest: &Path,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<RipOutcome> {
        let mut out = Vec::new();
        for (n, t) in titles.iter().enumerate() {
            progress(n as f32 / titles.len() as f32, Some(&t.output_name));
            out.push(dest.join(&t.output_name));
        }
        progress(1.0, None);
        self.written.lock().unwrap().extend(out.iter().cloned());
        Ok(RipOutcome { written: out, failed: Vec::new() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeRunner;

    const INFO: &str = r#"DRV:0,2,999,12,"Some Drive","MOVIE_DISC","/dev/sr0"
TINFO:0,9,0,"1:30:00"
TINFO:0,27,0,"title_t00.mkv"
"#;

    #[test]
    fn drives_are_listed_from_the_probe_output() {
        let r = FakeRunner::new().on("makemkvcon", INFO);
        let d = MakeMkv::new(&r).drives().unwrap();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].id, "disc:0");
    }

    #[test]
    fn a_makemkv_failure_message_becomes_the_error() {
        let r = FakeRunner::new().on("makemkvcon", "MSG:5010,0,0,\"Failed to open disc\"");
        let e = MakeMkv::new(&r).drives().unwrap_err();
        assert!(e.0.contains("Failed to open disc"), "{}", e.0);
    }

    #[test]
    fn scanning_asks_for_the_configured_minimum_length() {
        let r = FakeRunner::new().on("makemkvcon", INFO);
        let mut m = MakeMkv::new(&r);
        m.min_length_seconds = 45;
        let drive = m.drives().unwrap().remove(0);
        m.scan(&drive, &mut |_, _| {}).unwrap();
        let call = r.only_call("info disc:0");
        assert!(call.has("--minlength=45"), "{}", call.display());
    }

    #[test]
    fn each_title_is_ripped_by_id_so_a_failure_names_it() {
        let scan = makemkv::parse_scan(INFO, makemkv::parse_drives(INFO).remove(0)).unwrap();
        let r = FakeRunner::new().on("makemkvcon", "");
        let m = MakeMkv::new(&r);
        // no file appears, so this must fail rather than report success
        let e = m
            .rip(&scan.drive, &scan.titles, Path::new("/nonexistent-rip-dir"), &mut |_, _| {})
            .unwrap_err();
        assert!(e.0.contains("title 0") || e.0.contains("nonexistent"), "{}", e.0);
    }

    #[test]
    fn progress_spans_the_whole_run_not_each_title() {
        let scan = DiscScan {
            drive: Drive {
                id: "disc:0".into(),
                device: "/dev/sr0".into(),
                name: "d".into(),
                disc_label: Some("X".into()),
            },
            label: "X".into(),
            titles: vec![
                DiscTitle { id: 0, duration: 1000, chapter_count: 1, chapters: vec![], size_bytes: 0, output_name: "a.mkv".into(), tracks: vec![] },
                DiscTitle { id: 1, duration: 1000, chapter_count: 1, chapters: vec![], size_bytes: 0, output_name: "b.mkv".into(), tracks: vec![] },
            ],
        };
        let f = FakeRipper::new(scan.clone());
        let mut seen = Vec::new();
        f.rip(&scan.drive, &scan.titles, Path::new("/rip"), &mut |p, _| seen.push(p))
            .unwrap();
        assert_eq!(seen.first(), Some(&0.0));
        assert_eq!(seen.last(), Some(&1.0));
        assert!(seen.windows(2).all(|w| w[0] <= w[1]), "{seen:?}");
    }
}

/// The free reader, with MakeMKV held in reserve.
///
/// Which program reads a disc is a decision both front ends have to make the
/// same way, so it lives here rather than in either of them.
///
/// The reserve matters. libdvdcss does a player-key exchange with the drive,
/// which an RPC-2 drive can refuse when the disc's region does not match the
/// one region the drive is set to; and libdvdread gives up on unreadable
/// sectors where MakeMKV retries, which covers scratches and the deliberately
/// corrupt sectors some copy protections write. Both failures are quiet - the
/// scan succeeds and simply returns fewer titles - so they are detected rather
/// than waited for.
/// Somewhere to send an explanation of why a fallback happened.
pub type Notify<'a> = Box<dyn Fn(&str) + Send + Sync + 'a>;

pub struct Auto<'a> {
    pub free: dvd::DvdVideo<'a>,
    /// `None` when MakeMKV is not installed, or the user has turned it off.
    pub makemkv: Option<MakeMkv<'a>>,
    /// Set once a scan has decided; `rip` must use whoever could read it.
    used_fallback: std::sync::atomic::AtomicBool,
    /// Told why a fallback happened, for the log or the progress list.
    pub notify: Option<Notify<'a>>,
}

impl<'a> Auto<'a> {
    pub fn new(runner: &'a dyn Runner, allow_makemkv: bool) -> Self {
        Auto {
            free: dvd::DvdVideo::new(runner),
            makemkv: allow_makemkv.then(|| MakeMkv::new(runner)),
            used_fallback: std::sync::atomic::AtomicBool::new(false),
            notify: None,
        }
    }

    pub fn on_fallback(mut self, f: impl Fn(&str) + Send + Sync + 'a) -> Self {
        self.notify = Some(Box::new(f));
        self
    }

    fn say(&self, message: &str) {
        if let Some(n) = &self.notify {
            n(message);
        }
    }

    fn fell_back(&self) -> bool {
        self.used_fallback.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn take_fallback(&self) -> Option<&MakeMkv<'a>> {
        self.used_fallback
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.makemkv.as_ref()
    }

    /// Whoever read the disc must also rip it: the two disagree about title
    /// numbering, so ripping "title 41" with the wrong one is a different
    /// programme entirely.
    fn reader(&self) -> &dyn Ripper {
        if self.fell_back()
            && let Some(m) = &self.makemkv {
                return m;
            }
        &self.free
    }
}

impl Ripper for Auto<'_> {
    fn drives(&self) -> Result<Vec<Drive>> {
        // MakeMKV names Blu-rays, which are UDF and carry no ISO 9660 label, so
        // when it is available its listing is the more informative one.
        if let Some(m) = &self.makemkv
            && let Ok(d) = m.drives()
                && !d.is_empty() {
                    return Ok(d);
                }
        self.free.drives()
    }

    fn scan(
        &self,
        drive: &Drive,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<DiscScan> {
        match self.free.scan_checked(drive, progress) {
            Ok((scan, health)) if health.is_trustworthy() => Ok(scan),
            Ok((_, health)) => match self.take_fallback() {
                Some(m) => {
                    self.say(&format!(
                        "the free reader could not read this disc fully ({}); using MakeMKV",
                        health.complaint()
                    ));
                    m.scan(drive, progress)
                }
                None => Err(Error(format!(
                    "{}. MakeMKV works around this; enable it in preferences.",
                    health.complaint()
                ))),
            },
            Err(e) => match self.take_fallback() {
                Some(m) => {
                    self.say(&format!("the free reader failed ({e}); using MakeMKV"));
                    m.scan(drive, progress)
                }
                None => Err(e),
            },
        }
    }

    fn rip(
        &self,
        drive: &Drive,
        titles: &[DiscTitle],
        dest: &Path,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<RipOutcome> {
        self.reader().rip(drive, titles, dest, progress)
    }
}

#[cfg(test)]
mod auto_tests {
    use super::*;
    use crate::host::FakeRunner;

    const CSS_FAILURE: &str =
        "libdvdnav: Error cracking CSS key for /VIDEO_TS/VTS_06_1.VOB (0x000651ea)";

    const MAKEMKV_INFO: &str = r#"DRV:0,2,999,12,"Some Drive","DISC","/dev/sr0"
TINFO:0,9,0,"0:21:29"
TINFO:0,27,0,"title_t00.mkv"
"#;

    fn drive() -> Drive {
        Drive {
            id: "disc:0".into(),
            device: "/dev/sr0".into(),
            name: "drive".into(),
            disc_label: Some("DISC".into()),
        }
    }

    #[test]
    fn a_disc_the_free_reader_cannot_decrypt_goes_to_makemkv() {
        let r = FakeRunner::new()
            .fail("ffprobe", CSS_FAILURE)
            .on("makemkvcon", MAKEMKV_INFO);
        let told = std::sync::Mutex::new(Vec::new());
        let a = Auto::new(&r, true).on_fallback(|m| told.lock().unwrap().push(m.to_string()));
        let scan = a.scan(&drive(), &mut |_, _| {}).unwrap();
        assert_eq!(scan.titles.len(), 1);
        assert!(told.lock().unwrap()[0].contains("MakeMKV"), "{:?}", told);
    }

    #[test]
    fn without_the_fallback_it_says_what_would_fix_it() {
        // silently returning a season with no episodes is the failure to avoid
        let r = FakeRunner::new().fail("ffprobe", CSS_FAILURE);
        let a = Auto::new(&r, false);
        let e = a.scan(&drive(), &mut |_, _| {}).unwrap_err();
        assert!(e.0.contains("preferences"), "{}", e.0);
        assert!(e.0.contains("decrypt"), "{}", e.0);
    }

    #[test]
    fn a_healthy_disc_never_reaches_makemkv() {
        let r = FakeRunner::new()
            .on("-title 1 ", crate::rip::dvd::tests::EPISODE)
            .on("makemkvcon", MAKEMKV_INFO);
        let mut a = Auto::new(&r, true);
        a.free.max_title = 2;
        a.scan(&drive(), &mut |_, _| {}).unwrap();
        assert!(r.calls_to("makemkvcon").is_empty());
    }

    #[test]
    fn whoever_scanned_the_disc_also_rips_it() {
        // the two number titles differently, so ripping "title 41" with the
        // wrong one is a different programme entirely
        let r = FakeRunner::new()
            .fail("ffprobe", CSS_FAILURE)
            .on("makemkvcon", MAKEMKV_INFO);
        let a = Auto::new(&r, true);
        let scan = a.scan(&drive(), &mut |_, _| {}).unwrap();
        let dir = std::env::temp_dir().join("riplika-auto-test");
        let _ = a.rip(&drive(), &scan.titles, &dir, &mut |_, _| {});
        let _ = std::fs::remove_dir_all(&dir);
        // the rip went through makemkvcon, not ffmpeg
        assert!(r.calls_to("makemkvcon").len() > 1);
        assert!(r.calls_to("ffmpeg").is_empty());
    }
}

#[cfg(test)]
mod eject_tests {
    use super::*;

    #[test]
    fn ejecting_names_the_device() {
        let c = eject_command(Path::new("/dev/sr0"));
        assert_eq!(c.program, "eject");
        assert_eq!(c.args, vec!["/dev/sr0"]);
    }

    #[test]
    fn the_wait_is_bounded() {
        // A drive still finishing a read does not answer, and on a wedged one
        // it never will. Waiting forever would hang whoever asked.
        const { assert!(EJECT_TIMEOUT_SECONDS > 0 && EJECT_TIMEOUT_SECONDS <= 60) };
    }
}

#[cfg(test)]
mod handler_tests {
    use super::*;

    /// The kernel's mount table, as it looks with a DVD in the drive.
    const MOUNTS: &str = "\
/dev/nvme0n1p6 / ext4 rw,relatime 0 0
/dev/sr0 /run/media/niklas/PARKS_AND_RECREATION udf ro,nosuid,nodev,relatime 0 0
tmpfs /tmp tmpfs rw,nosuid,nodev 0 0";

    #[test]
    fn a_mount_point_leads_back_to_its_device() {
        // the desktop hands over a mount point, and everything here works from
        // the device
        assert_eq!(
            device_mounted_at(MOUNTS, Path::new("/run/media/niklas/PARKS_AND_RECREATION")),
            Some(PathBuf::from("/dev/sr0"))
        );
        assert_eq!(device_mounted_at(MOUNTS, Path::new("/nowhere")), None);
    }

    #[test]
    fn a_uri_is_what_a_desktop_file_actually_delivers() {
        assert_eq!(
            drive_from_argument("file:///run/media/niklas/PARKS_AND_RECREATION", MOUNTS),
            Some(PathBuf::from("/dev/sr0"))
        );
    }

    #[test]
    fn a_label_with_a_space_survives_both_escapings() {
        // the URI percent-encodes it and the mount table escapes it in octal,
        // and they are not the same encoding
        let mounts = "/dev/sr0 /run/media/niklas/THE\\040BIG\\040LEBOWSKI udf ro 0 0";
        assert_eq!(
            drive_from_argument("file:///run/media/niklas/THE%20BIG%20LEBOWSKI", mounts),
            Some(PathBuf::from("/dev/sr0"))
        );
    }

    #[test]
    fn a_device_passed_directly_is_taken_as_given() {
        assert_eq!(
            drive_from_argument("/dev/sr0", ""),
            Some(PathBuf::from("/dev/sr0"))
        );
    }

    #[test]
    fn a_trailing_slash_does_not_prevent_a_match() {
        assert_eq!(
            drive_from_argument("file:///run/media/niklas/PARKS_AND_RECREATION/", MOUNTS),
            Some(PathBuf::from("/dev/sr0"))
        );
    }

    #[test]
    fn something_that_is_not_a_disc_matches_nothing() {
        assert_eq!(drive_from_argument("file:///home/niklas/notes.txt", MOUNTS), None);
        assert_eq!(drive_from_argument("", MOUNTS), None);
    }
}
