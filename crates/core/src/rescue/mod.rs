//! Recovering what a damaged disc will still give up.
//!
//! This is GNU ddrescue's algorithm applied to a DVD. The algorithm is worth
//! copying exactly because its central insight is not obvious: **read the easy
//! data first**. A disc that is failing may not survive an hour of retries, and
//! each unreadable sector costs seconds while the drive does its own internal
//! retrying - so spending that time before the good 99% has been secured is a
//! way to end up with nothing.
//!
//! Four passes, narrowing each time:
//!
//! 1. **Copy** - large reads; on error, record the area and skip well ahead.
//! 2. **Trim** - approach each damaged area from both ends in small reads, to
//!    find how far the damage really extends rather than writing off the whole
//!    skipped span.
//! 3. **Scrape** - read what is left one sector at a time.
//! 4. **Retry** - go round the remaining bad sectors again, a few times.
//!
//! What a DVD adds over a generic block device is structure. The IFOs say which
//! sectors belong to which title, so a rescue can cover only the titles that
//! are wanted - on a Parks and Recreation disc the seven episodes are 5.24 GB of
//! 8.24 GB, and the two play-alls share their sectors, so nothing is read twice.
//! It also means damage can be reported as "episode 4, six minutes in" rather
//! than as a sector number.

pub mod dvdcss;
pub mod map;

use crate::{Error, Result};
use map::{Map, State};
use std::path::Path;

/// DVD sectors are always 2 KB.
pub const SECTOR: usize = 2048;

/// Sectors per read in the fast pass. Large enough to stream at the drive's
/// full rate, small enough that one error does not condemn much.
pub const BIG_READ: u64 = 128;

/// Sectors per read while trimming: small, since the point is precision.
pub const TRIM_READ: u64 = 8;

/// How far to jump ahead after an error in the fast pass.
///
/// Damage on an optical disc is usually a contiguous scratch rather than a
/// scattering, so the next sector after a bad one is very likely bad too.
/// Jumping means the good data beyond it is secured sooner.
pub const SKIP_AHEAD: u64 = 4096;

/// Why a read failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The drive could not read these sectors.
    Unreadable,
    /// Something else went wrong - the device disappeared, a seek failed.
    Fatal(String),
}

/// Somewhere sectors can be read from, one run at a time.
pub trait SectorSource {
    /// Read `count` sectors starting at `lba`.
    ///
    /// Returns the bytes on success. A partial read is a failure: the caller
    /// narrows the range itself rather than guessing which part arrived.
    fn read(&mut self, lba: u64, count: u64) -> std::result::Result<Vec<u8>, ReadError>;
}

/// Somewhere recovered sectors go.
pub trait SectorSink {
    fn write(&mut self, lba: u64, data: &[u8]) -> Result<()>;
}

/// A sink backed by a file, sparse where nothing has been written.
pub struct FileSink {
    file: std::fs::File,
    /// Sectors are written at `(lba - base) * SECTOR`, so a rescue of one title
    /// produces a file starting at that title rather than a mostly-empty image.
    base: u64,
}

impl FileSink {
    pub fn create(path: &Path, base: u64) -> Result<FileSink> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|e| Error(format!("{}: {e}", path.display())))?;
        Ok(FileSink { file, base })
    }
}

impl SectorSink for FileSink {
    fn write(&mut self, lba: u64, data: &[u8]) -> Result<()> {
        use std::io::{Seek, SeekFrom, Write};
        let offset = lba.saturating_sub(self.base) * SECTOR as u64;
        self.file
            .seek(SeekFrom::Start(offset))
            .map_err(|e| Error(e.to_string()))?;
        self.file.write_all(data).map_err(|e| Error(e.to_string()))
    }
}

