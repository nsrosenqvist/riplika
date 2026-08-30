//! Writing Matroska tags, with the targets Matroska actually defines.
//!
//! ffmpeg can carry metadata into a file but not the shape Matroska wants it
//! in. Its muxer writes every `-metadata` pair into one tag with an empty
//! `Targets`, so an episode ends up saying `SEASON_NUMBER=6` - which is MP4's
//! vocabulary, flat, in a format whose whole tag design is about saying which
//! level a fact belongs to. In MP4 those same names are right, because they
//! are what the iTunes atoms are called, so the answer is not to change them
//! everywhere but to write each container the way that container means it.
//!
//! Matroska nests by `TargetTypeValue`: 70 for the collection a thing belongs
//! to, 60 for the season, 50 for the episode or film itself. The show's name
//! is a fact about the collection, the season number about the season, the air
//! date about the episode, and saying so is the point.
//!
//! Written here rather than by calling `mkvpropedit`, which would do it in one
//! line and drag mkvtoolnix back into a Flatpak that was deliberately built
//! without it.

use crate::host::Fs;
use crate::model::{Item, Media, Role};
use crate::{Error, Result};
use std::path::Path;

// Element ids, with their length markers, as the format stores them.
const TAGS: &[u8] = &[0x12, 0x54, 0xC3, 0x67];
const TAG: &[u8] = &[0x73, 0x73];
const TARGETS: &[u8] = &[0x63, 0xC0];
const TARGET_TYPE_VALUE: &[u8] = &[0x68, 0xCA];
const TARGET_TYPE: &[u8] = &[0x63, 0xCA];
const SIMPLE_TAG: &[u8] = &[0x67, 0xC8];
const TAG_NAME: &[u8] = &[0x45, 0xA3];
const TAG_LANGUAGE: &[u8] = &[0x44, 0x7A];
const TAG_DEFAULT: &[u8] = &[0x44, 0x84];
const TAG_STRING: &[u8] = &[0x44, 0x87];
const SEGMENT: &[u8] = &[0x18, 0x53, 0x80, 0x67];
const SEEK_HEAD: &[u8] = &[0x11, 0x4D, 0x9B, 0x74];
const SEEK: &[u8] = &[0x4D, 0xBB];
const SEEK_ID: &[u8] = &[0x53, 0xAB];
const SEEK_POSITION: &[u8] = &[0x53, 0xAC];
const VOID: &[u8] = &[0xEC];

/// A size, in the variable-width form EBML stores lengths as.
fn vint(value: u64) -> Vec<u8> {
    for width in 1..=8usize {
        // The all-ones value at each width is reserved for "unknown", so the
        // largest a length may be is one below it.
        let bits = 7 * width as u32;
        if value <= (1u64 << bits) - 2 {
            return vint_fixed(value, width).expect("fits by construction");
        }
    }
    unreachable!("a tag element cannot be that long")
}

/// The same, forced to a width, for rewriting a size already in a file.
fn vint_fixed(value: u64, width: usize) -> Option<Vec<u8>> {
    let bits = 7 * width as u32;
    if bits < 64 && value > (1u64 << bits) - 2 {
        return None;
    }
    let mut out = vec![0u8; width];
    let mut v = value;
    for i in (0..width).rev() {
        out[i] = (v & 0xFF) as u8;
        v >>= 8;
    }
    out[0] |= 1u8 << (8 - width);
    Some(out)
}

fn element(id: &[u8], body: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend_from_slice(&vint(body.len() as u64));
    out.extend_from_slice(body);
    out
}

fn uint(id: &[u8], value: u64) -> Vec<u8> {
    let mut body = value.to_be_bytes().to_vec();
    while body.len() > 1 && body[0] == 0 {
        body.remove(0);
    }
    element(id, &body)
}

fn utf8(id: &[u8], value: &str) -> Vec<u8> {
    element(id, value.as_bytes())
}

