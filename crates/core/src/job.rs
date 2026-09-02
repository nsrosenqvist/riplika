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
    /// Checking a finished image against what it should be.
    Verify,
    Subtitles,
    /// Working out what this disc's lettering says, for a face nobody has a
    /// table for yet. Once per release, not once per disc.
    Lettering,
    Transcode,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Scan => "Scanning disc",
            Stage::Identify => "Identifying",
            Stage::Rip => "Ripping",
            Stage::Organise => "Sorting titles",
            Stage::Verify => "Verifying",
            Stage::Subtitles => "Reading subtitles",
            Stage::Lettering => "Learning this disc's lettering",
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
    /// Which glyph table this disc is being decoded with, and how well it fits.
    TableChosen {
        path: PathBuf,
        /// Share of the disc's glyph instances it can put a character to.
        covered: f32,
        /// Whether it was built for this disc just now.
        built: bool,
    },
    /// What reading the disc's own lettering settled.
    LetteringLearned {
        labelled: usize,
        ambiguous: usize,
        blank: usize,
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
    /// Whether the table having been built for another release has been.
    said_wrong_font: std::sync::atomic::AtomicBool,
}

impl<'a> Pipeline<'a> {
    pub fn new(ports: Ports<'a>, settings: JobSettings) -> Self {
        Pipeline {
            ports,
            settings,
            said_no_table: std::sync::atomic::AtomicBool::new(false),
            said_wrong_font: std::sync::atomic::AtomicBool::new(false),
        }
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
        // A film has no episodes, so nothing on its disc can be a longer cut of
        // one. Extended cuts are found by comparing an unclaimed episode-length
        // title against the episodes, and there are none to compare against, so
        // reading The Lion King's twenty-minute making-of on the chance is
        // twenty minutes of the disc spent on a comparison that never runs.
        let a_film = plan.iter().any(|i| matches!(i.role, Role::Feature));
        let redundant: Vec<String> = plan
            .iter()
            .filter(|i| {
                let could_be_a_longer_cut =
                    !a_film && matches!(i.role, Role::Extra) && range.contains(i.duration);
                let wanted = i.role.wanted(&self.settings)
                    || (could_be_a_longer_cut && self.settings.include_extended_cuts);
                !wanted
            })
            .filter_map(|i| i.source.file_name().map(|f| f.to_string_lossy().into_owned()))
            .collect();
        scan.titles.iter().filter(|t| !redundant.contains(&t.output_name)).cloned().collect()
    }