/// One MPEG-2 program stream padding packet, exactly filling a sector.
///
/// Zeros would be the obvious filler and are the wrong one: a demuxer reading
/// zeros where a packet header should be treats the stream as corrupt. Padding
/// packets are made to be skipped, so an unrecoverable sector costs a moment of
/// video rather than the rest of the file.
pub fn padding_sector() -> Vec<u8> {
    let mut s = Vec::with_capacity(SECTOR);
    s.extend_from_slice(&[0x00, 0x00, 0x01, 0xBE]);
    // the length field counts the bytes after it
    let payload = (SECTOR - 6) as u16;
    s.extend_from_slice(&payload.to_be_bytes());
    s.resize(SECTOR, 0xFF);
    s
}

/// What the rescue is doing, for a progress display.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub pass: &'static str,
    /// 0.0 to 1.0 over the whole area.
    pub fraction: f32,
    pub recovered_sectors: u64,
    pub bad_sectors: u64,
}

/// Runs the passes.
pub struct Rescue {
    pub map: Map,
    pub big_read: u64,
    pub trim_read: u64,
    pub skip_ahead: u64,
    /// How many times to go back over sectors that are still bad.
    pub retries: u32,
}

impl Rescue {
    pub fn new(map: Map) -> Rescue {
        Rescue {
            map,
            big_read: BIG_READ,
            trim_read: TRIM_READ,
            skip_ahead: SKIP_AHEAD,
            retries: 2,
        }
    }

    fn report(&self, pass: &'static str, report: &mut dyn FnMut(Progress)) {
        let total = self.map.total().max(1);
        report(Progress {
            pass,
            fraction: (self.map.recovered() + self.map.count(State::Bad)) as f32 / total as f32,
            recovered_sectors: self.map.recovered(),
            bad_sectors: self.map.count(State::Bad),
        });
    }

    /// Pass one: sweep up everything that reads easily.
    pub fn copy_pass(
        &mut self,
        source: &mut dyn SectorSource,
        sink: &mut dyn SectorSink,
        report: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        while let Some(run) = self.map.first(State::NonTried) {
            let mut at = run.start;
            while at < run.end {
                let count = self.big_read.min(run.end - at);
                match source.read(at, count) {
                    Ok(data) => {
                        sink.write(at, &data)?;
                        self.map.set(at, at + count, State::Finished);
                        at += count;
                    }
                    Err(ReadError::Fatal(e)) => return Err(Error(e)),
                    Err(ReadError::Unreadable) => {
                        // Damage is usually a contiguous scratch, so the sectors
                        // just past this one are probably bad too. Mark the
                        // region as needing a closer look and get on with the
                        // good data beyond it.
                        let skip = self.skip_ahead.min(run.end - at);
                        self.map.set(at, at + skip, State::NonTrimmed);
                        at += skip;
                    }
                }
                self.report("copying", report);
            }
        }
        Ok(())
    }

    /// Pass two: find how far the damage really goes.
    pub fn trim_pass(
        &mut self,
        source: &mut dyn SectorSource,
        sink: &mut dyn SectorSink,
        report: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        for run in self.map.all(State::NonTrimmed) {
            // forwards from the start
            let mut at = run.start;
            while at < run.end {
                let count = self.trim_read.min(run.end - at);
                match source.read(at, count) {
                    Ok(data) => {
                        sink.write(at, &data)?;
                        self.map.set(at, at + count, State::Finished);
                        at += count;
                    }
                    Err(ReadError::Fatal(e)) => return Err(Error(e)),
                    Err(ReadError::Unreadable) => break,
                }
            }
            // and backwards from the end, so the damage is bracketed
            let mut end = run.end;
            while end > at {
                let count = self.trim_read.min(end - at);
                let start = end - count;
                match source.read(start, count) {
                    Ok(data) => {
                        sink.write(start, &data)?;
                        self.map.set(start, end, State::Finished);
                        end = start;
                    }
                    Err(ReadError::Fatal(e)) => return Err(Error(e)),
                    Err(ReadError::Unreadable) => break,
                }
            }
            if end > at {
                self.map.set(at, end, State::NonScraped);
            }
            self.report("trimming", report);
        }
        Ok(())
    }

