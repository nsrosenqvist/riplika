//! Choosing which glyph table a disc should be decoded with.
//!
//! A table is per release. DVD subtitles are a rendered bitmap font and
//! different studios use different faces, so the table built for one disc fits
//! nothing else: of Frozen's 110 shapes, one is in the Parks and Recreation
//! table. Every disc used to be decoded against whichever single table was
//! installed, which is how The Lion King came out as 1,532 lines of
//! placeholders.
//!
//! Nothing is keyed or remembered. Each table is simply tried against the
//! shapes actually on the disc and the one that covers most of them wins, so a
//! second disc from the same release reuses the first one's table without
//! anything having to record that they are related, and a disc from a new
//! release fails the test and gets a table of its own.

use crate::host::Fs;
use crate::subs::segment::Line;
use crate::subs::table::Table;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// How much of a disc a table has to explain to be worth using.
///
/// Not 100%: a table that covers the alphabet and misses a bullet and a
/// trademark sign is the right table, and the two shapes it lacks come out as
/// placeholders rather than sending the whole disc to be read again. Well
/// clear of the 1% a table for the wrong release manages.
pub const FITS: f32 = 0.90;

/// How many instances of each shape a sample of the disc holds.
pub fn shapes(lines: &[Line], into: &mut BTreeMap<String, u64>) {
    for g in lines.iter().flat_map(|l| &l.glyphs) {
        *into.entry(g.key()).or_insert(0) += 1;
    }
}

/// The share of a disc's glyph instances this table can put a character to.
///
/// By instance rather than by distinct shape, because that is what a reader
/// sees: a table missing one shape that happens to be `e` is useless, and one
/// missing a symbol used twice in a film is not.
pub fn coverage(table: &Table, shapes: &BTreeMap<String, u64>) -> f32 {
    let total: u64 = shapes.values().sum();
    if total == 0 {
        return 0.0;
    }
    let known: u64 = shapes
        .iter()
        .filter(|(key, _)| table.get(key).is_some_and(|e| e.text.is_some()))
        .map(|(_, n)| n)
        .sum();
    known as f32 / total as f32
}

/// Every table on offer: the one configured, and any built for a disc before.
pub fn candidates(fs: &dyn Fs, configured: Option<&Path>, dir: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(p) = configured.filter(|p| fs.exists(p)) {
        out.push(p.to_path_buf());
    }
    if let Some(dir) = dir {
        let mut built: Vec<PathBuf> = fs
            .list(dir)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        // A stable order, so the same disc picks the same table between runs
        // when two of them fit equally well.
        built.sort();
        out.extend(built);
    }
    out.dedup();
    out
}

/// The best table for these shapes, if any of them fits.
pub fn best(
    fs: &dyn Fs,
    paths: &[PathBuf],
    shapes: &BTreeMap<String, u64>,
) -> Option<(PathBuf, Table, f32)> {
    closest(fs, paths, shapes).filter(|(_, _, c)| *c >= FITS)
}

/// The best table there is, whether or not it is good enough to use.
///
/// Worth having separately: a table that explains most of a disc and misses
/// the rest is the right thing to start from when the rest has to be read.
/// Building from nothing instead would spend a second reading on shapes that
/// were already labelled, and throw away the labels a person may have
/// corrected by hand.
pub fn closest(
    fs: &dyn Fs,
    paths: &[PathBuf],
    shapes: &BTreeMap<String, u64>,
) -> Option<(PathBuf, Table, f32)> {
    let mut best: Option<(PathBuf, Table, f32)> = None;
    for p in paths {
        let Ok(bytes) = fs.read(p) else { continue };
        let Ok(t) = Table::from_bytes(&bytes) else { continue };
        let c = coverage(&t, shapes);
        if best.as_ref().is_none_or(|(_, _, b)| c > *b) {
            best = Some((p.clone(), t, c));
        }
    }
    best
}

