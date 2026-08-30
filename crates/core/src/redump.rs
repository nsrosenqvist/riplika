//! Redump datfiles: what a correct dump of a disc weighs and hashes to.
//!
//! This is not a catalogue in the sense the film and music ones are. There is
//! no query, no network at the point of use, and nothing to search by name: a
//! datfile is a list of every disc a preservation project has verified, and the
//! only question it answers is "is this image byte-for-byte one of them".
//!
//! Which makes identification and verification the same act. A hit gives the
//! disc's canonical name *and* proves the dump is whole - and for a game that
//! second half matters more than it does anywhere else in this program, because
//! copy protection deliberately writes unreadable sectors, and an image that is
//! quietly missing them looks exactly like one that is not.
//!
//! One limit worth knowing: a disc with audio tracks is several files here, one
//! per track plus a cue sheet, so a single flat image of such a disc matches
//! nothing. [`Game::is_multi_track`] says which those are.

use crate::hash::Digests;
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rom {
    pub name: String,
    pub size: u64,
    pub crc32: Option<u32>,
    pub sha1: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Game {
    pub name: String,
    pub roms: Vec<Rom>,
}

impl Game {
    /// A disc whose data is spread over several files - tracks and a cue sheet.
    ///
    /// One flat image cannot match such a disc, because Redump never made one.
    pub fn is_multi_track(&self) -> bool {
        self.roms.iter().filter(|r| !r.name.to_lowercase().ends_with(".cue")).count() > 1
    }

    /// What the disc is called, without the region and revision in brackets.
    pub fn short_name(&self) -> &str {
        self.name.split(" (").next().unwrap_or(&self.name).trim()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dat {
    /// The system, e.g. "Sony - PlayStation".
    pub name: String,
    pub version: Option<String>,
    pub games: Vec<Game>,
}

/// A dumped image, recognised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found<'a> {
    pub game: &'a Game,
    pub rom: &'a Rom,
}

impl Dat {
    /// Find the disc this image is.
    ///
    /// Size first because it is free and rules out all but a handful, then the
    /// hash, which is what actually decides. A CRC32 is short enough to collide
    /// on purpose, so it is only trusted when the entry carries nothing better.
    pub fn find(&self, digests: &Digests) -> Option<Found<'_>> {
        let mut crc_only = None;
        for game in &self.games {
            for rom in &game.roms {
                if rom.size != digests.bytes {
                    continue;
                }
                match &rom.sha1 {
                    Some(sha1) if sha1.eq_ignore_ascii_case(&digests.sha1_hex()) => {
                        return Some(Found { game, rom });
                    }
                    Some(_) => continue,
                    None if rom.crc32 == Some(digests.crc32) => {
                        crc_only.get_or_insert(Found { game, rom });
                    }
                    None => continue,
                }
            }
        }
        crc_only
    }

    /// A game whose every track matches, in order.
    ///
    /// A disc with audio on it is only right when all of it is right, and one
    /// track matching proves nothing about the rest - a mis-cut boundary
    /// leaves track one perfect and everything after it shifted.
    pub fn find_all(&self, tracks: &[Digests]) -> Option<&Game> {
        self.games.iter().find(|game| {
            let roms: Vec<&Rom> =
                game.roms.iter().filter(|r| !r.name.to_lowercase().ends_with(".cue")).collect();
            roms.len() == tracks.len()
                && roms.iter().zip(tracks).all(|(rom, got)| matches(rom, got))
        })
    }

    pub fn len(&self) -> usize {
        self.games.len()
    }

    pub fn is_empty(&self) -> bool {
        self.games.is_empty()
    }
}

/// The systems a disc drive in a PC can actually read, with the names
/// redump.org files them under.
///
/// Not the whole list, deliberately. A GameCube or Xbox disc is a format an
/// ordinary drive cannot read at all, so offering to fetch its datfile would
/// promise something this program cannot do.
pub const SYSTEMS: &[(&str, &str)] = &[
    ("pc", "IBM PC compatible"),
    ("mac", "Apple Macintosh"),
    ("psx", "Sony PlayStation"),
    ("ps2", "Sony PlayStation 2"),
    ("pce", "NEC PC Engine CD & TurboGrafx CD"),
    ("ss", "Sega Saturn"),
    ("mcd", "Sega Mega CD & Sega CD"),
    ("3do", "3DO Interactive Multiplayer"),
    ("cdi", "Philips CD-i"),
    ("cd32", "Commodore Amiga CD32"),
    ("ngcd", "SNK Neo Geo CD"),
];

/// Where a system's datfile is downloaded from.
pub fn datfile_url(system: &str) -> String {
    format!("http://redump.org/datfile/{system}/")
}

/// The English name for a system slug, if it is one we list.
pub fn system_name(slug: &str) -> Option<&'static str> {
    SYSTEMS.iter().find(|(s, _)| *s == slug).map(|(_, name)| *name)
}