    /// The play-alls this run will not read, if there are any.
    ///
    /// Counted from the plan, not from how many titles were dropped. What
    /// `titles_to_rip` leaves out is play-alls *and* everything the settings do
    /// not want, so subtracting said "skipping 31 play-all titles, whose
    /// content is on the disc already" about two play-alls and twenty-nine
    /// extras somebody had unticked - which is two untruths in one sentence,
    /// and contradicted the "2 play-all titles" the same log printed later.
    pub fn play_alls_skipped(&self, plan: Option<&[Item]>) -> Option<Warning> {
        let n = plan?.iter().filter(|i| i.role == Role::PlayAll).count();
        (n > 0).then_some(Warning::PlayAllsSkipped { titles: n })
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
        // Whatever an earlier run left is dead weight - nothing reads it, and
        // this run is about to write over the same names anyway.
        self.empty_scratch(dest, events);
        // Written down before a byte is read, so a run that is cancelled, that
        // fails, or whose process is killed outright is still cleared up by
        // the next one. Recording it afterwards would have covered none of
        // those, which are exactly the runs that leave a disc behind.
        self.record_scratch(dest, titles);
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

    /// Take back the cache directory once a run is over.
    ///
    /// Called by whoever drove the stages, since only they know the run ended.
    ///
    /// A run that produced nothing keeps its files. They are a disc's worth of
    /// reading, `riplika process` can still turn them into episodes, and that
    /// beats reading the disc again - so the one case where the intermediate
    /// files are worth something is the one case they survive. They do not
    /// survive the next rip.
    pub fn discard_rip(&self, dir: &Path, report: &Report, events: Events) {
        if report.produced.is_empty() {
            return;
        }
        self.empty_scratch(dir, events);
    }

    /// Write down what this rip is about to put in `dir`.
    ///
    /// The names cannot be worked out later: MakeMKV chooses its own, and the
    /// folder is a preference that can point anywhere, so sweeping it by
    /// pattern would either miss files or delete somebody else's. Every title
    /// asked for is recorded rather than every title that succeeded, because a
    /// title that failed is the one most likely to have left a part-file.
    fn record_scratch(&self, dir: &Path, titles: &[DiscTitle]) {
        let lines: Vec<String> =
            titles.iter().map(|t| dir.join(&t.output_name).display().to_string()).collect();
        // Not being able to write it costs cleanup, not the rip.
        let _ = self.ports.fs.write(&scratch_note(dir), lines.join("\n").as_bytes());
    }

    /// Delete what a recorded rip put in `dir`, and nothing else.
    fn empty_scratch(&self, dir: &Path, events: Events) {
        let note = scratch_note(dir);
        let Ok(bytes) = self.ports.fs.read(&note) else {
            return;
        };
        let recorded: Vec<String> = String::from_utf8_lossy(&bytes)
            .lines()
            .filter_map(|l| Path::new(l.trim()).file_stem()?.to_str().map(str::to_string))
            .filter(|stem| !stem.is_empty())
            .collect();
        // By stem, not by name: the subtitles recognised from title_t41.mkv are
        // title_t41.eng.srt beside it, and a part-file is title_t41.mkv.part.
        // Both belong to a title this run read and neither is in the record.
        let ours = |name: &str| {
            recorded
                .iter()
                .any(|stem| name.starts_with(stem.as_str()) && name[stem.len()..].starts_with('.'))
        };
        for path in self.ports.fs.list(dir).unwrap_or_default() {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !ours(name) {
                continue;
            }
            if let Err(e) = self.ports.fs.remove_file(&path) {
                events(Event::Warning(Warning::CacheNotCleared {
                    path: path.clone(),
                    why: e.to_string(),
                }));
            }
        }
        let _ = self.ports.fs.remove_file(&note);
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
        // A film is worked out from what was ripped, not from the disc. There
        // is nothing to decompose - the feature is the longest title - and the
        // disc's copy would only have to be narrowed to the ripped files again.
        let st = if matches!(media, Media::Movie { .. }) {
            structure::feature(&plain)
        } else {
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
                // No play-all: fall back to the house-length cluster, and say
                // so, because that ordering is a guess where the other one is
                // evidence.
                let by_duration =
                    structure::episodes_by_duration(&plain, structure::EpisodeRange::default());
                if !by_duration.is_empty() {
                    events(Event::Warning(Warning::NoPlayAll { episodes: by_duration.len() }));
                    st.loose.retain(|k| !by_duration.contains(k));
                    st.episodes = by_duration;
                }
            }
            st
        };

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
        let st = if matches!(media, Media::Movie { .. }) {
            structure::feature(&shapes)
        } else {
            let mut st = structure::decompose(&shapes, structure::EpisodeRange::default());
            if st.episodes.is_empty() {
                let by_duration =
                    structure::episodes_by_duration(&shapes, structure::EpisodeRange::default());
                st.loose.retain(|k| !by_duration.contains(k));
                st.episodes = by_duration;
            }
            st
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

        let table = self.lettering(&outputs, media, &mut report, events);

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

            match self.produce_one(
                item,
                media,
                &dest,
                &table,
                Position { index: n, total: outputs.len() },
                events,
            ) {
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

    /// The subtitle streams this run is going to want, and where they live.
    fn wanted_subtitles(&self, outputs: &[&Item]) -> Option<Wanted> {
        let item = outputs.first()?;
        let info = self.ports.prober.probe(&item.source).ok()?;
        let streams = transcode::subtitles_to_recognise(&info, &self.settings.languages);
        if streams.is_empty() {
            return None;
        }
        let tracks = info.tracks_of(TrackKind::Subtitle);
        let languages = streams
            .iter()
            .map(|s| tracks.get(*s).map(|t| t.language.clone()).unwrap_or_else(|| "und".into()))
            .collect();
        Some(Wanted { source: item.source.clone(), streams, languages })
    }

    /// Read a sample of the disc and label a table from what it says.
    fn learn_lettering(
        &self,
        wanted: &Wanted,
        media: &Media,
        shapes: &std::collections::BTreeMap<String, u64>,
        start: Table,
        report: &mut Report,
        events: Events,
    ) -> Option<Table> {
        let Some(dir) = self.settings.tables_dir.clone() else {
            note(Warning::NoGlyphTable, report, events);
            return None;
        };
        let installed = subs::ocr::languages(self.ports.runner);
        if installed.is_empty() {
            note(Warning::CannotLearnLettering { shapes: shapes.len() }, report, events);
            return None;
        }
        if self.ports.fs.create_dir_all(&dir).is_err() {
            note(Warning::NoGlyphTable, report, events);
            return None;
        }

        events(Event::Stage(Stage::Lettering));
        let scratch = match subs::source::temp_dir("ocr") {
            Ok(d) => d,
            Err(_) => {
                note(Warning::NoGlyphTable, report, events);
                return None;
            }
        };
        let opts = subs::segment::SegOpts::default();
        let mut table = start;
        table.version = 1;
        table.source = media.title().to_string();
        let mut settled = subs::learn::Settled::default();

        for (n, (stream, language)) in wanted.streams.iter().zip(&wanted.languages).enumerate() {
            if self.ports.cancel.check().is_err() {
                break;
            }
            let code = lang::parse(language).code;
            let Some(data) = subs::ocr::data_for(&installed, &code) else {
                continue;
            };
            let Ok(src) = subs::source::load(self.ports.runner, &wanted.source, *stream) else {
                continue;
            };
            let reader = subs::ocr::Tesseract {
                runner: self.ports.runner,
                scratch: &scratch.0,
                language: data,
            };
            let cues = src.events();
            let share = 1.0 / wanted.streams.len() as f32;
            let base = n as f32 * share;
            let outcome = subs::learn::from_reader(
                &reader,
                subs::learn::Stream { events: &cues, palette: &src.idx.palette, opts: &opts },
                &mut table,
                subs::learn::Effort::default(),
                &mut |f| {
                    events(Event::Progress {
                        stage: Stage::Lettering,
                        fraction: base + f * share,
                        message: None,
                    });
                },
            );
            match outcome {
                Ok(s) => settled = s,
                Err(e) => note(
                    Warning::SubtitlesUnreadable { language: language.clone(), why: e.to_string() },
                    report,
                    events,
                ),
            }
        }

        if settled.labelled == 0 {
            note(Warning::CannotLearnLettering { shapes: shapes.len() }, report, events);
            return None;
        }
        let path = subs::tables::path_for(&dir, media.title());
        match serde_json::to_vec_pretty(&table)
            .map_err(|e| e.to_string())
            .and_then(|b| self.ports.fs.write(&path, &b).map_err(|e| e.to_string()))
        {
            Ok(()) => events(Event::TableChosen { path, covered: 1.0, built: true }),
            // Not fatal: it was built, it works for this disc, and the only
            // cost of not keeping it is reading the next disc of the set again.
            Err(why) => note(Warning::CacheNotCleared { path, why }, report, events),
        }
        events(Event::LetteringLearned {
            labelled: settled.labelled,
            ambiguous: settled.ambiguous,
            blank: table.unlabelled(),
        });
        Some(table)
    }

    /// The glyph table to decode this disc's subtitles with.
    ///
    /// Every table there is gets tried against the shapes actually on the disc,
    /// and the one that explains most of them wins. Nothing is keyed or
    /// remembered: a second disc of the same release reuses the first's table
    /// because it fits, not because anything wrote down that they are related.
    ///
    /// When none fits, the disc is read and labelled into a table of its own.
    /// That is the only way this can work for somebody who is not going to
    /// label a few hundred shapes by hand, and before it existed a disc from a
    /// studio the shipped table did not cover produced subtitles that were
    /// nothing but placeholders.
    fn lettering(
        &self,
        outputs: &[&Item],
        media: &Media,
        report: &mut Report,
        events: Events,
    ) -> Option<Table> {
        // A table named in preferences that is not there is a mistake worth
        // saying out loud, whatever else is available.
        if let Some(p) = &self.settings.glyph_table
            && !self.ports.fs.exists(p)
        {
            note(Warning::GlyphTableMissing { path: p.clone() }, report, events);
        }

        let streams = self.wanted_subtitles(outputs)?;
        let opts = subs::segment::SegOpts::default();
        let mut shapes = std::collections::BTreeMap::new();
        // One track per language, not one track for the disc. The face is
        // shared - that much was right - but the alphabet is not: a table
        // learned from English has never seen a-ring or an umlaut, and scored
        // against an English sample it fits at 97% and then cannot read a word
        // of the Swedish it was about to be used on.
        let mut sampled: Vec<&str> = Vec::new();
        for (stream, language) in streams.streams.iter().zip(&streams.languages) {
            if sampled.contains(&language.as_str()) {
                continue;
            }
            sampled.push(language);
            let src = match subs::source::load(self.ports.runner, &streams.source, *stream) {
                Ok(s) => s,
                // Silently giving up here is how this went unexplained for an
                // afternoon: the run said "no glyph table" as though none were
                // installed, when what had actually happened was that the
                // track could not be opened to look at.
                Err(e) => {
                    note(
                        Warning::SubtitlesUnreadable {
                            language: language.clone(),
                            why: e.to_string(),
                        },
                        report,
                        events,
                    );
                    continue;
                }
            };
            for ev in src.events().iter().take(SAMPLE_CUES) {
                subs::tables::shapes(
                    &subs::segment::segment(&ev.spu, &src.idx.palette, &opts),
                    &mut shapes,
                );
            }
        }
        if shapes.is_empty() {
            note(
                Warning::SubtitlesUnreadable {
                    language: streams.languages.first().cloned().unwrap_or_default(),
                    why: "no lettering found in any of the tracks wanted".into(),
                },
                report,
                events,
            );
            return None;
        }

        let paths = subs::tables::candidates(
            self.ports.fs,
            self.settings.glyph_table.as_deref(),
            self.settings.tables_dir.as_deref(),
        );
        let closest = subs::tables::closest(self.ports.fs, &paths, &shapes);
        if let Some((path, table, covered)) = &closest
            && *covered >= subs::tables::FITS
        {
            events(Event::TableChosen { path: path.clone(), covered: *covered, built: false });
            return Some(table.clone());
        }
        // Start from the best there is rather than from nothing: it may be
        // this disc's own table from a run that wanted fewer languages, and
        // reading again for shapes it already has is time spent to learn what
        // is already known.
        let start = closest.map(|(_, t, _)| t).unwrap_or_default();
        self.learn_lettering(&streams, media, &shapes, start, report, events)
    }

    fn produce_one(
        &self,
        item: &Item,
        media: &Media,
        dest: &Path,
        table: &Option<Table>,
        at: Position,
        events: Events,
    ) -> Result<Produced> {
        if let Some(parent) = dest.parent() {
            self.ports.fs.create_dir_all(parent)?;
        }
        let info = self.ports.prober.probe(&item.source)?;

        // Subtitles first. Recognising from the rip rather than from the
        // transcode means the SRTs exist before encoding starts, so they can be
        // inputs to the same pass - one ffmpeg invocation instead of three.
        // Announcing a stage blanks the progress bar, so each one here says
        // where the run has got to straight afterwards. Without that, the whole
        // of producing - a minute an episode, seven episodes - showed an empty
        // bar and no text, while the only thing that reported progress was the
        // ripper, which had finished.
        let name = dest.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
        // Episodes done, not a guess at how far into this one ffmpeg is: it is
        // run with -v error and says nothing until it exits, and a made-up
        // number that moves is worse than a true one that does not.
        let done = at.index as f32 / at.total.max(1) as f32;
        let position = |stage: Stage, events: Events| {
            events(Event::Stage(stage));
            events(Event::Progress { stage, fraction: done, message: Some(name.clone()) });
        };

        position(Stage::Subtitles, events);
        let (subtitles, failed, recognised) =
            self.recognise_all(&info, item, table, at.index, events)?;

        position(Stage::Transcode, events);
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
        // Streamed rather than run, so the bar moves within a title as well as
        // between them. A film is one title, so between them says nothing at
        // all: the whole encode showed 0%.
        let encode = plan.command();
        let length = info.duration.max(1) as f32;
        let out = {
            let mut watch = |line: &str| {
                if let Some(us) = line.strip_prefix("out_time_us=")
                    && let Ok(us) = us.trim().parse::<u64>()
                {
                    let within = ((us / 1000) as f32 / length).clamp(0.0, 1.0);
                    events(Event::Progress {
                        stage: Stage::Transcode,
                        fraction: done + within / at.total.max(1) as f32,
                        message: Some(name.clone()),
                    });
                }
            };
            self.ports.runner.stream(&encode, &mut watch)
        };
        match out {
            Ok(o) if !o.ok() => {
                let _ = self.ports.fs.remove_file(&partial);
                return Err(Error(format!("ffmpeg failed ({}): {}", o.status, o.last_error())));
            }
            Err(e) => {
                let _ = self.ports.fs.remove_file(&partial);
                return Err(e);
            }
            Ok(_) => {}
        }
        // Whatever this format still needs of the finished file - the tags
        // Matroska wants targets for, the chapter reference MP4 leaves behind.
        // Not worth failing a finished episode over: it plays either way.
        if let Err(e) =
            self.settings.container.format().finish(self.ports.fs, &partial, media, item)
        {
            events(Event::Warning(Warning::ItemSkipped {
                name: dest.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                why: format!("finishing the file: {}", e.0),
            }));
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
        // What has been kept so far, to notice a track that repeats one.
        let mut seen: Vec<Vec<u8>> = Vec::new();

        for stream in wanted {
            self.ports.cancel.check()?;
            let code =
                subs_tracks.get(stream).map(|t| t.language.clone()).unwrap_or_else(|| "und".into());
            let language = lang::parse(&code);
            let srt_path = srt_for(&item.source, &language.code, stream);

            match subs::recognise_to_file(
                self.ports.runner,
                &item.source,
                stream,
                &language,
                table,
                self.settings.words_dir.as_deref(),
                &srt_path,
            ) {
                // is_usable() already refuses a track with no cues, so an
                // empty one never reaches the encode - it keeps its bitmap
                // instead, which is a subtitle where an empty text track is
                // an entry in a menu that does nothing.
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
                    // A disc repeats its subtitles - for a second camera
                    // angle, or a widescreen cut of the same film - and four
                    // copies of one track is four ways to pick the same
                    // subtitle from a menu. Compared by what they say, since
                    // the same words off two streams are the same subtitle
                    // however the disc filed them.
                    let already = self
                        .ports
                        .fs
                        .read(&r.srt_path)
                        .ok()
                        .filter(|text| seen.iter().any(|s: &Vec<u8>| s == text));
                    if already.is_some() {
                        let _ = self.ports.fs.remove_file(&r.srt_path);
                        events(Event::Subtitle {
                            item: index,
                            language: language.name.clone(),
                            cues: r.cues,
                            unknown: r.unknown_glyphs,
                            recognised: true,
                        });
                        continue;
                    }
                    if let Ok(text) = self.ports.fs.read(&r.srt_path) {
                        seen.push(text);
                    }
                    inputs.push(SubtitleInput {
                        path: r.srt_path.clone(),
                        language: language.code.clone(),
                        forced: subs_tracks.get(stream).is_some_and(|t| t.forced),
                    });
                    recognised.push(r);
                }
                outcome => {
                    // Keeping the bitmap is the safety net: a language with an
                    // unusable text track still has *a* track, and losing the
                    // language entirely is much worse than the redundancy.
                    //
                    // What went wrong is said, though. This used to report
                    // zero cues and zero unknown glyphs whatever had happened,
                    // so a disc whose font is simply not in the table read the
                    // same as a track that would not extract - eight lines of
                    // "not recognised" and nothing to act on. The Lion King
                    // segmented perfectly: 1,179 cues, and 115 distinct shapes
                    // none of which the table had ever seen.
                    let (cues, unknown, shapes, why) = match &outcome {
                        Ok((_, d)) => (d.cues, d.unknown, d.distinct_unknown.len(), None),
                        Err(e) => (0, 0, 0, Some(e.to_string())),
                    };
                    events(Event::Subtitle {
                        item: index,
                        language: language.name.clone(),
                        cues,
                        unknown,
                        recognised: false,
                    });
                    match why {
                        Some(why) => events(Event::Warning(Warning::SubtitlesUnreadable {
                            language: language.name.to_string(),
                            why,
                        })),
                        // Said once for the disc. Every language track on a
                        // disc is rendered in the same font, so this is one
                        // fact about the disc rather than eight about tracks.
                        None if cues > 0 && !self.said_wrong_font.swap(true, Ordering::Relaxed) => {
                            events(Event::Warning(Warning::GlyphTableIsForAnotherFont { shapes }));
                        }
                        None => {}
                    }
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
        if let Some(w) = self.play_alls_skipped(plan.as_deref()) {
            events(Event::Warning(w));
        }
        let files = self.rip(&scan, &titles, rip_dir, events)?;
        let report = self
            .organise(&files, Some(&scan), &media, disc, events)
            .and_then(|items| self.produce(&items, &media, events));
        if let Ok(r) = &report {
            self.discard_rip(rip_dir, r, events);
        }
        report
    }
}

/// Say a warning once, to whoever is watching and to the report.
fn note(w: Warning, report: &mut Report, events: Events) {
    events(Event::Warning(w.clone()));
    report.warnings.push(w);
}

/// How many cues to look at before deciding which table fits.
///
/// Enough to see the alphabet several times over. The question is which face
/// the disc is set in, and that is answered by the first minute of dialogue.
const SAMPLE_CUES: usize = 80;

/// The subtitle streams a run wants, and where to read them from.
struct Wanted {
    source: PathBuf,
    streams: Vec<usize>,
    languages: Vec<String>,
}

/// Which of how many, for the events that say where a run has got to.
#[derive(Debug, Clone, Copy)]
struct Position {
    index: usize,
    total: usize,
}

/// Where a recognised subtitle track is written before it is muxed.
///
/// The stream number is in the name, not only the language. A disc carries the
/// same subtitle several times over - The Lion King has four English tracks
/// and four Swedish - and naming them all `title_t02.eng.srt` meant each
/// overwrote the last, so the encode was handed four inputs that were all the
/// same file and the episode came out with four copies of one track. Worse, a
/// track that failed deleted that shared path on its way out, throwing away
/// what a track before it had recognised: two Swedish tracks arrived holding
/// one cue between them.
fn srt_for(source: &Path, code: &str, stream: usize) -> PathBuf {
    source.with_extension(format!("{code}.{stream}.srt"))
}

/// Where a rip records what it put in the cache directory.
///
/// Hidden and fixed, so it is obvious what it belongs to and a second rip into
/// the same folder cannot end up with two of them.
fn scratch_note(dir: &Path) -> PathBuf {
    dir.join(".riplika-rip")
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
            forced: false,
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
                kind: None,
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
    fn two_tracks_of_one_language_do_not_write_to_the_same_file() {
        // The Lion King has four English subtitle tracks. All four were called
        // title_t02.eng.srt, so each overwrote the last and the encode muxed
        // four inputs that were one file - four identical tracks, 1330 cues
        // each, where the disc held two different ones.
        let src = Path::new("/rip/title_t02.mkv");
        let paths: Vec<PathBuf> = [0, 1, 2, 3].iter().map(|s| srt_for(src, "eng", *s)).collect();
        let mut unique = paths.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), paths.len(), "{paths:?}");
    }

    #[test]
    fn a_recognised_track_is_still_filed_under_the_title_it_came_from() {
        // The cache is emptied by taking out everything sharing a title's
        // name, so a subtitle that does not share it is a subtitle left behind.
        let p = srt_for(Path::new("/rip/title_t02.mkv"), "swe", 5);
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with("title_t02."), "{name}");
        assert!(name.ends_with(".srt"), "{name}");
    }

    #[test]
    fn the_language_is_still_in_the_name_where_a_person_would_look() {
        let p = srt_for(Path::new("/rip/title_t02.mkv"), "swe", 5);
        assert!(p.to_string_lossy().contains("swe"), "{}", p.display());
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

    #[test]
    fn only_the_play_alls_are_counted_as_play_alls() {
        // The log said "skipping 31 play-all titles, whose content is on the
        // disc already" for two play-alls and twenty-nine extras that had been
        // unticked, then said "2 play-all titles" four lines later. A disc that
        // drops something for each reason is the only one that shows it.
        let mut scan = disc_with_a_play_all();
        scan.titles.push(DiscTitle {
            id: 3,
            duration: 300_000,
            chapter_count: 1,
            chapters: vec![300_000],
            size_bytes: 0,
            output_name: "title_t03.mkv".into(),
            tracks: vec![],
        });
        let h = Harness { ripper: FakeRipper::new(scan.clone()), ..harness() };
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
            JobSettings { include_extras: false, ..settings() },
        );
        let mut sink = |_: Event| {};
        let media = p.identify(&scan, &mut sink).remove(0).media;
        let plan = p.preview(&scan, &media, Some(1), Path::new("/rip"));
        let play_alls = plan.as_deref().unwrap().iter().filter(|i| i.role == Role::PlayAll).count();
        let dropped = scan.titles.len() - p.titles_to_rip(&scan, plan.as_deref()).len();
        assert_eq!(play_alls, 1);
        assert!(
            dropped > play_alls,
            "the unticked extra should be dropped too, not only {play_alls}"
        );
        assert_eq!(
            p.play_alls_skipped(plan.as_deref()),
            Some(Warning::PlayAllsSkipped { titles: play_alls })
        );
    }

    #[test]
    fn the_bar_moves_inside_a_title_and_not_only_between_them() {
        // A film is one title, so "how many titles are done" is 0% for the
        // whole encode and 100% at the end. ffmpeg is told -progress because
        // -v error means it otherwise says nothing at all.
        let mut h = harness();
        h.runner = h.runner.on("libx264", "out_time_us=300000000\nout_time_us=600000000\n");
        let (_, _, events) = run_all(&h, settings());

        let inside: Vec<f32> = events
            .iter()
            .filter_map(|e| match e {
                Event::Progress { stage: Stage::Transcode, fraction, .. } => Some(*fraction),
                _ => None,
            })
            .filter(|f| *f > 0.0 && *f < 1.0)
            .collect();
        assert!(inside.len() >= 2, "the encode reported {inside:?}, so the bar cannot move");
        assert!(
            inside.windows(2).any(|w| w[1] > w[0]),
            "the encode never reported going forwards: {inside:?}"
        );
    }

    #[test]
    fn every_stage_announced_while_producing_says_where_the_run_is() {
        // A stage change blanks the progress bar, and producing announces two
        // of them per episode. Nothing said the position again afterwards, so
        // transcoding showed an empty bar and no text for its whole run while
        // ffmpeg was working perfectly well.
        let h = harness();
        let (_, _, events) = run_all(&h, settings());
        let mut checked = 0;
        for (i, e) in events.iter().enumerate() {
            let Event::Stage(s) = e else { continue };
            if !matches!(s, Stage::Subtitles | Stage::Transcode) {
                continue;
            }
            match events.get(i + 1) {
                Some(Event::Progress { stage, .. }) if stage == s => checked += 1,
                other => panic!("{s:?} is followed by {other:?}, so the bar stays empty"),
            }
        }
        assert!(checked >= 2, "producing announced no stages at all");
    }

    /// A film disc: one long title and a pile of short ones, none of which is
    /// a play-all and one of which is episode-shaped.
    fn film_disc(durations: &[Millis]) -> DiscScan {
        let mut scan = fake_disc();
        scan.label = "LKD-0E-YW1.1_DES".into();
        scan.titles = durations
            .iter()
            .enumerate()
            .map(|(i, d)| DiscTitle {
                id: i as u32,
                duration: *d,
                chapter_count: 2,
                chapters: vec![*d / 2, *d - *d / 2],
                size_bytes: 0,
                output_name: format!("title_t{i:02}.mkv"),
                tracks: vec![],
            })
            .collect();
        scan
    }

    #[test]
    fn a_film_is_the_longest_title_and_is_read_even_with_extras_unticked() {
        // The Lion King came out as a 19:41 making-of. The 1:24:45 feature was
        // outside the fifteen-to-forty-five-minute episode window and the
        // making-of was inside it, so the making-of was named as the film and
        // the film was filed as an extra - which, with extras unticked, meant
        // the film was never read off the disc at all.
        let scan = film_disc(&[5_085_000, 1_181_000, 254_000, 73_000]);
        let h = Harness { ripper: FakeRipper::new(scan.clone()), ..harness() };
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
            JobSettings { include_extras: false, ..settings() },
        );
        let media =
            Media::Movie { title: "The Lion King".into(), year: Some(1994), provider_id: None };
        let plan = p.preview(&scan, &media, None, Path::new("/rip")).unwrap();

        let features: Vec<&Item> = plan.iter().filter(|i| i.role == Role::Feature).collect();
        assert_eq!(features.len(), 1, "a film disc holds one film");
        assert!(
            features[0].source.ends_with("title_t00.mkv"),
            "the feature is {:?}, not the longest title",
            features[0].source
        );

        // The half that cost the disc: an extra is not read at all, so naming
        // the wrong title the feature loses the film rather than misfiling it.
        let read = p.titles_to_rip(&scan, Some(&plan));
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].output_name, "title_t00.mkv");
    }

