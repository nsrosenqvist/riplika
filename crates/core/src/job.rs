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
use crate::transcode::{self, SubtitleInput, analyze};
use crate::{Error, Result, lang, naming};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

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
    /// What the disc turned out to hold, once the titles have been sorted.
    ///
    /// Carried as counts rather than as a sentence so that each consumer can
    /// phrase it for itself - the window in the reader's language, the log in
    /// English so it stays the same in a bug report.
    Plan(crate::model::Plan),
    /// Something went wrong that did not stop the run.
    Warning(Warning),
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
    pub warnings: Vec<Warning>,
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
    /// Whether the missing glyph table has been mentioned yet.
    said_no_table: std::sync::atomic::AtomicBool,
}

impl<'a> Pipeline<'a> {
    pub fn new(ports: Ports<'a>, settings: JobSettings) -> Self {
        Pipeline { ports, settings, said_no_table: std::sync::atomic::AtomicBool::new(false) }
    }

    pub fn drives(&self) -> Result<Vec<Drive>> {
        self.ports.ripper.drives()
    }

    pub fn scan(&self, drive: &Drive, events: Events) -> Result<DiscScan> {
        events(Event::Stage(Stage::Scan));
        self.ports.ripper.scan(drive, &mut |fraction, message| {
            events(Event::Progress {
                stage: Stage::Scan,
                fraction,
                message: message.map(str::to_string),
            });
        })
    }

    /// Guess what the disc is. Never fatal: an unidentified disc can still be
    /// ripped, and the user can say what it is.
    pub fn identify(&self, scan: &DiscScan, events: Events) -> Vec<Candidate> {
        events(Event::Stage(Stage::Identify));
        match identify::identify(scan, self.ports.catalogue) {
            Ok(c) => c,
            Err(e) => {
                events(Event::Warning(Warning::CouldNotIdentify { why: e.to_string() }));
                Vec::new()
            }
        }
    }

    /// Look something up by hand, for when the guess was wrong.
    pub fn search(&self, query: &str, season: Option<u32>) -> Result<Vec<Candidate>> {
        identify::search(self.ports.catalogue, query, season)
    }

    /// Which titles are worth reading.
    ///
    /// A play-all replays episodes that are also on the disc individually, so
    /// reading it means reading the same video a second time. On a Parks and
    /// Recreation disc the two play-alls are two and a half hours of video that
    /// is already being read as seven episodes.
    ///
    /// This is only possible when the scan reported chapter durations, since
    /// those are what identify a play-all. Without them every title is read,
    /// because guessing which to skip risks skipping an episode.
    pub fn titles_to_rip(&self, scan: &DiscScan, plan: Option<&[Item]>) -> Vec<DiscTitle> {
        let Some(plan) = plan else {
            return scan.titles.clone();
        };
        // Not read: play-alls always, and anything that would not be produced.
        // An extended cut cannot be told from an ordinary extra before the file
        // exists, so an episode-length title is read if *either* is wanted.
        let range = structure::EpisodeRange::default();
        let redundant: Vec<String> = plan
            .iter()
            .filter(|i| {
                let could_be_a_longer_cut =
                    matches!(i.role, Role::Extra) && range.contains(i.duration);
                let wanted = i.role.wanted(&self.settings)
                    || (could_be_a_longer_cut && self.settings.include_extended_cuts);
                !wanted
            })
            .filter_map(|i| i.source.file_name().map(|f| f.to_string_lossy().into_owned()))
            .collect();
        scan.titles.iter().filter(|t| !redundant.contains(&t.output_name)).cloned().collect()
    }

