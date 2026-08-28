//! Running the four stages in order, and saying what is happening.
//!
//! The pipeline reports through an event callback rather than printing. A CLI
//! renders events as lines and a GUI renders them as progress bars, and neither
//! needs the other's idea of what output looks like.
//!
//! It is also deliberately *interruptible in the middle*. `rip`, `organise` and
//! `produce` are separate calls, because the GUI has to stop between the second
//! and third to show what was identified and let the user correct it. A single
//! `run()` that did everything would work for a script and be useless for a
//! window.

use crate::host::{Cancel, Fs, Runner};
use crate::identify::{self, catalogue::Catalogue, structure};
use crate::media::Prober;
use crate::model::*;
use crate::rip::Ripper;
use crate::subs::{self, table::Table};
use crate::transcode::{self, analyze, SubtitleInput};
use crate::{lang, naming, Error, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Scan,
    Identify,
    Rip,
    Organise,
    Subtitles,
    Transcode,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Scan => "Scanning disc",
            Stage::Identify => "Identifying",
            Stage::Rip => "Ripping",
            Stage::Organise => "Sorting titles",
            Stage::Subtitles => "Reading subtitles",
            Stage::Transcode => "Transcoding",
        }
    }
}

/// Something worth telling the user about.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Stage(Stage),
    Progress {
        stage: Stage,
        /// 0.0 to 1.0.
        fraction: f32,
        message: Option<String>,
    },
    ItemStarted {
        index: usize,
        total: usize,
        name: String,
    },
    ItemFinished {
        index: usize,
        destination: PathBuf,
        bytes: u64,
    },
    Subtitle {
        item: usize,
        language: String,
        cues: usize,
        unknown: usize,
        recognised: bool,
    },
    /// Something went wrong that did not stop the run.
    Warning(String),
}

pub type Events<'a> = &'a mut dyn FnMut(Event);

/// One finished output file.
#[derive(Debug, Clone, PartialEq)]
pub struct Produced {
    pub item: Item,
    pub destination: PathBuf,
    pub bytes: u64,
    pub subtitles: Vec<RecognisedSubtitle>,
}

/// What a run did, for the results screen.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub produced: Vec<Produced>,
    pub skipped: Vec<(PathBuf, String)>,
    pub warnings: Vec<String>,
}

impl Report {
    pub fn total_bytes(&self) -> u64 {
        self.produced.iter().map(|p| p.bytes).sum()
    }

    pub fn is_complete(&self) -> bool {
        self.skipped.is_empty()
    }
}

/// The outside world, in one bundle.
///
/// Every field is a trait object, so a test constructs a `Ports` of fakes and
/// drives the whole pipeline in milliseconds with no disc, no ffmpeg and no
/// network.
pub struct Ports<'a> {
    pub runner: &'a dyn Runner,
    pub prober: &'a dyn Prober,
    pub ripper: &'a dyn Ripper,
    pub catalogue: &'a dyn Catalogue,
    pub fs: &'a dyn Fs,
    pub cancel: Cancel,
}

pub struct Pipeline<'a> {
    pub ports: Ports<'a>,
    pub settings: JobSettings,
}

impl<'a> Pipeline<'a> {
    pub fn new(ports: Ports<'a>, settings: JobSettings) -> Self {
        Pipeline { ports, settings }
    }

    pub fn drives(&self) -> Result<Vec<Drive>> {
        self.ports.ripper.drives()
    }

    pub fn scan(&self, drive: &Drive, events: Events) -> Result<DiscScan> {
        events(Event::Stage(Stage::Scan));
        self.ports.ripper.scan(drive)
    }

    /// Guess what the disc is. Never fatal: an unidentified disc can still be
    /// ripped, and the user can say what it is.
    pub fn identify(&self, scan: &DiscScan, events: Events) -> Vec<Candidate> {
        events(Event::Stage(Stage::Identify));
        match identify::identify(scan, self.ports.catalogue) {
            Ok(c) => c,
            Err(e) => {
                events(Event::Warning(format!("could not identify the disc: {e}")));
                Vec::new()
            }
        }
    }

    /// Look something up by hand, for when the guess was wrong.
    pub fn search(&self, query: &str, season: Option<u32>) -> Result<Vec<Candidate>> {
        identify::search(self.ports.catalogue, query, season)
    }

    /// Stage one: read the disc.
    pub fn rip(&self, scan: &DiscScan, dest: &Path, events: Events) -> Result<Vec<PathBuf>> {
        events(Event::Stage(Stage::Rip));
        self.ports.fs.create_dir_all(dest)?;
        self.ports.ripper.rip(
            &scan.drive,
            &scan.titles,
            dest,
            &mut |fraction, message| {
                events(Event::Progress {
                    stage: Stage::Rip,
                    fraction,
                    message: message.map(str::to_string),
                });
            },
        )
    }

