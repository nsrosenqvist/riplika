//! Stage one: getting the disc onto disk.

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
                DiscTitle { id: 0, duration: 1000, chapter_count: 1, size_bytes: 0, output_name: "a.mkv".into(), tracks: vec![] },
                DiscTitle { id: 1, duration: 1000, chapter_count: 1, size_bytes: 0, output_name: "b.mkv".into(), tracks: vec![] },
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