/// One `SimpleTag`: a name, a value, and the two fields readers expect.
fn simple(name: &str, value: &str) -> Vec<u8> {
    let mut body = utf8(TAG_NAME, name);
    // "und" and 1 are the defaults, and writing them costs six bytes and saves
    // arguing with readers that do not apply defaults.
    body.extend_from_slice(&utf8(TAG_LANGUAGE, "und"));
    body.extend_from_slice(&uint(TAG_DEFAULT, 1));
    body.extend_from_slice(&utf8(TAG_STRING, value));
    element(SIMPLE_TAG, &body)
}

/// One `Tag`: what level it is about, and what it says at that level.
fn tag(level: u64, kind: &str, pairs: &[(&str, String)]) -> Option<Vec<u8>> {
    if pairs.is_empty() {
        return None;
    }
    let mut targets = uint(TARGET_TYPE_VALUE, level);
    targets.extend_from_slice(&utf8(TARGET_TYPE, kind));
    let mut body = element(TARGETS, &targets);
    for (name, value) in pairs {
        body.extend_from_slice(&simple(name, value));
    }
    Some(element(TAG, &body))
}

/// The whole `Tags` element for one produced file.
///
/// Returns nothing when there is nothing worth saying, so a file is not given
/// an empty element for the sake of it.
pub fn tags_element(media: &Media, item: &Item) -> Option<Vec<u8>> {
    let mut tags: Vec<u8> = Vec::new();

    match (media, &item.role) {
        (Media::Series { title, year, .. }, Role::Episode { season, number })
        | (Media::Series { title, year, .. }, Role::ExtendedCut { season, number }) => {
            let mut collection = vec![("TITLE", title.clone())];
            if let Some(y) = year {
                collection.push(("DATE_RELEASED", y.to_string()));
            }
            collection.push(("CONTENT_TYPE", "Television".to_string()));
            if let Some(t) = tag(70, "COLLECTION", &collection) {
                tags.extend_from_slice(&t);
            }
            if let Some(t) = tag(60, "SEASON", &[("PART_NUMBER", season.to_string())]) {
                tags.extend_from_slice(&t);
            }
            let name = if matches!(item.role, Role::ExtendedCut { .. }) {
                format!("{} (Extended Cut)", item.title)
            } else {
                item.title.clone()
            };
            let mut episode = vec![("TITLE", name), ("PART_NUMBER", number.to_string())];
            if let Some(d) = &item.air_date {
                episode.push(("DATE_RELEASED", d.clone()));
            }
            if let Some(t) = tag(50, "EPISODE", &episode) {
                tags.extend_from_slice(&t);
            }
        }
        (Media::Movie { title, year, .. }, _) => {
            let mut film = vec![("TITLE", title.clone())];
            if let Some(y) = year {
                film.push(("DATE_RELEASED", y.to_string()));
            }
            film.push(("CONTENT_TYPE", "Movie".to_string()));
            if let Some(t) = tag(50, "MOVIE", &film) {
                tags.extend_from_slice(&t);
            }
        }
        // Bonus material is not an episode of anything. It gets its own name
        // and the collection it came off, which is all that is true about it.
        (Media::Series { title, .. }, _) => {
            if let Some(t) = tag(70, "COLLECTION", &[("TITLE", title.clone())]) {
                tags.extend_from_slice(&t);
            }
            if !item.title.is_empty()
                && let Some(t) = tag(50, "EPISODE", &[("TITLE", item.title.clone())])
            {
                tags.extend_from_slice(&t);
            }
        }
    }

    (!tags.is_empty()).then(|| element(TAGS, &tags))
}