    /// Stage one: read the disc.
    pub fn rip(
        &self,
        scan: &DiscScan,
        titles: &[DiscTitle],
        dest: &Path,
        events: Events,
    ) -> Result<Vec<PathBuf>> {
        events(Event::Stage(Stage::Rip));
        self.ports.fs.create_dir_all(dest)?;
        let outcome = {
            let mut report = |fraction: f32, message: Option<&str>| {
                events(Event::Progress {
                    stage: Stage::Rip,
                    fraction,
                    message: message.map(str::to_string),
                });
            };
            self.ports.ripper.rip(&scan.drive, titles, dest, &mut report)?
        };

        // Titles that could not be read are reported, not thrown: a disc is
        // mostly menus and transitions, and losing one of those should not cost
        // the episodes.
        for (id, why) in &outcome.failed {
            events(Event::Warning(Warning::TitleUnreadable { title: *id, why: why.clone() }));
        }
        if outcome.written.is_empty() {
            return Err(Error(format!(
                "nothing could be read from the disc ({} titles failed)",
                outcome.failed.len()
            )));
        }
        Ok(outcome.written)
    }

    /// Stages two and three: sort the ripped files out and name them.
    ///
    /// The result is data, not action. The caller is expected to show it and
    /// let it be edited before anything is written.
    pub fn organise(
        &self,
        files: &[PathBuf],
        scan: Option<&DiscScan>,
        media: &Media,
        disc: Option<u32>,
        events: Events,
    ) -> Result<Vec<Item>> {
        events(Event::Stage(Stage::Organise));
        self.ports.cancel.check()?;

        let shapes = identify::shapes(self.ports.prober, files)?;
        let plain: Vec<structure::TitleShape> = shapes.iter().map(|(s, _)| s.clone()).collect();

        // Decompose from the disc where there is one, rather than from what was
        // ripped. A play-all is not ripped - it is the same video again - so
        // deciding the episode order from the files means deciding it without
        // the one title that states the order. That reads as "no play-all on
        // this disc" on a disc that has one, and falls back to ordering by
        // duration, which the fallback itself calls a guess where this is
        // evidence.
        let from_disc: Option<Vec<structure::TitleShape>> =
            scan.filter(|s| s.titles.iter().any(|t| !t.chapters.is_empty())).map(|s| {
                s.titles
                    .iter()
                    .enumerate()
                    .map(|(i, t)| structure::TitleShape {
                        key: t.output_name.clone(),
                        order: i as u32,
                        duration: t.duration,
                        chapters: t.chapters.clone(),
                    })
                    .collect()
            });
        let mut st = structure::decompose(
            from_disc.as_deref().unwrap_or(&plain),
            structure::EpisodeRange::default(),
        );

        // Whatever the disc said, only what was ripped can be worked on.
        let ripped: Vec<String> = plain.iter().map(|s| s.key.clone()).collect();
        if from_disc.is_some() {
            st.episodes.retain(|k| ripped.contains(k));
            st.loose.retain(|k| ripped.contains(k));
            st.extras.retain(|k| ripped.contains(k));
        }

        if st.episodes.is_empty() {
            // No play-all: fall back to the house-length cluster, and say so,
            // because that ordering is a guess where the other one is evidence.
            let by_duration =
                structure::episodes_by_duration(&plain, structure::EpisodeRange::default());
            if !by_duration.is_empty() {
                events(Event::Warning(Warning::NoPlayAll { episodes: by_duration.len() }));
                st.loose.retain(|k| !by_duration.contains(k));
                st.episodes = by_duration;
            }
        }

        let dir = files.first().and_then(|f| f.parent()).map(Path::to_path_buf).unwrap_or_default();

        let extended = if st.loose.is_empty() || !self.settings.include_extended_cuts {
            // Comparing pictures means decoding every loose title against every
            // episode. Not worth doing for cuts that will not be produced.
            Vec::new()
        } else {
            match structure::find_extended_cuts(self.ports.runner, &dir, &st.loose, &st.episodes) {
                Ok(e) => e,
                Err(e) => {
                    events(Event::Warning(Warning::ExtendedCutsUncomparable {
                        why: e.to_string(),
                    }));
                    Vec::new()
                }
            }
        };

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
                    self.settings.episode_template.as_deref(),
                ));
            }
        }
        events(Event::Plan(crate::model::Plan::of(&items)));
        Ok(items)
    }

    fn season_dir(&self, media: &Media) -> PathBuf {
        match media {
            Media::Series { season, .. } => {
                self.settings.output_dir.join(naming::sanitize(&format!("Season {season:02}")))
            }
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
            if let Some(t) = scan.titles.iter().find(|t| item.source.ends_with(&t.output_name)) {
                item.duration = t.duration;
            }
            if item.role.is_output() {
                item.destination = Some(naming::destination(
                    &self.settings.output_dir,
                    media,
                    item,
                    self.settings.container,
                    self.settings.episode_template.as_deref(),
                ));
            }
        }
        Some(items)
    }

    /// Stage four and the encode: produce the output files.
    pub fn produce(&self, items: &[Item], media: &Media, events: Events) -> Result<Report> {
        let mut report = Report::default();
        let outputs: Vec<&Item> = items.iter().filter(|i| i.role.wanted(&self.settings)).collect();

        let table = match &self.settings.glyph_table {
            Some(p) if self.ports.fs.exists(p) => match Table::load(p) {
                Ok(t) => Some(t),
                Err(e) => {
                    let w = Warning::GlyphTableUnreadable { path: p.clone(), why: e.to_string() };
                    events(Event::Warning(w.clone()));
                    report.warnings.push(w);
                    None
                }
            },
            Some(p) => {
                let w = Warning::GlyphTableMissing { path: p.clone() };
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
                    events(Event::Warning(Warning::ItemSkipped {
                        name: item
                            .source
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned(),
                        why: e.to_string(),
                    }));
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
        if self.settings.container == Container::Mp4 {
            // ffmpeg leaves a reference to a chapter track it never wrote. The
            // file plays either way, so a failure here is not worth failing a
            // finished episode over - it is tidying, not producing.
            let _ = transcode::mp4::drop_dangling_chapter_refs(self.ports.fs, &partial);
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
            // Said once, for the disc, rather than once for every title on it.
            // A disc of forty titles said it forty times, which buries the
            // warnings that are about a particular file.
            if !wanted.is_empty() && !self.said_no_table.swap(true, Ordering::Relaxed) {
                events(Event::Warning(Warning::NoGlyphTable));
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
            let code =
                subs_tracks.get(stream).map(|t| t.language.clone()).unwrap_or_else(|| "und".into());
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
                        events(Event::Warning(Warning::UnrecognisedGlyphs {
                            language: language.name.to_string(),
                            glyphs: r.unknown_glyphs,
                        }));
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
                candidates.into_iter().next().map(|c| c.media).ok_or_else(|| {
                    Error(format!(
                        "could not identify {:?}; say what it is with --title",
                        scan.label
                    ))
                })?
            }
        };
        let disc = identify::label::parse(&scan.label).disc;
        // Work out what is worth reading before reading it.
        let plan = self.preview(&scan, &media, disc, rip_dir);
        let titles = self.titles_to_rip(&scan, plan.as_deref());
        if titles.len() < scan.titles.len() {
            events(Event::Warning(Warning::PlayAllsSkipped {
                titles: scan.titles.len() - titles.len(),
            }));
        }
        let files = self.rip(&scan, &titles, rip_dir, events)?;
        let items = self.organise(&files, Some(&scan), &media, disc, events)?;
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
                DiscTitle {
                    id: 0,
                    duration: 2_550_000,
                    chapter_count: 4,
                    chapters: vec![],
                    size_bytes: 0,
                    output_name: "title_t00.mkv".into(),
                    tracks: vec![],
                },
                DiscTitle {
                    id: 1,
                    duration: 1_275_000,
                    chapter_count: 2,
                    chapters: vec![],
                    size_bytes: 0,
                    output_name: "title_t01.mkv".into(),
                    tracks: vec![],
                },
                DiscTitle {
                    id: 2,
                    duration: 1_275_000,
                    chapter_count: 2,
                    chapters: vec![],
                    size_bytes: 0,
                    output_name: "title_t02.mkv".into(),
                    tracks: vec![],
                },
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
        JobSettings { output_dir: PathBuf::from("/media"), ..JobSettings::default() }
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
        let files = p.rip(&scan, &scan.titles, Path::new("/rip"), &mut sink).unwrap();
        let items = p.organise(&files, None, &media, Some(1), &mut sink).unwrap();
        let report = p.produce(&items, &media, &mut sink).unwrap();
        (items, report, events)
    }

    #[test]
    fn a_missing_glyph_table_is_mentioned_once_for_the_disc() {
        // It was said once per title. A disc of forty titles said it forty
        // times, burying the warnings that are about a particular file.
        let h = harness();
        let mut s = settings();
        s.glyph_table = None;
        let (_, _, events) = run_all(&h, s);
        let said =
            events.iter().filter(|e| matches!(e, Event::Warning(Warning::NoGlyphTable))).count();
        assert!(said <= 1, "said it {said} times");
    }

    #[test]
    fn the_episode_order_comes_from_the_disc_not_from_what_was_ripped() {
        // A play-all is deliberately not ripped - it is the same video again -
        // so working the order out from the ripped files means working it out
        // without the one title that states the order. That reported "no
        // play-all title on this disc" for a disc that has one, and fell back
        // to ordering by duration, which is a guess where this is evidence.
        let h = harness();
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
            settings(),
        );
        let mut warnings = Vec::new();
        let mut sink = |e: Event| {
            if let Event::Warning(w) = e {
                warnings.push(w);
            }
        };
        let scan = disc_with_a_play_all();
        let media = p.identify(&scan, &mut sink).remove(0).media;

        // the two episodes, as they are after the play-all was skipped
        let files = vec![PathBuf::from("/rip/title_t01.mkv"), PathBuf::from("/rip/title_t02.mkv")];
        let items = p.organise(&files, Some(&scan), &media, Some(1), &mut sink).unwrap();

        assert!(
            !warnings.iter().any(|w| matches!(w, Warning::NoPlayAll { .. })),
            "said there is no play-all on a disc that has one: {warnings:?}"
        );
        let episodes: Vec<&Item> =
            items.iter().filter(|i| matches!(i.role, Role::Episode { .. })).collect();
        assert_eq!(episodes.len(), 2, "{items:#?}");
        // and the play-all itself is not written out, having never been read
        assert!(!items.iter().any(|i| i.role == Role::Episode { season: 7, number: 3 }));
    }

    /// The fake disc, with chapter times so the play-all can be recognised.
    fn disc_with_a_play_all() -> DiscScan {
        let ep = vec![600_000, 675_000, 1_000];
        let mut scan = fake_disc();
        scan.titles[0].chapters = [&ep[..2], &ep[..2], &[1_000][..]].concat();
        scan.titles[1].chapters = ep.clone();
        scan.titles[2].chapters = ep;
        scan
    }

    #[test]
    fn a_whole_disc_goes_through_without_touching_hardware() {
        let h = harness();
        let (items, report, _) = run_all(&h, settings());
        let episodes: Vec<&Item> =
            items.iter().filter(|i| matches!(i.role, Role::Episode { .. })).collect();
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
        assert!(!report.produced.iter().any(|p| p.destination.to_string_lossy().contains("t00")));
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
        assert!(h.fs.created_dirs().iter().any(|d| d == Path::new("/media/Season 07")));
    }

    #[test]
    fn each_episode_gets_exactly_one_ffmpeg_encode() {
        // the shell version needed three passes per episode
        let h = harness();
        run_all(&h, settings());
        let encodes = h.runner.calls_to("ffmpeg").into_iter().filter(|c| c.has("libx264")).count();
        assert_eq!(encodes, 2);
    }

    #[test]
    fn tags_are_written_during_that_same_pass() {
        let h = harness();
        run_all(&h, settings());
        let encode = h.runner.calls_to("ffmpeg").into_iter().find(|c| c.has("libx264")).unwrap();
        let meta = encode.values_of("-metadata");
        assert!(meta.contains(&"show=Parks and Recreation"), "{:?}", meta);
        assert!(meta.contains(&"season_number=7"));
        assert!(meta.contains(&"media_type=10"));
    }

    #[test]
    fn without_a_glyph_table_the_bitmaps_are_kept_rather_than_lost() {
        let h = harness();
        let (_, _, events) = run_all(&h, settings());
        // Asking for the kind rather than for a substring: this used to match
        // any warning that happened to mention a glyph table.
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Warning(Warning::NoGlyphTable | Warning::GlyphTableMissing { .. })
        )));
        let encode = h.runner.calls_to("ffmpeg").into_iter().find(|c| c.has("libx264")).unwrap();
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
        let files = p.rip(&scan, &scan.titles, Path::new("/rip"), &mut sink).unwrap();
        let items = p.organise(&files, None, &media, Some(1), &mut sink).unwrap();
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
        let files = p.rip(&scan, &scan.titles, Path::new("/rip"), &mut sink).unwrap();
        cancel.cancel();
        assert!(p.organise(&files, None, &media, Some(1), &mut sink).is_err());
    }

    #[test]
    fn language_settings_reach_the_encode() {
        let h = harness();
        let mut s = settings();
        s.languages = lang::LanguageSet::parse("english");
        s.audio = Quality::Low;
        run_all(&h, s);
        let encode = h.runner.calls_to("ffmpeg").into_iter().find(|c| c.has("libx264")).unwrap();
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
        let encode = h.runner.calls_to("ffmpeg").into_iter().find(|c| c.has("libx264")).unwrap();
        let target = encode.args.last().unwrap();
        assert!(target.ends_with(".part"), "{target}");
        // ...which is why the format has to be stated. ffmpeg chooses its muxer
        // from the extension, and ".part" is not one, so without this it
        // refuses the output and the run produces nothing at all. These two
        // assertions belong together: the first is the reason for the second.
        assert_eq!(encode.value_of("-f"), Some("mp4"), "{}", encode.display());
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
        let items = p.organise(&files, None, &media, Some(2), &mut sink).unwrap();
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

        let episodes: Vec<&Item> =
            items.iter().filter(|i| matches!(i.role, Role::Episode { .. })).collect();
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

#[cfg(test)]
mod selection_tests {
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

    /// Two episodes and the play-all that is both of them again.
    fn scan() -> DiscScan {
        DiscScan {
            drive: Drive {
                id: "disc:0".into(),
                device: "/dev/sr0".into(),
                name: "d".into(),
                disc_label: Some("PARKS_AND_RECREATION_S7D1".into()),
            },
            label: "PARKS_AND_RECREATION_S7D1".into(),
            titles: vec![
                title(0, 2_550_000, &[600_000, 675_000, 600_000, 675_000]),
                title(1, 1_275_000, &[600_000, 675_000]),
                title(2, 1_275_000, &[600_000, 675_000]),
            ],
        }
    }

    fn media() -> Media {
        Media::Series {
            title: "Parks and Recreation".into(),
            year: Some(2009),
            season: 7,
            provider_id: Some("1633".into()),
        }
    }

    #[test]
    fn a_play_all_is_not_read_because_its_content_is_read_anyway() {
        // On the disc this was built against, the two play-alls are two and a
        // half hours of video that is already being read as seven episodes.
        let runner = FakeRunner::new();
        let prober = FakeProber::new();
        let ripper = FakeRipper::new(scan());
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
        let s = scan();
        let plan = p.preview(&s, &media(), Some(1), Path::new("/rip")).unwrap();
        let titles = p.titles_to_rip(&s, Some(&plan));

        assert_eq!(titles.len(), 2, "only the episodes");
        assert!(
            !titles.iter().any(|t| t.output_name == "title_t00.mkv"),
            "the play-all was read anyway"
        );
    }

    #[test]
    fn without_chapter_durations_everything_is_read() {
        // MakeMKV reports no chapter durations, so nothing can be identified as
        // redundant - and guessing risks skipping an episode.
        let runner = FakeRunner::new();
        let prober = FakeProber::new();
        let ripper = FakeRipper::new(scan());
        let http = FakeHttp::new();
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
        let s = scan();
        assert_eq!(p.titles_to_rip(&s, None).len(), s.titles.len());
    }
}

/// Turns a stream of progress fractions into a time remaining.
///
/// From elapsed time and how far along we are, which needs no knowledge of what
/// is being done - and is why the rip reports its progress weighted by running
/// time rather than by title count. Weighted by count, the fraction jumps and
/// stalls and any estimate from it is nonsense.
#[derive(Debug, Clone)]
pub struct Eta {
    started: std::time::Instant,
    /// Smoothed seconds-per-unit-of-progress.
    rate: Option<f64>,
    last: Option<(std::time::Instant, f32)>,
}

impl Default for Eta {
    fn default() -> Self {
        Self::new()
    }
}

impl Eta {
    pub fn new() -> Eta {
        Eta { started: std::time::Instant::now(), rate: None, last: None }
    }

    /// How long the whole job has been going.
    pub fn elapsed(&self) -> std::time::Duration {
        self.started.elapsed()
    }

    /// Note where we have got to, and say how much longer it looks like.
    ///
    /// Returns `None` until there is enough to say anything honest: a guess
    /// made from the first two per cent of an hour-long rip is worse than no
    /// guess, because it will be wrong by an order of magnitude and it will be
    /// believed.
    pub fn update(&mut self, fraction: f32) -> Option<std::time::Duration> {
        let now = std::time::Instant::now();
        let fraction = fraction.clamp(0.0, 1.0);

        if let Some((then, before)) = self.last {
            let moved = (fraction - before) as f64;
            let seconds = now.duration_since(then).as_secs_f64();
            if moved > 0.0 && seconds > 0.0 {
                let instant = seconds / moved;
                // Smoothed, because an optical drive's rate is not steady: it
                // slows over a layer change and stalls on a retry, and an
                // estimate that lurched with it would be unreadable.
                self.rate = Some(match self.rate {
                    Some(r) => r * 0.8 + instant * 0.2,
                    None => instant,
                });
            }
        }
        self.last = Some((now, fraction));

        let rate = self.rate?;
        if !(0.02..1.0).contains(&fraction) {
            return None;
        }
        let remaining = rate * (1.0 - fraction as f64);
        (remaining.is_finite() && remaining >= 0.0)
            .then(|| std::time::Duration::from_secs_f64(remaining.min(24.0 * 3600.0)))
    }
}

/// How much longer, rounded to what an estimate this soft can support.
///
/// The rounding is the decision and stays here; the words are the window's, so
/// that they can be translated. Below ten minutes to the nearest minute, then
/// to five - claiming "37 minutes" on an estimate built from read speed is a
/// precision the number does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remaining {
    LessThanAMinute,
    AboutAMinute,
    Minutes(u64),
    Hours(u64),
    HoursAndMinutes(u64, u64),
}

