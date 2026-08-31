//! Identifying a music CD.
//!
//! This is a different job from identifying a film, and an easier one. A DVD
//! has to be guessed at from a volume label and the shape of its titles; a CD
//! states what it is. The table of contents hashes to an id that names one
//! pressing, so the lookup is exact and there is usually nothing to choose
//! between.
//!
//! Which is why this does not implement [`Catalogue`](super::catalogue::Catalogue).
//! That trait searches by title and returns episodes by season, and neither
//! question makes sense here - the disc is not searched for, it is looked up,
//! and what comes back is tracks.

use crate::identify::catalogue::Http;
use crate::model::Millis;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumTrack {
    pub number: u32,
    pub title: String,
    /// Set only when the track is credited to somebody other than the album
    /// artist. That difference is what makes a compilation a compilation.
    pub artist: Option<String>,
    pub duration: Option<Millis>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Album {
    pub title: String,
    pub artist: String,
    pub tracks: Vec<AlbumTrack>,
    pub date: Option<String>,
    pub country: Option<String>,
    pub barcode: Option<String>,
    pub label: Option<String>,
    pub catalogue_number: Option<String>,
    /// Which disc of the release is in the drive, and how many it came with.
    pub disc: u32,
    pub disc_count: u32,
    /// What this disc is called, when the discs of a set are named.
    pub disc_title: Option<String>,
    /// MusicBrainz's id for the release. Also the key the cover art archive is
    /// addressed by, so it is worth keeping even when nothing else is.
    pub release_id: String,
    pub has_cover_art: bool,
}

impl Album {
    pub fn year(&self) -> Option<u32> {
        self.date.as_ref()?.get(..4)?.parse().ok()
    }

    /// Tracks credited to somebody other than the album artist.
    ///
    /// Worth knowing before tagging: on a compilation the per-track artist is
    /// the useful one, and the album artist is what keeps the tracks together
    /// in a library rather than scattering them under twenty names.
    pub fn is_compilation(&self) -> bool {
        self.tracks.iter().any(|t| t.artist.is_some())
    }

    pub fn is_multi_disc(&self) -> bool {
        self.disc_count > 1
    }

    pub fn duration(&self) -> Millis {
        self.tracks.iter().filter_map(|t| t.duration).sum()
    }

    /// A line telling one pressing from another.
    ///
    /// Rarely needed - most discs match exactly one release - but when a disc
    /// id covers several pressings they differ by year, country and label and
    /// by nothing else, so those are what get shown.
    pub fn detail(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(year) = self.year() {
            parts.push(year.to_string());
        }
        if let Some(country) = &self.country {
            parts.push(country.clone());
        }
        if let Some(label) = &self.label {
            parts.push(label.clone());
        }
        if self.is_multi_disc() {
            parts.push(format!("disc {} of {}", self.disc, self.disc_count));
        }
        parts.join(" - ")
    }
}

impl Album {
    /// Build an album from the disc's own CD-Text.
    ///
    /// Everything a catalogue would have added is missing - no release date,
    /// no label, no barcode, no cover - but the names are right, which is what
    /// decides where the files go and what they are called. Far better than
    /// twelve files called "Track 03".
    pub fn from_cd_text(toc: &crate::disc::Toc, text: &crate::cdtext::CdText) -> Album {
        Album {
            title: text.album.clone().unwrap_or_default(),
            artist: text.performer.clone().unwrap_or_default(),
            tracks: toc
                .audio_tracks()
                .map(|t| AlbumTrack {
                    number: u32::from(t.number),
                    title: text
                        .title_of(t.number)
                        .map(str::to_string)
                        // A disc that names most of its tracks and not one of
                        // them still beats nothing; the gap gets a number.
                        .unwrap_or_else(|| format!("Track {:02}", t.number)),
                    artist: text
                        .tracks
                        .iter()
                        .find(|x| x.number == t.number)
                        .and_then(|x| x.performer.clone())
                        .filter(|a| Some(a) != text.performer.as_ref()),
                    duration: toc.track_duration(t.number),
                })
                .collect(),
            date: None,
            country: None,
            barcode: None,
            label: None,
            catalogue_number: None,
            disc: 1,
            disc_count: 1,
            disc_title: None,
            release_id: String::new(),
            has_cover_art: false,
        }
    }
}

/// A release the catalogue offered, before its tracks have been fetched.
///
/// Searching by name and looking a release up are two requests, because the
/// search endpoint does not carry track listings. This is what comes back from
/// the first: enough to choose by, and the id to ask with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub release_id: String,
    pub title: String,
    pub artist: String,
    pub date: Option<String>,
    pub country: Option<String>,
    /// How many tracks the release says this disc has.
    ///
    /// Worth showing next to what the drive reports: a name can match a
    /// release that is not the pressing in the tray, and a different track
    /// count is the cheapest sign of it.
    pub tracks: usize,
    /// "CD", "Mixed Mode CD", and so on. Absent when the catalogue does not say.
    pub format: Option<String>,
    /// How well the catalogue thinks the name matched, out of a hundred.
    pub score: u32,
}

