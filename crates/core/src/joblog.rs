//! A record of one disc, kept so a season can be reviewed afterwards.
//!
//! A season is six or seven discs done over as many evenings, and by the end
//! there is no way to answer "did episode four of disc two have unrecognised
//! glyphs?" from memory. Each disc writes its own file, all in one directory,
//! named so that sorting them puts a season in order.
//!
//! In `$XDG_STATE_HOME`, which is where the specification puts logs: state that
//! should persist between runs but is not configuration and is not worth
//! backing up.

use crate::job::Event;
use crate::model::DiscScan;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where the logs live.
pub fn directory() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("riplika")
        .join("logs")
}

/// The local time, as a stamp that sorts.
///
/// Local rather than UTC because these are read by a person remembering which
/// evening they did which disc, and one implementation rather than two because
/// a season ripped partly from the window and partly from the terminal would
/// otherwise sort into an order that is neither.
pub fn now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // localtime_r rather than localtime: the latter returns a shared buffer,
    // and this is called from whichever thread is running a job.
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return "unknown-time".into();
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
}

/// A file name that sorts a season into the order it was ripped.
///
/// Time first, because that is the order the discs were done in and the order
/// anyone reviewing them wants; the label after it, so a directory listing says
/// which disc each one was without opening it.
pub fn file_name(started: &str, label: &str) -> String {
    let safe: String = label
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '-' })
        .collect();
    let safe = safe.trim_matches('-');
    if safe.is_empty() { format!("{started}.log") } else { format!("{started}-{safe}.log") }
}

/// Writes one disc's run to a file as it happens.
///
/// As it happens, not at the end: the runs worth reading afterwards are the
/// ones that were interrupted or that failed, and a log assembled at the end
/// would not exist for either.
pub struct JobLog {
    file: Option<std::fs::File>,
    path: PathBuf,
}

impl JobLog {
    /// Start a log. Failure to open one is not fatal - a rip is still worth
    /// doing without a record of it.
    pub fn start(label: &str, details: &[String], started: &str) -> JobLog {
        let dir = directory();
        let path = dir.join(file_name(started, label));
        let file =
            std::fs::create_dir_all(&dir).ok().and_then(|_| std::fs::File::create(&path).ok());

        let mut log = JobLog { file, path };
        for line in details {
            log.write(line);
        }
        log.write(&format!("started: {started}"));
        log.write("");
        log
    }

    /// A log for a disc, headed with what the disc is.
    pub fn for_disc(scan: &DiscScan, started: &str) -> JobLog {
        Self::start(
            &scan.label,
            &[
                format!("disc:    {}", scan.label),
                format!("drive:   {} ({})", scan.drive.name, scan.drive.device),
                format!("titles:  {}", scan.titles.len()),
            ],
            started,
        )
    }