    /// Pass three: one sector at a time through what is left.
    pub fn scrape_pass(
        &mut self,
        source: &mut dyn SectorSource,
        sink: &mut dyn SectorSink,
        report: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        for run in self.map.all(State::NonScraped) {
            for lba in run.start..run.end {
                match source.read(lba, 1) {
                    Ok(data) => {
                        sink.write(lba, &data)?;
                        self.map.set(lba, lba + 1, State::Finished);
                    }
                    Err(ReadError::Fatal(e)) => return Err(Error(e)),
                    Err(ReadError::Unreadable) => self.map.set(lba, lba + 1, State::Bad),
                }
            }
            self.report("scraping", report);
        }
        Ok(())
    }

    /// Pass four: try the bad sectors again. Drives are not deterministic.
    pub fn retry_pass(
        &mut self,
        source: &mut dyn SectorSource,
        sink: &mut dyn SectorSink,
        report: &mut dyn FnMut(Progress),
    ) -> Result<()> {
        for _ in 0..self.retries {
            let bad = self.map.all(State::Bad);
            if bad.is_empty() {
                break;
            }
            for run in bad {
                for lba in run.start..run.end {
                    if let Ok(data) = source.read(lba, 1) {
                        sink.write(lba, &data)?;
                        self.map.set(lba, lba + 1, State::Finished);
                    }
                }
            }
            self.report("retrying", report);
        }
        Ok(())
    }

    /// Fill whatever could not be recovered, so the output is a whole file.
    ///
    /// Without this the holes are zeros, and a demuxer meeting zeros where a
    /// packet header belongs gives up on the rest of the stream.
    pub fn fill_holes(&self, sink: &mut dyn SectorSink) -> Result<u64> {
        let padding = padding_sector();
        let mut filled = 0;
        for run in self.map.all(State::Bad) {
            for lba in run.start..run.end {
                sink.write(lba, &padding)?;
                filled += 1;
            }
        }
        Ok(filled)
    }

    /// All four passes, then fill.
    pub fn run(
        &mut self,
        source: &mut dyn SectorSource,
        sink: &mut dyn SectorSink,
        report: &mut dyn FnMut(Progress),
    ) -> Result<u64> {
        self.copy_pass(source, sink, report)?;
        self.trim_pass(source, sink, report)?;
        self.scrape_pass(source, sink, report)?;
        self.retry_pass(source, sink, report)?;
        self.fill_holes(sink)
    }
}

/// A source that fails on chosen sectors, for testing the passes.
#[cfg(test)]
pub struct FakeDisc {
    pub bad: Vec<std::ops::Range<u64>>,
    pub reads: usize,
    /// Sectors read individually, to check the passes narrow as they should.
    pub single_reads: usize,
    /// Every sector touched, so a resumed rescue can be shown not to re-read.
    pub touched: std::collections::BTreeSet<u64>,
}

#[cfg(test)]
impl FakeDisc {
    pub fn new(bad: impl Into<Vec<std::ops::Range<u64>>>) -> FakeDisc {
        FakeDisc { bad: bad.into(), reads: 0, single_reads: 0, touched: Default::default() }
    }

    fn is_bad(&self, lba: u64) -> bool {
        self.bad.iter().any(|r| r.contains(&lba))
    }
}

