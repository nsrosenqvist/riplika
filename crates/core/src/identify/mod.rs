//! Stage two: working out what the disc is.
//!
//! Two independent sources of evidence, deliberately kept apart. The volume
//! label says what the disc claims to be; the disc's own structure - how many
//! episode-length titles, how long they run, how they group under a play-all -
//! says what is physically there. A candidate is only trusted when the two
//! agree, and the reasons are carried along so a wrong guess is visible rather
//! than mysterious.
//!
//! Nothing here decides anything irreversibly. Identification produces ranked
//! candidates and an editable mapping; choosing among them is the user's, which
//! is what makes the GUI's "not this? search for the right one" possible.

pub mod catalogue;
pub mod label;
pub mod structure;

use crate::Result;
use crate::media::{MediaInfo, Prober};
use crate::model::{Candidate, DiscScan, Episode, Item, Media, Millis, Role};
use catalogue::{Catalogue, MediaKind};
use std::path::{Path, PathBuf};
use structure::{EpisodeRange, Structure, TitleShape};

/// How far a title's runtime may sit from the catalogue's stated runtime and
/// still be the same episode. Broad: a "30 minute" slot is 21 minutes of show,
/// and catalogues record the slot.
const RUNTIME_SLACK: f32 = 0.5;

/// Identify a disc from its label, confirmed against its structure.
pub fn identify(scan: &DiscScan, cat: &dyn Catalogue) -> Result<Vec<Candidate>> {
    let guess = label::parse(&scan.label);
    if guess.title.is_empty() {
        return Ok(Vec::new());
    }

    let kind = if guess.season.is_some() { MediaKind::Series } else { MediaKind::Movie };

    // A disc with a season marker is television; one without could be either, so
    // ask for both rather than ruling film in or out on a naming convention.
    let mut hits = cat.search(&guess.title, kind, guess.season)?;
    if kind == MediaKind::Movie {
        hits.extend(cat.search(&guess.title, MediaKind::Series, Some(1))?);
    }

    let range = EpisodeRange::default();
    let episode_durations: Vec<Millis> =
        scan.titles.iter().map(|t| t.duration).filter(|d| range.contains(*d)).collect();

    let mut out = Vec::new();
    for hit in hits {
        let mut reasons = vec![format!("volume label {:?} reads as {:?}", scan.label, guess.title)];
        let mut confidence = hit.score * 0.6;

        if let (Media::Series { season, provider_id, .. }, Some(id)) =
            (&hit.media, hit.media.provider_id())
        {
            match cat.episodes(&id, *season) {
                Ok(eps) if !eps.is_empty() => {
                    reasons.push(format!("season {season} has {} episodes", eps.len()));
                    if episode_durations.len() <= eps.len() && !episode_durations.is_empty() {
                        confidence += 0.15;
                        reasons.push(format!(
                            "{} episode-length titles on the disc fit within that",
                            episode_durations.len()
                        ));
                    }
                    if let Some(m) = runtime_agreement(&episode_durations, &eps) {
                        confidence += 0.25 * m;
                        reasons
                            .push(format!("runtimes agree with the catalogue ({:.0}%)", m * 100.0));
                    }
                }
                _ => {
                    let _ = provider_id;
                    reasons.push(format!("no episode list for season {season}"));
                }
            }
        }

        out.push(Candidate {
            media: hit.media,
            confidence: confidence.clamp(0.0, 1.0),
            reasons,
            detail: hit.detail,
        });
    }
    out.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    Ok(out)
}

impl Media {
    pub fn provider_id(&self) -> Option<String> {
        match self {
            Media::Series { provider_id, .. } | Media::Movie { provider_id, .. } => {
                provider_id.clone()
            }
        }
    }
}

/// Share of the disc's titles whose runtime is consistent with the season's.
fn runtime_agreement(durations: &[Millis], episodes: &[Episode]) -> Option<f32> {
    let stated: Vec<u32> = episodes.iter().filter_map(|e| e.runtime_minutes).collect();
    if stated.is_empty() || durations.is_empty() {
        return None;
    }
    let nominal = (stated.iter().sum::<u32>() as f32 / stated.len() as f32) * 60_000.0;
    let ok = durations
        .iter()
        .filter(|d| {
            let d = **d as f32;
            (d - nominal).abs() / nominal <= RUNTIME_SLACK
        })
        .count();
    Some(ok as f32 / durations.len() as f32)
}