    /// A rip's intermediate files are the size of the disc, so leaving them is
    /// the difference between a cache folder and a second copy of the library.
    fn cache_after(
        h: &Harness,
        seed: &[&str],
        run: impl Fn(&Pipeline, &mut dyn FnMut(Event)),
    ) -> Vec<String> {
        for f in seed {
            h.fs.write(Path::new(f), b"x").unwrap();
        }
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
        let mut sink = |_: Event| {};
        run(&p, &mut sink);
        let mut left: Vec<String> =
            h.fs.list(Path::new("/rip"))
                .unwrap()
                .iter()
                .map(|f| f.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
        left.sort();
        left
    }

    #[test]
    fn a_finished_run_takes_its_intermediate_files_back_out_of_the_cache() {
        // 14 GB of title_t*.mkv accumulated across three days of ripping,
        // because every front end drove the stages itself and none of them
        // deleted anything at the end.
        let h = harness();
        let left = cache_after(
            &h,
            &["/rip/title_t00.mkv", "/rip/title_t01.mkv", "/rip/title_t02.mkv"],
            |p, sink| {
                let scan = p.scan(&fake_disc().drive, sink).unwrap();
                let media = p.identify(&scan, sink).remove(0).media;
                let files = p.rip(&scan, &scan.titles, Path::new("/rip"), sink).unwrap();
                let items = p.organise(&files, None, &media, Some(1), sink).unwrap();
                let report = p.produce(&items, &media, sink).unwrap();
                assert!(!report.produced.is_empty());
                p.discard_rip(Path::new("/rip"), &report, sink);
            },
        );
        assert!(left.is_empty(), "the cache still holds {left:?}");
    }

    #[test]
    fn clearing_the_cache_touches_only_what_the_rip_put_there() {
        // The folder is a preference and can be pointed anywhere, so this can
        // never be a sweep of everything in it.
        let h = harness();
        let left = cache_after(
            &h,
            &["/rip/title_t01.mkv", "/rip/holiday.mkv", "/rip/notes.txt"],
            |p, sink| {
                let scan = p.scan(&fake_disc().drive, sink).unwrap();
                let media = p.identify(&scan, sink).remove(0).media;
                let files = p.rip(&scan, &scan.titles, Path::new("/rip"), sink).unwrap();
                let items = p.organise(&files, None, &media, Some(1), sink).unwrap();
                let report = p.produce(&items, &media, sink).unwrap();
                p.discard_rip(Path::new("/rip"), &report, sink);
            },
        );
        assert_eq!(left, vec!["holiday.mkv".to_string(), "notes.txt".to_string()]);
    }

    #[test]
    fn subtitles_and_part_files_go_with_the_title_they_belong_to() {
        // What is recorded is title_t01.mkv; what is beside it afterwards is
        // title_t01.eng.srt and, if something died mid-mux, title_t01.mkv.part.
        let h = harness();
        let left = cache_after(
            &h,
            &["/rip/title_t01.mkv", "/rip/title_t01.eng.srt", "/rip/title_t01.mkv.part"],
            |p, sink| {
                let scan = p.scan(&fake_disc().drive, sink).unwrap();
                let media = p.identify(&scan, sink).remove(0).media;
                let files = p.rip(&scan, &scan.titles, Path::new("/rip"), sink).unwrap();
                let items = p.organise(&files, None, &media, Some(1), sink).unwrap();
                let report = p.produce(&items, &media, sink).unwrap();
                p.discard_rip(Path::new("/rip"), &report, sink);
            },
        );
        assert!(left.is_empty(), "the cache still holds {left:?}");
    }

    #[test]
    fn the_next_rip_clears_what_a_run_that_never_finished_left() {
        // The window froze between ripping and transcoding and was killed, so
        // nothing was ever going to run at the end of that run. The record is
        // written before the disc is read for exactly this.
        let h = harness();
        let left = cache_after(&h, &[], |p, sink| {
            let scan = p.scan(&fake_disc().drive, sink).unwrap();
            p.rip(&scan, &scan.titles, Path::new("/rip"), sink).unwrap();
            // as if the process died here
            for f in ["title_t00.mkv", "title_t01.mkv", "title_t01.eng.srt"] {
                h.fs.write(&Path::new("/rip").join(f), b"x").unwrap();
            }
            p.rip(&scan, &scan.titles, Path::new("/rip"), sink).unwrap();
        });
        assert_eq!(left, vec![".riplika-rip".to_string()]);
    }

    #[test]
    fn a_run_that_produced_nothing_keeps_what_it_read() {
        // Reading the disc took forty minutes and `riplika process` can still
        // turn these into episodes. Deleting them would mean reading it again.
        let h = harness();
        let left = cache_after(&h, &["/rip/title_t01.mkv"], |p, sink| {
            let scan = p.scan(&fake_disc().drive, sink).unwrap();
            p.rip(&scan, &scan.titles, Path::new("/rip"), sink).unwrap();
            p.discard_rip(Path::new("/rip"), &Report::default(), sink);
        });
        assert!(left.contains(&"title_t01.mkv".to_string()), "left {left:?}");
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
            kind: None,
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
                kind: None,
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
    /// When the fraction last *changed*, and what to.
    ///
    /// Not "when we were last told something". Progress arrives twice a second
    /// whether or not it has moved, and anchoring on the last message meant a
    /// minute of standing still followed by one per cent was measured as one
    /// per cent in half a second. Every stalled second was thrown away, and
    /// the rate came out several times faster than the drive was going.
    moved: Option<(std::time::Instant, f32)>,
}

impl Default for Eta {
    fn default() -> Self {
        Self::new()
    }
}

impl Eta {
    pub fn new() -> Eta {
        Eta::started_at(std::time::Instant::now())
    }

    /// The same, from a clock the caller holds, so this can be tested without
    /// sleeping through the thing being measured.
    pub fn started_at(now: std::time::Instant) -> Eta {
        Eta { started: now, rate: None, moved: None }
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
        self.update_at(std::time::Instant::now(), fraction)
    }

    /// The same, from a clock the caller holds.
    pub fn update_at(
        &mut self,
        now: std::time::Instant,
        fraction: f32,
    ) -> Option<std::time::Duration> {
        let fraction = fraction.clamp(0.0, 1.0);

        match self.moved {
            None => self.moved = Some((now, fraction)),
            Some((then, before)) if fraction > before => {
                let seconds = now.duration_since(then).as_secs_f64();
                if seconds > 0.0 {
                    let instant = seconds / (fraction - before) as f64;
                    // Smoothed, because an optical drive's rate is not steady:
                    // it slows over a layer change and stalls on a retry, and
                    // an estimate that lurched with it would be unreadable.
                    self.rate = Some(match self.rate {
                        Some(r) => r * 0.8 + instant * 0.2,
                        None => instant,
                    });
                    self.moved = Some((now, fraction));
                }
            }
            // A title being retried reports from zero again. Measuring the
            // climb back from where it got to before would call it free.
            Some((_, before)) if fraction < before => self.moved = Some((now, fraction)),
            Some(_) => {}
        }

        let smoothed = self.rate?;
        if !(0.02..1.0).contains(&fraction) {
            return None;
        }
        // What the stage has actually averaged, which is the half that cannot
        // ignore time: every second counts towards it, including the ones
        // where nothing moved, so the number grows while the bar is still
        // instead of sitting at "6 minutes left" for six minutes.
        let average = now.duration_since(self.started).as_secs_f64() / fraction as f64;
        // The slower of the two beliefs. A rip that promises six minutes and
        // takes nine is the failure worth avoiding; one that says twelve and
        // takes nine only ever improves as it goes.
        let rate = smoothed.max(average);
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

    /// A clock the test holds, so these do not sleep through what they measure.
    fn at(start: std::time::Instant, seconds: f64) -> std::time::Instant {
        start + std::time::Duration::from_secs_f64(seconds)
    }

    #[test]
    fn a_steady_rate_gives_a_sensible_estimate() {
        let t0 = std::time::Instant::now();
        let mut eta = Eta::started_at(t0);
        eta.update_at(t0, 0.0);
        // half done in a minute, so about a minute to go
        let left = eta.update_at(at(t0, 60.0), 0.5).expect("half way is enough to estimate from");
        assert!((left.as_secs_f64() - 60.0).abs() < 1.0, "{left:?}");
    }

    #[test]
    fn a_drive_that_slows_is_followed_rather_than_jumped_after() {
        // an optical drive is not steady - it slows over a layer change and
        // stalls on a retry - so the estimate is smoothed
        let t0 = std::time::Instant::now();
        let mut eta = Eta::started_at(t0);
        eta.update_at(t0, 0.0);
        let fast = eta.update_at(at(t0, 30.0), 0.5).unwrap();
        let after_a_stall = eta.update_at(at(t0, 90.0), 0.51).unwrap();
        assert!(after_a_stall > fast);
    }

    #[test]
    fn standing_still_is_counted_rather_than_thrown_away() {
        // Progress arrives twice a second whether or not it has moved. Taking
        // the last message as the anchor measured a minute of standing still
        // followed by one per cent as one per cent in half a second, and the
        // window promised six minutes on a rip with nine to go.
        let t0 = std::time::Instant::now();
        let mut eta = Eta::started_at(t0);
        eta.update_at(t0, 0.0);
        eta.update_at(at(t0, 10.0), 0.05);
        // sixty seconds of being told the same thing
        for i in 0..120 {
            eta.update_at(at(t0, 10.0 + i as f64 * 0.5), 0.05);
        }
        let left = eta.update_at(at(t0, 70.0), 0.06).expect("an estimate by now");
        // 70s for six per cent is about eighteen minutes left, not four
        assert!(left.as_secs() > 600, "{left:?} - the stall was discarded again");
    }

    #[test]
    fn the_estimate_grows_while_nothing_moves() {
        // The complaint that started this: "we have been on 5% for a while and
        // it still says six minutes left".
        let t0 = std::time::Instant::now();
        let mut eta = Eta::started_at(t0);
        eta.update_at(t0, 0.0);
        eta.update_at(at(t0, 15.0), 0.05);
        let first = eta.update_at(at(t0, 20.0), 0.05).unwrap();
        let later = eta.update_at(at(t0, 200.0), 0.05).unwrap();
        assert!(later > first, "still {later:?} after three minutes of nothing");
    }

    #[test]
    fn a_title_starting_over_does_not_make_the_rest_look_free() {
        // A retry reports from zero again. Measuring the climb back from where
        // it had got to before would count it as progress that cost nothing.
        let t0 = std::time::Instant::now();
        let mut eta = Eta::started_at(t0);
        eta.update_at(t0, 0.0);
        eta.update_at(at(t0, 60.0), 0.5);
        eta.update_at(at(t0, 61.0), 0.0);
        let left = eta.update_at(at(t0, 121.0), 0.5).unwrap();
        // half of it took a minute, twice, so a minute more at best
        assert!(left.as_secs() >= 60, "{left:?}");
    }

    #[test]
    fn a_real_rip_is_estimated_within_a_couple_of_minutes_from_five_per_cent() {
        // The Lion King's feature: 9 minutes 20 to read, and at five per cent
        // the window said "about 6 minutes left". Read speed is near enough
        // steady once the drive is going, so five per cent in twenty-eight
        // seconds is all it takes to know that.
        let whole = 560.0;
        let t0 = std::time::Instant::now();
        let mut eta = Eta::started_at(t0);
        let mut said = None;
        for step in 0..=10 {
            let f = step as f32 / 200.0; // up to five per cent
            said = eta.update_at(at(t0, f as f64 * whole), f);
        }
        let left = said.expect("five per cent of nine minutes is enough to speak");
        let truth = whole * 0.95;
        assert!((left.as_secs_f64() - truth).abs() < 120.0, "said {left:?}, and {truth}s was left");
    }

    #[test]
    fn a_finished_job_claims_nothing() {
        let t0 = std::time::Instant::now();
        let mut eta = Eta::started_at(t0);
        eta.update_at(t0, 0.2);
        assert_eq!(eta.update_at(at(t0, 20.0), 1.0), None);
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
