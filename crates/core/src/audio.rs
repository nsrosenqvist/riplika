//! Writing a ripped CD out.
//!
//! Sibling of [`format`](crate::format) rather than part of it. That trait
//! answers about muxers, subtitle codecs and where the index goes, and a music
//! file has none of those; what the two share is the shape - the format answers
//! for itself, so the planner never branches on which one it is.
//!
//! The tag vocabularies do not overlap, which is the main reason this exists.
//! Vorbis comments are uppercase and spell out `TRACKNUMBER` and `TOTALTRACKS`
//! as separate fields; ID3 wants lowercase `track` with both numbers in one.
//! Writing either set into the other produces a file that tags without
//! complaint and shows up untitled in a library.

use crate::host::Command;
use crate::identify::music::{Album, AlbumTrack};
use crate::model::Quality;
use crate::naming::{self, MusicFields, render, sanitize};
use crate::prefs::{AudioFormat, FLAC_COMPRESSION};
use std::path::{Path, PathBuf};

pub trait AudioTarget: Send + Sync {
    fn extension(&self) -> &'static str;

    /// What ffmpeg calls the encoder.
    fn encoder(&self) -> &'static str;

    /// What ffmpeg calls the muxer.
    ///
    /// Said outright rather than inferred from the name, because the file is
    /// written to a `.part` path while it is being made and ffmpeg would have
    /// nothing to infer from - the same trap the video side fell into.
    fn muxer(&self) -> &'static str;

    /// Encoder settings for a tier.
    fn quality_args(&self, quality: Quality) -> Vec<String>;

    /// What this format calls each piece of metadata.
    fn tags(&self, album: &Album, track: &AlbumTrack) -> Vec<(String, String)>;
}

pub struct Flac;

impl AudioTarget for Flac {
    fn extension(&self) -> &'static str {
        "flac"
    }
    fn encoder(&self) -> &'static str {
        "flac"
    }
    fn muxer(&self) -> &'static str {
        "flac"
    }

    /// The tier is deliberately ignored: FLAC is lossless, so every level
    /// decodes to the same audio and only the size moves. The settings screen
    /// switches the chooser off for the same reason.
    fn quality_args(&self, _quality: Quality) -> Vec<String> {
        vec!["-compression_level".into(), FLAC_COMPRESSION.to_string()]
    }

    fn tags(&self, album: &Album, track: &AlbumTrack) -> Vec<(String, String)> {
        let mut t = Tagging::new();
        t.set("TITLE", Some(track.title.clone()));
        t.set("ARTIST", Some(track.artist.clone().unwrap_or_else(|| album.artist.clone())));
        t.set("ALBUM", Some(album.title.clone()));
        t.set("ALBUMARTIST", Some(album.artist.clone()));
        t.set("DATE", album.date.clone());
        t.set("TRACKNUMBER", Some(track.number.to_string()));
        t.set("TOTALTRACKS", Some(album.tracks.len().to_string()));
        if album.is_multi_disc() {
            t.set("DISCNUMBER", Some(album.disc.to_string()));
            t.set("TOTALDISCS", Some(album.disc_count.to_string()));
            t.set("DISCSUBTITLE", album.disc_title.clone());
        }
        t.set("LABEL", album.label.clone());
        t.set("CATALOGNUMBER", album.catalogue_number.clone());
        t.set("BARCODE", album.barcode.clone());
        t.set("MUSICBRAINZ_ALBUMID", non_empty(&album.release_id));
        if album.is_compilation() {
            t.set("COMPILATION", Some("1".into()));
        }
        t.done()
    }
}

pub struct Mp3;

impl AudioTarget for Mp3 {
    fn extension(&self) -> &'static str {
        "mp3"
    }
    fn encoder(&self) -> &'static str {
        "libmp3lame"
    }
    fn muxer(&self) -> &'static str {
        "mp3"
    }

    fn quality_args(&self, quality: Quality) -> Vec<String> {
        vec!["-q:a".into(), quality.lame_vbr().to_string()]
    }

    fn tags(&self, album: &Album, track: &AlbumTrack) -> Vec<(String, String)> {
        let mut t = Tagging::new();
        t.set("title", Some(track.title.clone()));
        t.set("artist", Some(track.artist.clone().unwrap_or_else(|| album.artist.clone())));
        t.set("album", Some(album.title.clone()));
        t.set("album_artist", Some(album.artist.clone()));
        t.set("date", album.date.clone());
        // ID3 puts the total in the same field, separated by a slash, rather
        // than in one of its own.
        t.set("track", Some(format!("{}/{}", track.number, album.tracks.len())));
        if album.is_multi_disc() {
            t.set("disc", Some(format!("{}/{}", album.disc, album.disc_count)));
        }
        // ffmpeg maps this to TPUB; the two below have no frame of their own
        // and become TXXX, which is where every other tagger puts them too.
        t.set("publisher", album.label.clone());
        t.set("CATALOGNUMBER", album.catalogue_number.clone());
        t.set("BARCODE", album.barcode.clone());
        t.set("MusicBrainz Album Id", non_empty(&album.release_id));
        if album.is_compilation() {
            // Becomes TCMP, which is what keeps a compilation from splitting
            // into one album per track artist.
            t.set("compilation", Some("1".into()));
        }
        t.done()
    }
}