pub fn remaining(d: std::time::Duration) -> Remaining {
    let s = d.as_secs();
    match s {
        0..=45 => Remaining::LessThanAMinute,
        46..=90 => Remaining::AboutAMinute,
        91..=600 => Remaining::Minutes((s + 30) / 60),
        _ => {
            let minutes = (s + 150) / 300 * 5;
            if minutes >= 120 {
                // "about 3 hours" for two and a half is a fifth too long, and
                // long jobs are exactly where a wrong number is noticed.
                match (minutes / 60, minutes % 60) {
                    (h, 0) => Remaining::Hours(h),
                    (h, m) => Remaining::HoursAndMinutes(h, m),
                }
            } else {
                Remaining::Minutes(minutes)
            }
        }
    }
}

/// The estimate in English, for the command line.
pub fn describe_remaining(d: std::time::Duration) -> String {
    match remaining(d) {
        Remaining::LessThanAMinute => "less than a minute left".into(),
        Remaining::AboutAMinute => "about a minute left".into(),
        Remaining::Minutes(m) => format!("about {m} minutes left"),
        Remaining::Hours(h) => format!("about {h} hours left"),
        Remaining::HoursAndMinutes(h, m) => format!("about {h} hours {m} minutes left"),
    }
}

#[cfg(test)]
mod eta_tests {
    use super::*;