    /// Stages two and three: sort the ripped files out and name them.
    ///
    /// The result is data, not action. The caller is expected to show it and
    /// let it be edited before anything is written.
    pub fn organise(
        &self,
        files: &[PathBuf],
        media: &Media,
        disc: Option<u32>,
        events: Events,
    ) -> Result<Vec<Item>> {
        events(Event::Stage(Stage::Organise));
        self.ports.cancel.check()?;

        let shapes = identify::shapes(self.ports.prober, files)?;
        let plain: Vec<structure::TitleShape> = shapes.iter().map(|(s, _)| s.clone()).collect();
        let mut st = structure::decompose(&plain, structure::EpisodeRange::default());

        if st.episodes.is_empty() {
            // No play-all: fall back to the house-length cluster, and say so,
            // because that ordering is a guess where the other one is evidence.
            let by_duration = structure::episodes_by_duration(&plain, structure::EpisodeRange::default());
            if !by_duration.is_empty() {
                events(Event::Warning(format!(
                    "no play-all title on this disc; ordering {} episodes by disc layout instead",
                    by_duration.len()
                )));
                st.loose.retain(|k| !by_duration.contains(k));
                st.episodes = by_duration;
            }
        }

        let dir = files
            .first()
            .and_then(|f| f.parent())
            .map(Path::to_path_buf)
            .unwrap_or_default();

        let extended = if st.loose.is_empty() {
            Vec::new()
        } else {
            match structure::find_extended_cuts(self.ports.runner, &dir, &st.loose, &st.episodes) {
                Ok(e) => e,
                Err(e) => {
                    events(Event::Warning(format!(
                        "could not compare titles for extended cuts: {e}"
                    )));
                    Vec::new()
                }
            }
        };

        let episodes = match (media, media.provider_id()) {
            (Media::Series { season, .. }, Some(id)) => self
                .ports
                .catalogue
                .episodes(&id, *season)
                .unwrap_or_default(),
            _ => Vec::new(),
        };

        let existing = self
            .ports
            .fs
            .list(&self.season_dir(media))
            .map(|f| identify::existing_episode_numbers(&f))
            .unwrap_or_default();
        let offset = identify::episode_offset(disc, st.episodes.len(), &existing);
        if offset > 0 {
            events(Event::Progress {
                stage: Stage::Organise,
                fraction: 1.0,
                message: Some(format!("continuing from episode {}", offset + 1)),
            });
        }

        let mut items = identify::assign(media, &episodes, &st, &dir, offset, &extended);
        for item in &mut items {
            if let Some((shape, _)) = shapes.iter().find(|(s, _)| item.source.ends_with(&s.key)) {
                item.duration = shape.duration;
            }
            if item.role.is_output() {
                item.destination = Some(naming::destination(
                    &self.settings.output_dir,
                    media,
                    item,
                    self.settings.container,
                ));
            }
        }
        Ok(items)
    }

    fn season_dir(&self, media: &Media) -> PathBuf {
        match media {
            Media::Series { season, .. } => self
                .settings
                .output_dir
                .join(naming::sanitize(&format!("Season {season:02}"))),
            Media::Movie { .. } => self.settings.output_dir.clone(),
        }
    }

