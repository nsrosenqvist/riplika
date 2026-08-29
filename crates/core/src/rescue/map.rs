//! The recovery map: what is known about every sector of a disc.
//!
//! This is the piece that makes rescue resumable, and resumability is the whole
//! point. A scratched disc is often readable after being cleaned, or in a
//! different drive, or simply on the third attempt an hour later - but only if
//! the work already done is not thrown away each time. The map records the
//! state of every region, so a second run reads only what is still missing.
//!
//! The states and their meanings are GNU ddrescue's, because the algorithm is
//! its and borrowing the vocabulary makes the two comparable.

use crate::{Error, Result};
use std::fmt;

/// What is known about a run of sectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    /// Never attempted.
    NonTried,
    /// Skipped over during the fast pass after an error nearby; the damage is
    /// somewhere in here but its extent is unknown.
    NonTrimmed,
    /// Trimmed to its real extent, but not yet read sector by sector.
    NonScraped,
    /// Read individually and failed.
    Bad,
    /// Recovered.
    Finished,
}

impl State {
    /// ddrescue's map-file characters.
    pub fn symbol(self) -> char {
        match self {
            State::NonTried => '?',
            State::NonTrimmed => '*',
            State::NonScraped => '/',
            State::Bad => '-',
            State::Finished => '+',
        }
    }

    pub fn from_symbol(c: char) -> Option<State> {
        Some(match c {
            '?' => State::NonTried,
            '*' => State::NonTrimmed,
            '/' => State::NonScraped,
            '-' => State::Bad,
            '+' => State::Finished,
            _ => return None,
        })
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            State::NonTried => "untried",
            State::NonTrimmed => "untrimmed",
            State::NonScraped => "unscraped",
            State::Bad => "bad",
            State::Finished => "recovered",
        })
    }
}

/// A half-open run of sectors, `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    pub start: u64,
    pub end: u64,
    pub state: State,
}

impl Run {
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Every sector of the area being rescued, exactly once, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Map {
    runs: Vec<Run>,
}

impl Map {
    /// A map covering `[start, end)`, nothing attempted.
    pub fn new(start: u64, end: u64) -> Map {
        Map {
            runs: if end > start {
                vec![Run { start, end, state: State::NonTried }]
            } else {
                Vec::new()
            },
        }
    }

    /// A map covering several disjoint areas - the sectors of the titles the
    /// user actually wants, rather than the whole disc.
    pub fn over(areas: &[(u64, u64)]) -> Map {
        let mut sorted: Vec<(u64, u64)> = areas.iter().copied().filter(|(a, b)| b > a).collect();
        sorted.sort();
        let mut runs: Vec<Run> = Vec::new();
        for (start, end) in sorted {
            match runs.last_mut() {
                // touching or overlapping areas become one run
                Some(last) if start <= last.end => last.end = last.end.max(end),
                _ => runs.push(Run { start, end, state: State::NonTried }),
            }
        }
        Map { runs }
    }

    pub fn runs(&self) -> &[Run] {
        &self.runs
    }

    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// Total sectors covered.
    pub fn total(&self) -> u64 {
        self.runs.iter().map(Run::len).sum()
    }

    /// Sectors in a given state.
    pub fn count(&self, state: State) -> u64 {
        self.runs.iter().filter(|r| r.state == state).map(Run::len).sum()
    }

    pub fn recovered(&self) -> u64 {
        self.count(State::Finished)
    }

    /// Is there nothing left to try?
    pub fn is_done(&self) -> bool {
        self.runs.iter().all(|r| matches!(r.state, State::Finished | State::Bad))
    }