    #[test]
    fn nothing_is_claimed_before_there_is_anything_to_claim() {
        // a guess from the first two per cent of an hour-long rip is worse than
        // no guess: wrong by an order of magnitude, and believed
        let mut eta = Eta::new();
        assert_eq!(eta.update(0.0), None);
        assert_eq!(eta.update(0.01), None);
    }

    #[test]
    fn a_steady_rate_gives_a_sensible_estimate() {
        let mut eta = Eta::new();
        eta.update(0.0);
        std::thread::sleep(std::time::Duration::from_millis(60));
        // half done in ~60ms, so ~60ms to go
        let left = eta.update(0.5).expect("half way is enough to estimate from");
        assert!(left.as_millis() < 1000, "{left:?}");
    }

    #[test]
    fn a_drive_that_slows_is_followed_rather_than_jumped_after() {
        // an optical drive is not steady - it slows over a layer change and
        // stalls on a retry - so the estimate is smoothed
        let mut eta = Eta::new();
        eta.update(0.0);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let fast = eta.update(0.5).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(60));
        let after_a_stall = eta.update(0.51).unwrap();
        // it rose, but not to the full six seconds per point the stall implies
        assert!(after_a_stall > fast);
        assert!(after_a_stall.as_secs_f64() < 3.0, "{after_a_stall:?}");
    }

    #[test]
    fn a_finished_job_claims_nothing() {
        let mut eta = Eta::new();
        eta.update(0.2);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert_eq!(eta.update(1.0), None);
    }

    #[test]
    fn phrases_are_no_more_precise_than_the_estimate_deserves() {
        use std::time::Duration as D;
        // the rounding is the part that stayed here, so it is the part tested
        assert_eq!(remaining(D::from_secs(20)), Remaining::LessThanAMinute);
        assert_eq!(remaining(D::from_secs(240)), Remaining::Minutes(4));
        assert_eq!(remaining(D::from_secs(9000)), Remaining::HoursAndMinutes(2, 30));
        assert_eq!(remaining(D::from_secs(10800)), Remaining::Hours(3));
        assert_eq!(describe_remaining(D::from_secs(20)), "less than a minute left");
        assert_eq!(describe_remaining(D::from_secs(70)), "about a minute left");
        assert_eq!(describe_remaining(D::from_secs(240)), "about 4 minutes left");
        // beyond ten minutes, to the nearest five
        assert_eq!(describe_remaining(D::from_secs(2220)), "about 35 minutes left");
        assert_eq!(describe_remaining(D::from_secs(9000)), "about 2 hours 30 minutes left");
        assert_eq!(describe_remaining(D::from_secs(10800)), "about 3 hours left");
    }

    #[test]
    fn a_stalled_drive_does_not_produce_an_absurd_number() {
        let mut eta = Eta::new();
        eta.update(0.1);
        std::thread::sleep(std::time::Duration::from_millis(30));
        let left = eta.update(0.1000001);
        // capped at a day rather than reported as centuries
        if let Some(d) = left {
            assert!(d.as_secs() <= 24 * 3600);
        }
    }
}