    /// Work out the plan from a scan alone, without ripping anything.
    ///
    /// Only possible when the scanner reported chapter *durations*: those are
    /// what decompose a play-all, and MakeMKV gives only a count. When they are
    /// there this is what `--dry-run` should show, and it costs nothing beyond
    /// the scan that has already happened.
    pub fn preview(
        &self,
        scan: &DiscScan,
        media: &Media,
        disc: Option<u32>,
        rip_dir: &Path,
    ) -> Option<Vec<Item>> {
        if scan.titles.iter().all(|t| t.chapters.is_empty()) {
            return None;
        }
        let shapes: Vec<structure::TitleShape> = scan
            .titles
            .iter()
            .enumerate()
            .map(|(i, t)| structure::TitleShape {
                key: t.output_name.clone(),
                order: i as u32,
                duration: t.duration,
                chapters: t.chapters.clone(),
            })
            .collect();
        let mut st = structure::decompose(&shapes, structure::EpisodeRange::default());
        if st.episodes.is_empty() {
            let by_duration =
                structure::episodes_by_duration(&shapes, structure::EpisodeRange::default());
            st.loose.retain(|k| !by_duration.contains(k));
            st.episodes = by_duration;
        }

        let episodes = match (media, media.provider_id()) {
            (Media::Series { season, .. }, Some(id)) => {
                self.ports.catalogue.episodes(&id, *season).unwrap_or_default()
            }
            _ => Vec::new(),
        };
        let existing = self
            .ports
            .fs
            .list(&self.season_dir(media))
            .map(|f| identify::existing_episode_numbers(&f))
            .unwrap_or_default();
        let offset = identify::episode_offset(disc, st.episodes.len(), &existing);

        // Extended cuts need the pictures compared, which needs the files, so a
        // preview cannot spot them. Everything else is the real mapping.
        let mut items = identify::assign(media, &episodes, &st, rip_dir, offset, &[]);
        for item in &mut items {
            if let Some(t) = scan
                .titles
                .iter()
                .find(|t| item.source.ends_with(&t.output_name))
            {
                item.duration = t.duration;
            }
            if item.role.is_output() {
                item.destination = Some(naming::destination(
                    &self.settings.output_dir,
                    media,
                    item,
                    self.settings.container,
                ));
            }
        }
        Some(items)
    }

    /// Stage four and the encode: produce the output files.
    pub fn produce(&self, items: &[Item], media: &Media, events: Events) -> Result<Report> {
        let mut report = Report::default();
        let outputs: Vec<&Item> = items.iter().filter(|i| i.role.is_output()).collect();

        let table = match &self.settings.glyph_table {
            Some(p) if self.ports.fs.exists(p) => match Table::load(p) {
                Ok(t) => Some(t),
                Err(e) => {
                    let w = format!("glyph table {}: {e}", p.display());
                    events(Event::Warning(w.clone()));
                    report.warnings.push(w);
                    None
                }
            },
            Some(p) => {
                let w = format!("glyph table {} does not exist", p.display());
                events(Event::Warning(w.clone()));
                report.warnings.push(w);
                None
            }
            None => None,
        };

        for (n, item) in outputs.iter().enumerate() {
            self.ports.cancel.check()?;
            let Some(dest) = item.destination.clone() else {
                continue;
            };
            events(Event::ItemStarted {
                index: n,
                total: outputs.len(),
                name: dest
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            });

            match self.produce_one(item, media, &dest, &table, n, events) {
                Ok(p) => {
                    events(Event::ItemFinished {
                        index: n,
                        destination: p.destination.clone(),
                        bytes: p.bytes,
                    });
                    report.produced.push(p);
                }
                Err(e) => {
                    // One bad title must not abandon the other twenty-one.
                    events(Event::Warning(format!(
                        "{}: {e}",
                        item.source.file_name().unwrap_or_default().to_string_lossy()
                    )));
                    report.skipped.push((item.source.clone(), e.0));
                }
            }
        }
        Ok(report)
    }

    fn produce_one(
        &self,
        item: &Item,
        media: &Media,
        dest: &Path,
        table: &Option<Table>,
        index: usize,
        events: Events,
    ) -> Result<Produced> {
        if let Some(parent) = dest.parent() {
            self.ports.fs.create_dir_all(parent)?;
        }
        let info = self.ports.prober.probe(&item.source)?;

        // Subtitles first. Recognising from the rip rather than from the
        // transcode means the SRTs exist before encoding starts, so they can be
        // inputs to the same pass - one ffmpeg invocation instead of three.
        events(Event::Stage(Stage::Subtitles));
        let (subtitles, failed, recognised) =
            self.recognise_all(&info, item, table, index, events)?;

        events(Event::Stage(Stage::Transcode));
        let analysis = analyze::analyze(self.ports.runner, &item.source, &info)?;
        // Encode to a temporary name and rename on success. ffmpeg writing
        // straight to the destination means an interrupted run leaves a
        // truncated file at the final path - which the *next* run then counts
        // as a finished episode and skips, silently losing it.
        let partial = partial_path(dest);
        let _ = self.ports.fs.remove_file(&partial);
        let plan = transcode::plan(
            &item.source,
            &partial,
            &info,
            &analysis,
            &self.settings,
            subtitles,
            &failed,
            naming::tags(media, item),
        );
        if let Err(e) = self.ports.runner.require(&plan.command()) {
            let _ = self.ports.fs.remove_file(&partial);
            return Err(e);
        }
        self.ports.fs.rename(&partial, dest)?;

        let bytes = self.ports.fs.size(dest).unwrap_or(0);
        for s in &recognised {
            let _ = self.ports.fs.remove_file(&s.srt_path);
        }
        Ok(Produced {
            item: (*item).clone(),
            destination: dest.to_path_buf(),
            bytes,
            subtitles: recognised,
        })
    }