impl AudioFormat {
    pub fn target(self) -> &'static dyn AudioTarget {
        match self {
            AudioFormat::Flac => &Flac,
            AudioFormat::Mp3 => &Mp3,
        }
    }
}

/// Collects tags, dropping the ones there is nothing to say for.
///
/// An empty tag is worse than a missing one: it displaces whatever the player
/// would otherwise have fallen back to, so a blank ALBUMARTIST hides the
/// artist rather than leaving it alone.
struct Tagging(Vec<(String, String)>);

impl Tagging {
    fn new() -> Self {
        Tagging(Vec::new())
    }

    fn set(&mut self, key: &str, value: Option<String>) {
        if let Some(v) = value.filter(|v| !v.trim().is_empty()) {
            self.0.push((key.to_string(), v));
        }
    }

    fn done(self) -> Vec<(String, String)> {
        self.0
    }
}

fn non_empty(s: &str) -> Option<String> {
    if s.trim().is_empty() { None } else { Some(s.to_string()) }
}

/// Turn one ripped track into its finished file.
///
/// The cover is attached as a stream rather than written afterwards, and it
/// carries `comment="Cover (front)"` because without it ffmpeg files the
/// picture as type 0, "Other" - embedded, and invisible to every player that
/// asks for the front cover specifically.
pub fn encode_command(
    target: &dyn AudioTarget,
    quality: Quality,
    source: &Path,
    cover: Option<&Path>,
    dest: &Path,
    album: &Album,
    track: &AlbumTrack,
) -> Command {
    let mut cmd = Command::new("ffmpeg").args(["-v", "error", "-y", "-i"]).path(source);
    if let Some(art) = cover {
        cmd = cmd.arg("-i").path(art);
    }
    cmd = cmd.args(["-map", "0:a"]);
    if cover.is_some() {
        cmd = cmd.args([
            "-map",
            "1:v",
            "-c:v",
            "copy",
            "-disposition:v",
            "attached_pic",
            "-metadata:s:v",
            "comment=Cover (front)",
        ]);
    }
    cmd = cmd.args(["-c:a", target.encoder()]).args(target.quality_args(quality));
    for (key, value) in target.tags(album, track) {
        cmd = cmd.args(["-metadata", &format!("{key}={value}")]);
    }
    cmd.args(["-f", target.muxer()]).path(dest)
}

/// Where a track goes.
///
/// `Artist/Album (Year)/` by default, which is what Jellyfin reads without
/// being told anything, and the template decides only the filename. Same
/// division as the video side, where `Season NN/` is fixed and the episode
/// name is not. A set with more than one disc gets a folder per disc so track
/// numbers from different discs do not collide.
///
/// A template containing a slash takes the whole thing over instead:
/// `{artist}/{album}/{track} - {title}` means exactly that, and none of the
/// above is put in front of it. It is the only way to lay a library out any
/// other way, and doing both would produce `Artist/Album/Artist/Album/`.
pub fn track_path(
    root: &Path,
    album: &Album,
    track: &AlbumTrack,
    extension: &str,
    template: Option<&str>,
) -> PathBuf {
    let template = template.unwrap_or(naming::DEFAULT_TRACK_TEMPLATE);
    let fields = fields(album, track);

    // A template with slashes in it says where the file goes as well as what
    // it is called, so it describes the whole path under the library and the
    // usual artist/album folders are not added in front of it - somebody who
    // has written "{artist}/{album}/..." meant that and not "artist/album"
    // twice over.
    if template.contains('/') {
        let mut parts = naming::render_path(template, &fields);
        if let Some(name) = parts.pop() {
            let mut path = root.to_path_buf();
            for dir in parts {
                path = path.join(dir);
            }
            return path.join(sanitize(&format!("{name}.{extension}")));
        }
    }

    let mut path = root.join(sanitize(&album.artist)).join(sanitize(&album_folder(album)));
    if album.is_multi_disc() {
        path = path.join(sanitize(&format!("Disc {}", album.disc)));
    }
    let stem = render(template, &fields);
    path.join(sanitize(&format!("{stem}.{extension}")))
}