/// What the user says this is, when no catalogue agrees.
///
/// The catalogues do not have everything - a regional release, a box set of
/// something obscure, a disc of home video, or simply no network - and refusing
/// to rip a disc because a website has not heard of it is refusing to do the
/// job. Nothing downstream requires a catalogue: episodes without entries are
/// named "Episode 3" and can be renamed, which is a far better position than an
/// unread disc.
///
/// A season number is what makes it a series. Without one there is nothing to
/// number episodes by, so it is a film.
pub fn unverified(title: &str, season: Option<u32>) -> Media {
    match season {
        Some(season) => {
            Media::Series { title: title.trim().to_string(), year: None, season, provider_id: None }
        }
        None => Media::Movie { title: title.trim().to_string(), year: None, provider_id: None },
    }
}

/// Search a catalogue directly, for when the guess was wrong.
pub fn search(cat: &dyn Catalogue, query: &str, season: Option<u32>) -> Result<Vec<Candidate>> {
    let kind = if season.is_some() { MediaKind::Series } else { MediaKind::Movie };
    let mut hits = cat.search(query, kind, season)?;
    if kind == MediaKind::Movie {
        hits.extend(cat.search(query, MediaKind::Series, Some(1))?);
    }
    hits.sort_by(|a, b| b.score.total_cmp(&a.score));
    Ok(hits
        .into_iter()
        .map(|h| Candidate {
            confidence: h.score,
            media: h.media,
            // "searched for X" is the same on every row and restates the box
            // the user just typed in. What distinguishes these results is what
            // the works are, not that a search happened.
            reasons: Vec::new(),
            detail: h.detail,
        })
        .collect())
}

/// Describe the ripped files: durations and chapters, which need a probe.
pub fn shapes(prober: &dyn Prober, files: &[PathBuf]) -> Result<Vec<(TitleShape, MediaInfo)>> {
    let mut out = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let info = prober.probe(f)?;
        out.push((
            TitleShape {
                key: f.file_name().unwrap_or_default().to_string_lossy().into_owned(),
                order: i as u32,
                duration: info.duration,
                chapters: info.chapter_durations(),
            },
            info,
        ));
    }
    Ok(out)
}

/// Which episode number the first title on this disc should get.
///
/// Genuinely ambiguous from one disc: a season split 5/5/4 and one split 4/4/6
/// look identical from disc two. So this guesses, and the guess is presented
/// for confirmation rather than applied silently.
///
/// `already_present` is the strongest evidence available - episode numbers
/// already in the output directory mean the earlier discs are done, and the
/// next free number is simply correct.
pub fn episode_offset(disc: Option<u32>, count_on_disc: usize, already_present: &[u32]) -> u32 {
    if let Some(max) = already_present.iter().max() {
        return *max;
    }
    match disc {
        Some(d) if d > 1 => (d - 1) * count_on_disc as u32,
        _ => 0,
    }
}