#[cfg(test)]
impl SectorSource for FakeDisc {
    fn read(&mut self, lba: u64, count: u64) -> std::result::Result<Vec<u8>, ReadError> {
        self.reads += 1;
        if count == 1 {
            self.single_reads += 1;
        }
        self.touched.extend(lba..lba + count);
        // a read fails if any sector in it is bad, as a real drive's would
        if (lba..lba + count).any(|s| self.is_bad(s)) {
            return Err(ReadError::Unreadable);
        }
        let mut data = Vec::with_capacity((count as usize) * SECTOR);
        for s in lba..lba + count {
            data.extend(std::iter::repeat_n((s % 251) as u8, SECTOR));
        }
        Ok(data)
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct MemorySink(pub std::collections::BTreeMap<u64, Vec<u8>>);

#[cfg(test)]
impl SectorSink for MemorySink {
    fn write(&mut self, lba: u64, data: &[u8]) -> Result<()> {
        for (i, chunk) in data.chunks(SECTOR).enumerate() {
            self.0.insert(lba + i as u64, chunk.to_vec());
        }
        Ok(())
    }
}

#[cfg(test)]
// `[500..501]` says "one damaged sector" more clearly than the alternatives
// clippy suggests, and these tests are about which sectors are damaged.
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;

    fn run_rescue(bad: impl Into<Vec<std::ops::Range<u64>>>, total: u64) -> (Rescue, FakeDisc, MemorySink) {
        let mut disc = FakeDisc::new(bad);
        let mut sink = MemorySink::default();
        let mut r = Rescue::new(Map::new(0, total));
        r.skip_ahead = 64;
        r.run(&mut disc, &mut sink, &mut |_| {}).unwrap();
        (r, disc, sink)
    }

    #[test]
    fn an_undamaged_disc_is_copied_in_one_pass() {
        let (r, disc, sink) = run_rescue([], 1024);
        assert_eq!(r.map.recovered(), 1024);
        assert_eq!(r.map.count(State::Bad), 0);
        assert_eq!(sink.0.len(), 1024);
        // 1024 sectors at 128 a read, and nothing read individually
        assert_eq!(disc.reads, 8);
        assert_eq!(disc.single_reads, 0);
    }

    #[test]
    fn a_single_bad_sector_costs_only_itself() {
        // the whole point: one scratch should not condemn the region round it
        let (r, _, sink) = run_rescue([500..501], 1024);
        assert_eq!(r.map.count(State::Bad), 1);
        assert_eq!(r.map.recovered(), 1023);
        assert!(sink.0.contains_key(&499) && sink.0.contains_key(&501));
        // the hole is present in the output, but as padding rather than data
        assert_eq!(&sink.0[&500][..4], &[0x00, 0x00, 0x01, 0xBE]);
    }

    #[test]
    fn a_contiguous_scratch_is_bracketed_exactly() {
        let (r, _, _) = run_rescue([300..340], 1024);
        assert_eq!(r.map.count(State::Bad), 40);
        assert_eq!(r.map.recovered(), 1024 - 40);
        // and the map says precisely where
        let bad = r.map.all(State::Bad);
        assert_eq!(bad.len(), 1);
        assert_eq!((bad[0].start, bad[0].end), (300, 340));
    }

    #[test]
    fn the_good_data_is_secured_before_the_damage_is_worked_on() {
        // if the drive dies partway, what survives should be the easy 99%
        let mut disc = FakeDisc::new([500..600]);
        let mut sink = MemorySink::default();
        let mut r = Rescue::new(Map::new(0, 2048));
        r.skip_ahead = 256;
        r.copy_pass(&mut disc, &mut sink, &mut |_| {}).unwrap();
        // everything outside the skipped span is already in hand
        assert!(r.map.recovered() >= 2048 - 256);
        // and nothing has been read a sector at a time yet
        assert_eq!(disc.single_reads, 0);
    }

    #[test]
    fn several_separate_scratches_are_all_found() {
        let (r, _, _) = run_rescue([100..110, 700..705, 900..901], 1024);
        assert_eq!(r.map.count(State::Bad), 16);
        assert_eq!(r.map.all(State::Bad).len(), 3);
    }

    #[test]
    fn damage_at_the_very_end_does_not_run_off_the_map() {
        let (r, _, _) = run_rescue([1020..1024], 1024);
        assert_eq!(r.map.total(), 1024);
        assert_eq!(r.map.count(State::Bad), 4);
    }

    #[test]
    fn a_disc_that_is_entirely_unreadable_ends_rather_than_looping() {
        let (r, _, _) = run_rescue([0..1024], 1024);
        assert_eq!(r.map.recovered(), 0);
        assert_eq!(r.map.count(State::Bad), 1024);
        assert!(r.map.is_done());
    }

    #[test]
    fn a_drive_that_succeeds_on_the_second_attempt_is_given_one() {
        // drives are not deterministic, which is why the retry pass exists
        struct Flaky {
            seen: std::collections::HashSet<u64>,
        }
        impl SectorSource for Flaky {
            fn read(&mut self, lba: u64, count: u64) -> std::result::Result<Vec<u8>, ReadError> {
                if (lba..lba + count).any(|s| s == 500) && self.seen.insert(500) {
                    return Err(ReadError::Unreadable);
                }
                Ok(vec![0u8; (count as usize) * SECTOR])
            }
        }
        let mut disc = Flaky { seen: Default::default() };
        let mut sink = MemorySink::default();
        let mut r = Rescue::new(Map::new(0, 1024));
        r.run(&mut disc, &mut sink, &mut |_| {}).unwrap();
        assert_eq!(r.map.count(State::Bad), 0);
    }

    #[test]
    fn a_fatal_error_stops_rather_than_being_recorded_as_damage() {
        // the device disappearing is not the same as a scratch
        struct Gone;
        impl SectorSource for Gone {
            fn read(&mut self, _: u64, _: u64) -> std::result::Result<Vec<u8>, ReadError> {
                Err(ReadError::Fatal("device disappeared".into()))
            }
        }
        let mut r = Rescue::new(Map::new(0, 1024));
        let e = r
            .run(&mut Gone, &mut MemorySink::default(), &mut |_| {})
            .unwrap_err();
        assert!(e.0.contains("disappeared"), "{}", e.0);
    }

    #[test]
    fn holes_are_filled_with_padding_a_demuxer_will_skip() {
        let (r, _, sink) = run_rescue([500..502], 1024);
        assert_eq!(r.map.count(State::Bad), 2);
        // the sectors exist in the output, and are valid padding packets
        let filler = sink.0.get(&500).unwrap();
        assert_eq!(filler.len(), SECTOR);
        assert_eq!(&filler[..4], &[0x00, 0x00, 0x01, 0xBE]);
        assert_eq!(u16::from_be_bytes([filler[4], filler[5]]) as usize, SECTOR - 6);
    }

    #[test]
    fn a_padding_packet_declares_its_own_length_correctly() {
        let p = padding_sector();
        assert_eq!(p.len(), SECTOR);
        let declared = u16::from_be_bytes([p[4], p[5]]) as usize;
        assert_eq!(declared + 6, SECTOR, "a demuxer would mis-skip this");
    }

    #[test]
    fn only_the_wanted_areas_are_read() {
        // rescuing seven episodes rather than a whole disc
        let mut disc = FakeDisc::new([]);
        let mut sink = MemorySink::default();
        let mut r = Rescue::new(Map::over(&[(100, 200), (500, 600)]));
        r.run(&mut disc, &mut sink, &mut |_| {}).unwrap();
        assert_eq!(r.map.recovered(), 200);
        assert!(sink.0.contains_key(&100) && sink.0.contains_key(&599));
        assert!(!sink.0.contains_key(&300), "read outside the requested areas");
    }

    #[test]
    fn progress_only_moves_forwards() {
        let mut disc = FakeDisc::new([300..340]);
        let mut sink = MemorySink::default();
        let mut r = Rescue::new(Map::new(0, 1024));
        let mut seen: Vec<f32> = Vec::new();
        r.run(&mut disc, &mut sink, &mut |p| seen.push(p.fraction)).unwrap();
        assert!(seen.windows(2).all(|w| w[0] <= w[1] + 1e-6), "{seen:?}");
        assert!((seen.last().unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_rescue_can_be_stopped_and_resumed_from_its_map() {
        // the reason the map exists: clean the disc, put it back, carry on
        let mut disc = FakeDisc::new([500..600]);
        let mut sink = MemorySink::default();
        let mut first = Rescue::new(Map::new(0, 2048));
        first.skip_ahead = 256;
        first.copy_pass(&mut disc, &mut sink, &mut |_| {}).unwrap();
        let already = first.map.all(State::Finished);
        let saved = first.map.to_text();

        // ... a different session, and this time the disc reads cleanly
        let mut second = Rescue::new(Map::from_text(&saved).unwrap());
        let mut healthy = FakeDisc::new([]);
        second.run(&mut healthy, &mut sink, &mut |_| {}).unwrap();
        assert_eq!(second.map.recovered(), 2048);

        // the point of the map: nothing already recovered was read again
        for run in already {
            for lba in run.start..run.end {
                assert!(
                    !healthy.touched.contains(&lba),
                    "sector {lba} was re-read despite being recovered already"
                );
            }
        }
    }
}

/// Rescue a set of sector ranges from a disc into an image file.
///
/// The map file beside the image is what makes this resumable: run it again
/// after cleaning the disc and only the parts still missing are attempted.
pub fn rescue_ranges(
    device: &Path,
    ranges: &[(u64, u64)],
    plain: &[(u64, u64)],
    image: &Path,
    map_path: &Path,
    report: &mut dyn FnMut(Progress),
) -> Result<Map> {
    let mut disc = dvdcss::Dvd::open(device)?;
    // The descriptors and IFOs are not encrypted; decrypting them would destroy
    // them, and the loss would not be visible until the image failed to open.
    disc.set_plain_ranges(plain);

    // Resume if a map from a previous attempt is beside the image.
    let map = std::fs::read_to_string(map_path)
        .ok()
        .and_then(|t| Map::from_text(&t).ok())
        .unwrap_or_else(|| Map::over(ranges));

    // Sectors go at their true addresses, so the result is a disc image rather
    // than a fragment: a demuxer seeks to absolute positions, and an image that
    // began at the first rescued sector would have every one of them shifted.
    // The file is sparse, so the untouched parts cost nothing.
    let mut sink = FileSink::create(image, 0)?;
    let mut rescue = Rescue::new(map);
    let result = rescue.run(&mut disc, &mut sink, report);

    // Write the map whatever happened - a rescue that was interrupted is
    // exactly the one whose progress must not be lost.
    if let Some(dir) = map_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(map_path, rescue.map.to_text());
    result?;
    Ok(rescue.map)
}

#[cfg(test)]
mod image_tests {
    use super::*;

    /// The image must be a *disc* image, not a fragment.
    ///
    /// A demuxer seeks to absolute sector addresses. An image that began at the
    /// first rescued sector would have every address shifted, and the first
    /// attempt at this wrote exactly that - 750 MB of correctly decrypted video
    /// that no player could open.
    #[test]
    fn sectors_are_written_at_their_true_addresses() {
        let path = std::env::temp_dir().join(format!("riplika-image-{}.iso", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut sink = FileSink::create(&path, 0).unwrap();
            sink.write(1000, &vec![0xAB; SECTOR]).unwrap();
        }
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 1001 * SECTOR as u64);

        // and the data really is at sector 1000
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data[1000 * SECTOR], 0xAB);
        assert_eq!(data[999 * SECTOR], 0x00);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_image_of_one_title_is_sparse_rather_than_huge() {
        // writing at true addresses means the file spans the disc; it must not
        // also occupy the disc
        let path = std::env::temp_dir().join(format!("riplika-sparse-{}.iso", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut sink = FileSink::create(&path, 0).unwrap();
            sink.write(2_000_000, &vec![0xCD; SECTOR]).unwrap();
        }
        let meta = std::fs::metadata(&path).unwrap();
        assert!(meta.len() > 4_000_000_000);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            // 512-byte blocks actually allocated: a few, not four million
            assert!(meta.blocks() < 1000, "{} blocks allocated", meta.blocks());
        }
        let _ = std::fs::remove_file(&path);
    }
}
