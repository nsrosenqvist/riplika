//! Asking the internet what a disc is.
//!
//! There is no database keyed by DVD. The nearest things are Redump and
//! DVD-Video hash registries, which cover games and preservation rather than
//! retail television, and neither is queryable for "which episodes are on this
//! disc". So identification works the other way round: guess a title from the
//! volume label, look it up in a TV/film catalogue, then confirm the guess
//! against the disc's own structure - the episode count and runtimes have to
//! agree with what is physically there.
//!
//! HTTP is behind a trait. Not for symmetry with the rest of the crate but
//! because tests that reach the network are tests that fail on a train.

use crate::model::{Episode, Media};
use crate::{Error, Result};

/// Fetches a URL. The only network access in the crate.
pub trait Http: Send + Sync {
    fn get(&self, url: &str) -> Result<String>;
}

/// What kind of thing we are looking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Series,
    Movie,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatalogueHit {
    pub media: Media,
    /// The catalogue's own confidence in the match, 0.0 to 1.0.
    pub score: f32,
}

/// A source of titles and episode lists.
pub trait Catalogue: Send + Sync {
    fn name(&self) -> &'static str;

    /// Look up a title. `season` is a hint for building the returned [`Media`],
    /// not a filter.
    fn search(&self, query: &str, kind: MediaKind, season: Option<u32>) -> Result<Vec<CatalogueHit>>;

    /// Episodes of one season, in broadcast order.
    fn episodes(&self, provider_id: &str, season: u32) -> Result<Vec<Episode>>;
}

/// Percent-encode a query string.
pub fn encode(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for b in q.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// TVmaze. Free, no key, and complete for broadcast television.
pub struct TvMaze<'a> {
    pub http: &'a dyn Http,
}

const TVMAZE: &str = "https://api.tvmaze.com";

fn year_of(date: Option<&str>) -> Option<u32> {
    date?.get(..4)?.parse().ok()
}

/// Parse `/search/shows` output.
pub fn parse_tvmaze_search(json: &str, season: Option<u32>) -> Result<Vec<CatalogueHit>> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| Error(format!("TVmaze search: {e}")))?;
    let mut out = Vec::new();
    for hit in v.as_array().unwrap_or(&vec![]) {
        let Some(show) = hit.get("show") else { continue };
        let Some(name) = show.get("name").and_then(|n| n.as_str()) else { continue };
        out.push(CatalogueHit {
            media: Media::Series {
                title: name.to_string(),
                year: year_of(show.get("premiered").and_then(|p| p.as_str())),
                season: season.unwrap_or(1),
                provider_id: show.get("id").map(|i| i.to_string()),
            },
            score: hit
                .get("score")
                .and_then(|s| s.as_f64())
                .unwrap_or(0.0)
                .clamp(0.0, 1.0) as f32,
        });
    }
    Ok(out)
}

/// Parse `/shows/{id}/episodes` output, keeping one season.
pub fn parse_tvmaze_episodes(json: &str, season: u32) -> Result<Vec<Episode>> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| Error(format!("TVmaze episodes: {e}")))?;
    let mut out = Vec::new();
    for e in v.as_array().unwrap_or(&vec![]) {
        let s = e.get("season").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        if s != season {
            continue;
        }
        out.push(Episode {
            season: s,
            number: e.get("number").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            title: e
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            air_date: e
                .get("airdate")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            runtime_minutes: e.get("runtime").and_then(|x| x.as_u64()).map(|x| x as u32),
        });
    }
    // A special numbered 0, or a missing number, would otherwise sort into the
    // middle of the season and shift every episode after it.
    out.retain(|e| e.number > 0);
    out.sort_by_key(|e| e.number);
    Ok(out)
}

impl Catalogue for TvMaze<'_> {
    fn name(&self) -> &'static str {
        "TVmaze"
    }

    fn search(&self, query: &str, kind: MediaKind, season: Option<u32>) -> Result<Vec<CatalogueHit>> {
        if kind == MediaKind::Movie {
            // TVmaze is television only; saying so beats returning nonsense
            return Ok(Vec::new());
        }
        let body = self
            .http
            .get(&format!("{TVMAZE}/search/shows?q={}", encode(query)))?;
        parse_tvmaze_search(&body, season)
    }

    fn episodes(&self, provider_id: &str, season: u32) -> Result<Vec<Episode>> {
        let body = self
            .http
            .get(&format!("{TVMAZE}/shows/{provider_id}/episodes"))?;
        parse_tvmaze_episodes(&body, season)
    }
}