    /// Recognise every wanted bitmap subtitle track, each in its own language.
    ///
    /// Returns the SRTs to mux, the streams that failed, and what was produced.
    fn recognise_all(
        &self,
        info: &MediaInfoAlias,
        item: &Item,
        table: &Option<Table>,
        index: usize,
        events: Events,
    ) -> Result<(Vec<SubtitleInput>, Vec<usize>, Vec<RecognisedSubtitle>)> {
        let wanted = transcode::subtitles_to_recognise(info, &self.settings.languages);
        let Some(table) = table else {
            if !wanted.is_empty() {
                events(Event::Warning(
                    "no glyph table, so subtitles stay as bitmaps".into(),
                ));
            }
            // Without recognition the bitmaps are all there is: keep every one,
            // since dropping them would lose those languages outright.
            return Ok((Vec::new(), wanted, Vec::new()));
        };

        let subs_tracks = info.tracks_of(TrackKind::Subtitle);
        let mut inputs = Vec::new();
        let mut failed = Vec::new();
        let mut recognised = Vec::new();

        for stream in wanted {
            self.ports.cancel.check()?;
            let code = subs_tracks
                .get(stream)
                .map(|t| t.language.clone())
                .unwrap_or_else(|| "und".into());
            let language = lang::parse(&code);
            let srt_path = item.source.with_extension(format!("{}.srt", language.code));

            match subs::recognise_to_file(
                self.ports.runner,
                &item.source,
                stream,
                &language,
                table,
                self.settings.words_dir.as_deref(),
                &srt_path,
            ) {
                Ok((r, detail)) if detail.is_usable() => {
                    events(Event::Subtitle {
                        item: index,
                        language: language.name.clone(),
                        cues: r.cues,
                        unknown: r.unknown_glyphs,
                        recognised: true,
                    });
                    if r.unknown_glyphs > 0 {
                        events(Event::Warning(format!(
                            "{}: {} unrecognised glyphs - the table may not cover {}",
                            language.name, r.unknown_glyphs, language.name
                        )));
                    }
                    inputs.push(SubtitleInput {
                        path: r.srt_path.clone(),
                        language: language.code.clone(),
                    });
                    recognised.push(r);
                }
                Ok((_, _)) | Err(_) => {
                    // Keeping the bitmap is the safety net: a language with an
                    // unusable text track still has *a* track, and losing the
                    // language entirely is much worse than the redundancy.
                    events(Event::Subtitle {
                        item: index,
                        language: language.name.clone(),
                        cues: 0,
                        unknown: 0,
                        recognised: false,
                    });
                    let _ = self.ports.fs.remove_file(&srt_path);
                    failed.push(stream);
                }
            }
        }
        Ok((inputs, failed, recognised))
    }

    /// Everything, for a script that has already made the choices.
    pub fn run(
        &self,
        drive: &Drive,
        media: Option<Media>,
        rip_dir: &Path,
        events: Events,
    ) -> Result<Report> {
        let scan = self.scan(drive, events)?;
        let media = match media {
            Some(m) => m,
            None => {
                let candidates = self.identify(&scan, events);
                candidates
                    .into_iter()
                    .next()
                    .map(|c| c.media)
                    .ok_or_else(|| {
                        Error(format!(
                            "could not identify {:?}; say what it is with --title",
                            scan.label
                        ))
                    })?
            }
        };
        let disc = identify::label::parse(&scan.label).disc;
        let files = self.rip(&scan, rip_dir, events)?;
        let items = self.organise(&files, &media, disc, events)?;
        self.produce(&items, &media, events)
    }
}

/// Where a file is written while it is still being made.
///
/// The suffix comes after the extension so the half-written file is not a valid
/// media file by name either - nothing scanning the directory will pick it up.
pub fn partial_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_os_string();
    s.push(".part");
    PathBuf::from(s)
}