/// Turn a sorted-out disc into the list of files to produce.
pub fn assign(
    media: &Media,
    episodes: &[Episode],
    st: &Structure,
    dir: &Path,
    offset: u32,
    extended: &[(String, String, f32)],
) -> Vec<Item> {
    let season = match media {
        Media::Series { season, .. } => *season,
        Media::Movie { .. } => 0,
    };
    let mut items = Vec::new();
    for (i, key) in st.episodes.iter().enumerate() {
        let number = offset + i as u32 + 1;
        let ep = episodes.iter().find(|e| e.number == number);
        items.push(Item {
            source: dir.join(key),
            role: if matches!(media, Media::Movie { .. }) {
                Role::Feature
            } else {
                Role::Episode { season, number }
            },
            // A missing catalogue entry must not lose the file: it still gets a
            // name, just a generic one the user can correct.
            title: ep.map(|e| e.title.clone()).unwrap_or_else(|| format!("Episode {number}")),
            air_date: ep.and_then(|e| e.air_date.clone()),
            duration: 0,
            destination: None,
        });
    }

    for (cut, of, _) in extended {
        // an extended cut inherits the episode number of what it duplicates
        let number =
            st.episodes.iter().position(|e| e == of).map(|i| offset + i as u32 + 1).unwrap_or(0);
        let ep = episodes.iter().find(|e| e.number == number);
        items.push(Item {
            source: dir.join(cut),
            role: Role::ExtendedCut { season, number },
            title: ep.map(|e| e.title.clone()).unwrap_or_else(|| format!("Episode {number}")),
            air_date: ep.and_then(|e| e.air_date.clone()),
            duration: 0,
            destination: None,
        });
    }

    let claimed: Vec<&String> = extended.iter().map(|(c, _, _)| c).collect();
    for key in st.loose.iter().chain(st.extras.iter()) {
        if claimed.contains(&key) {
            continue;
        }
        items.push(Item {
            source: dir.join(key),
            role: Role::Extra,
            title: Path::new(key)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| key.clone()),
            air_date: None,
            duration: 0,
            destination: None,
        });
    }

    // Play-alls are listed so the user can see they were understood, and are
    // never written out.
    for (key, _) in &st.play_alls {
        items.push(Item {
            source: dir.join(key),
            role: Role::PlayAll,
            title: key.clone(),
            air_date: None,
            duration: 0,
            destination: None,
        });
    }
    items
}

