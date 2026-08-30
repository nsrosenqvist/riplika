//! CD-Text: what the disc says about itself.
//!
//! Rare enough not to rely on and common enough to be worth asking, this is
//! the fallback for a disc MusicBrainz has never seen. It names the album, the
//! artist and every track, which is everything needed to file a rip properly -
//! without a network, and without anybody typing.
//!
//! It has to be asked for with a raw SCSI command. The kernel wraps the table
//! of contents in an ioctl of its own but not this, so it goes out through
//! `SG_IO` - the same reason [`disc`](crate::disc) opens the device rather
//! than going through [`Fs`](crate::host::Fs), and with the same seam: the
//! parsing below takes bytes and knows nothing about where they came from.

use std::path::Path;

/// `READ TOC/PMA/ATIP`, asking for the CD-Text format.
const READ_TOC: u8 = 0x43;
const FORMAT_CD_TEXT: u8 = 0x05;

/// Enough for any CD-Text a disc actually carries; the real one measured here
/// came to 616 bytes.
const BUFFER: usize = 8192;

const PACK: usize = 18;
const TITLE: u8 = 0x80;
const PERFORMER: u8 = 0x81;

/// Repeats whatever the previous track said - which is how a single-artist
/// album avoids spelling the name out twelve times.
const REPEAT_PREVIOUS: u8 = 0x09;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CdText {
    pub album: Option<String>,
    pub performer: Option<String>,
    /// By track number, as the disc numbers them.
    pub tracks: Vec<TrackText>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackText {
    pub number: u8,
    pub title: Option<String>,
    pub performer: Option<String>,
}

impl CdText {
    /// Is there anything here worth having?
    ///
    /// A disc can answer the command and say nothing, which is not the same as
    /// refusing it, and neither is worth reporting as a find.
    pub fn is_useful(&self) -> bool {
        self.album.is_some() && self.tracks.iter().any(|t| t.title.is_some())
    }

    pub fn title_of(&self, track: u8) -> Option<&str> {
        self.tracks.iter().find(|t| t.number == track)?.title.as_deref()
    }
}

/// Pull the fields out of a `READ TOC` CD-Text response, header and all.
pub fn parse(response: &[u8]) -> CdText {
    // Two bytes of length, two reserved, then packs of eighteen.
    let Some(body) = response.get(4..) else {
        return CdText::default();
    };
    let stated = response
        .get(..2)
        .map(|h| usize::from(u16::from_be_bytes([h[0], h[1]])))
        .unwrap_or(0)
        .saturating_sub(2);
    let body = &body[..stated.min(body.len())];

    let titles = strings_of(body, TITLE);
    let performers = strings_of(body, PERFORMER);

    let mut text = CdText {
        // Track zero is the disc itself rather than a track on it.
        album: titles.iter().find(|(n, _)| *n == 0).map(|(_, s)| s.clone()),
        performer: performers.iter().find(|(n, _)| *n == 0).map(|(_, s)| s.clone()),
        tracks: Vec::new(),
    };
    for (number, title) in titles.iter().filter(|(n, _)| *n > 0) {
        text.tracks.push(TrackText {
            number: *number,
            title: Some(title.clone()),
            performer: performers
                .iter()
                .find(|(n, _)| n == number)
                .map(|(_, s)| s.clone())
                .or_else(|| text.performer.clone()),
        });
    }
    text
}

/// Every string of one pack type, paired with the track it belongs to.
///
/// The data is one run of NUL-terminated strings spread across the packs, and
/// a string can begin in one pack and end in the next - so the bytes are
/// gathered first and cut afterwards, rather than each pack being read on its
/// own.
fn strings_of(body: &[u8], want: u8) -> Vec<(u8, String)> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut first_track = None;
    for pack in body.chunks_exact(PACK) {
        if pack[0] != want {
            continue;
        }
        // Only the first language block. A disc carrying several says the same
        // thing in each, and picking one beats concatenating them all.
        if (pack[3] >> 4) & 0x07 != 0 {
            continue;
        }
        // Double-byte text, which is a different character set entirely; better
        // to have no title than a mangled one.
        if pack[3] & 0x80 != 0 {
            continue;
        }
        if first_track.is_none() {
            first_track = Some(pack[1]);
        }
        bytes.extend_from_slice(&pack[4..16]);
    }
    let Some(first) = first_track else {
        return Vec::new();
    };

    let mut out: Vec<(u8, String)> = Vec::new();
    let mut track = first;
    let mut previous = String::new();
    for field in bytes.split(|b| *b == 0) {
        let text = if field == [REPEAT_PREVIOUS] {
            previous.clone()
        } else {
            // ISO-8859-1, which is what CD-Text uses unless it says otherwise,
            // and it says otherwise by setting the double-byte bit checked
            // above.
            field.iter().map(|b| *b as char).collect::<String>()
        };
        // The tail of the last pack is padding rather than a run of nameless
        // tracks, but an empty field still belongs to a track - so the count
        // moves on and nothing is recorded.
        if !text.is_empty() {
            previous.clone_from(&text);
            out.push((track, text));
        }
        let Some(next) = track.checked_add(1) else { break };
        track = next;
    }
    out
}