/// Alias so the signature above stays readable.
type MediaInfoAlias = crate::media::MediaInfo;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{FakeFs, FakeRunner};
    use crate::identify::catalogue::{FakeHttp, TvMaze};
    use crate::media::{Chapter, FakeProber, MediaInfo};
    use crate::rip::FakeRipper;

    const SEARCH: &str = r#"[{"score":0.94,"show":{"id":1633,"name":"Parks and Recreation","premiered":"2009-04-09"}}]"#;
    const EPISODES: &str = r#"[
      {"name":"2017","season":7,"number":1,"airdate":"2015-01-13","runtime":30},
      {"name":"Ron and Jammy","season":7,"number":2,"airdate":"2015-01-13","runtime":30}
    ]"#;

    fn track(kind: TrackKind, index: usize, codec: &str, lang: &str) -> Track {
        Track {
            kind,
            index,
            codec: codec.into(),
            language: lang.into(),
            channels: 6,
            title: None,
            default: index == 0,
        }
    }

    fn episode_info(chapters: &[Millis]) -> MediaInfo {
        let mut start = 0;
        MediaInfo {
            duration: chapters.iter().sum(),
            width: 720,
            height: 480,
            sample_aspect: Some("32:27".into()),
            declared_fps: 29.97,
            chapters: chapters
                .iter()
                .map(|d| {
                    let c = Chapter { start, end: start + d };
                    start += d;
                    c
                })
                .collect(),
            tracks: vec![
                track(TrackKind::Video, 0, "mpeg2video", "und"),
                track(TrackKind::Audio, 0, "ac3", "eng"),
                track(TrackKind::Subtitle, 0, "dvd_subtitle", "eng"),
            ],
        }
    }

    /// A disc with two episodes and the play-all that orders them.
    fn fake_disc() -> DiscScan {
        DiscScan {
            drive: Drive {
                id: "disc:0".into(),
                device: "/dev/sr0".into(),
                name: "drive".into(),
                disc_label: Some("PARKS_AND_RECREATION_S7D1".into()),
            },
            label: "PARKS_AND_RECREATION_S7D1".into(),
            titles: vec![
                DiscTitle { id: 0, duration: 2_550_000, chapter_count: 4, chapters: vec![], size_bytes: 0, output_name: "title_t00.mkv".into(), tracks: vec![] },
                DiscTitle { id: 1, duration: 1_275_000, chapter_count: 2, chapters: vec![], size_bytes: 0, output_name: "title_t01.mkv".into(), tracks: vec![] },
                DiscTitle { id: 2, duration: 1_275_000, chapter_count: 2, chapters: vec![], size_bytes: 0, output_name: "title_t02.mkv".into(), tracks: vec![] },
            ],
        }
    }

    struct Harness {
        runner: FakeRunner,
        prober: FakeProber,
        ripper: FakeRipper,
        http: FakeHttp,
        fs: FakeFs,
    }

    fn harness() -> Harness {
        let ep = episode_info(&[600_000, 675_000]);
        let play = episode_info(&[600_000, 675_000, 600_000, 675_000]);
        Harness {
            runner: FakeRunner::new()
                .on("-i /rip/title_t01.mkv -an", "frame=  479")
                .on("-i /rip/title_t02.mkv -an", "frame=  479")
                .on("cropdetect", "crop=720:480:0:0"),
            prober: FakeProber::new()
                .with("/rip/title_t00.mkv", play)
                .with("/rip/title_t01.mkv", ep.clone())
                .with("/rip/title_t02.mkv", ep),
            ripper: FakeRipper::new(fake_disc()),
            http: FakeHttp::new().on("/search/shows", SEARCH).on("/episodes", EPISODES),
            fs: FakeFs::new(),
        }
    }

    fn settings() -> JobSettings {
        JobSettings {
            output_dir: PathBuf::from("/media"),
            ..JobSettings::default()
        }
    }

    fn run_all(h: &Harness, s: JobSettings) -> (Vec<Item>, Report, Vec<Event>) {
        let cat = TvMaze { http: &h.http };
        let p = Pipeline::new(
            Ports {
                runner: &h.runner,
                prober: &h.prober,
                ripper: &h.ripper,
                catalogue: &cat,
                fs: &h.fs,
                cancel: Cancel::new(),
            },
            s,
        );
        let mut events = Vec::new();
        let mut sink = |e: Event| events.push(e);
        let scan = p.scan(&fake_disc().drive, &mut sink).unwrap();
        let media = p.identify(&scan, &mut sink).remove(0).media;
        let files = p.rip(&scan, Path::new("/rip"), &mut sink).unwrap();
        let items = p.organise(&files, &media, Some(1), &mut sink).unwrap();
        let report = p.produce(&items, &media, &mut sink).unwrap();
        (items, report, events)
    }

    #[test]
    fn a_whole_disc_goes_through_without_touching_hardware() {
        let h = harness();
        let (items, report, _) = run_all(&h, settings());
        let episodes: Vec<&Item> = items
            .iter()
            .filter(|i| matches!(i.role, Role::Episode { .. }))
            .collect();
        assert_eq!(episodes.len(), 2);
        assert_eq!(report.produced.len(), 2);
        assert!(report.is_complete());
    }

    #[test]
    fn the_play_all_is_understood_and_not_written_out() {
        let h = harness();
        let (items, report, _) = run_all(&h, settings());
        assert!(items.iter().any(|i| i.role == Role::PlayAll));
        // three ripped titles, two output files
        assert_eq!(report.produced.len(), 2);
        assert!(!report
            .produced
            .iter()
            .any(|p| p.destination.to_string_lossy().contains("t00")));
    }

    #[test]
    fn episodes_are_named_and_filed_from_the_catalogue() {
        let h = harness();
        let (_, report, _) = run_all(&h, settings());
        assert_eq!(
            report.produced[0].destination,
            PathBuf::from("/media/Season 07/Parks and Recreation - S07E01 - 2017.mp4")
        );
        assert_eq!(
            report.produced[1].destination,
            PathBuf::from("/media/Season 07/Parks and Recreation - S07E02 - Ron and Jammy.mp4")
        );
    }

    #[test]
    fn the_output_directory_is_created_before_writing_into_it() {
        let h = harness();
        run_all(&h, settings());
        assert!(h
            .fs
            .created_dirs()
            .iter()
            .any(|d| d == Path::new("/media/Season 07")));
    }

    #[test]
    fn each_episode_gets_exactly_one_ffmpeg_encode() {
        // the shell version needed three passes per episode
        let h = harness();
        run_all(&h, settings());
        let encodes = h
            .runner
            .calls_to("ffmpeg")
            .into_iter()
            .filter(|c| c.has("libx264"))
            .count();
        assert_eq!(encodes, 2);
    }

    #[test]
    fn tags_are_written_during_that_same_pass() {
        let h = harness();
        run_all(&h, settings());
        let encode = h
            .runner
            .calls_to("ffmpeg")
            .into_iter()
            .find(|c| c.has("libx264"))
            .unwrap();
        let meta = encode.values_of("-metadata");
        assert!(meta.contains(&"show=Parks and Recreation"), "{:?}", meta);
        assert!(meta.contains(&"season_number=7"));
        assert!(meta.contains(&"media_type=10"));
    }

    #[test]
    fn without_a_glyph_table_the_bitmaps_are_kept_rather_than_lost() {
        let h = harness();
        let (_, _, events) = run_all(&h, settings());
        assert!(events.iter().any(|e| matches!(e, Event::Warning(w) if w.contains("glyph table"))));
        let encode = h
            .runner
            .calls_to("ffmpeg")
            .into_iter()
            .find(|c| c.has("libx264"))
            .unwrap();
        // the English bitmap survives, so the language is not lost
        assert!(encode.values_of("-map").contains(&"0:s:0"), "{}", encode.display());
    }

    #[test]
    fn a_failing_title_does_not_abandon_the_rest() {
        let h = harness();
        // the first encode fails; the second must still be attempted
        let runner = FakeRunner::new()
            .on("-i /rip/title_t01.mkv -an", "frame=  479")
            .on("-i /rip/title_t02.mkv -an", "frame=  479")
            .on("cropdetect", "crop=720:480:0:0")
            .fail("S07E01", "Invalid data found when processing input");
        let cat = TvMaze { http: &h.http };
        let p = Pipeline::new(
            Ports {
                runner: &runner,
                prober: &h.prober,
                ripper: &h.ripper,
                catalogue: &cat,
                fs: &h.fs,
                cancel: Cancel::new(),
            },
            settings(),
        );
        let mut sink = |_: Event| {};
        let scan = p.scan(&fake_disc().drive, &mut sink).unwrap();
        let media = p.identify(&scan, &mut sink).remove(0).media;
        let files = p.rip(&scan, Path::new("/rip"), &mut sink).unwrap();
        let items = p.organise(&files, &media, Some(1), &mut sink).unwrap();
        let report = p.produce(&items, &media, &mut sink).unwrap();
        assert_eq!(report.produced.len(), 1);
        assert_eq!(report.skipped.len(), 1);
        assert!(!report.is_complete());
        assert!(report.skipped[0].1.contains("Invalid data"), "{:?}", report.skipped);
    }

    #[test]
    fn progress_reaches_every_stage_in_order() {
        let h = harness();
        let (_, _, events) = run_all(&h, settings());
        let stages: Vec<Stage> = events
            .iter()
            .filter_map(|e| match e {
                Event::Stage(s) => Some(*s),
                _ => None,
            })
            .collect();
        for want in [Stage::Scan, Stage::Identify, Stage::Rip, Stage::Organise, Stage::Transcode] {
            assert!(stages.contains(&want), "missing {want:?} in {stages:?}");
        }
        let first = |s: Stage| stages.iter().position(|x| *x == s).unwrap();
        assert!(first(Stage::Scan) < first(Stage::Rip));
        assert!(first(Stage::Rip) < first(Stage::Organise));
        assert!(first(Stage::Organise) < first(Stage::Transcode));
    }

    #[test]
    fn every_item_reports_a_start_and_a_finish() {
        let h = harness();
        let (_, _, events) = run_all(&h, settings());
        let started = events.iter().filter(|e| matches!(e, Event::ItemStarted { .. })).count();
        let finished = events.iter().filter(|e| matches!(e, Event::ItemFinished { .. })).count();
        assert_eq!(started, 2);
        assert_eq!(finished, 2);
    }

    #[test]
    fn cancelling_stops_the_run_partway() {
        let h = harness();
        let cat = TvMaze { http: &h.http };
        let cancel = Cancel::new();
        let p = Pipeline::new(
            Ports {
                runner: &h.runner,
                prober: &h.prober,
                ripper: &h.ripper,
                catalogue: &cat,
                fs: &h.fs,
                cancel: cancel.clone(),
            },
            settings(),
        );
        let mut sink = |_: Event| {};
        let scan = p.scan(&fake_disc().drive, &mut sink).unwrap();
        let media = p.identify(&scan, &mut sink).remove(0).media;
        let files = p.rip(&scan, Path::new("/rip"), &mut sink).unwrap();
        cancel.cancel();
        assert!(p.organise(&files, &media, Some(1), &mut sink).is_err());
    }

    #[test]
    fn language_settings_reach_the_encode() {
        let h = harness();
        let mut s = settings();
        s.languages = lang::LanguageSet::parse("english");
        s.audio = Quality::Low;
        run_all(&h, s);
        let encode = h
            .runner
            .calls_to("ffmpeg")
            .into_iter()
            .find(|c| c.has("libx264"))
            .unwrap();
        assert_eq!(encode.value_of("-c:a"), Some("aac"));
        assert_eq!(encode.value_of("-b:a"), Some("96k"));
        assert_eq!(encode.values_of("-map"), vec!["0:v:0", "0:a:0", "0:s:0"]);
    }

    #[test]
    fn the_encode_writes_to_a_temporary_name_and_renames_on_success() {
        // ffmpeg writing straight to the destination means an interrupted run
        // leaves a truncated file at the final path, which the next run counts
        // as a finished episode and skips
        let h = harness();
        let (_, report, _) = run_all(&h, settings());
        let encode = h
            .runner
            .calls_to("ffmpeg")
            .into_iter()
            .find(|c| c.has("libx264"))
            .unwrap();
        let target = encode.args.last().unwrap();
        assert!(target.ends_with(".part"), "{target}");
        // and the finished file is at the real path, not the temporary one
        assert!(report.produced[0].destination.to_string_lossy().ends_with(".mp4"));
    }

    #[test]
    fn a_half_written_file_is_not_mistaken_for_a_finished_episode() {
        let numbers = crate::identify::existing_episode_numbers(&[
            PathBuf::from("Parks and Recreation - S07E01 - 2017.mp4"),
            PathBuf::from("Parks and Recreation - S07E02 - Ron and Jammy.mp4.part"),
        ]);
        assert_eq!(numbers, vec![1]);
    }

    #[test]
    fn a_second_disc_continues_the_numbering_from_what_is_on_disk() {
        let h = harness();
        let fs = FakeFs::new()
            .with_file("/media/Season 07/Parks and Recreation - S07E01 - 2017.mp4", "x")
            .with_file("/media/Season 07/Parks and Recreation - S07E02 - Ron and Jammy.mp4", "x");
        let cat = TvMaze { http: &h.http };
        let p = Pipeline::new(
            Ports {
                runner: &h.runner,
                prober: &h.prober,
                ripper: &h.ripper,
                catalogue: &cat,
                fs: &fs,
                cancel: Cancel::new(),
            },
            settings(),
        );
        let mut sink = |_: Event| {};
        let files = vec![
            PathBuf::from("/rip/title_t00.mkv"),
            PathBuf::from("/rip/title_t01.mkv"),
            PathBuf::from("/rip/title_t02.mkv"),
        ];
        let media = Media::Series {
            title: "Parks and Recreation".into(),
            year: Some(2009),
            season: 7,
            provider_id: Some("1633".into()),
        };
        let items = p.organise(&files, &media, Some(2), &mut sink).unwrap();
        let first = items.iter().find(|i| matches!(i.role, Role::Episode { .. })).unwrap();
        assert_eq!(first.role, Role::Episode { season: 7, number: 3 });
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use crate::host::{Cancel, FakeFs, FakeRunner};
    use crate::identify::catalogue::{FakeHttp, TvMaze};
    use crate::media::FakeProber;
    use crate::rip::FakeRipper;

    const SEARCH: &str = r#"[{"score":0.94,"show":{"id":1633,"name":"Parks and Recreation","premiered":"2009-04-09"}}]"#;
    const EPISODES: &str = r#"[
      {"name":"2017","season":7,"number":1,"airdate":"2015-01-13","runtime":30},
      {"name":"Ron and Jammy","season":7,"number":2,"airdate":"2015-01-13","runtime":30}
    ]"#;

    fn drive() -> Drive {
        Drive {
            id: "disc:0".into(),
            device: "/dev/sr0".into(),
            name: "d".into(),
            disc_label: Some("PARKS_AND_RECREATION_S7D1".into()),
        }
    }

    fn title(id: u32, duration: Millis, chapters: &[Millis]) -> DiscTitle {
        DiscTitle {
            id,
            duration,
            chapter_count: chapters.len(),
            chapters: chapters.to_vec(),
            size_bytes: 0,
            output_name: format!("title_t{id:02}.mkv"),
            tracks: vec![],
        }
    }

    /// Two episodes and the play-all that orders them, as the free reader
    /// reports it: with chapter durations.
    fn scan_with_chapters() -> DiscScan {
        DiscScan {
            drive: drive(),
            label: "PARKS_AND_RECREATION_S7D1".into(),
            titles: vec![
                title(0, 2_550_000, &[600_000, 675_000, 600_000, 675_000]),
                title(1, 1_275_000, &[600_000, 675_000]),
                title(2, 1_275_000, &[600_000, 675_000]),
            ],
        }
    }

    fn season7() -> Media {
        Media::Series {
            title: "Parks and Recreation".into(),
            year: Some(2009),
            season: 7,
            provider_id: Some("1633".into()),
        }
    }

    #[test]
    fn a_preview_maps_episodes_without_reading_the_disc() {
        // A "dry run" that rips the whole disc and then stops is not a dry run,
        // it is eight gigabytes read for nothing.
        let runner = FakeRunner::new();
        let prober = FakeProber::new();
        let ripper = FakeRipper::new(scan_with_chapters());
        let http = FakeHttp::new().on("/search/shows", SEARCH).on("/episodes", EPISODES);
        let cat = TvMaze { http: &http };
        let fs = FakeFs::new();
        let p = Pipeline::new(
            Ports {
                runner: &runner,
                prober: &prober,
                ripper: &ripper,
                catalogue: &cat,
                fs: &fs,
                cancel: Cancel::new(),
            },
            JobSettings { output_dir: PathBuf::from("/media"), ..JobSettings::default() },
        );

        let items = p
            .preview(&scan_with_chapters(), &season7(), Some(1), Path::new("/rip"))
            .expect("chapter durations were reported, so a preview is possible");

        let episodes: Vec<&Item> = items
            .iter()
            .filter(|i| matches!(i.role, Role::Episode { .. }))
            .collect();
        assert_eq!(episodes.len(), 2);
        assert_eq!(episodes[0].role, Role::Episode { season: 7, number: 1 });
        assert_eq!(episodes[0].title, "2017");
        assert!(items.iter().any(|i| i.role == Role::PlayAll));

        // and nothing was read: no ffmpeg, no ripping
        assert!(runner.calls().is_empty(), "{:?}", runner.calls());
        assert!(ripper.written.lock().unwrap().is_empty());
    }

    #[test]
    fn a_scanner_without_chapter_durations_cannot_preview() {
        // MakeMKV reports a chapter count but not the durations, and the
        // durations are what decompose a play-all
        let runner = FakeRunner::new();
        let prober = FakeProber::new();
        let mut scan = scan_with_chapters();
        for t in &mut scan.titles {
            t.chapters.clear();
        }
        let ripper = FakeRipper::new(scan.clone());
        let http = FakeHttp::new().on("/search/shows", SEARCH).on("/episodes", EPISODES);
        let cat = TvMaze { http: &http };
        let fs = FakeFs::new();
        let p = Pipeline::new(
            Ports {
                runner: &runner,
                prober: &prober,
                ripper: &ripper,
                catalogue: &cat,
                fs: &fs,
                cancel: Cancel::new(),
            },
            JobSettings::default(),
        );
        assert!(p.preview(&scan, &season7(), Some(1), Path::new("/rip")).is_none());
    }
}