/// Where the Segment's length is written, and how wide that field is.
///
/// Everything in a Matroska file lives inside one Segment whose length is
/// recorded up front, so adding anything to the end means correcting it.
fn segment_length_field(head: &[u8]) -> Option<(usize, usize, u64)> {
    let mut at = 0usize;
    while at + 4 <= head.len() {
        // Top-level ids here are four bytes (EBML header, then Segment).
        let id = &head[at..at + 4];
        let size_at = at + 4;
        let first = *head.get(size_at)?;
        let width = (first.leading_zeros() + 1) as usize;
        if width > 8 || size_at + width > head.len() {
            return None;
        }
        let mut value = if width == 8 { 0 } else { (first & (0xFF >> width)) as u64 };
        for b in &head[size_at + 1..size_at + width] {
            value = (value << 8) | *b as u64;
        }
        if id == SEGMENT {
            return Some((size_at, width, value));
        }
        // Not the Segment, so step over it: only the EBML header precedes it.
        at = size_at + width + value as usize;
    }
    None
}

/// Is this length the "unknown" form - all ones after the width marker?
fn is_unknown(value: u64, width: usize) -> bool {
    let bits = 7 * width as u32;
    bits < 64 && value == (1u64 << bits) - 1
}

/// Write the tags for one file into it.
///
/// Appended rather than woven in: everything already written keeps the offset
/// it had, which matters because the cue points and cluster positions
/// elsewhere in the file are absolute. Matroska allows more than one Tags
/// element, and a reader concatenates them.
///
/// Returns whether anything was written.
pub fn write(fs: &dyn Fs, path: &Path, media: &Media, item: &Item) -> Result<bool> {
    let Some(element) = tags_element(media, item) else { return Ok(false) };

    let head = fs.read_range(path, 0, 4096)?;
    let Some((size_at, width, current)) = segment_length_field(&head) else {
        return Err(Error(format!("{}: no Matroska segment", path.display())));
    };

    let body = (size_at + width) as u64;
    let tags_at = fs.size(path)? - body;

    if !is_unknown(current, width) {
        let grown = current + element.len() as u64;
        let Some(patched) = vint_fixed(grown, width) else {
            // The length would need a wider field than the file left room for,
            // which would mean moving everything after it.
            return Err(Error(format!("{}: segment length cannot grow", path.display())));
        };
        // Append first: a file with a length that is too short still reads,
        // where one claiming bytes that are not there does not.
        fs.append(path, &element)?;
        fs.write_at(path, size_at as u64, &patched)?;
    } else {
        fs.append(path, &element)?;
    }

    // And say where it went, or nothing will look.
    point_seek_head_at(fs, path, body, tags_at)?;
    Ok(true)
}

/// A `Void` element of exactly this many bytes, padding included.
fn void_of(total: usize) -> Option<Vec<u8>> {
    // One byte of id, then a length, then that many bytes of nothing.
    for width in 1..=8usize {
        let payload = total.checked_sub(1 + width)?;
        let encoded = vint_fixed(payload as u64, width)?;
        if encoded.len() == width && 1 + width + payload == total {
            let mut out = VOID.to_vec();
            out.extend_from_slice(&encoded);
            out.extend(std::iter::repeat_n(0u8, payload));
            return Some(out);
        }
    }
    None
}

/// Read one element's id, header length and body length at `at`.
fn element_at(d: &[u8], at: usize) -> Option<(Vec<u8>, usize, usize)> {
    let first = *d.get(at)?;
    let idw = (first.leading_zeros() + 1) as usize;
    if idw > 4 || at + idw >= d.len() {
        return None;
    }
    let id = d[at..at + idw].to_vec();
    let sf = *d.get(at + idw)?;
    let w = (sf.leading_zeros() + 1) as usize;
    if w > 8 || at + idw + w > d.len() {
        return None;
    }
    let mut size = if w == 8 { 0 } else { (sf & (0xFF >> w)) as u64 };
    for b in &d[at + idw + 1..at + idw + w] {
        size = (size << 8) | *b as u64;
    }
    Some((id, idw + w, size as usize))
}