/// Ask the drive. `None` when the disc has no CD-Text, or the drive will not
/// give it up - neither of which is a fault worth reporting.
pub fn read(device: &Path) -> Option<CdText> {
    let response = read_raw(device)?;
    let text = parse(&response);
    text.is_useful().then_some(text)
}

fn read_raw(device: &Path) -> Option<Vec<u8>> {
    use std::os::fd::AsRawFd;

    let file = std::fs::File::open(device).ok()?;
    let mut buffer = vec![0u8; BUFFER];
    let length = (BUFFER as u16).to_be_bytes();
    let mut cdb: [u8; 10] = [READ_TOC, 0, FORMAT_CD_TEXT, 0, 0, 0, 0, length[0], length[1], 0];
    let mut sense = [0u8; 32];

    let mut hdr: SgIoHdr = unsafe { std::mem::zeroed() };
    hdr.interface_id = i32::from(b'S');
    hdr.dxfer_direction = SG_DXFER_FROM_DEV;
    hdr.cmd_len = cdb.len() as u8;
    hdr.mx_sb_len = sense.len() as u8;
    hdr.dxfer_len = buffer.len() as u32;
    hdr.dxferp = buffer.as_mut_ptr().cast();
    hdr.cmdp = cdb.as_mut_ptr();
    hdr.sbp = sense.as_mut_ptr();
    hdr.timeout = 10_000;

    if unsafe { libc::ioctl(file.as_raw_fd(), SG_IO as _, &raw mut hdr) } != 0 {
        return None;
    }
    // A disc with no CD-Text answers with an error rather than an empty list.
    if hdr.status != 0 || hdr.host_status != 0 {
        return None;
    }
    let got = buffer.len().saturating_sub(hdr.resid.max(0) as usize);
    buffer.truncate(got);
    (got >= 4 + PACK).then_some(buffer)
}

const SG_IO: libc::c_ulong = 0x2285;
const SG_DXFER_FROM_DEV: i32 = -3;

/// Mirrors the kernel's `struct sg_io_hdr`.
#[repr(C)]
struct SgIoHdr {
    interface_id: i32,
    dxfer_direction: i32,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut libc::c_void,
    cmdp: *mut u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: i32,
    usr_ptr: *mut libc::c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: i32,
    duration: u32,
    info: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    /// What `/dev/sr0` actually returned for Shawn McDonald's "Roots" - header,
    /// packs, padding and all. Recorded rather than invented, because the parts
    /// that go wrong here are the ones a hand-built fixture would tidy away:
    /// strings that begin in one pack and end in the next, and a performer
    /// field that says "same as the last one" eleven times.
    const ROOTS: &str = concat!(
        "AmYAAIAAAABSb290cwBDbGFyaXQMq4ABAQZ5AENhcHRpdmF0ZWQgx4ACAgoAV2FzaCBNZSBDbGW1KoADAwthbgBT",
        "aGFkb3dsYW6EcIAEBAlkcwBMaWdodABXYWy+b4AGBQN0eiBJbiAzAFJvb3RqeoAHBgRzAFNsb3cgRG93bgCGMoAJ",
        "BwBHcmVlZABUaW1lAFe5CIALCAFpbnRlcgBIYWxsZWw8OYAMCQZ1amFoAAAAAAAAAADHE4EACgBTaGF3biBNY0Rv",
        "bmE2GIEACwxsZABTaGF3biBNY0TxuYEBDAlvbmFsZABTaGF3biCE+YECDQZNY0RvbmFsZABTaGE0nIEDDgN3biBN",
        "Y0RvbmFsZACJ5YEEDwBTaGF3biBNY0RvbmErboEEEAxsZABTaGF3biBNY0TKNIEFEQlvbmFsZABTaGF3biC0E4EG",
        "EgZNY0RvbmFsZABTaGHytIEHEwN3biBNY0RvbmFsZAC5D4EIFABTaGF3biBNY0RvbmE3poEIFQxsZABTaGF3biBN",
        "Y0TwB4EJFglvbmFsZABTaGF3biB44oEKFwZNY0RvbmFsZABTaGHIh4ELGAN3biBNY0RvbmFsZABjMIEMGQBTaGF3",
        "biBNY0RvbmHBu4EMGgxsZAAAAAAAAAAAAAAsgI0AGwBNYXN0ZXJlZCB1c2nAY40AHAxuZyBTQURpRSB2NS6G/40A",
        "HQ82LjEAAAAAAAAAAADN540JHgAAAAAAAAAAAAAAAABhv48AHwAAAQwAChEAAAAAAADyX48BIAAAAAAAAAQAAyEA",
        "AADTSY8CIQAAAAAACQAAAAAAAAB86g=="
    );

