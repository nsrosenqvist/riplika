//! Turning a music CD into an album on disk.
//!
//! The pipeline in [`job`](crate::job) cannot be reused for this. It scans
//! titles, works out what they are from their structure, transcodes video and
//! recognises subtitles, and a CD has none of those. What the two do share is
//! the vocabulary - the same [`Event`]s, [`Stage`]s and [`Report`] - so the
//! window shows progress for a music disc without knowing that it is one.

use crate::audio;
use crate::disc::{DiscKind, Toc};
use crate::host::{Cancel, Fs, Runner};
use crate::identify::catalogue::Http;
use crate::identify::music::{Album, MusicBrainz, MusicCatalogue, cover_art_url};
use crate::job::{Event, Produced, Report, Stage};
use crate::model::{Item, JobSettings, Role, Warning};
use crate::rip::cd::CdAudio;
use crate::{Error, Result};
use std::path::{Path, PathBuf};

pub struct Ports<'a> {
    pub runner: &'a dyn Runner,
    pub fs: &'a dyn Fs,
    pub http: &'a dyn Http,
    pub cancel: Cancel,
}

/// The disc to read, and how much of it.
pub struct Disc<'a> {
    pub device: &'a Path,
    pub toc: &'a Toc,
    /// Which tracks to take, by number. `None` takes all of them.
    ///
    /// A selection, not a smaller disc: the table of contents has to stay
    /// whole because a track's length is the distance to the next one, and the
    /// listing has to stay whole because a track is "6 of 12" either way.
    pub tracks: Option<&'a [u8]>,
}

impl<'a> Disc<'a> {
    pub fn whole(device: &'a Path, toc: &'a Toc) -> Self {
        Disc { device, toc, tracks: None }
    }
}

/// What the disc turned out to be.
#[derive(Debug, Clone)]
pub struct Found {
    pub toc: Toc,
    /// Usually exactly one. More than one means the same pressing was issued
    /// more than once, and somebody has to say which.
    pub albums: Vec<Album>,
    /// The names came off the disc itself rather than from a catalogue.
    ///
    /// Worth saying: CD-Text carries names and nothing else, so there is no
    /// release date, no label and no cover art to be had, and a rip made this
    /// way is not missing them by accident.
    pub from_cd_text: bool,
    /// Why the catalogue could not be asked, when it could not be.
    ///
    /// An empty `albums` means one of two entirely different things - the
    /// catalogue answered and had nothing, or it never answered - and saying
    /// "no release matches this disc" for the second is a lie that sends
    /// somebody looking for the fault in their disc. MusicBrainz allows one
    /// request a second and refuses the rest, so the second case is not rare.
    pub lookup_failed: Option<String>,
}

impl Found {
    /// Was the disc asked about and genuinely unknown?
    pub fn is_unknown(&self) -> bool {
        self.albums.is_empty() && self.lookup_failed.is_none()
    }
}

/// Read the disc and take its word for what it is, asking nobody.
///
/// What is left when there is no network: CD-Text names the album, the artist
/// and the tracks, and nothing else.
pub fn identify_from_disc(device: &Path, events: &mut dyn FnMut(Event)) -> Result<Found> {
    events(Event::Stage(Stage::Scan));
    let toc = read_toc(device)?;
    events(Event::Stage(Stage::Identify));
    Ok(from_disc(device, toc).unwrap_or_else(|toc| Found {
        toc,
        albums: Vec::new(),
        lookup_failed: None,
        from_cd_text: false,
    }))
}

fn read_toc(device: &Path) -> Result<Toc> {
    match crate::disc::identify(device) {
        DiscKind::Audio(toc) => Ok(toc),
        _ => Err(Error(format!("{} is not holding a music CD", device.display()))),
    }
}