    /// Record what happened to `[start, end)`.
    ///
    /// Splitting and merging happen here so every other part of the rescue can
    /// simply say what it learned and not think about the bookkeeping.
    pub fn set(&mut self, start: u64, end: u64, state: State) {
        if end <= start {
            return;
        }
        let mut out: Vec<Run> = Vec::with_capacity(self.runs.len() + 2);
        for run in self.runs.drain(..) {
            // wholly outside the change
            if run.end <= start || run.start >= end {
                out.push(run);
                continue;
            }
            // the part before
            if run.start < start {
                out.push(Run { start: run.start, end: start, state: run.state });
            }
            // the part after
            if run.end > end {
                out.push(Run { start: end.max(run.start), end: run.end, state: run.state });
            }
        }
        out.push(Run { start, end, state });
        out.sort_by_key(|r| r.start);
        out.retain(|r| !r.is_empty());

        // Merge neighbours that agree, or the map grows without bound over a
        // long rescue and every scan of it gets slower.
        let mut merged: Vec<Run> = Vec::with_capacity(out.len());
        for run in out {
            match merged.last_mut() {
                Some(last) if last.state == run.state && last.end == run.start => {
                    last.end = run.end;
                }
                _ => merged.push(run),
            }
        }
        self.runs = merged;
    }

    /// The first run in this state, if any.
    pub fn first(&self, state: State) -> Option<Run> {
        self.runs.iter().copied().find(|r| r.state == state)
    }

    /// Every run in this state, as a snapshot that is safe to modify the map
    /// while iterating.
    pub fn all(&self, state: State) -> Vec<Run> {
        self.runs.iter().copied().filter(|r| r.state == state).collect()
    }

    /// Render in ddrescue's map-file format, so the same tools can read it.
    pub fn to_text(&self) -> String {
        let mut s = String::from("# Rescue map written by riplika\n# pos size status\n");
        for r in &self.runs {
            s.push_str(&format!("0x{:08X}  0x{:08X}  {}\n", r.start, r.len(), r.state.symbol()));
        }
        s
    }

    pub fn from_text(text: &str) -> Result<Map> {
        let mut runs = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (Some(pos), Some(size), Some(status)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let parse = |s: &str| -> Option<u64> {
                s.strip_prefix("0x")
                    .map(|h| u64::from_str_radix(h, 16))
                    .unwrap_or_else(|| s.parse())
                    .ok()
            };
            let (Some(pos), Some(size)) = (parse(pos), parse(size)) else {
                continue;
            };
            let Some(state) = status.chars().next().and_then(State::from_symbol) else {
                continue;
            };
            runs.push(Run { start: pos, end: pos + size, state });
        }
        if runs.is_empty() {
            return Err(Error("rescue map is empty or unreadable".into()));
        }
        runs.sort_by_key(|r| r.start);
        Ok(Map { runs })
    }