/// TMDB, which also covers film. Needs an API key in `TMDB_API_KEY`.
pub struct Tmdb<'a> {
    pub http: &'a dyn Http,
    pub key: String,
}

impl<'a> Tmdb<'a> {
    /// None when no key is configured, so the caller can fall back quietly.
    pub fn from_env(http: &'a dyn Http) -> Option<Self> {
        std::env::var("TMDB_API_KEY")
            .ok()
            .filter(|k| !k.trim().is_empty())
            .map(|key| Tmdb { http, key })
    }
}

/// Parse a TMDB `/search/movie` or `/search/tv` response.
pub fn parse_tmdb_search(json: &str, kind: MediaKind, season: Option<u32>) -> Result<Vec<CatalogueHit>> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| Error(format!("TMDB search: {e}")))?;
    let results = v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    // TMDB's popularity is unbounded, so rank by position instead: the first
    // result is the most popular match, and that ordering is what we need.
    let n = results.len().max(1) as f32;
    let mut out = Vec::new();
    for (i, r) in results.iter().enumerate() {
        let id = r.get("id").map(|i| i.to_string());
        let score = 1.0 - (i as f32 / n) * 0.5;
        let media = match kind {
            MediaKind::Movie => Media::Movie {
                title: r.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                year: year_of(r.get("release_date").and_then(|d| d.as_str())),
                provider_id: id,
            },
            MediaKind::Series => Media::Series {
                title: r.get("name").and_then(|t| t.as_str()).unwrap_or("").to_string(),
                year: year_of(r.get("first_air_date").and_then(|d| d.as_str())),
                season: season.unwrap_or(1),
                provider_id: id,
            },
        };
        if media.title().is_empty() {
            continue;
        }
        out.push(CatalogueHit { media, score });
    }
    Ok(out)
}

/// Parse TMDB's `/tv/{id}/season/{n}`.
pub fn parse_tmdb_season(json: &str, season: u32) -> Result<Vec<Episode>> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| Error(format!("TMDB season: {e}")))?;
    let mut out = Vec::new();
    for e in v.get("episodes").and_then(|x| x.as_array()).unwrap_or(&vec![]) {
        let number = e.get("episode_number").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
        if number == 0 {
            continue;
        }
        out.push(Episode {
            season,
            number,
            title: e.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
            air_date: e
                .get("air_date")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            runtime_minutes: e.get("runtime").and_then(|x| x.as_u64()).map(|x| x as u32),
        });
    }
    out.sort_by_key(|e| e.number);
    Ok(out)
}

impl Catalogue for Tmdb<'_> {
    fn name(&self) -> &'static str {
        "TMDB"
    }

    fn search(&self, query: &str, kind: MediaKind, season: Option<u32>) -> Result<Vec<CatalogueHit>> {
        let path = match kind {
            MediaKind::Movie => "movie",
            MediaKind::Series => "tv",
        };
        let body = self.http.get(&format!(
            "https://api.themoviedb.org/3/search/{path}?api_key={}&query={}",
            self.key,
            encode(query)
        ))?;
        parse_tmdb_search(&body, kind, season)
    }

    fn episodes(&self, provider_id: &str, season: u32) -> Result<Vec<Episode>> {
        let body = self.http.get(&format!(
            "https://api.themoviedb.org/3/tv/{provider_id}/season/{season}?api_key={}",
            self.key
        ))?;
        parse_tmdb_season(&body, season)
    }
}

/// Try several catalogues, keeping whichever answers.
///
/// TVmaze has no film at all and TMDB needs a key, so neither alone covers a
/// shelf of discs. Failures are skipped rather than propagated: one provider
/// being down should not stop identification.
pub struct Catalogues<'a>(pub Vec<Box<dyn Catalogue + 'a>>);

impl Catalogue for Catalogues<'_> {
    fn name(&self) -> &'static str {
        "catalogues"
    }

    fn search(&self, query: &str, kind: MediaKind, season: Option<u32>) -> Result<Vec<CatalogueHit>> {
        let mut out = Vec::new();
        let mut last_error = None;
        for c in &self.0 {
            match c.search(query, kind, season) {
                Ok(hits) => out.extend(hits),
                Err(e) => last_error = Some(e),
            }
        }
        if out.is_empty()
            && let Some(e) = last_error {
                return Err(e);
            }
        out.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(out)
    }

    fn episodes(&self, provider_id: &str, season: u32) -> Result<Vec<Episode>> {
        for c in &self.0 {
            if let Ok(e) = c.episodes(provider_id, season)
                && !e.is_empty() {
                    return Ok(e);
                }
        }
        Ok(Vec::new())
    }
}

