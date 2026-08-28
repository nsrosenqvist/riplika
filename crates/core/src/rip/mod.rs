//! Stage one: getting the disc onto disk.

pub mod dvd;
pub mod iso;
pub mod makemkv;

use crate::host::Runner;
use crate::model::{DiscScan, DiscTitle, Drive};
use crate::{Error, Result};
use std::path::{Path, PathBuf};

/// Titles shorter than this are menu loops and transitions, not content.
pub const DEFAULT_MIN_LENGTH_SECONDS: u32 = 120;

/// Reads discs. Behind a trait so the pipeline can be tested without one.
pub trait Ripper: Send + Sync {
    /// Drives on this machine, and what is loaded in them.
    fn drives(&self) -> Result<Vec<Drive>>;

    /// Enumerate a disc's titles without ripping it.
    fn scan(&self, drive: &Drive) -> Result<DiscScan>;

    /// Rip `titles` into `dest`, reporting progress from 0.0 to 1.0.
    ///
    /// Returns the files written, in the order the titles were given.
    fn rip(
        &self,
        drive: &Drive,
        titles: &[DiscTitle],
        dest: &Path,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<Vec<PathBuf>>;
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

    fn scan(&self, drive: &Drive) -> Result<DiscScan> {
        let out = self
            .runner
            .run(&makemkv::scan_command(&drive.id, self.min_length_seconds))?;
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
    ) -> Result<Vec<PathBuf>> {
        std::fs::create_dir_all(dest).map_err(|e| Error(format!("{}: {e}", dest.display())))?;

        // One invocation per title, rather than `all`. It costs a little in
        // disc seeking but means a failure names the title it happened on, and
        // a run can be resumed by skipping what is already there.
        let mut written = Vec::new();
        for (n, title) in titles.iter().enumerate() {
            let out_path = dest.join(&title.output_name);
            let base = n as f32 / titles.len() as f32;
            let span = 1.0 / titles.len() as f32;

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

            if let Some(msg) = makemkv::parse_error(&out.stdout) {
                return Err(Error(format!("title {}: {msg}", title.id)));
            }
            // MakeMKV exits zero having saved nothing, so the file is the proof
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

    fn scan(&self, _drive: &Drive) -> Result<DiscScan> {
        Ok(self.scan.clone())
    }

    fn rip(
        &self,
        _drive: &Drive,
        titles: &[DiscTitle],
        dest: &Path,
        progress: &mut dyn FnMut(f32, Option<&str>),
    ) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for (n, t) in titles.iter().enumerate() {
            progress(n as f32 / titles.len() as f32, Some(&t.output_name));
            out.push(dest.join(&t.output_name));
        }
        progress(1.0, None);
        self.written.lock().unwrap().extend(out.iter().cloned());
        Ok(out)
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
        m.scan(&drive).unwrap();
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

    fn scan(&self, drive: &Drive) -> Result<DiscScan> {
        match self.free.scan_checked(drive) {
            Ok((scan, health)) if health.is_trustworthy() => Ok(scan),
            Ok((_, health)) => match self.take_fallback() {
                Some(m) => {
                    self.say(&format!(
                        "the free reader could not read this disc fully ({}); using MakeMKV",
                        health.complaint()
                    ));
                    m.scan(drive)
                }
                None => Err(Error(format!(
                    "{}. MakeMKV works around this; enable it in preferences.",
                    health.complaint()
                ))),
            },
            Err(e) => match self.take_fallback() {
                Some(m) => {
                    self.say(&format!("the free reader failed ({e}); using MakeMKV"));
                    m.scan(drive)
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
    ) -> Result<Vec<PathBuf>> {
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
        let scan = a.scan(&drive()).unwrap();
        assert_eq!(scan.titles.len(), 1);
        assert!(told.lock().unwrap()[0].contains("MakeMKV"), "{:?}", told);
    }

    #[test]
    fn without_the_fallback_it_says_what_would_fix_it() {
        // silently returning a season with no episodes is the failure to avoid
        let r = FakeRunner::new().fail("ffprobe", CSS_FAILURE);
        let a = Auto::new(&r, false);
        let e = a.scan(&drive()).unwrap_err();
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
        a.scan(&drive()).unwrap();
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
        let scan = a.scan(&drive()).unwrap();
        let dir = std::env::temp_dir().join("riplika-auto-test");
        let _ = a.rip(&drive(), &scan.titles, &dir, &mut |_, _| {});
        let _ = std::fs::remove_dir_all(&dir);
        // the rip went through makemkvcon, not ffmpeg
        assert!(r.calls_to("makemkvcon").len() > 1);
        assert!(r.calls_to("ffmpeg").is_empty());
    }
}