impl Match {
    /// The year, without the rest of the date.
    pub fn year(&self) -> Option<&str> {
        self.date.as_deref().and_then(|d| d.get(..4))
    }

    /// What distinguishes this release from the others with the same name.
    pub fn detail(&self) -> String {
        let mut parts = Vec::new();
        if let Some(year) = self.year() {
            parts.push(year.to_string());
        }
        if let Some(country) = &self.country {
            parts.push(country.clone());
        }
        if let Some(format) = &self.format {
            parts.push(format.clone());
        }
        parts.push(format!("{} tracks", self.tracks));
        parts.join(" - ")
    }
}

/// A source of album details, looked up by what the disc says it is.
pub trait MusicCatalogue: Send + Sync {
    fn name(&self) -> &'static str;

    /// Every release this exact disc belongs to.
    ///
    /// Usually one. More than one means the same pressing was issued more than
    /// once and the user has to say which, exactly as for a film.
    fn by_disc_id(&self, disc_id: &str) -> Result<Vec<Album>>;

    /// Releases whose name matches what was typed.
    ///
    /// For the disc the catalogue has never seen, or has seen and got wrong.
    /// A search cannot prove which pressing is in the drive the way a disc id
    /// can, so what comes back is offered to be chosen from rather than used.
    fn search(&self, query: &str) -> Result<Vec<Match>>;

    /// One release in full, once the reader has said which.
    fn by_release_id(&self, id: &str) -> Result<Option<Album>>;
}

pub struct MusicBrainz<'a> {
    http: &'a dyn Http,
}

impl<'a> MusicBrainz<'a> {
    pub fn new(http: &'a dyn Http) -> Self {
        Self { http }
    }

    /// Everything needed in one request: MusicBrainz allows one a second, so
    /// asking once for artists, tracks and labels together beats three polite
    /// waits for the same answer.
    pub fn lookup_url(disc_id: &str) -> String {
        format!(
            "https://musicbrainz.org/ws/2/discid/{disc_id}?fmt=json&inc=artists+recordings+labels"
        )
    }

    /// Releases matching a name. Carries no track listings; see [`release_url`].
    ///
    /// [`release_url`]: MusicBrainz::release_url
    pub fn search_url(query: &str, limit: u32) -> String {
        format!(
            "https://musicbrainz.org/ws/2/release/?query={}&fmt=json&limit={limit}",
            encoded(query)
        )
    }

    /// One release in full, by id. The second of the two requests a search needs.
    pub fn release_url(id: &str) -> String {
        format!("https://musicbrainz.org/ws/2/release/{id}?fmt=json&inc=artists+recordings+labels")
    }
}

impl MusicCatalogue for MusicBrainz<'_> {
    fn name(&self) -> &'static str {
        "MusicBrainz"
    }

    fn by_disc_id(&self, disc_id: &str) -> Result<Vec<Album>> {
        let body = self.http.get(&Self::lookup_url(disc_id))?;
        parse_disc_lookup(&body, disc_id)
    }

    fn search(&self, query: &str) -> Result<Vec<Match>> {
        let body = self.http.get(&Self::search_url(query, SEARCH_LIMIT))?;
        parse_search(&body)
    }

    fn by_release_id(&self, id: &str) -> Result<Option<Album>> {
        let body = self.http.get(&Self::release_url(id))?;
        parse_release(&body)
    }
}

/// How many search results to ask for.
///
/// Enough that an album issued in several countries still shows the one you
/// have, few enough to read without scrolling past the answer.
const SEARCH_LIMIT: u32 = 25;