pub fn fields(album: &Album, track: &AlbumTrack) -> MusicFields {
    MusicFields {
        albumartist: album.artist.clone(),
        // On a compilation this is the one that differs line by line; on a
        // single-artist album the two are the same and it makes no odds.
        artist: track.artist.clone().unwrap_or_else(|| album.artist.clone()),
        album: album.title.clone(),
        title: track.title.clone(),
        track: Some(track.number),
        disc: album.is_multi_disc().then_some(album.disc),
        year: album.year(),
        date: album.date.clone(),
    }
}

fn album_folder(album: &Album) -> String {
    match album.year() {
        Some(year) => format!("{} ({year})", album.title),
        None => album.title.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(number: u32, title: &str, artist: Option<&str>) -> AlbumTrack {
        AlbumTrack {
            number,
            title: title.into(),
            artist: artist.map(str::to_string),
            duration: Some(205_000),
        }
    }

    fn album() -> Album {
        Album {
            title: "Roots".into(),
            artist: "Shawn McDonald".into(),
            tracks: vec![track(1, "Clarity", None), track(8, "Slow Down", None)],
            date: Some("2008-03-11".into()),
            country: Some("US".into()),
            barcode: Some("094639104222".into()),
            label: Some("Sparrow Records".into()),
            catalogue_number: Some("SPD91042".into()),
            disc: 1,
            disc_count: 1,
            disc_title: None,
            release_id: "43b353ce".into(),
            has_cover_art: true,
        }
    }

    fn tag<'a>(tags: &'a [(String, String)], key: &str) -> Option<&'a str> {
        tags.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    #[test]
    fn flac_is_tagged_in_the_vorbis_vocabulary() {
        let a = album();
        let t = Flac.tags(&a, &a.tracks[1]);
        assert_eq!(tag(&t, "TITLE"), Some("Slow Down"));
        assert_eq!(tag(&t, "ALBUMARTIST"), Some("Shawn McDonald"));
        // Vorbis keeps the total in a field of its own.
        assert_eq!(tag(&t, "TRACKNUMBER"), Some("8"));
        assert_eq!(tag(&t, "TOTALTRACKS"), Some("2"));
        assert_eq!(tag(&t, "CATALOGNUMBER"), Some("SPD91042"));
        assert_eq!(tag(&t, "MUSICBRAINZ_ALBUMID"), Some("43b353ce"));
    }

    #[test]
    fn mp3_is_tagged_in_the_id3_vocabulary_instead() {
        let a = album();
        let t = Mp3.tags(&a, &a.tracks[1]);
        assert_eq!(tag(&t, "title"), Some("Slow Down"));
        assert_eq!(tag(&t, "album_artist"), Some("Shawn McDonald"));
        // ID3 puts both numbers in the one field.
        assert_eq!(tag(&t, "track"), Some("8/2"));
        assert_eq!(tag(&t, "TRACKNUMBER"), None, "that is the other format's word");
        // ffmpeg turns this one into TPUB.
        assert_eq!(tag(&t, "publisher"), Some("Sparrow Records"));
    }

    #[test]
    fn nothing_worth_saying_means_no_tag_rather_than_an_empty_one() {
        // An empty tag displaces the fallback a player would otherwise use, so
        // a blank ALBUMARTIST hides the artist instead of leaving it alone.
        let mut a = album();
        a.label = None;
        a.catalogue_number = Some("   ".into());
        a.barcode = Some(String::new());
        let t = Flac.tags(&a, &a.tracks[0]);
        for absent in ["LABEL", "CATALOGNUMBER", "BARCODE"] {
            assert_eq!(tag(&t, absent), None, "{absent} should not be written blank");
        }
    }

    #[test]
    fn a_single_disc_release_is_not_told_it_is_disc_one_of_one() {
        let a = album();
        assert_eq!(tag(&Flac.tags(&a, &a.tracks[0]), "DISCNUMBER"), None);
        assert_eq!(tag(&Mp3.tags(&a, &a.tracks[0]), "disc"), None);
    }

    #[test]
    fn a_box_set_numbers_its_discs_in_both_vocabularies() {
        let mut a = album();
        a.disc = 2;
        a.disc_count = 4;
        a.disc_title = Some("Late Years".into());
        assert_eq!(tag(&Flac.tags(&a, &a.tracks[0]), "DISCNUMBER"), Some("2"));
        assert_eq!(tag(&Flac.tags(&a, &a.tracks[0]), "TOTALDISCS"), Some("4"));
        assert_eq!(tag(&Flac.tags(&a, &a.tracks[0]), "DISCSUBTITLE"), Some("Late Years"));
        assert_eq!(tag(&Mp3.tags(&a, &a.tracks[0]), "disc"), Some("2/4"));
    }

    #[test]
    fn a_compilation_is_flagged_so_it_does_not_split_into_one_album_per_artist() {
        let mut a = album();
        a.artist = "Various Artists".into();
        a.tracks = vec![track(1, "One", Some("A Band")), track(2, "Two", Some("Another"))];
        assert_eq!(tag(&Flac.tags(&a, &a.tracks[0]), "COMPILATION"), Some("1"));
        assert_eq!(tag(&Mp3.tags(&a, &a.tracks[0]), "compilation"), Some("1"));
        // The track's own artist is the useful one; the album artist keeps the
        // record together.
        assert_eq!(tag(&Flac.tags(&a, &a.tracks[0]), "ARTIST"), Some("A Band"));
        assert_eq!(tag(&Flac.tags(&a, &a.tracks[0]), "ALBUMARTIST"), Some("Various Artists"));
    }

    #[test]
    fn a_track_with_no_artist_of_its_own_takes_the_albums() {
        let a = album();
        assert_eq!(tag(&Flac.tags(&a, &a.tracks[0]), "ARTIST"), Some("Shawn McDonald"));
    }

    #[test]
    fn flac_ignores_the_tier_and_mp3_does_not() {
        assert_eq!(Flac.quality_args(Quality::High), Flac.quality_args(Quality::Low));
        assert_ne!(Mp3.quality_args(Quality::High), Mp3.quality_args(Quality::Low));
        assert_eq!(Mp3.quality_args(Quality::Medium), vec!["-q:a", "2"]);
    }

    #[test]
    fn the_encode_states_its_format_because_the_file_is_written_to_a_part_path() {
        let a = album();
        let cmd = encode_command(
            &Flac,
            Quality::High,
            Path::new("/tmp/t.wav"),
            None,
            Path::new("/tmp/t.part"),
            &a,
            &a.tracks[0],
        );
        assert_eq!(cmd.value_of("-f"), Some("flac"));
        assert_eq!(cmd.value_of("-c:a"), Some("flac"));
        assert_eq!(cmd.args.last().unwrap(), "/tmp/t.part");
    }

    #[test]
    fn the_cover_is_filed_as_a_front_cover_and_not_as_other() {
        // Without the comment ffmpeg writes picture type 0, "Other": embedded,
        // and invisible to every player that asks for the front cover.
        let a = album();
        let cmd = encode_command(
            &Flac,
            Quality::High,
            Path::new("/tmp/t.wav"),
            Some(Path::new("/tmp/cover.jpg")),
            Path::new("/tmp/t.part"),
            &a,
            &a.tracks[0],
        );
        assert!(cmd.has("attached_pic"));
        assert!(cmd.args.iter().any(|a| a == "comment=Cover (front)"), "{:?}", cmd.args);
    }

    #[test]
    fn with_no_cover_there_is_no_second_input_to_map() {
        let a = album();
        let cmd = encode_command(
            &Flac,
            Quality::High,
            Path::new("/tmp/t.wav"),
            None,
            Path::new("/tmp/t.part"),
            &a,
            &a.tracks[0],
        );
        assert!(!cmd.has("attached_pic"));
        assert!(!cmd.args.iter().any(|a| a == "1:v"), "{:?}", cmd.args);
    }

    #[test]
    fn each_tag_is_passed_as_its_own_metadata_argument() {
        let a = album();
        let cmd = encode_command(
            &Mp3,
            Quality::High,
            Path::new("/tmp/t.wav"),
            None,
            Path::new("/tmp/t.part"),
            &a,
            &a.tracks[1],
        );
        assert!(cmd.args.iter().any(|x| x == "track=8/2"), "{:?}", cmd.args);
    }

    #[test]
    fn a_track_lands_where_a_library_will_look_for_it() {
        let a = album();
        assert_eq!(
            track_path(Path::new("/music"), &a, &a.tracks[1], "flac", None),
            Path::new("/music/Shawn McDonald/Roots (2008)/08 - Slow Down.flac")
        );
    }

    #[test]
    fn a_template_with_slashes_in_it_lays_out_the_folders_too() {
        let a = album();
        assert_eq!(
            track_path(
                Path::new("/music"),
                &a,
                &a.tracks[1],
                "flac",
                Some("{artist}/{album}/{track} - {title}")
            ),
            Path::new("/music/Shawn McDonald/Roots/08 - Slow Down.flac"),
            "the usual Artist/Album (Year) is not put in front of it as well"
        );
    }

    #[test]
    fn a_slash_in_a_name_cannot_invent_a_folder() {
        // The template is split before the fields are filled in, so a band
        // with a slash in its name is one directory, not two - and a value
        // cannot climb out of the library by containing "../" either.
        let mut a = album();
        a.artist = "AC/DC".into();
        a.tracks[0].artist = Some("../../etc".into());
        let p = track_path(
            Path::new("/music"),
            &a,
            &a.tracks[0],
            "flac",
            Some("{albumartist}/{artist}/{title}"),
        );
        assert_eq!(p, Path::new("/music/AC-DC/..-..-etc/Clarity.flac"), "{p:?}");
        assert!(p.starts_with("/music"));
    }

    #[test]
    fn a_template_that_renders_to_nothing_shortens_the_path_rather_than_naming_a_folder_nothing() {
        let mut a = album();
        a.date = None;
        let p = track_path(
            Path::new("/music"),
            &a,
            &a.tracks[0],
            "flac",
            Some("{year}/{album}/{title}"),
        );
        assert_eq!(p, Path::new("/music/Roots/Clarity.flac"), "{p:?}");
    }

    #[test]
    fn a_template_decides_the_filename_and_not_the_folders() {
        let a = album();
        assert_eq!(
            track_path(
                Path::new("/music"),
                &a,
                &a.tracks[1],
                "flac",
                Some("{track} {artist} - {title} [{year}]")
            ),
            Path::new(
                "/music/Shawn McDonald/Roots (2008)/08 Shawn McDonald - Slow Down [2008].flac"
            )
        );
    }

    #[test]
    fn a_template_can_ask_for_the_track_artist_which_is_the_useful_one_on_a_compilation() {
        let mut a = album();
        a.artist = "Various Artists".into();
        a.tracks = vec![track(1, "One", Some("A Band"))];
        let p = track_path(
            Path::new("/music"),
            &a,
            &a.tracks[0],
            "flac",
            Some("{track} - {artist} - {title}"),
        );
        assert_eq!(p, Path::new("/music/Various Artists/Roots (2008)/01 - A Band - One.flac"));
    }

    #[test]
    fn a_token_that_is_not_a_music_token_is_left_standing_rather_than_dropped() {
        let a = album();
        let p =
            track_path(Path::new("/music"), &a, &a.tracks[1], "flac", Some("{season} - {title}"));
        // A typo has to be visible, not a silent gap.
        assert!(p.to_string_lossy().contains("{season}"), "{}", p.display());
    }

    #[test]
    fn a_box_set_gets_a_folder_per_disc_so_track_numbers_do_not_collide() {
        let mut a = album();
        a.disc = 2;
        a.disc_count = 3;
        assert_eq!(
            track_path(Path::new("/music"), &a, &a.tracks[0], "flac", None),
            Path::new("/music/Shawn McDonald/Roots (2008)/Disc 2/01 - Clarity.flac")
        );
    }

    #[test]
    fn the_default_name_leaves_the_artist_to_the_tags() {
        // Every music server reads tags rather than filenames, and repeating
        // the artist on each of twelve files says nothing the tags do not.
        let mut a = album();
        a.artist = "Various Artists".into();
        a.tracks = vec![track(1, "One", Some("A Band"))];
        assert_eq!(
            track_path(Path::new("/music"), &a, &a.tracks[0], "mp3", None),
            Path::new("/music/Various Artists/Roots (2008)/01 - One.mp3")
        );
    }

    #[test]
    fn characters_smb_refuses_do_not_reach_the_filename() {
        let mut a = album();
        a.title = "AC/DC: Live?".into();
        a.tracks = vec![track(1, "Who Made Who?", None)];
        let p = track_path(Path::new("/music"), &a, &a.tracks[0], "flac", None);
        let text = p.to_string_lossy();
        // The colon, slash and question mark are gone from the names - but the
        // separators between them are still separators.
        assert!(!text.trim_start_matches("/music/").contains(':'), "{text}");
        assert!(!text.contains('?'), "{text}");
    }

    #[test]
    fn an_album_with_no_date_is_not_filed_under_an_empty_year() {
        let mut a = album();
        a.date = None;
        let p = track_path(Path::new("/music"), &a, &a.tracks[0], "flac", None);
        assert!(p.to_string_lossy().contains("/Roots/"), "{}", p.display());
    }
}