/// Every datfile in a directory, in name order.
///
/// A datfile covers one system, so a collection means several - and which
/// systems somebody has is their business, so the directory is read rather
/// than a list being kept.
pub fn load_all(fs: &dyn crate::host::Fs, dir: &std::path::Path) -> Vec<(std::path::PathBuf, Dat)> {
    let mut found = Vec::new();
    let Ok(entries) = fs.list(dir) else { return found };
    let mut paths: Vec<_> = entries
        .into_iter()
        .filter(|p| {
            p.extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("dat") || e.eq_ignore_ascii_case("xml"))
        })
        .collect();
    paths.sort();
    for path in paths {
        // One unreadable datfile should not hide the rest.
        if let Ok(bytes) = fs.read(&path)
            && let Ok(dat) = parse(&String::from_utf8_lossy(&bytes))
        {
            found.push((path, dat));
        }
    }
    found
}

/// Read a datfile.
///
/// Written by hand rather than with an XML library because the shape is fixed
/// and tiny - a header, then games, then roms - and the alternative is a
/// dependency that would be used for this one file.
pub fn parse(xml: &str) -> Result<Dat> {
    let mut dat = Dat { name: header_field(xml, "name"), version: None, games: Vec::new() };
    dat.version = Some(header_field(xml, "version")).filter(|v| !v.is_empty());

    let mut rest = xml;
    while let Some(at) = rest.find("<game ") {
        rest = &rest[at..];
        let Some(open_end) = rest.find('>') else { break };
        let name = attribute(&rest[..open_end], "name").unwrap_or_default();
        // A game with no closing tag is a truncated file; take what is left.
        let body_end = rest.find("</game>").unwrap_or(rest.len());
        let body = &rest[open_end..body_end];

        let mut roms = Vec::new();
        let mut scan = body;
        while let Some(r) = scan.find("<rom ") {
            scan = &scan[r..];
            let end = scan.find('>').unwrap_or(scan.len());
            let tag = &scan[..end];
            if let Some(rom) = rom_of(tag) {
                roms.push(rom);
            }
            scan = &scan[end.min(scan.len())..];
            if scan.is_empty() {
                break;
            }
            scan = &scan[1.min(scan.len())..];
        }
        if !name.is_empty() {
            dat.games.push(Game { name, roms });
        }
        rest = &rest[body_end.min(rest.len())..];
        if rest.is_empty() {
            break;
        }
        rest = &rest[1.min(rest.len())..];
    }

    if dat.games.is_empty() {
        return Err(Error("no games in this datfile; is it a Redump datfile?".into()));
    }
    Ok(dat)
}

/// Does one file match one entry? Size first because it is free.
fn matches(rom: &Rom, got: &Digests) -> bool {
    if rom.size != got.bytes {
        return false;
    }
    match &rom.sha1 {
        Some(sha1) => sha1.eq_ignore_ascii_case(&got.sha1_hex()),
        None => rom.crc32 == Some(got.crc32),
    }
}

fn rom_of(tag: &str) -> Option<Rom> {
    let name = attribute(tag, "name")?;
    Some(Rom {
        name,
        size: attribute(tag, "size")?.parse().ok()?,
        crc32: attribute(tag, "crc").and_then(|c| u32::from_str_radix(&c, 16).ok()),
        sha1: attribute(tag, "sha1"),
    })
}

/// One `<name>value</name>` out of the header.
fn header_field(xml: &str, field: &str) -> String {
    let Some(header) = section(xml, "header") else { return String::new() };
    let open = format!("<{field}>");
    let close = format!("</{field}>");
    let Some(start) = header.find(&open) else { return String::new() };
    let after = &header[start + open.len()..];
    let Some(end) = after.find(&close) else { return String::new() };
    unescape(&after[..end])
}

fn section<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let start = xml.find(&format!("<{tag}>"))? + tag.len() + 2;
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    Some(&xml[start..end])
}