/// What the disc says about itself, or the table of contents back unused.
fn from_disc(device: &Path, toc: Toc) -> std::result::Result<Found, Toc> {
    match crate::cdtext::read(device) {
        Some(text) => {
            let album = Album::from_cd_text(&toc, &text);
            Ok(Found { toc, albums: vec![album], lookup_failed: None, from_cd_text: true })
        }
        None => Err(toc),
    }
}

/// Read the disc and ask what it is.
pub fn identify(device: &Path, http: &dyn Http, events: &mut dyn FnMut(Event)) -> Result<Found> {
    events(Event::Stage(Stage::Scan));
    let toc = read_toc(device)?;

    events(Event::Stage(Stage::Identify));
    // Not being able to name the album is no reason to refuse the disc, so a
    // failure here is carried rather than raised - but it is carried, not
    // discarded.
    let (albums, lookup_failed) = match MusicBrainz::new(http).by_disc_id(&toc.musicbrainz_id()) {
        Ok(albums) => (albums, None),
        Err(why) => {
            events(Event::Warning(Warning::CouldNotIdentify { why: why.to_string() }));
            (Vec::new(), Some(why.to_string()))
        }
    };
    // Nothing from the catalogue - either it had never heard of the disc, or it
    // never answered. Either way the disc may still say what it is.
    if albums.is_empty() {
        match from_disc(device, toc) {
            Ok(found) => return Ok(Found { lookup_failed, ..found }),
            Err(toc) => return Ok(Found { toc, albums, lookup_failed, from_cd_text: false }),
        }
    }
    Ok(Found { toc, albums, lookup_failed, from_cd_text: false })
}

/// Rip, encode and tag every track of `album`.
///
/// `scratch` is where the raw audio lands between being read and being
/// encoded. Taken as an argument rather than made here, for the same reason
/// the video pipeline takes its rip directory: it is a decision about the
/// machine - which disk has room, which one is not a RAM-backed `/tmp` - and
/// it is what lets this run against a fake filesystem with no disc.
pub fn rip(
    ports: &Ports,
    disc: &Disc,
    album: &Album,
    settings: &JobSettings,
    scratch: &Path,
    events: &mut dyn FnMut(Event),
) -> Result<Report> {
    let target = settings.music_format.target();
    let cover = fetch_cover(ports, album, scratch, events);
    let cd = CdAudio::new(ports.runner);

    // Tracks the disc has that the listing knows about. A data track at the end
    // of an enhanced CD is not music and is not in the listing either.
    let wanted: Vec<_> = disc
        .toc
        .audio_tracks()
        .filter(|t| disc.tracks.is_none_or(|w| w.contains(&t.number)))
        .filter_map(|t| {
            album.tracks.iter().find(|x| x.number == u32::from(t.number)).map(|m| (t.number, m))
        })
        .collect();
    if wanted.is_empty() {
        return Err(Error("nothing on this disc matches the release listing".into()));
    }

    let mut report = Report::default();
    let total = wanted.len();
    for (index, (number, meta)) in wanted.iter().enumerate() {
        ports.cancel.check()?;
        events(Event::ItemStarted { index, total, name: meta.title.clone() });

        let dest = audio::track_path(
            &settings.output_dir,
            album,
            meta,
            target.extension(),
            settings.music_template.as_deref(),
        );
        if let Some(dir) = dest.parent() {
            ports.fs.create_dir_all(dir)?;
        }

        events(Event::Progress {
            stage: Stage::Rip,
            fraction: index as f32 / total as f32,
            message: Some(meta.title.clone()),
        });
        let wav = scratch.join(format!("track{number:02}.wav"));
        match read_one(&cd, ports, disc, *number, &wav) {
            Ok(()) => {}
            // One unreadable track should not cost the other eleven.
            Err(e) => {
                events(Event::Warning(Warning::ItemSkipped {
                    name: meta.title.clone(),
                    why: e.to_string(),
                }));
                report.skipped.push((dest, e.to_string()));
                continue;
            }
        }

        events(Event::Progress {
            stage: Stage::Transcode,
            fraction: index as f32 / total as f32,
            message: Some(meta.title.clone()),
        });
        // Written under a temporary name and moved only once it is whole: a
        // file that appears under its real name is one the next run counts as
        // finished.
        let part = dest.with_extension(format!("{}.part", target.extension()));
        let cmd = audio::encode_command(
            target,
            settings.music_quality,
            &wav,
            cover.as_deref(),
            &part,
            album,
            meta,
        );
        let out = ports.runner.run(&cmd)?;
        if !out.ok() {
            let why = out.last_error().to_string();
            let _ = ports.fs.remove_file(&part);
            events(Event::Warning(Warning::ItemSkipped {
                name: meta.title.clone(),
                why: why.clone(),
            }));
            report.skipped.push((dest, why));
            continue;
        }
        ports.fs.rename(&part, &dest)?;
        let _ = ports.fs.remove_file(&wav);

        let bytes = ports.fs.size(&dest).unwrap_or(0);
        events(Event::ItemFinished { index, destination: dest.clone(), bytes });
        report.produced.push(Produced {
            item: Item {
                source: wav,
                // Not consulted on this path: nothing here files a track by its
                // role, and a track is not an episode, an extra or a feature.
                role: Role::Feature,
                title: meta.title.clone(),
                air_date: album.date.clone(),
                duration: meta.duration.unwrap_or(0),
                destination: Some(dest),
            },
            destination: PathBuf::new(),
            bytes,
            subtitles: Vec::new(),
        });
    }
    Ok(report)
}