/// Where a table built for this disc should be written.
///
/// Named after the disc so a person looking in the folder can see what it is
/// for. The name is only a name: which table gets used is decided by trying
/// them, never by matching this against anything.
pub fn path_for(dir: &Path, label: &str) -> PathBuf {
    let name: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    let name = name.trim_matches('-').to_string();
    dir.join(format!("{}.json", if name.is_empty() { "disc" } else { &name }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeFs;
    use crate::subs::segment::Glyph;
    use crate::subs::table::Table;

    fn glyph(shape: u8) -> Glyph {
        Glyph { x: 0, y: 0, w: 2, h: 2, bits: vec![1, shape, shape, 1] }
    }

    /// A table that knows the given shapes.
    fn knowing(shapes: &[u8]) -> Table {
        let mut t = Table::default();
        for (n, s) in shapes.iter().enumerate() {
            let i = t.observe(&glyph(*s));
            t.vote(i, &(b'a' + n as u8).to_string());
        }
        t.apply_votes(0.5);
        t
    }

    fn seen(shapes: &[(u8, u64)]) -> BTreeMap<String, u64> {
        shapes.iter().map(|(s, n)| (glyph(*s).key(), *n)).collect()
    }

    #[test]
    fn coverage_is_counted_by_instance_not_by_shape() {
        // A table missing one shape that happens to be "e" is useless; one
        // missing a symbol that appears twice in a film is not. Counting
        // distinct shapes cannot tell those apart.
        let t = knowing(&[1, 2]);
        let common_missing = seen(&[(1, 1), (2, 1), (3, 500)]);
        let rare_missing = seen(&[(1, 500), (2, 500), (3, 2)]);
        assert!(coverage(&t, &common_missing) < 0.1);
        assert!(coverage(&t, &rare_missing) > 0.99);
    }

    #[test]
    fn a_table_for_another_release_is_not_used_at_all() {
        // The Lion King against the Parks and Recreation table: 1% of shapes,
        // and 1,532 lines of placeholders if it is used anyway.
        let fs = FakeFs::new();
        let t = knowing(&[1, 2, 3]);
        fs.write(Path::new("/t/parks.json"), &serde_json::to_vec(&t).unwrap()).unwrap();
        let disc = seen(&[(7, 100), (8, 100), (1, 2)]);
        assert!(best(&fs, &[PathBuf::from("/t/parks.json")], &disc).is_none());
    }

    #[test]
    fn the_table_that_explains_most_of_the_disc_wins() {
        let fs = FakeFs::new();
        for (name, shapes) in [("a.json", &[1u8, 2][..]), ("b.json", &[1, 2, 3, 4][..])] {
            let t = knowing(shapes);
            fs.write(&PathBuf::from("/t").join(name), &serde_json::to_vec(&t).unwrap()).unwrap();
        }
        let disc = seen(&[(1, 10), (2, 10), (3, 10), (4, 10)]);
        let paths = vec![PathBuf::from("/t/a.json"), PathBuf::from("/t/b.json")];
        let (p, _, c) = best(&fs, &paths, &disc).expect("b covers all of it");
        assert_eq!(p, PathBuf::from("/t/b.json"));
        assert!((c - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_shape_the_table_has_but_never_labelled_does_not_count_as_covered() {
        // An unlabelled shape produces a placeholder, which is exactly what
        // this test is meant to prevent, so it cannot count towards fitting.
        let mut t = Table::default();
        t.observe(&glyph(1));
        t.observe(&glyph(2));
        assert_eq!(coverage(&t, &seen(&[(1, 5), (2, 5)])), 0.0);
    }

    #[test]
    fn nonsense_in_the_folder_is_stepped_over_rather_than_fatal() {
        // The folder is on disk and anything can be in it. A half-written table
        // must not stop a disc being ripped.
        let fs = FakeFs::new();
        fs.write(Path::new("/t/broken.json"), b"{ not json").unwrap();
        let good = knowing(&[1]);
        fs.write(Path::new("/t/good.json"), &serde_json::to_vec(&good).unwrap()).unwrap();
        let paths = vec![PathBuf::from("/t/broken.json"), PathBuf::from("/t/good.json")];
        let (p, _, _) = best(&fs, &paths, &seen(&[(1, 5)])).expect("the good one still fits");
        assert_eq!(p, PathBuf::from("/t/good.json"));
    }

    #[test]
    fn a_table_that_nearly_fits_is_still_offered_to_build_on() {
        // The Lion King's own table, learned from its English tracks, covered
        // 97% of an English sample and had never seen an a-ring. Starting the
        // Swedish from nothing would read again for every shape it already
        // had, and throw away any label a person had corrected.
        let fs = FakeFs::new();
        let t = knowing(&[1, 2, 3]);
        fs.write(Path::new("/t/lk.json"), &serde_json::to_vec(&t).unwrap()).unwrap();
        let disc = seen(&[(1, 100), (2, 100), (3, 100), (9, 40)]);
        let paths = vec![PathBuf::from("/t/lk.json")];
        assert!(best(&fs, &paths, &disc).is_none(), "it does not fit");
        let (_, _, covered) = closest(&fs, &paths, &disc).expect("but it is still the closest");
        assert!(covered > 0.8 && covered < FITS, "covered {covered}");
    }

    #[test]
    fn a_built_table_is_named_after_the_disc_it_was_built_for() {
        assert_eq!(
            path_for(Path::new("/t"), "LKD-0E-YW1.1_DES"),
            PathBuf::from("/t/lkd-0e-yw1-1-des.json")
        );
        // a label of nothing but punctuation still has to make a filename
        assert_eq!(path_for(Path::new("/t"), "///"), PathBuf::from("/t/disc.json"));
    }

    #[test]
    fn the_configured_table_is_offered_before_the_ones_built_here() {
        // It was labelled and checked by a person, so where two fit equally it
        // should be the one used.
        let fs = FakeFs::new();
        fs.write(Path::new("/shipped.json"), b"{}").unwrap();
        fs.write(Path::new("/d/z.json"), b"{}").unwrap();
        fs.write(Path::new("/d/a.json"), b"{}").unwrap();
        let got = candidates(&fs, Some(Path::new("/shipped.json")), Some(Path::new("/d")));
        assert_eq!(got[0], PathBuf::from("/shipped.json"));
        assert_eq!(got[1..], [PathBuf::from("/d/a.json"), PathBuf::from("/d/z.json")]);
    }

    #[test]
    fn a_configured_table_that_is_not_there_is_not_offered() {
        let fs = FakeFs::new();
        assert!(candidates(&fs, Some(Path::new("/gone.json")), None).is_empty());
    }
}