/// The value of `key="..."` in an opening tag.
fn attribute(tag: &str, key: &str) -> Option<String> {
    // Searching for the key alone would match `name` inside `filename`; the
    // space in front anchors it to the start of an attribute.
    let needle = format!(" {key}=\"");
    let at = tag.find(&needle)?;
    let after = &tag[at + needle.len()..];
    let end = after.find('"')?;
    Some(unescape(&after[..end]))
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            // A bare ampersand, which is not legal XML but happens.
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..end];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => match entity.strip_prefix('#') {
                Some(number) => {
                    let code = match number.strip_prefix('x').or_else(|| number.strip_prefix('X')) {
                        Some(hex) => u32::from_str_radix(hex, 16).ok(),
                        None => number.parse().ok(),
                    };
                    match code.and_then(char::from_u32) {
                        Some(c) => out.push(c),
                        None => out.push_str(&rest[..=end]),
                    }
                }
                // Something we do not know: leave it as it was written rather
                // than dropping it.
                None => out.push_str(&rest[..=end]),
            },
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cut from the real NEC PC Engine datfile, keeping what trips a parser:
    /// an ampersand in the system name, a multi-track game whose tracks are
    /// separate files, and a single-file game beside it.
    const DAT: &str = r#"<?xml version="1.0"?>
<!DOCTYPE datafile PUBLIC "-//Logiqx//DTD ROM Management Datafile//EN" "http://www.logiqx.com/Dats/datafile.dtd">
<datafile>
	<header>
		<name>NEC - PC Engine CD &amp; TurboGrafx CD</name>
		<description>NEC - PC Engine CD &amp; TurboGrafx CD - Discs (551)</description>
		<version>2026-06-14 14-24-19</version>
		<author>redump.org</author>
	</header>
	<game name="Hatsukoi Monogatari (Japan) (Rev 1)">
		<category>Games</category>
		<description>Hatsukoi Monogatari (Japan) (Rev 1)</description>
		<rom name="Hatsukoi Monogatari (Japan) (Rev 1).cue" size="687" crc="80098e86" md5="500c9c829c34918a0ec648599db288a7" sha1="9577e852c061e70a5cc0272231a10c5876fc8caa"/>
		<rom name="Hatsukoi Monogatari (Japan) (Rev 1) (Track 1).bin" size="10633392" crc="9d3626e2" md5="392a5a3b157100b3bd15a7a0935a0643" sha1="a129332bf4d4a44a5098a74ba86f1150eded4bc7"/>
		<rom name="Hatsukoi Monogatari (Japan) (Rev 1) (Track 2).bin" size="301157136" crc="0fedf856" md5="0ffacd67eeb50d00789709c18449d2a4" sha1="75bcec88e76e4a6fc6ec2b60de03fb37afda7ace"/>
	</game>
	<game name="Some Data Disc (Europe)">
		<category>Games</category>
		<rom name="Some Data Disc (Europe).iso" size="4700372992" crc="deadbeef" md5="00000000000000000000000000000000" sha1="da39a3ee5e6b4b0d3255bfef95601890afd80709"/>
	</game>