/// Real HTTP, via ureq.
#[derive(Default)]
pub struct UreqHttp;

impl Http for UreqHttp {
    fn get(&self, url: &str) -> Result<String> {
        let mut resp = ureq::get(url)
            .call()
            .map_err(|e| Error(format!("{url}: {e}")))?;
        resp.body_mut()
            .read_to_string()
            .map_err(|e| Error(format!("{url}: {e}")))
    }
}

/// Serves canned responses, matched by substring.
#[derive(Debug, Default)]
pub struct FakeHttp {
    pub responses: std::sync::Mutex<Vec<(String, String)>>,
    pub requested: std::sync::Mutex<Vec<String>>,
}

impl FakeHttp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn on(self, pattern: &str, body: &str) -> Self {
        self.responses
            .lock()
            .unwrap()
            .push((pattern.into(), body.into()));
        self
    }

    pub fn requested(&self) -> Vec<String> {
        self.requested.lock().unwrap().clone()
    }
}

impl Http for FakeHttp {
    fn get(&self, url: &str) -> Result<String> {
        self.requested.lock().unwrap().push(url.to_string());
        self.responses
            .lock()
            .unwrap()
            .iter()
            .find(|(p, _)| url.contains(p.as_str()))
            .map(|(_, b)| b.clone())
            .ok_or_else(|| Error(format!("no canned response for {url}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = r#"[
      {"score":0.94,"show":{"id":1633,"name":"Parks and Recreation","premiered":"2009-04-09"}},
      {"score":0.11,"show":{"id":9999,"name":"Parks","premiered":null}}
    ]"#;

    const EPISODES: &str = r#"[
      {"name":"Pilot","season":1,"number":1,"airdate":"2009-04-09","runtime":30},
      {"name":"2017","season":7,"number":1,"airdate":"2015-01-13","runtime":30},
      {"name":"Ron and Jammy","season":7,"number":2,"airdate":"2015-01-13","runtime":30},
      {"name":"A Special","season":7,"number":0,"airdate":"","runtime":null}
    ]"#;

    #[test]
    fn search_results_carry_the_id_needed_to_fetch_episodes() {
        let hits = parse_tvmaze_search(SEARCH, Some(7)).unwrap();
        assert_eq!(hits.len(), 2);
        match &hits[0].media {
            Media::Series { title, year, season, provider_id } => {
                assert_eq!(title, "Parks and Recreation");
                assert_eq!(*year, Some(2009));
                assert_eq!(*season, 7);
                assert_eq!(provider_id.as_deref(), Some("1633"));
            }
            _ => panic!("expected a series"),
        }
        assert!((hits[0].score - 0.94).abs() < 0.001);
    }

    #[test]
    fn a_missing_premiere_date_is_not_a_failure() {
        let hits = parse_tvmaze_search(SEARCH, None).unwrap();
        assert_eq!(hits[1].media.year(), None);
    }

    #[test]
    fn only_the_requested_season_comes_back_in_order() {
        let eps = parse_tvmaze_episodes(EPISODES, 7).unwrap();
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0].number, 1);
        assert_eq!(eps[0].title, "2017");
        assert_eq!(eps[1].title, "Ron and Jammy");
        assert_eq!(eps[0].air_date.as_deref(), Some("2015-01-13"));
    }

    #[test]
    fn specials_numbered_zero_are_excluded() {
        // otherwise a special sorts to the front and shifts every episode
        let eps = parse_tvmaze_episodes(EPISODES, 7).unwrap();
        assert!(eps.iter().all(|e| e.number > 0));
    }

    #[test]
    fn an_unknown_season_yields_nothing_rather_than_everything() {
        assert!(parse_tvmaze_episodes(EPISODES, 99).unwrap().is_empty());
    }

    #[test]
    fn the_query_is_encoded_so_punctuation_survives() {
        assert_eq!(encode("Parks and Recreation"), "Parks%20and%20Recreation");
        assert_eq!(encode("Ron & Tammy"), "Ron%20%26%20Tammy");
        assert_eq!(encode("plain-name_1.0~x"), "plain-name_1.0~x");
    }

    #[test]
    fn tvmaze_builds_the_urls_it_should() {
        let http = FakeHttp::new()
            .on("/search/shows", SEARCH)
            .on("/episodes", EPISODES);
        let c = TvMaze { http: &http };
        c.search("Parks and Recreation", MediaKind::Series, Some(7)).unwrap();
        c.episodes("1633", 7).unwrap();
        let urls = http.requested();
        assert!(urls[0].ends_with("/search/shows?q=Parks%20and%20Recreation"), "{}", urls[0]);
        assert!(urls[1].ends_with("/shows/1633/episodes"), "{}", urls[1]);
    }

    #[test]
    fn tvmaze_declines_film_rather_than_guessing() {
        let http = FakeHttp::new();
        let c = TvMaze { http: &http };
        assert!(c.search("Lebowski", MediaKind::Movie, None).unwrap().is_empty());
        // and it did not waste a request finding that out
        assert!(http.requested().is_empty());
    }

    #[test]
    fn tmdb_movie_results_carry_a_year() {
        let json = r#"{"results":[{"id":115,"title":"The Big Lebowski","release_date":"1998-03-06"}]}"#;
        let hits = parse_tmdb_search(json, MediaKind::Movie, None).unwrap();
        assert_eq!(hits[0].media.title(), "The Big Lebowski");
        assert_eq!(hits[0].media.year(), Some(1998));
    }

    #[test]
    fn tmdb_ranks_by_position_since_popularity_is_unbounded() {
        let json = r#"{"results":[{"id":1,"title":"A"},{"id":2,"title":"B"},{"id":3,"title":"C"}]}"#;
        let hits = parse_tmdb_search(json, MediaKind::Movie, None).unwrap();
        assert!(hits[0].score > hits[1].score);
        assert!(hits[1].score > hits[2].score);
    }

    #[test]
    fn a_result_with_no_title_is_skipped_rather_than_named_empty() {
        let json = r#"{"results":[{"id":1},{"id":2,"title":"B"}]}"#;
        let hits = parse_tmdb_search(json, MediaKind::Movie, None).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn several_catalogues_merge_and_rank_together() {
        struct One(&'static str, f32);
        impl Catalogue for One {
            fn name(&self) -> &'static str { "one" }
            fn search(&self, _: &str, _: MediaKind, _: Option<u32>) -> Result<Vec<CatalogueHit>> {
                Ok(vec![CatalogueHit {
                    media: Media::Movie { title: self.0.into(), year: None, provider_id: None },
                    score: self.1,
                }])
            }
            fn episodes(&self, _: &str, _: u32) -> Result<Vec<Episode>> { Ok(vec![]) }
        }
        let c = Catalogues(vec![Box::new(One("low", 0.2)), Box::new(One("high", 0.9))]);
        let hits = c.search("x", MediaKind::Movie, None).unwrap();
        assert_eq!(hits[0].media.title(), "high");
    }

    #[test]
    fn one_catalogue_failing_does_not_stop_the_others() {
        struct Broken;
        impl Catalogue for Broken {
            fn name(&self) -> &'static str { "broken" }
            fn search(&self, _: &str, _: MediaKind, _: Option<u32>) -> Result<Vec<CatalogueHit>> {
                Err(Error("network down".into()))
            }
            fn episodes(&self, _: &str, _: u32) -> Result<Vec<Episode>> { Err(Error("down".into())) }
        }
        let http = FakeHttp::new().on("/search/shows", SEARCH);
        let c = Catalogues(vec![Box::new(Broken), Box::new(TvMaze { http: &http })]);
        assert_eq!(c.search("Parks", MediaKind::Series, None).unwrap().len(), 2);
    }

    #[test]
    fn every_catalogue_failing_is_reported() {
        struct Broken;
        impl Catalogue for Broken {
            fn name(&self) -> &'static str { "broken" }
            fn search(&self, _: &str, _: MediaKind, _: Option<u32>) -> Result<Vec<CatalogueHit>> {
                Err(Error("network down".into()))
            }
            fn episodes(&self, _: &str, _: u32) -> Result<Vec<Episode>> { Err(Error("down".into())) }
        }
        let c = Catalogues(vec![Box::new(Broken)]);
        assert!(c.search("x", MediaKind::Series, None).is_err());
    }
}