    /// A log for a folder that was ripped earlier.
    pub fn for_folder(dir: &Path, files: usize, started: &str) -> JobLog {
        let label = dir
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| "folder".into());
        Self::start(
            &label,
            &[format!("folder:  {}", dir.display()), format!("files:   {files}")],
            started,
        )
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&mut self, line: &str) {
        if let Some(f) = &mut self.file {
            let _ = writeln!(f, "{line}");
            // Flushed each line: an interrupted run is exactly the one whose
            // log is worth having, and a buffer would lose its ending.
            let _ = f.flush();
        }
    }

    /// Record an event, if it is one worth keeping.
    ///
    /// Progress is not: it arrives hundreds of times a second and says nothing
    /// afterwards that the surrounding lines do not.
    pub fn record(&mut self, event: &Event) {
        let line = match event {
            Event::Progress { .. } => return,
            Event::Stage(s) => format!("== {}", s.label()),
            Event::ItemStarted { index, total, name } => {
                format!("[{}/{}] {name}", index + 1, total)
            }
            Event::ItemFinished { destination, bytes, .. } => format!(
                "   wrote {} ({} MB)",
                destination.file_name().unwrap_or_default().to_string_lossy(),
                bytes / 1_048_576
            ),
            Event::TableChosen { path, covered, built } => {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                if *built {
                    format!("   lettering learned from this disc -> {name}")
                } else {
                    format!("   lettering: {name} ({:.0}% of this disc)", covered * 100.0)
                }
            }
            Event::LetteringLearned { labelled, ambiguous, blank } => {
                format!("   {labelled} shapes labelled, {ambiguous} ambiguous, {blank} left blank")
            }
            Event::Subtitle { language, cues, unknown, recognised, .. } => {
                if *recognised {
                    format!("   subtitles {language}: {cues} cues, {unknown} unrecognised glyphs")
                } else {
                    format!("   subtitles {language}: not recognised, bitmap kept")
                }
            }
            Event::Warning(w) => format!("   warning: {}", w.text()),
            // The line worth having when reading back a season: six logs, and
            // what each disc actually held. Written here rather than taken
            // from the window because a log that changes language with the
            // reader is a log that cannot be searched or pasted into a report.
            Event::Plan(p) => {
                // The line worth having when reading back a season: six logs,
                // and what each disc actually held.
                for line in p.lines() {
                    self.write(&format!("   {line}"));
                }
                return;
            }
        };
        self.write(&line);
    }

    /// Close the record with what came of it.
    pub fn finish(&mut self, summary: &str) {
        self.write("");
        self.write(summary);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_sorts_a_season_into_the_order_it_was_ripped() {
        let discs = [
            file_name("2026-08-27T2015", "PARKS_AND_RECREATION_S6D1"),
            file_name("2026-08-29T1102", "PARKS_AND_RECREATION_S6D3"),
            file_name("2026-08-28T1930", "PARKS_AND_RECREATION_S6D2"),
        ];
        let mut sorted = discs.to_vec();
        sorted.sort();
        assert_eq!(sorted[0], discs[0], "disc one first");
        assert_eq!(sorted[2], discs[1], "disc three last");
    }

    #[test]
    fn the_label_is_in_the_name_so_a_listing_says_which_disc() {
        assert_eq!(
            file_name("2026-08-29T1102", "PARKS_AND_RECREATION_S6D1"),
            "2026-08-29T1102-PARKS_AND_RECREATION_S6D1.log"
        );
    }

    #[test]
    fn a_label_that_would_not_be_a_filename_is_made_into_one() {
        assert_eq!(file_name("t", "A/B: C"), "t-A-B--C.log");
        assert_eq!(file_name("t", ""), "t.log");
        assert_eq!(file_name("t", "///"), "t.log");
    }

    /// A log to a real file, so what was written can be read back.
    fn scratch(name: &str) -> (JobLog, PathBuf) {
        let path =
            std::env::temp_dir().join(format!("riplika-joblog-{}-{name}.log", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        (JobLog { file: Some(file), path: path.clone() }, path)
    }

    #[test]
    fn the_plan_is_recorded_so_a_season_can_be_read_back() {
        // six discs, six logs, and this is the line that says which is which
        let (mut log, path) = scratch("plan");
        log.record(&Event::Plan(crate::model::Plan {
            episodes: 7,
            features: 0,
            extended_cuts: 0,
            extras: 23,
            play_alls: 1,
        }));
        let written = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(written.contains("holds 7 episodes, 23 extras"), "{written}");
        assert!(written.contains("skipping 1 play-all title"), "{written}");
    }

    #[test]
    fn a_plan_holding_nothing_writes_no_line() {
        // a blank line in a log reads as something having gone wrong
        let (mut log, path) = scratch("empty");
        log.record(&Event::Plan(crate::model::Plan::default()));
        let written = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(written, "");
    }

    #[test]
    fn progress_is_not_recorded() {
        // it arrives hundreds of times a second and says nothing afterwards
        // that the lines around it do not
        let mut log = JobLog { file: None, path: PathBuf::new() };
        log.record(&Event::Progress {
            stage: crate::job::Stage::Rip,
            fraction: 0.5,
            message: None,
        });
        // nothing to assert but the absence of a panic; the shape is the point
    }

    #[test]
    fn the_stamp_is_a_sortable_local_time() {
        let s = now();
        assert_eq!(s.len(), 15, "{s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[10..11], "T");
        // and it parses back as a plausible date
        let year: i32 = s[..4].parse().unwrap();
        assert!((2020..2100).contains(&year), "{s}");
    }

    #[test]
    fn logs_go_where_the_specification_puts_state() {
        let dir = directory();
        let shown = dir.to_string_lossy();
        assert!(shown.contains("riplika"), "{shown}");
        assert!(shown.ends_with("logs"), "{shown}");
        // not config, which is backed up, and not cache, which is thrown away
        assert!(!shown.contains(".config"), "{shown}");
        assert!(!shown.contains(".cache"), "{shown}");
    }
}