</datafile>"#;

    fn dat() -> Dat {
        parse(DAT).expect("the datfile parses")
    }

    #[test]
    fn the_header_says_which_system_it_covers() {
        let d = dat();
        assert_eq!(d.name, "NEC - PC Engine CD & TurboGrafx CD", "the entity was not decoded");
        assert_eq!(d.version.as_deref(), Some("2026-06-14 14-24-19"));
    }

    #[test]
    fn every_game_and_all_of_its_files_are_read() {
        let d = dat();
        assert_eq!(d.len(), 2);
        assert_eq!(d.games[0].name, "Hatsukoi Monogatari (Japan) (Rev 1)");
        assert_eq!(d.games[0].roms.len(), 3);
        assert_eq!(d.games[1].roms.len(), 1);
    }

    #[test]
    fn each_file_carries_its_size_and_hashes() {
        let rom = &dat().games[1].roms[0];
        assert_eq!(rom.size, 4_700_372_992, "a disc is bigger than a u32 holds");
        assert_eq!(rom.crc32, Some(0xdead_beef));
        assert_eq!(rom.sha1.as_deref(), Some("da39a3ee5e6b4b0d3255bfef95601890afd80709"));
    }

    #[test]
    fn a_disc_with_audio_tracks_is_several_files_and_says_so() {
        // One flat image cannot match it, because no such image exists here.
        let d = dat();
        assert!(d.games[0].is_multi_track());
        assert!(!d.games[1].is_multi_track(), "one track and a cue is still one disc");
    }

    #[test]
    fn the_region_and_revision_are_not_part_of_the_name() {
        assert_eq!(dat().games[0].short_name(), "Hatsukoi Monogatari");
    }

    fn digests(bytes: u64, sha1: &str, crc32: u32) -> Digests {
        let mut raw = [0u8; 20];
        for (i, b) in raw.iter_mut().enumerate() {
            *b = u8::from_str_radix(&sha1[i * 2..i * 2 + 2], 16).unwrap();
        }
        Digests { crc32, sha1: raw, bytes }
    }

    #[test]
    fn an_image_that_matches_is_named_by_the_disc_it_is() {
        let d = dat();
        let found = d
            .find(&digests(4_700_372_992, "da39a3ee5e6b4b0d3255bfef95601890afd80709", 0xdead_beef))
            .expect("it should match");
        assert_eq!(found.game.name, "Some Data Disc (Europe)");
        assert_eq!(found.rom.name, "Some Data Disc (Europe).iso");
    }

    #[test]
    fn an_image_of_the_right_size_and_the_wrong_contents_does_not_match() {
        // This is the whole point: a dump missing its protection sectors is
        // often exactly the right length.
        let d = dat();
        assert!(
            d.find(&digests(
                4_700_372_992,
                "0000000000000000000000000000000000000000",
                0xdead_beef
            ))
            .is_none(),
            "the hash has to decide, not the size and not the crc"
        );
    }

    #[test]
    fn an_image_of_the_wrong_size_is_not_even_hashed_against() {
        let d = dat();
        assert!(d.find(&digests(123, "da39a3ee5e6b4b0d3255bfef95601890afd80709", 0)).is_none());
    }

    #[test]
    fn a_disc_of_several_tracks_matches_when_all_of_them_do() {
        let d = dat();
        let tracks = [
            digests(10_633_392, "a129332bf4d4a44a5098a74ba86f1150eded4bc7", 0x9d36_26e2),
            digests(301_157_136, "75bcec88e76e4a6fc6ec2b60de03fb37afda7ace", 0x0fed_f856),
        ];
        let game = d.find_all(&tracks).expect("both tracks match");
        assert_eq!(game.name, "Hatsukoi Monogatari (Japan) (Rev 1)");
    }

    #[test]
    fn one_good_track_out_of_two_is_not_a_match() {
        // A boundary cut one sector wrong leaves the first track perfect and
        // everything after it shifted, which is exactly the failure that must
        // not read as success.
        let d = dat();
        let tracks = [
            digests(10_633_392, "a129332bf4d4a44a5098a74ba86f1150eded4bc7", 0x9d36_26e2),
            digests(301_157_136, "0000000000000000000000000000000000000000", 0),
        ];
        assert!(d.find_all(&tracks).is_none());
    }

    #[test]
    fn a_disc_with_the_wrong_number_of_tracks_is_not_that_disc() {
        let d = dat();
        let one = [digests(10_633_392, "a129332bf4d4a44a5098a74ba86f1150eded4bc7", 0x9d36_26e2)];
        assert!(d.find_all(&one).is_none(), "two tracks were expected");
    }

    #[test]
    fn a_track_of_a_multi_track_disc_still_matches_on_its_own() {
        // Useful once tracks are dumped separately, which is what a disc with
        // audio on it needs.
        let d = dat();
        let found = d
            .find(&digests(10_633_392, "a129332bf4d4a44a5098a74ba86f1150eded4bc7", 0x9d36_26e2))
            .expect("track one should match");
        assert!(found.rom.name.contains("Track 1"));
    }

    #[test]
    fn the_systems_offered_are_ones_a_pc_drive_can_actually_read() {
        // A GameCube or Xbox disc is a format an ordinary drive cannot read,
        // so offering its datfile would promise something impossible.
        let slugs: Vec<&str> = SYSTEMS.iter().map(|(s, _)| *s).collect();
        for readable in ["pc", "psx", "ps2", "mac"] {
            assert!(slugs.contains(&readable), "{readable} should be offered");
        }
        for unreadable in ["gc", "wii", "xbox", "ps3"] {
            assert!(!slugs.contains(&unreadable), "{unreadable} cannot be read here");
        }
    }

    #[test]
    fn a_system_is_fetched_from_its_own_address() {
        assert_eq!(datfile_url("psx"), "http://redump.org/datfile/psx/");
        assert_eq!(system_name("psx"), Some("Sony PlayStation"));
        assert_eq!(system_name("nonesuch"), None);
    }

    #[test]
    fn something_that_is_not_a_datfile_is_refused_rather_than_read_as_empty() {
        assert!(parse("<html><body>404</body></html>").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn the_entities_that_appear_in_game_names_are_decoded() {
        assert_eq!(unescape("Tom &amp; Jerry"), "Tom & Jerry");
        assert_eq!(unescape("&lt;tag&gt;"), "<tag>");
        assert_eq!(unescape("say &quot;hi&quot;"), "say \"hi\"");
        assert_eq!(unescape("it&apos;s"), "it's");
        assert_eq!(unescape("&#65;&#x42;"), "AB");
        assert_eq!(unescape("nothing here"), "nothing here");
        // Left as written rather than swallowed, so a surprise is visible.
        assert_eq!(unescape("&unknown;"), "&unknown;");
        assert_eq!(unescape("100% & rising"), "100% & rising");
    }
}