/// Episode numbers already written into a directory, from their filenames.
pub fn existing_episode_numbers(files: &[PathBuf]) -> Vec<u32> {
    let mut out = Vec::new();
    for f in files {
        // Only finished files count. A half-written ".part" left by an
        // interrupted run must not be read as an episode already done.
        let is_media = f
            .extension()
            .map(|e| {
                let e = e.to_string_lossy().to_ascii_lowercase();
                e == "mp4" || e == "mkv" || e == "m4v"
            })
            .unwrap_or(false);
        if !is_media {
            continue;
        }
        let name = f.file_name().unwrap_or_default().to_string_lossy().to_uppercase();
        // look for SxxEyy
        let bytes: Vec<char> = name.chars().collect();
        for i in 0..bytes.len() {
            if bytes[i] != 'E' || i == 0 {
                continue;
            }
            let digits: String = bytes[i + 1..].iter().take_while(|c| c.is_ascii_digit()).collect();
            // an "E" that follows "Sdd" is an episode marker, not a word
            let preceded_by_season =
                bytes[..i].iter().rev().take_while(|c| c.is_ascii_digit()).count() > 0;
            if preceded_by_season && let Ok(n) = digits.parse::<u32>() {
                out.push(n);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_with_no_catalogue_behind_it_is_still_usable() {
        // The catalogues do not have everything, and refusing to rip a disc
        // because a website has not heard of it is refusing to do the job.
        let m = unverified("Nikkes Hemmavideo 1997", Some(1));
        assert_eq!(m.title(), "Nikkes Hemmavideo 1997");
        assert_eq!(m.season(), Some(1));
        assert!(m.provider_id().is_none(), "it came from nowhere and should say so");
    }

    #[test]
    fn a_season_is_what_makes_it_a_series() {
        // without one there is nothing to number episodes by
        assert!(matches!(unverified("Heat", None), Media::Movie { .. }));
        assert!(matches!(unverified("Heat", Some(2)), Media::Series { .. }));
    }

    #[test]
    fn a_name_is_taken_as_typed_apart_from_the_spaces_around_it() {
        assert_eq!(unverified("  The Office  ", Some(3)).title(), "The Office");
    }

    #[test]
    fn episodes_are_numbered_even_with_nothing_to_name_them() {
        // the whole point: a disc still comes out as files
        let media = unverified("Something Obscure", Some(2));
        let st = structure::Structure {
            play_alls: Vec::new(),
            episodes: vec!["t01.mkv".into(), "t02.mkv".into()],
            loose: Vec::new(),
            extras: Vec::new(),
        };
        let items = assign(&media, &[], &st, Path::new("/rip"), 0, &[]);
        let names: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(names, vec!["Episode 1", "Episode 2"]);
    }
    use crate::identify::catalogue::{FakeHttp, TvMaze};
    use crate::model::{DiscTitle, Drive};

    const SEARCH: &str = r#"[{"score":0.94,"show":{"id":1633,"name":"Parks and Recreation","premiered":"2009-04-09"}}]"#;
    const EPISODES: &str = r#"[
      {"name":"2017","season":7,"number":1,"airdate":"2015-01-13","runtime":30},
      {"name":"Ron and Jammy","season":7,"number":2,"airdate":"2015-01-13","runtime":30},
      {"name":"William Henry Harrison","season":7,"number":3,"airdate":"2015-01-20","runtime":30},
      {"name":"Leslie and Ron","season":7,"number":4,"airdate":"2015-01-20","runtime":30}
    ]"#;

    fn scan(label: &str, durations: &[Millis]) -> DiscScan {
        DiscScan {
            drive: Drive {
                id: "disc:0".into(),
                device: "/dev/sr0".into(),
                name: "drive".into(),
                disc_label: Some(label.into()),
            },
            label: label.into(),
            titles: durations
                .iter()
                .enumerate()
                .map(|(i, d)| DiscTitle {
                    id: i as u32,
                    duration: *d,
                    chapter_count: 6,
                    chapters: Vec::new(),
                    size_bytes: 0,
                    output_name: format!("title_t{i:02}.mkv"),
                    tracks: vec![],
                })
                .collect(),
        }
    }

    fn tvmaze() -> FakeHttp {
        FakeHttp::new().on("/search/shows", SEARCH).on("/episodes", EPISODES)
    }

    #[test]
    fn a_labelled_disc_is_identified_with_its_reasons() {
        let http = tvmaze();
        let cat = TvMaze { http: &http };
        // four 21-minute episodes, as season 7 disc 1 really holds
        let s = scan("PARKS_AND_RECREATION_S7D1", &[1_275_000; 4]);
        let c = identify(&s, &cat).unwrap();
        assert_eq!(c[0].media.title(), "Parks and Recreation");
        assert!(matches!(c[0].media, Media::Series { season: 7, .. }));
        assert!(c[0].confidence > 0.8, "{}", c[0].confidence);
        assert!(c[0].reasons.iter().any(|r| r.contains("volume label")));
        assert!(c[0].reasons.iter().any(|r| r.contains("runtimes agree")));
    }

    #[test]
    fn structure_that_contradicts_the_label_lowers_confidence() {
        let http = tvmaze();
        let cat = TvMaze { http: &http };
        // one 90-minute title is not four half-hour episodes
        let s = scan("PARKS_AND_RECREATION_S7D1", &[5_400_000]);
        let weak = identify(&s, &cat).unwrap();
        let strong = identify(&scan("PARKS_AND_RECREATION_S7D1", &[1_275_000; 4]), &cat).unwrap();
        assert!(weak[0].confidence < strong[0].confidence);
    }

    #[test]
    fn an_unreadable_label_yields_no_candidates_rather_than_a_wrong_one() {
        let http = tvmaze();
        let cat = TvMaze { http: &http };
        assert!(identify(&scan("", &[1_275_000]), &cat).unwrap().is_empty());
    }

    #[test]
    fn runtime_agreement_tolerates_the_slot_versus_show_difference() {
        // catalogues record the 30-minute slot; the show is 21 minutes
        let eps: Vec<Episode> = (1..=4)
            .map(|n| Episode {
                season: 7,
                number: n,
                title: "x".into(),
                air_date: None,
                runtime_minutes: Some(30),
            })
            .collect();
        let m = runtime_agreement(&[1_275_000; 4], &eps).unwrap();
        assert!((m - 1.0).abs() < 0.001);
        // but an hour-long title is not a half-hour episode
        assert!(runtime_agreement(&[3_600_000; 4], &eps).unwrap() < 0.5);
    }

    #[test]
    fn a_search_lets_the_user_override_a_wrong_guess() {
        let http = tvmaze();
        let cat = TvMaze { http: &http };
        let c = search(&cat, "Parks and Recreation", Some(7)).unwrap();
        assert_eq!(c[0].media.title(), "Parks and Recreation");
        assert!(matches!(c[0].media, Media::Series { season: 7, .. }));
    }

    #[test]
    fn the_first_disc_starts_at_episode_one() {
        assert_eq!(episode_offset(Some(1), 4, &[]), 0);
        assert_eq!(episode_offset(None, 4, &[]), 0);
    }

    #[test]
    fn a_later_disc_continues_from_a_uniform_split() {
        assert_eq!(episode_offset(Some(3), 4, &[]), 8);
    }

    #[test]
    fn what_is_already_on_disk_beats_the_uniform_guess() {
        // a 5/5/4 season would be mis-numbered by the guess alone
        assert_eq!(episode_offset(Some(3), 4, &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]), 10);
    }

    #[test]
    fn existing_episodes_are_read_back_out_of_filenames() {
        let files: Vec<PathBuf> = [
            "Parks and Recreation - S07E01 - 2017.mp4",
            "Parks and Recreation - S07E02 - Ron and Jammy.mp4",
            "Some Extra.mp4",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        assert_eq!(existing_episode_numbers(&files), vec![1, 2]);
    }

    fn structure_of(episodes: &[&str], loose: &[&str], extras: &[&str]) -> Structure {
        Structure {
            play_alls: vec![("play.mkv".into(), episodes.iter().map(|s| s.to_string()).collect())],
            episodes: episodes.iter().map(|s| s.to_string()).collect(),
            loose: loose.iter().map(|s| s.to_string()).collect(),
            extras: extras.iter().map(|s| s.to_string()).collect(),
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

    fn episodes() -> Vec<Episode> {
        catalogue::parse_tvmaze_episodes(EPISODES, 7).unwrap()
    }

    #[test]
    fn episodes_are_numbered_and_titled_from_the_catalogue() {
        let st = structure_of(&["a.mkv", "b.mkv"], &[], &[]);
        let items = assign(&season7(), &episodes(), &st, Path::new("/rip"), 0, &[]);
        assert_eq!(items[0].role, Role::Episode { season: 7, number: 1 });
        assert_eq!(items[0].title, "2017");
        assert_eq!(items[0].air_date.as_deref(), Some("2015-01-13"));
        assert_eq!(items[1].title, "Ron and Jammy");
    }

    #[test]
    fn an_offset_shifts_the_whole_disc() {
        let st = structure_of(&["a.mkv", "b.mkv"], &[], &[]);
        let items = assign(&season7(), &episodes(), &st, Path::new("/rip"), 2, &[]);
        assert_eq!(items[0].role, Role::Episode { season: 7, number: 3 });
        assert_eq!(items[0].title, "William Henry Harrison");
    }

    #[test]
    fn a_missing_catalogue_entry_still_produces_a_named_file() {
        let st = structure_of(&["a.mkv"], &[], &[]);
        let items = assign(&season7(), &[], &st, Path::new("/rip"), 0, &[]);
        assert_eq!(items[0].title, "Episode 1");
        assert_eq!(items[0].role, Role::Episode { season: 7, number: 1 });
    }

    #[test]
    fn an_extended_cut_takes_the_number_of_what_it_duplicates() {
        let st = structure_of(&["a.mkv", "b.mkv"], &["long.mkv"], &[]);
        let ext = vec![("long.mkv".to_string(), "b.mkv".to_string(), 0.9f32)];
        let items = assign(&season7(), &episodes(), &st, Path::new("/rip"), 0, &ext);
        let cut = items.iter().find(|i| i.source.ends_with("long.mkv")).unwrap();
        assert_eq!(cut.role, Role::ExtendedCut { season: 7, number: 2 });
        assert_eq!(cut.title, "Ron and Jammy");
    }

    #[test]
    fn a_loose_title_that_is_not_an_extended_cut_becomes_an_extra() {
        let st = structure_of(&["a.mkv"], &["mystery.mkv"], &["short.mkv"]);
        let items = assign(&season7(), &episodes(), &st, Path::new("/rip"), 0, &[]);
        let roles: Vec<&Role> = items.iter().map(|i| &i.role).collect();
        assert_eq!(roles.iter().filter(|r| ***r == Role::Extra).count(), 2);
    }

    #[test]
    fn the_play_all_is_listed_but_never_written() {
        let st = structure_of(&["a.mkv"], &[], &[]);
        let items = assign(&season7(), &episodes(), &st, Path::new("/rip"), 0, &[]);
        let pa = items.iter().find(|i| i.role == Role::PlayAll).unwrap();
        assert!(!pa.role.is_output());
    }
}