/// Percent-encode a query for a URL.
///
/// Everything but the unreserved set, because a search is whatever somebody
/// typed: an ampersand in a band's name would otherwise end the query and
/// silently search for half of it.
fn encoded(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for byte in query.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Everything a searched release needs, which is less than a disc lookup needs.
pub fn parse_search(json: &str) -> Result<Vec<Match>> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| Error(format!("MusicBrainz sent something unreadable: {e}")))?;
    if let Some(why) = v.get("error").and_then(Value::as_str) {
        return Err(Error(format!("MusicBrainz: {why}")));
    }
    let text =
        |v: Option<&Value>| v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
    Ok(v.get("releases")
        .and_then(Value::as_array)
        .map(|rs| {
            rs.iter()
                .filter_map(|r| {
                    let medium = r.get("media").and_then(Value::as_array).and_then(|m| m.first());
                    Some(Match {
                        release_id: text(r.get("id"))?,
                        title: text(r.get("title"))?,
                        artist: credit(r).unwrap_or_default(),
                        date: text(r.get("date")),
                        country: text(r.get("country")),
                        // The release-level count covers every disc of a box
                        // set; the medium's is the one to weigh against a TOC.
                        tracks: medium
                            .and_then(|m| m.get("track-count"))
                            .or_else(|| r.get("track-count"))
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as usize,
                        format: text(medium.and_then(|m| m.get("format"))),
                        score: r.get("score").and_then(Value::as_u64).unwrap_or(0) as u32,
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

/// One release, looked up by its id rather than found by a disc.
///
/// No disc id to match a medium against, so a box set falls back to its first
/// disc - which is why what comes out of here is offered to be checked rather
/// than trusted the way a disc lookup is.
pub fn parse_release(json: &str) -> Result<Option<Album>> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| Error(format!("MusicBrainz sent something unreadable: {e}")))?;
    if let Some(why) = v.get("error").and_then(Value::as_str) {
        return Err(Error(format!("MusicBrainz: {why}")));
    }
    Ok(album_of(&v, ""))
}

/// Where the front cover lives, for a release id.
pub fn cover_art_url(release_id: &str) -> String {
    format!("https://coverartarchive.org/release/{release_id}/front")
}

pub fn parse_disc_lookup(json: &str, disc_id: &str) -> Result<Vec<Album>> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| Error(format!("MusicBrainz sent something unreadable: {e}")))?;
    // A disc it has never seen comes back as an error object, not an empty
    // list, and saying so beats reporting nothing found for a lookup that
    // never happened.
    if let Some(why) = v.get("error").and_then(Value::as_str) {
        return Err(Error(format!("MusicBrainz: {why}")));
    }
    let releases = v.get("releases").and_then(Value::as_array);
    Ok(releases
        .map(|rs| rs.iter().filter_map(|r| album_of(r, disc_id)).collect())
        .unwrap_or_default())
}

fn album_of(release: &Value, disc_id: &str) -> Option<Album> {
    let media = release.get("media")?.as_array()?;
    // A release can be a box set. The disc id says which of its discs is in the
    // drive; taking the first would number disc four's tracks as disc one's.
    let medium = media.iter().find(|m| holds_disc(m, disc_id)).or_else(|| media.first())?;

    let artist = credit(release).unwrap_or_default();
    let tracks = medium
        .get("tracks")
        .and_then(Value::as_array)
        .map(|ts| ts.iter().filter_map(|t| track_of(t, &artist)).collect())
        .unwrap_or_default();

    let label_info = release.get("label-info").and_then(Value::as_array).and_then(|l| l.first());
    let text =
        |v: Option<&Value>| v.and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);

    Some(Album {
        title: text(release.get("title"))?,
        artist,
        tracks,
        date: text(release.get("date")),
        country: text(release.get("country")),
        barcode: text(release.get("barcode")),
        label: text(label_info.and_then(|l| l.get("label")).and_then(|l| l.get("name"))),
        catalogue_number: text(label_info.and_then(|l| l.get("catalog-number"))),
        disc: medium.get("position").and_then(Value::as_u64).unwrap_or(1) as u32,
        disc_count: media.len() as u32,
        disc_title: text(medium.get("title")),
        release_id: text(release.get("id")).unwrap_or_default(),
        has_cover_art: release
            .get("cover-art-archive")
            .and_then(|c| c.get("front"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn holds_disc(medium: &Value, disc_id: &str) -> bool {
    medium.get("discs").and_then(Value::as_array).is_some_and(|discs| {
        discs.iter().any(|d| d.get("id").and_then(Value::as_str) == Some(disc_id))
    })
}

fn track_of(track: &Value, album_artist: &str) -> Option<AlbumTrack> {
    Some(AlbumTrack {
        number: track.get("position").and_then(Value::as_u64)? as u32,
        title: track.get("title").and_then(Value::as_str)?.to_string(),
        // Only worth recording when it differs; carrying the album artist on
        // every track of a single-artist album says nothing.
        artist: credit(track).filter(|a| a != album_artist),
        duration: track.get("length").and_then(Value::as_u64).or_else(|| {
            track.get("recording").and_then(|r| r.get("length")).and_then(Value::as_u64)
        }),
    })
}

/// An artist credit, joined the way MusicBrainz means it to be read.
///
/// The credit is a list of parts, each carrying the phrase that joins it to the
/// next - "feat.", " & ", and so on. Taking only the first name would turn a
/// collaboration into a solo record.
fn credit(v: &Value) -> Option<String> {
    let parts = v.get("artist-credit")?.as_array()?;
    let mut out = String::new();
    for part in parts {
        out.push_str(part.get("name").and_then(Value::as_str).unwrap_or_default());
        out.push_str(part.get("joinphrase").and_then(Value::as_str).unwrap_or_default());
    }
    let out = out.trim().to_string();
    if out.is_empty() { None } else { Some(out) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identify::catalogue::FakeHttp;

    const DISC: &str = "sgDgBzHLi5stPYlOC7Jc6FPWdM8-";

    /// Trimmed from what MusicBrainz actually answered for the disc in the
    /// drive, keeping the shape: credits are lists with join phrases, the
    /// label sits under `label-info`, and the medium names its own discs.
    fn roots() -> String {
        format!(
            r#"{{
              "id": "{DISC}",
              "releases": [{{
                "id": "43b353ce-f15a-4b01-9dcb-3bbc280c97f1",
                "title": "Roots",
                "date": "2008-03-11",
                "country": "US",
                "barcode": "094639104222",
                "cover-art-archive": {{ "front": true, "count": 1 }},
                "artist-credit": [{{ "name": "Shawn McDonald", "joinphrase": "" }}],
                "label-info": [{{
                  "label": {{ "name": "Sparrow Records" }},
                  "catalog-number": "SPD91042"
                }}],
                "media": [{{
                  "position": 1,
                  "format": "CD",
                  "title": "",
                  "track-count": 2,
                  "discs": [{{ "id": "{DISC}" }}],
                  "tracks": [
                    {{ "position": 1, "title": "Clarity", "length": 205000,
                       "artist-credit": [{{ "name": "Shawn McDonald", "joinphrase": "" }}] }},
                    {{ "position": 2, "title": "Captivated", "length": 246000,
                       "artist-credit": [{{ "name": "Shawn McDonald", "joinphrase": "" }}] }}
                  ]
                }}]
              }}]
            }}"#
        )
    }

    fn one(json: &str) -> Album {
        let albums = parse_disc_lookup(json, DISC).unwrap();
        assert_eq!(albums.len(), 1, "expected exactly one release");
        albums.into_iter().next().unwrap()
    }

    #[test]
    fn a_disc_becomes_an_album_with_everything_worth_tagging() {
        let a = one(&roots());
        assert_eq!(a.title, "Roots");
        assert_eq!(a.artist, "Shawn McDonald");
        assert_eq!(a.year(), Some(2008));
        assert_eq!(a.country.as_deref(), Some("US"));
        assert_eq!(a.barcode.as_deref(), Some("094639104222"));
        assert_eq!(a.label.as_deref(), Some("Sparrow Records"));
        assert_eq!(a.catalogue_number.as_deref(), Some("SPD91042"));
        assert_eq!(a.release_id, "43b353ce-f15a-4b01-9dcb-3bbc280c97f1");
        assert!(a.has_cover_art);
    }

    #[test]
    fn the_tracks_come_out_numbered_and_timed() {
        let a = one(&roots());
        assert_eq!(a.tracks.len(), 2);
        assert_eq!(a.tracks[0].number, 1);
        assert_eq!(a.tracks[0].title, "Clarity");
        assert_eq!(a.tracks[0].duration, Some(205_000));
        assert_eq!(a.duration(), 451_000);
    }

    #[test]
    fn a_track_by_the_album_artist_is_not_credited_twice() {
        let a = one(&roots());
        assert!(a.tracks.iter().all(|t| t.artist.is_none()));
        assert!(!a.is_compilation());
    }

    #[test]
    fn a_single_disc_release_says_so() {
        let a = one(&roots());
        assert_eq!((a.disc, a.disc_count), (1, 1));
        assert!(!a.is_multi_disc());
        assert_eq!(a.disc_title, None, "an unnamed disc is not named the empty string");
    }

    #[test]
    fn the_disc_in_the_drive_decides_which_of_a_box_set_is_read() {
        let json = format!(
            r#"{{"releases": [{{
              "id": "r1", "title": "The Complete Works",
              "artist-credit": [{{ "name": "Someone", "joinphrase": "" }}],
              "media": [
                {{ "position": 1, "title": "Early Years", "discs": [{{ "id": "other" }}],
                   "tracks": [{{ "position": 1, "title": "Wrong One", "length": 1 }}] }},
                {{ "position": 2, "title": "Late Years", "discs": [{{ "id": "{DISC}" }}],
                   "tracks": [{{ "position": 1, "title": "Right One", "length": 2 }}] }}
              ]
            }}]}}"#
        );
        let a = one(&json);
        assert_eq!((a.disc, a.disc_count), (2, 2));
        assert_eq!(a.disc_title.as_deref(), Some("Late Years"));
        assert_eq!(a.tracks[0].title, "Right One");
    }

    #[test]
    fn a_release_that_names_no_disc_still_yields_its_first_medium() {
        let json = r#"{"releases": [{
          "id": "r1", "title": "Album",
          "artist-credit": [{ "name": "Someone", "joinphrase": "" }],
          "media": [{ "position": 1, "tracks": [{ "position": 1, "title": "Only" }] }]
        }]}"#;
        let a = one(json);
        assert_eq!(a.tracks[0].title, "Only");
        assert_eq!(a.tracks[0].duration, None);
    }

    #[test]
    fn a_collaboration_keeps_every_name_and_the_word_between_them() {
        let json = r#"{"releases": [{
          "id": "r1", "title": "Split",
          "artist-credit": [
            { "name": "Aphex Twin", "joinphrase": " & " },
            { "name": "Squarepusher", "joinphrase": "" }
          ],
          "media": [{ "position": 1, "tracks": [{ "position": 1, "title": "A" }] }]
        }]}"#;
        assert_eq!(one(json).artist, "Aphex Twin & Squarepusher");
    }

    #[test]
    fn a_track_credited_to_somebody_else_is_what_makes_a_compilation() {
        let json = r#"{"releases": [{
          "id": "r1", "title": "Now That's What I Call Music",
          "artist-credit": [{ "name": "Various Artists", "joinphrase": "" }],
          "media": [{ "position": 1, "tracks": [
            { "position": 1, "title": "One", "artist-credit": [{ "name": "A Band", "joinphrase": "" }] },
            { "position": 2, "title": "Two", "artist-credit": [{ "name": "Another", "joinphrase": "" }] }
          ] }]
        }]}"#;
        let a = one(json);
        assert!(a.is_compilation());
        assert_eq!(a.tracks[0].artist.as_deref(), Some("A Band"));
    }

    #[test]
    fn the_length_falls_back_to_the_recordings_when_the_track_has_none() {
        let json = r#"{"releases": [{
          "id": "r1", "title": "Album",
          "artist-credit": [{ "name": "Someone", "joinphrase": "" }],
          "media": [{ "position": 1, "tracks": [
            { "position": 1, "title": "One", "recording": { "length": 12345 } }
          ] }]
        }]}"#;
        assert_eq!(one(json).tracks[0].duration, Some(12345));
    }

    #[test]
    fn a_disc_musicbrainz_has_never_seen_says_which_it_was() {
        let err = parse_disc_lookup(r#"{"error": "Not Found"}"#, DISC).unwrap_err();
        assert!(err.to_string().contains("Not Found"), "{err}");
    }

    #[test]
    fn an_answer_that_is_not_json_is_an_error_not_a_panic() {
        assert!(parse_disc_lookup("<html>502</html>", DISC).is_err());
    }

    #[test]
    fn no_releases_at_all_is_empty_rather_than_an_error() {
        assert_eq!(parse_disc_lookup(r#"{"releases": []}"#, DISC).unwrap().len(), 0);
    }

    #[test]
    fn pressings_are_told_apart_by_what_differs_between_them() {
        let a = one(&roots());
        assert_eq!(a.detail(), "2008 - US - Sparrow Records");
    }

    #[test]
    fn everything_is_asked_for_in_the_one_request_the_rate_limit_allows() {
        let url = MusicBrainz::lookup_url(DISC);
        assert!(url.contains(DISC), "{url}");
        for want in ["artists", "recordings", "labels"] {
            assert!(url.contains(want), "{url} does not ask for {want}");
        }
    }

    #[test]
    fn the_lookup_goes_out_over_http_and_comes_back_an_album() {
        let http = FakeHttp::new().on("discid", &roots());
        let mb = MusicBrainz::new(&http);
        let albums = mb.by_disc_id(DISC).unwrap();
        assert_eq!(albums[0].title, "Roots");
        assert!(http.requested()[0].contains(DISC));
    }

    #[test]
    fn a_disc_that_names_itself_can_be_filed_without_asking_anybody() {
        use crate::cdtext::{CdText, TrackText};
        use crate::disc::{Toc, Track};

        let toc = Toc {
            tracks: vec![
                Track { number: 1, start: 0, is_data: false },
                Track { number: 2, start: 15_000, is_data: false },
            ],
            leadout: 30_000,
        };
        let text = CdText {
            album: Some("Roots".into()),
            performer: Some("Shawn McDonald".into()),
            tracks: vec![
                TrackText {
                    number: 1,
                    title: Some("Clarity".into()),
                    performer: Some("Shawn McDonald".into()),
                },
                TrackText {
                    number: 2,
                    title: Some("Captivated".into()),
                    performer: Some("Shawn McDonald".into()),
                },
            ],
        };
        let a = Album::from_cd_text(&toc, &text);
        assert_eq!(a.title, "Roots");
        assert_eq!(a.artist, "Shawn McDonald");
        assert_eq!(a.tracks.len(), 2);
        assert_eq!(a.tracks[0].title, "Clarity");
        // Lengths come off the table of contents, since CD-Text has none.
        assert_eq!(a.tracks[0].duration, Some(200_000));
        // A performer matching the album's is not repeated onto the track, so
        // this does not look like a compilation.
        assert!(!a.is_compilation());
        assert!(!a.has_cover_art);
        assert_eq!(a.date, None);
        assert_eq!(a.release_id, "");
    }

    #[test]
    fn a_track_the_disc_forgot_to_name_still_gets_a_name() {
        use crate::cdtext::{CdText, TrackText};
        use crate::disc::{Toc, Track};

        let toc = Toc {
            tracks: vec![
                Track { number: 1, start: 0, is_data: false },
                Track { number: 2, start: 15_000, is_data: false },
            ],
            leadout: 30_000,
        };
        let text = CdText {
            album: Some("Odds and Ends".into()),
            performer: Some("Someone".into()),
            tracks: vec![TrackText {
                number: 1,
                title: Some("The Only Named One".into()),
                performer: None,
            }],
        };
        let a = Album::from_cd_text(&toc, &text);
        assert_eq!(a.tracks[1].title, "Track 02");
    }

    #[test]
    fn the_cover_is_addressed_by_the_release_it_belongs_to() {
        assert_eq!(
            cover_art_url("43b353ce-f15a-4b01-9dcb-3bbc280c97f1"),
            "https://coverartarchive.org/release/43b353ce-f15a-4b01-9dcb-3bbc280c97f1/front"
        );
    }

    /// Trimmed from a real answer to `release/?query=cool boarders`, keeping
    /// the fields this reads and the two results that make the point: a name
    /// can match a cassette that is not the disc in the drive.
    const SEARCH: &str = r#"{"count": 2, "releases": [
      {"id": "f700bd4c-6ece-4c4c-929b-38f998d07ecb", "score": 100,
       "title": "Liquid Cool Boarders", "date": "2011-09", "country": "DK",
       "track-count": 2, "artist-credit": [{"name": "Jonas Frederiksen"}],
       "media": [{"format": "Cassette", "track-count": 2}]},
      {"id": "f3485f7b-34a3-49a4-b05a-db85d17cdeee", "score": 100,
       "title": "Cool Boarders 2", "date": "1997", "country": "US",
       "track-count": 21, "artist-credit": [{"name": "Namba Atsunori"}],
       "media": [{"format": "Mixed Mode CD", "track-count": 21}]}]}"#;

    #[test]
    fn a_search_reads_what_there_is_to_choose_between() {
        let found = parse_search(SEARCH).expect("it parses");
        assert_eq!(found.len(), 2);
        let cb = &found[1];
        assert_eq!(cb.title, "Cool Boarders 2");
        assert_eq!(cb.artist, "Namba Atsunori");
        assert_eq!(cb.release_id, "f3485f7b-34a3-49a4-b05a-db85d17cdeee");
        assert_eq!(cb.tracks, 21);
        assert_eq!(cb.format.as_deref(), Some("Mixed Mode CD"));
        assert_eq!(cb.year(), Some("1997"));
    }

    #[test]
    fn a_result_says_enough_to_tell_the_wrong_pressing_from_the_right_one() {
        // Both scored a hundred on the name. What separates them is that one
        // is a cassette with two tracks, which is not the disc in the drive.
        let found = parse_search(SEARCH).expect("it parses");
        assert!(found[0].detail().contains("Cassette"), "{}", found[0].detail());
        assert!(found[0].detail().contains("2 tracks"), "{}", found[0].detail());
        assert!(found[1].detail().contains("21 tracks"), "{}", found[1].detail());
    }

    #[test]
    fn a_search_that_found_nothing_is_empty_rather_than_an_error() {
        assert_eq!(parse_search(r#"{"count": 0, "releases": []}"#).unwrap(), Vec::new());
    }

    #[test]
    fn a_refusal_is_reported_rather_than_read_as_no_results() {
        let err = parse_search(r#"{"error": "Rate limit exceeded"}"#).unwrap_err();
        assert!(err.to_string().contains("Rate limit"), "{err}");
    }

    #[test]
    fn a_query_is_encoded_so_a_band_with_an_ampersand_still_searches_for_itself() {
        // Unencoded, everything after the ampersand becomes another URL
        // parameter and the search quietly runs on half the name.
        let url = MusicBrainz::search_url("Florence & the Machine", 25);
        assert!(url.contains("Florence%20%26%20the%20Machine"), "{url}");
        assert!(url.ends_with("&fmt=json&limit=25"), "{url}");
    }

    #[test]
    fn a_query_of_non_english_letters_survives_the_url() {
        let url = MusicBrainz::search_url("Sigur Rós", 5);
        assert!(url.contains("Sigur%20R%C3%B3s"), "{url}");
    }

    #[test]
    fn a_release_looked_up_by_id_reads_the_same_as_one_found_by_disc() {
        // The two endpoints answer with the same release object; only the
        // wrapping differs. Reusing the disc fixture's release proves the
        // parser is reading the release and not the envelope around it.
        let whole: serde_json::Value = serde_json::from_str(&roots()).unwrap();
        let one = whole.get("releases").unwrap().as_array().unwrap()[0].to_string();

        let album = parse_release(&one).expect("it parses").expect("there is a release");
        assert_eq!(album.title, "Roots");
        assert_eq!(album.artist, "Shawn McDonald");
        assert_eq!(album.tracks.len(), 2);
        assert_eq!(album.label.as_deref(), Some("Sparrow Records"));
    }

    #[test]
    fn a_box_set_looked_up_by_id_falls_back_to_its_first_disc() {
        // With no disc id there is nothing to say which disc of a set is in
        // the drive. Taking the first is the only thing to do and is why a
        // searched release is offered to be checked rather than trusted.
        let whole: serde_json::Value = serde_json::from_str(&roots()).unwrap();
        let one = whole.get("releases").unwrap().as_array().unwrap()[0].to_string();
        let album = parse_release(&one).unwrap().unwrap();
        assert_eq!(album.disc, 1);
    }

    #[test]
    fn a_release_that_is_not_there_is_nothing_rather_than_an_error() {
        assert_eq!(parse_release("{}").expect("valid json"), None);
    }

    #[test]
    fn a_release_is_looked_up_by_its_own_id() {
        assert_eq!(
            MusicBrainz::release_url("f3485f7b"),
            "https://musicbrainz.org/ws/2/release/f3485f7b?fmt=json&inc=artists+recordings+labels"
        );
    }
}