/// Point the seek head at the tags, so a reader looks for them.
///
/// Everything a Matroska file keeps after its clusters is found through the
/// seek head; a reader does not scan to the end hoping. An element appended
/// without an entry here is in the file, is structurally valid, and is invisible
/// to every player - which is exactly what happened the first time.
///
/// The entry has to be added without moving anything, so it is taken out of the
/// `Void` that muxers leave after the seek head for the purpose: the seek head
/// grows by what the entry costs and the void shrinks by the same.
fn point_seek_head_at(fs: &dyn Fs, path: &Path, body: u64, tags_at: u64) -> Result<()> {
    let window = fs.read_range(path, body, 8192)?;
    let Some((id, head, size)) = element_at(&window, 0) else {
        return Err(Error(format!("{}: no seek head", path.display())));
    };
    if id != SEEK_HEAD {
        return Err(Error(format!("{}: no seek head", path.display())));
    }
    let old_total = head + size;
    let Some((void_id, void_head, void_size)) = element_at(&window, old_total) else {
        return Err(Error(format!("{}: nothing spare after the seek head", path.display())));
    };
    if void_id != VOID {
        return Err(Error(format!("{}: nothing spare after the seek head", path.display())));
    }

    let mut entry = element(SEEK_ID, TAGS);
    entry.extend_from_slice(&uint(SEEK_POSITION, tags_at));
    let entry = element(SEEK, &entry);

    let mut grown = window[head..old_total].to_vec();
    grown.extend_from_slice(&entry);
    let grown = element(SEEK_HEAD, &grown);

    let spare = old_total + void_head + void_size;
    let Some(remaining) = spare.checked_sub(grown.len()) else {
        return Err(Error(format!("{}: no room to note the tags", path.display())));
    };
    let Some(void) = void_of(remaining) else {
        return Err(Error(format!("{}: no room to note the tags", path.display())));
    };

    let mut patch = grown;
    patch.extend_from_slice(&void);
    debug_assert_eq!(patch.len(), spare, "the seek head must not move what follows it");
    fs.write_at(path, body, &patch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::FakeFs;
    use std::path::PathBuf;

    /// Walk the element back out, so the bytes are checked and not just built.
    fn parse(d: &[u8], at: usize, end: usize, out: &mut Vec<(String, String)>) {
        let mut i = at;
        while i < end {
            let idw = if d[i] >= 0x80 {
                1
            } else if d[i] >= 0x40 {
                2
            } else if d[i] >= 0x20 {
                3
            } else {
                4
            };
            let id = &d[i..i + idw];
            let first = d[i + idw];
            let w = (first.leading_zeros() + 1) as usize;
            let mut size = if w == 8 { 0 } else { (first & (0xFF >> w)) as u64 };
            for b in &d[i + idw + 1..i + idw + w] {
                size = (size << 8) | *b as u64;
            }
            let body = i + idw + w;
            let stop = body + size as usize;
            match id {
                TAGS | TAG | TARGETS | SIMPLE_TAG => parse(d, body, stop, out),
                TARGET_TYPE_VALUE => {
                    let mut v = 0u64;
                    for b in &d[body..stop] {
                        v = (v << 8) | *b as u64;
                    }
                    out.push(("@level".into(), v.to_string()));
                }
                TARGET_TYPE | TAG_NAME | TAG_STRING => {
                    let text = String::from_utf8_lossy(&d[body..stop]).into_owned();
                    let key = if id == TARGET_TYPE {
                        "@type"
                    } else if id == TAG_NAME {
                        "name"
                    } else {
                        "value"
                    };
                    out.push((key.into(), text));
                }
                _ => {}
            }
            i = stop;
        }
    }

    fn flatten(bytes: &[u8]) -> Vec<(String, String)> {
        let mut out = Vec::new();
        parse(bytes, 0, bytes.len(), &mut out);
        out
    }

    fn episode() -> (Media, Item) {
        let media = Media::Series {
            title: "Parks and Recreation".into(),
            year: Some(2009),
            season: 6,
            provider_id: None,
        };
        let item = Item {
            source: PathBuf::from("/rip/title_t06.mkv"),
            role: Role::Episode { season: 6, number: 1 },
            title: "London (1)".into(),
            air_date: Some("2013-09-26".into()),
            duration: 1_293_000,
            destination: None,
        };
        (media, item)
    }

    #[test]
    fn each_fact_is_filed_at_the_level_it_is_about() {
        // The point of Matroska's targets, and what ffmpeg cannot write: the
        // show's name belongs to the collection, the season number to the
        // season, the air date to the episode.
        let (m, i) = episode();
        let flat = flatten(&tags_element(&m, &i).unwrap());
        let joined: Vec<String> = flat.iter().map(|(k, v)| format!("{k}={v}")).collect();
        let text = joined.join(" ");
        assert!(text.contains("@level=70 @type=COLLECTION"), "{text}");
        assert!(text.contains("@level=60 @type=SEASON"), "{text}");
        assert!(text.contains("@level=50 @type=EPISODE"), "{text}");

        // and each name lands under the right one
        let mut level = String::new();
        let mut seen: Vec<(String, String, String)> = Vec::new();
        let mut name = String::new();
        for (k, v) in &flat {
            match k.as_str() {
                "@level" => level = v.clone(),
                "name" => name = v.clone(),
                "value" => seen.push((level.clone(), name.clone(), v.clone())),
                _ => {}
            }
        }
        assert!(seen.contains(&("70".into(), "TITLE".into(), "Parks and Recreation".into())));
        assert!(seen.contains(&("70".into(), "CONTENT_TYPE".into(), "Television".into())));
        assert!(seen.contains(&("60".into(), "PART_NUMBER".into(), "6".into())));
        assert!(seen.contains(&("50".into(), "TITLE".into(), "London (1)".into())));
        assert!(seen.contains(&("50".into(), "PART_NUMBER".into(), "1".into())));
        assert!(seen.contains(&("50".into(), "DATE_RELEASED".into(), "2013-09-26".into())));
    }

    #[test]
    fn a_film_is_a_film_and_not_an_episode_of_nothing() {
        let media = Media::Movie { title: "Heat".into(), year: Some(1995), provider_id: None };
        let item = Item {
            source: PathBuf::from("/rip/title_t01.mkv"),
            role: Role::Feature,
            title: "Heat".into(),
            air_date: None,
            duration: 10_000_000,
            destination: None,
        };
        let text: String = flatten(&tags_element(&media, &item).unwrap())
            .iter()
            .map(|(k, v)| format!("{k}={v} "))
            .collect();
        assert!(text.contains("@type=MOVIE"), "{text}");
        assert!(!text.contains("SEASON"), "{text}");
        assert!(text.contains("value=1995"), "{text}");
    }

    #[test]
    fn an_extended_cut_says_so_in_its_title() {
        let (m, mut i) = episode();
        i.role = Role::ExtendedCut { season: 6, number: 1 };
        let text: String = flatten(&tags_element(&m, &i).unwrap())
            .iter()
            .map(|(k, v)| format!("{k}={v} "))
            .collect();
        assert!(text.contains("London (1) (Extended Cut)"), "{text}");
    }

    #[test]
    fn lengths_are_written_the_way_ebml_reads_them() {
        assert_eq!(vint(0), vec![0x80]);
        assert_eq!(vint(1), vec![0x81]);
        assert_eq!(vint(126), vec![0xFE]);
        // 127 is the one-byte "unknown", so a real length steps up a width
        assert_eq!(vint(127), vec![0x40, 0x7F]);
        assert_eq!(vint(0x3FFE), vec![0x7F, 0xFE]);
        assert_eq!(vint_fixed(1, 8).unwrap()[0], 0x01);
    }

    /// A file shaped the way a muxer writes one: a header, then a segment
    /// beginning with a seek head and the spare room muxers leave after it.
    fn skeleton() -> Vec<u8> {
        let mut seek_head = element(SEEK_HEAD, &{
            let mut e = element(SEEK_ID, &[0x15, 0x49, 0xA9, 0x66]); // Info
            e.extend_from_slice(&uint(SEEK_POSITION, 200));
            element(SEEK, &e)
        });
        seek_head.extend_from_slice(&void_of(66).unwrap());
        let mut segment_body = seek_head;
        segment_body.extend_from_slice(&[9u8; 64]);

        let mut file = vec![0x1A, 0x45, 0xDF, 0xA3, 0x84, 1, 2, 3, 4];
        file.extend_from_slice(SEGMENT);
        file.extend_from_slice(&vint_fixed(segment_body.len() as u64, 8).unwrap());
        file.extend_from_slice(&segment_body);
        file
    }

    #[test]
    fn the_segment_length_is_found_and_grown() {
        let file = skeleton();
        let (at, width, value) = segment_length_field(&file).unwrap();
        assert_eq!(width, 8);
        assert_eq!(at + width + value as usize, file.len(), "the length must cover the segment");
        assert!(!is_unknown(value, width));
    }

    #[test]
    fn a_void_is_made_to_an_exact_size() {
        // it must fill precisely the room the seek head gave up, or everything
        // after it moves
        for total in 2..80usize {
            assert_eq!(void_of(total).unwrap().len(), total, "void of {total}");
        }
        assert!(void_of(1).is_none(), "a void needs an id and a length");
    }

    #[test]
    fn writing_tags_leaves_every_existing_byte_alone() {
        // Cue points and cluster positions in a Matroska file are absolute, so
        // anything that moved a byte would break playback rather than tagging.
        let before = skeleton();
        let fs = FakeFs::new();
        let path = Path::new("/out/ep.mkv.part");
        fs.write(path, &before).unwrap();
        let (m, i) = episode();
        assert!(write(&fs, path, &m, &i).unwrap());

        let after = fs.read(path).unwrap();
        assert!(after.len() > before.len(), "nothing was written");
        // the payload after the seek head's spare room is exactly where it was
        assert_eq!(after[before.len() - 64..before.len()], before[before.len() - 64..]);
        // and the segment claims exactly what follows it
        let (at, w, v) = segment_length_field(&after).unwrap();
        assert_eq!(at + w + v as usize, after.len(), "the segment length is wrong");
    }

    #[test]
    fn the_seek_head_is_told_where_the_tags_went() {
        // Without this the element is in the file, structurally valid, and
        // invisible: a reader finds what follows the clusters through the seek
        // head rather than scanning to the end. That is what happened first.
        let fs = FakeFs::new();
        let path = Path::new("/out/ep.mkv.part");
        fs.write(path, &skeleton()).unwrap();
        let (m, i) = episode();
        write(&fs, path, &m, &i).unwrap();
        let after = fs.read(path).unwrap();

        let (size_at, width, _) = segment_length_field(&after).unwrap();
        let body = size_at + width;
        let (id, head, size) = element_at(&after[body..], 0).unwrap();
        assert_eq!(id, SEEK_HEAD);
        let entries = &after[body + head..body + head + size];
        assert!(entries.windows(4).any(|w| w == TAGS), "the seek head does not mention the tags");

        // and where it says they are is where they are
        let element_len = tags_element(&m, &i).unwrap().len();
        let tags_at = after.len() - body - element_len;
        let (found, _, _) = element_at(&after[body + tags_at..], 0).unwrap();
        assert_eq!(found, TAGS, "the recorded position is not where the tags are");
    }

    #[test]
    fn a_file_that_is_not_matroska_is_refused_rather_than_corrupted() {
        let fs = FakeFs::new();
        let path = Path::new("/out/ep.mp4");
        fs.write(path, b"\x00\x00\x00\x20ftypisom").unwrap();
        let (m, i) = episode();
        assert!(write(&fs, path, &m, &i).is_err());
    }
}