    fn roots() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.decode(ROOTS).expect("fixture decodes")
    }

    #[test]
    fn the_disc_names_itself() {
        let text = parse(&roots());
        assert_eq!(text.album.as_deref(), Some("Roots"));
        assert_eq!(text.performer.as_deref(), Some("Shawn McDonald"));
    }

    #[test]
    fn a_title_split_across_two_packs_comes_back_whole() {
        // "Clarity" ends six bytes into the second pack; reading each pack on
        // its own would give "Clarit" and a stray "y".
        let text = parse(&roots());
        assert_eq!(text.title_of(1), Some("Clarity"));
        assert_eq!(text.title_of(2), Some("Captivated"));
    }

    #[test]
    fn every_track_on_the_disc_is_named() {
        let text = parse(&roots());
        assert_eq!(text.tracks.len(), 12);
        assert_eq!(text.title_of(12), Some("Hallelujah"));
        assert_eq!(text.title_of(6), Some("Waltz In 3"));
    }

    #[test]
    fn the_tracks_are_numbered_from_one_and_not_from_the_album() {
        // Track zero is the disc itself, and counting it as a track would put
        // every title one place out.
        let text = parse(&roots());
        assert_eq!(text.tracks.first().map(|t| t.number), Some(1));
        assert!(text.tracks.iter().all(|t| t.number > 0));
    }

    #[test]
    fn a_performer_that_says_same_again_is_filled_in() {
        let text = parse(&roots());
        for t in &text.tracks {
            assert_eq!(t.performer.as_deref(), Some("Shawn McDonald"), "track {}", t.number);
        }
    }

    #[test]
    fn this_is_worth_having() {
        assert!(parse(&roots()).is_useful());
    }

    #[test]
    fn a_disc_that_answers_with_nothing_is_not_a_find() {
        assert!(!parse(&[0, 2, 0, 0]).is_useful());
        assert!(!parse(&[]).is_useful());
        assert!(!CdText::default().is_useful());
    }

    #[test]
    fn a_truncated_response_is_parsed_as_far_as_it_goes_rather_than_panicking() {
        let full = roots();
        for cut in [0, 1, 3, 4, 5, 21, 40, 100] {
            let _ = parse(&full[..cut.min(full.len())]);
        }
    }

    #[test]
    fn a_response_claiming_more_than_it_carries_does_not_read_past_the_end() {
        let mut short = roots();
        short.truncate(40);
        // The header still says 614 bytes are coming.
        let text = parse(&short);
        assert_eq!(text.album.as_deref(), Some("Roots"));
    }

    fn pack(kind: u8, track: u8, seq: u8, flags: u8, data: &[u8; 12]) -> Vec<u8> {
        let mut p = vec![kind, track, seq, flags];
        p.extend_from_slice(data);
        p.extend_from_slice(&[0, 0]); // the CRC, which nothing here checks
        p
    }

    fn response(packs: &[Vec<u8>]) -> Vec<u8> {
        let len = (packs.len() * PACK + 2) as u16;
        let mut out = len.to_be_bytes().to_vec();
        out.extend_from_slice(&[0, 0]);
        for p in packs {
            out.extend_from_slice(p);
        }
        out
    }

    #[test]
    fn only_the_first_language_block_is_taken() {
        // A bilingual disc says the same thing twice; taking both would give a
        // title with the German glued onto the end of the English.
        let english = pack(TITLE, 0, 0, 0x00, b"Hello\0One\0\0\0");
        let german = pack(TITLE, 0, 0, 0x10, b"Hallo\0Eins\0\0");
        let text = parse(&response(&[english, german]));
        assert_eq!(text.album.as_deref(), Some("Hello"));
        assert_eq!(text.title_of(1), Some("One"));
    }

    #[test]
    fn double_byte_text_is_left_alone_rather_than_mangled() {
        // A different character set entirely: better no title than nonsense.
        let wide = pack(TITLE, 0, 0, 0x80, b"\0R\0o\0o\0t\0s\0\0");
        assert_eq!(parse(&response(&[wide])).album, None);
    }
}