    /// A one-line summary for a progress display.
    pub fn summary(&self, sector_bytes: u64) -> String {
        let gb = |n: u64| n as f64 * sector_bytes as f64 / 1e9;
        format!(
            "{:.2} GB recovered, {:.2} GB left, {} bad sectors",
            gb(self.recovered()),
            gb(self.total() - self.recovered() - self.count(State::Bad)),
            self.count(State::Bad)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_map_is_entirely_untried() {
        let m = Map::new(0, 100);
        assert_eq!(m.total(), 100);
        assert_eq!(m.count(State::NonTried), 100);
        assert!(!m.is_done());
    }

    #[test]
    fn recording_a_middle_section_splits_the_run() {
        let mut m = Map::new(0, 100);
        m.set(40, 60, State::Finished);
        assert_eq!(
            m.runs(),
            &[
                Run { start: 0, end: 40, state: State::NonTried },
                Run { start: 40, end: 60, state: State::Finished },
                Run { start: 60, end: 100, state: State::NonTried },
            ]
        );
        assert_eq!(m.recovered(), 20);
    }

    #[test]
    fn neighbours_that_agree_are_merged() {
        // without this the map grows without bound over a long rescue
        let mut m = Map::new(0, 100);
        for i in 0..50 {
            m.set(i, i + 1, State::Finished);
        }
        assert_eq!(m.runs().len(), 2);
        assert_eq!(m.recovered(), 50);
    }

    #[test]
    fn a_later_record_overrides_an_earlier_one() {
        let mut m = Map::new(0, 100);
        m.set(10, 20, State::Bad);
        m.set(10, 20, State::Finished);
        assert_eq!(m.count(State::Bad), 0);
        assert_eq!(m.recovered(), 10);
    }

    #[test]
    fn an_overlapping_record_is_absorbed_cleanly() {
        let mut m = Map::new(0, 100);
        m.set(10, 30, State::Bad);
        m.set(20, 40, State::Finished);
        assert_eq!(m.count(State::Bad), 10); // 10..20 survives
        assert_eq!(m.recovered(), 20); // 20..40
        assert_eq!(m.total(), 100); // nothing lost or duplicated
    }

    #[test]
    fn the_map_always_covers_the_area_exactly_once() {
        let mut m = Map::new(0, 1000);
        m.set(100, 200, State::Bad);
        m.set(150, 300, State::NonScraped);
        m.set(0, 50, State::Finished);
        m.set(950, 1000, State::Finished);
        assert_eq!(m.total(), 1000);
        // and the runs are contiguous and ordered
        let mut at = 0;
        for r in m.runs() {
            assert_eq!(r.start, at, "gap or overlap at {at}");
            at = r.end;
        }
        assert_eq!(at, 1000);
    }

    #[test]
    fn a_map_can_cover_several_separate_areas() {
        // rescuing only the episodes means the map has holes by design
        let m = Map::over(&[(100, 200), (500, 600)]);
        assert_eq!(m.total(), 200);
        assert_eq!(m.runs().len(), 2);
    }

    #[test]
    fn touching_areas_become_one_run() {
        let m = Map::over(&[(100, 200), (200, 300), (150, 250)]);
        assert_eq!(m.runs().len(), 1);
        assert_eq!(m.total(), 200);
    }

    #[test]
    fn done_means_nothing_left_to_try_not_nothing_bad() {
        let mut m = Map::new(0, 100);
        m.set(0, 90, State::Finished);
        m.set(90, 100, State::Bad);
        assert!(m.is_done());
        assert_eq!(m.recovered(), 90);
    }

    #[test]
    fn a_map_round_trips_through_its_file_format() {
        let mut m = Map::new(0, 1000);
        m.set(100, 200, State::Bad);
        m.set(300, 400, State::NonTrimmed);
        m.set(0, 100, State::Finished);
        let text = m.to_text();
        assert_eq!(Map::from_text(&text).unwrap(), m);
    }

    #[test]
    fn the_file_format_is_ddrescues_so_its_tools_can_read_it() {
        let mut m = Map::new(0, 0x1000);
        m.set(0, 0x800, State::Finished);
        let text = m.to_text();
        assert!(text.contains("0x00000000  0x00000800  +"), "{text}");
        assert!(text.contains("0x00000800  0x00000800  ?"), "{text}");
    }

    #[test]
    fn a_corrupt_map_file_is_an_error_rather_than_an_empty_rescue() {
        // silently starting from scratch would throw away hours of reading
        assert!(Map::from_text("").is_err());
        assert!(Map::from_text("# just a comment\n").is_err());
        assert!(Map::from_text("garbage\n").is_err());
    }

    #[test]
    fn unknown_lines_are_skipped_but_good_ones_still_load() {
        let m = Map::from_text("nonsense here\n0x0 0x100 +\n").unwrap();
        assert_eq!(m.recovered(), 0x100);
    }

    #[test]
    fn the_summary_reports_in_useful_units() {
        let mut m = Map::new(0, 1_000_000);
        m.set(0, 500_000, State::Finished);
        let s = m.summary(2048);
        assert!(s.contains("1.02 GB recovered"), "{s}");
    }
}