fn read_one(cd: &CdAudio, ports: &Ports, disc: &Disc, number: u8, wav: &Path) -> Result<()> {
    cd.rip_track(disc.device, number, wav)?;
    let size = ports.fs.size(wav)?;
    cd.check_size(disc.toc, number, size)
}

/// Fetch the front cover once for the album, not once per track.
fn fetch_cover(
    ports: &Ports,
    album: &Album,
    scratch: &Path,
    events: &mut dyn FnMut(Event),
) -> Option<PathBuf> {
    if !album.has_cover_art {
        return None;
    }
    let path = scratch.join("cover.jpg");
    match ports.http.get_bytes(&cover_art_url(&album.release_id)) {
        Ok(bytes) => ports.fs.write(&path, &bytes).ok().map(|()| path),
        // Art is worth having and not worth failing the rip over.
        Err(why) => {
            events(Event::Warning(Warning::CouldNotIdentify {
                why: format!("no cover art: {why}"),
            }));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::disc::Track;
    use crate::host::{FakeFs, FakeRunner};
    use crate::identify::catalogue::FakeHttp;
    use crate::identify::music::AlbumTrack;
    use crate::prefs::AudioFormat;

    /// A two-track disc, small enough that the expected sizes fit in a test.
    fn toc() -> Toc {
        Toc {
            tracks: vec![
                Track { number: 1, start: 0, is_data: false },
                Track { number: 2, start: 1, is_data: false },
            ],
            leadout: 2,
        }
    }

    fn album() -> Album {
        Album {
            title: "Roots".into(),
            artist: "Shawn McDonald".into(),
            tracks: vec![
                AlbumTrack { number: 1, title: "Clarity".into(), artist: None, duration: None },
                AlbumTrack { number: 2, title: "Captivated".into(), artist: None, duration: None },
            ],
            date: Some("2008-03-11".into()),
            country: None,
            barcode: None,
            label: None,
            catalogue_number: None,
            disc: 1,
            disc_count: 1,
            disc_title: None,
            release_id: "abc".into(),
            has_cover_art: false,
        }
    }

    /// One frame of audio plus the WAV header - what a whole track weighs on
    /// this made-up disc.
    fn whole_track() -> String {
        "x".repeat(2352 + 44)
    }

    fn settings(root: &str) -> JobSettings {
        JobSettings {
            output_dir: PathBuf::from(root),
            music_format: AudioFormat::Flac,
            ..Default::default()
        }
    }

    fn run(fs: &FakeFs, runner: &FakeRunner) -> Result<Report> {
        let http = FakeHttp::new();
        let ports = Ports { runner, fs, http: &http, cancel: Cancel::new() };
        let mut events = |_: Event| {};
        rip(
            &ports,
            &Disc::whole(Path::new("/dev/riplika-no-such-device"), &toc()),
            &album(),
            &settings("/music"),
            Path::new("/scratch"),
            &mut events,
        )
    }

    #[test]
    fn every_track_is_read_encoded_and_given_its_real_name() {
        let fs = FakeFs::new()
            .with_file("/scratch/track01.wav", &whole_track())
            .with_file("/scratch/track02.wav", &whole_track());
        let runner = FakeRunner::new();
        let report = run(&fs, &runner).unwrap();

        assert_eq!(report.produced.len(), 2);
        assert_eq!(
            report.produced[0].item.destination,
            Some(PathBuf::from("/music/Shawn McDonald/Roots (2008)/01 - Clarity.flac"))
        );
        assert!(report.is_complete());
    }

    #[test]
    fn a_track_is_read_before_it_is_encoded_and_not_the_other_way_round() {
        let fs = FakeFs::new()
            .with_file("/scratch/track01.wav", &whole_track())
            .with_file("/scratch/track02.wav", &whole_track());
        let runner = FakeRunner::new();
        run(&fs, &runner).unwrap();
        let programs: Vec<String> = runner.calls().iter().map(|c| c.program.clone()).collect();
        assert_eq!(programs, ["cdparanoia", "ffmpeg", "cdparanoia", "ffmpeg"]);
    }

    #[test]
    fn nothing_appears_under_its_real_name_until_it_is_whole() {
        // A file that turns up under its final name is one the next run counts
        // as finished, so the encode writes elsewhere and is moved after.
        let fs = FakeFs::new().with_file("/scratch/track01.wav", &whole_track());
        let runner = FakeRunner::new();
        let ports =
            Ports { runner: &runner, fs: &fs, http: &FakeHttp::new(), cancel: Cancel::new() };
        let mut events = |_: Event| {};
        let mut one = album();
        one.tracks.truncate(1);
        rip(
            &ports,
            &Disc::whole(Path::new("/dev/riplika-no-such-device"), &toc()),
            &one,
            &settings("/music"),
            Path::new("/scratch"),
            &mut events,
        )
        .unwrap();
        let written = runner.calls().last().unwrap().args.last().unwrap().clone();
        assert!(written.ends_with(".part"), "encoded straight to its real name: {written}");
    }

    #[test]
    fn a_short_read_costs_that_track_and_not_the_others() {
        // One unreadable track should not throw away the eleven that read.
        let fs = FakeFs::new()
            .with_file("/scratch/track01.wav", "far too small")
            .with_file("/scratch/track02.wav", &whole_track());
        let runner = FakeRunner::new();
        let report = run(&fs, &runner).unwrap();
        assert_eq!(report.produced.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        assert!(!report.is_complete(), "a disc with a hole in it is not a finished job");
        assert_eq!(report.produced[0].item.title, "Captivated");
    }

    #[test]
    fn a_failed_encode_is_reported_rather_than_left_as_a_part_file() {
        let fs = FakeFs::new()
            .with_file("/scratch/track01.wav", &whole_track())
            .with_file("/scratch/track02.wav", &whole_track());
        let runner = FakeRunner::new().fail("ffmpeg", "Invalid argument");
        let report = run(&fs, &runner).unwrap();
        assert!(report.produced.is_empty());
        assert_eq!(report.skipped.len(), 2);
        assert!(!fs.files().iter().any(|p| p.to_string_lossy().ends_with(".part")));
    }

    #[test]
    fn a_data_track_is_not_ripped_as_though_it_were_music() {
        let mut t = toc();
        t.tracks.push(Track { number: 3, start: 2, is_data: true });
        t.leadout = 3;
        let fs = FakeFs::new()
            .with_file("/scratch/track01.wav", &whole_track())
            .with_file("/scratch/track02.wav", &whole_track());
        let runner = FakeRunner::new();
        let http = FakeHttp::new();
        let ports = Ports { runner: &runner, fs: &fs, http: &http, cancel: Cancel::new() };
        let mut events = |_: Event| {};
        let report = rip(
            &ports,
            &Disc::whole(Path::new("/dev/riplika-no-such-device"), &t),
            &album(),
            &settings("/music"),
            Path::new("/scratch"),
            &mut events,
        )
        .unwrap();
        assert_eq!(report.produced.len(), 2, "the data track is not a song");
    }

    #[test]
    fn a_cancelled_run_stops_rather_than_finishing_quietly() {
        let fs = FakeFs::new().with_file("/scratch/track01.wav", &whole_track());
        let runner = FakeRunner::new();
        let http = FakeHttp::new();
        let cancel = Cancel::new();
        cancel.cancel();
        let ports = Ports { runner: &runner, fs: &fs, http: &http, cancel };
        let mut events = |_: Event| {};
        let r = rip(
            &ports,
            &Disc::whole(Path::new("/dev/riplika-no-such-device"), &toc()),
            &album(),
            &settings("/music"),
            Path::new("/scratch"),
            &mut events,
        );
        assert!(r.is_err());
        assert!(runner.calls().is_empty(), "it should not have read anything");
    }

    #[test]
    fn a_selection_takes_some_tracks_without_pretending_the_disc_is_smaller() {
        // Narrowing the disc instead would make track two's length run to the
        // lead-out, and tag it as track two of one.
        let fs = FakeFs::new()
            .with_file("/scratch/track01.wav", &whole_track())
            .with_file("/scratch/track02.wav", &whole_track());
        let runner = FakeRunner::new();
        let http = FakeHttp::new();
        let ports = Ports { runner: &runner, fs: &fs, http: &http, cancel: Cancel::new() };
        let mut events = |_: Event| {};
        let report = rip(
            &ports,
            &Disc {
                device: Path::new("/dev/riplika-no-such-device"),
                toc: &toc(),
                tracks: Some(&[2]),
            },
            &album(),
            &settings("/music"),
            Path::new("/scratch"),
            &mut events,
        )
        .unwrap();
        assert_eq!(report.produced.len(), 1);
        assert_eq!(report.produced[0].item.title, "Captivated");
        // The total in the tags comes from the listing, which is still whole.
        let tags = runner.calls().last().unwrap().args.join(" ");
        assert!(tags.contains("TOTALTRACKS=2"), "{tags}");
    }

    #[test]
    fn a_catalogue_that_could_not_be_reached_is_not_the_same_as_an_unknown_disc() {
        // Both come back with no album, and telling somebody "no release
        // matches this disc" when the request never went out sends them
        // looking for a fault in a disc that has none.
        let asked =
            Found { toc: toc(), albums: Vec::new(), lookup_failed: None, from_cd_text: false };
        let never = Found {
            toc: toc(),
            albums: Vec::new(),
            lookup_failed: Some("503 rate limited".into()),
            from_cd_text: false,
        };
        assert!(asked.is_unknown());
        assert!(!never.is_unknown());
    }

    #[test]
    fn a_disc_whose_tracks_are_not_in_the_listing_says_so() {
        let fs = FakeFs::new();
        let runner = FakeRunner::new();
        let http = FakeHttp::new();
        let ports = Ports { runner: &runner, fs: &fs, http: &http, cancel: Cancel::new() };
        let mut events = |_: Event| {};
        let mut empty = album();
        empty.tracks.clear();
        let err = rip(
            &ports,
            &Disc::whole(Path::new("/dev/riplika-no-such-device"), &toc()),
            &empty,
            &settings("/music"),
            Path::new("/scratch"),
            &mut events,
        )
        .unwrap_err();
        assert!(err.to_string().contains("listing"), "{err}");
    }
}
