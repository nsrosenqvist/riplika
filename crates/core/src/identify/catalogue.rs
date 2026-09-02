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

    /// Fetch something that is not text.
    ///
    /// Cover art is a JPEG, and a round trip through `String` would not leave
    /// it a JPEG.
    fn get_bytes(&self, url: &str) -> Result<Vec<u8>>;
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
    /// What the work is, in a line - who broadcast it, what kind of thing it
    /// is, and when it ran.
    ///
    /// This is what tells nine similarly-named shows apart. A search for "Bear
    /// Grylls" returns a dozen titles that differ mainly by broadcaster.
    pub detail: Option<String>,
    /// Where a picture of the work can be fetched, when the catalogue has one.
    ///
    /// A whole URL, because the two catalogues disagree about what they hand
    /// back: TVmaze gives one outright and TMDB gives a path that has to be
    /// hung off an image host. Sorting that out here means nothing downstream
    /// has to know which catalogue answered.
    pub poster: Option<String>,
}

/// Where TMDB's images live. The width is a choice: 342 is the smallest that
/// does not look soft at the size this is shown, and the largest worth
/// fetching for a thumbnail.
pub const TMDB_IMAGE_BASE: &str = "https://image.tmdb.org/t/p/w342";

/// A source of titles and episode lists.
pub trait Catalogue: Send + Sync {
    fn name(&self) -> &'static str;

    /// Short tag stamped onto the ids this catalogue issues.
    ///
    /// Ids are only meaningful to the catalogue that minted them - TVmaze's
    /// 1633 and TMDB's 1633 are different shows - so an id carries its origin
    /// and is only ever handed back to the same place.
    fn prefix(&self) -> &'static str;

    /// Look up a title. `season` is a hint for building the returned [`Media`],
    /// not a filter.
    fn search(
        &self,
        query: &str,
        kind: MediaKind,
        season: Option<u32>,
    ) -> Result<Vec<CatalogueHit>>;

    /// Episodes of one season, in broadcast order.
    fn episodes(&self, provider_id: &str, season: u32) -> Result<Vec<Episode>>;
}

/// Take the catalogue tag off an id, refusing one that belongs elsewhere.
pub fn strip_prefix<'a>(id: &'a str, prefix: &str) -> Result<&'a str> {
    match id.split_once(':') {
        Some((p, rest)) if p == prefix => Ok(rest),
        Some((p, _)) => Err(Error(format!("id belongs to {p}, not {prefix}"))),
        // Ids minted before they carried a tag; assume they are ours.
        None => Ok(id),
    }
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

/// A line describing a show, for telling similar ones apart.
///
/// Ordered by how much each part narrows things down: the broadcaster first,
/// since that is what separates a dozen shows sharing a presenter's name, then
/// what kind of programme it is, then when it ran.
pub fn describe_show(
    network: Option<&str>,
    kind: Option<&str>,
    premiered: Option<&str>,
    ended: Option<&str>,
    status: Option<&str>,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(n) = network.filter(|n| !n.is_empty()) {
        parts.push(n.to_string());
    }
    if let Some(k) = kind.filter(|k| !k.is_empty()) {
        parts.push(k.to_string());
    }
    match (year_of(premiered), year_of(ended)) {
        // A range only earns its place when it spans more than the year
        // already shown beside the title.
        (Some(from), Some(to)) if to > from => parts.push(format!("{from}-{to}")),
        (Some(_), None) if status == Some("Running") => parts.push("ongoing".into()),
        _ => {}
    }
    if parts.is_empty() { None } else { Some(parts.join(" \u{b7} ")) }
}

/// Parse `/search/shows` output.
pub fn parse_tvmaze_search(json: &str, season: Option<u32>) -> Result<Vec<CatalogueHit>> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| Error(format!("TVmaze search: {e}")))?;
    let mut out = Vec::new();
    for hit in v.as_array().unwrap_or(&vec![]) {
        let Some(show) = hit.get("show") else { continue };
        let Some(name) = show.get("name").and_then(|n| n.as_str()) else { continue };
        let text = |k: &str| show.get(k).and_then(|x| x.as_str());
        // A show is on a broadcast network or a streaming service, never both.
        let network = show
            .get("network")
            .or_else(|| show.get("webChannel"))
            .and_then(|n| n.get("name"))
            .and_then(|n| n.as_str());
        out.push(CatalogueHit {
            media: Media::Series {
                title: name.to_string(),
                year: year_of(text("premiered")),
                season: season.unwrap_or(1),
                provider_id: show.get("id").map(|i| format!("tvmaze:{i}")),
            },
            score: hit.get("score").and_then(|s| s.as_f64()).unwrap_or(0.0).clamp(0.0, 1.0) as f32,
            poster: show
                .get("image")
                .and_then(|i| i.get("medium").or_else(|| i.get("original")))
                .and_then(|u| u.as_str())
                .map(str::to_string),
            detail: describe_show(
                network,
                text("type"),
                text("premiered"),
                text("ended"),
                text("status"),
            ),
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
            title: e.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
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

    fn prefix(&self) -> &'static str {
        "tvmaze"
    }

    fn search(
        &self,
        query: &str,
        kind: MediaKind,
        season: Option<u32>,
    ) -> Result<Vec<CatalogueHit>> {
        if kind == MediaKind::Movie {
            // TVmaze is television only; saying so beats returning nonsense
            return Ok(Vec::new());
        }
        let body = self.http.get(&format!("{TVMAZE}/search/shows?q={}", encode(query)))?;
        parse_tvmaze_search(&body, season)
    }

    fn episodes(&self, provider_id: &str, season: u32) -> Result<Vec<Episode>> {
        let id = strip_prefix(provider_id, self.prefix())?;
        let body = self.http.get(&format!("{TVMAZE}/shows/{id}/episodes"))?;
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
    ///
    /// From the login keyring first, then the environment - a script or a
    /// container has no keyring and should not need one.
    pub fn configured(http: &'a dyn Http) -> Option<Self> {
        crate::secret::tmdb_key().map(|key| Tmdb { http, key })
    }
}

/// Parse a TMDB `/search/movie` or `/search/tv` response.
pub fn parse_tmdb_search(
    json: &str,
    kind: MediaKind,
    season: Option<u32>,
) -> Result<Vec<CatalogueHit>> {
    let v: serde_json::Value =
        serde_json::from_str(json).map_err(|e| Error(format!("TMDB search: {e}")))?;
    let results = v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    // TMDB's popularity is unbounded, so rank by position instead: the first
    // result is the most popular match, and that ordering is what we need.
    let n = results.len().max(1) as f32;
    let mut out = Vec::new();
    for (i, r) in results.iter().enumerate() {
        let id = r.get("id").map(|i| format!("tmdb:{i}"));
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
        // TMDB's search results carry no broadcaster, so this is thinner
        let detail = describe_show(
            r.get("original_language").and_then(|l| l.as_str()),
            None,
            r.get("first_air_date").or_else(|| r.get("release_date")).and_then(|d| d.as_str()),
            None,
            None,
        );
        let poster =
            r.get("poster_path").and_then(|p| p.as_str()).map(|p| format!("{TMDB_IMAGE_BASE}{p}"));
        out.push(CatalogueHit { media, score, detail, poster });
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

    fn prefix(&self) -> &'static str {
        "tmdb"
    }

    fn search(
        &self,
        query: &str,
        kind: MediaKind,
        season: Option<u32>,
    ) -> Result<Vec<CatalogueHit>> {
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
        let id = strip_prefix(provider_id, self.prefix())?;
        let body = self.http.get(&format!(
            "https://api.themoviedb.org/3/tv/{id}/season/{season}?api_key={}",
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

    fn prefix(&self) -> &'static str {
        ""
    }

    /// Ask each catalogue in turn and take the first that answers.
    ///
    /// In order, not merged: two catalogues that both know a show return it
    /// twice, and a list offering the same programme twice is a worse answer
    /// than one. The order is the caller's preference - TMDB first when a key
    /// is configured, since it is better data and it is what a media server
    /// will use for the same files.
    fn search(
        &self,
        query: &str,
        kind: MediaKind,
        season: Option<u32>,
    ) -> Result<Vec<CatalogueHit>> {
        let mut last_error = None;
        for c in &self.0 {
            match c.search(query, kind, season) {
                Ok(hits) if !hits.is_empty() => return Ok(hits),
                Ok(_) => {}
                Err(e) => last_error = Some(e),
            }
        }
        match last_error {
            Some(e) => Err(e),
            None => Ok(Vec::new()),
        }
    }

    /// Ask whoever minted the id.
    fn episodes(&self, provider_id: &str, season: u32) -> Result<Vec<Episode>> {
        let origin = provider_id.split_once(':').map(|(p, _)| p);
        for c in &self.0 {
            if (origin.is_none() || origin == Some(c.prefix()))
                && let Ok(e) = c.episodes(provider_id, season)
                && !e.is_empty()
            {
                return Ok(e);
            }
        }
        Ok(Vec::new())
    }
}

/// Real HTTP, via ureq.
#[derive(Default)]
pub struct UreqHttp;

/// Who we say we are.
///
/// MusicBrainz asks that clients identify themselves and name somewhere to
/// complain to, and throttles or blocks anonymous ones when it is busy. The
/// other catalogues do not ask, but there is no reason to be anonymous to them
/// either.
pub const USER_AGENT: &str =
    concat!("Riplika/", env!("CARGO_PKG_VERSION"), " ( https://github.com/nsrosenqvist/riplika )");

impl Http for UreqHttp {
    fn get(&self, url: &str) -> Result<String> {
        let mut resp = ureq::get(url)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| Error(format!("{url}: {e}")))?;
        resp.body_mut().read_to_string().map_err(|e| Error(format!("{url}: {e}")))
    }

    fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        let mut resp = ureq::get(url)
            .header("User-Agent", USER_AGENT)
            .call()
            .map_err(|e| Error(format!("{url}: {e}")))?;
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut resp.body_mut().as_reader(), &mut buf)
            .map_err(|e| Error(format!("{url}: {e}")))?;
        Ok(buf)
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
        self.responses.lock().unwrap().push((pattern.into(), body.into()));
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

    fn get_bytes(&self, url: &str) -> Result<Vec<u8>> {
        self.get(url).map(String::into_bytes)
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
                assert_eq!(provider_id.as_deref(), Some("tvmaze:1633"));
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
        let http = FakeHttp::new().on("/search/shows", SEARCH).on("/episodes", EPISODES);
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
        let json =
            r#"{"results":[{"id":115,"title":"The Big Lebowski","release_date":"1998-03-06"}]}"#;
        let hits = parse_tmdb_search(json, MediaKind::Movie, None).unwrap();
        assert_eq!(hits[0].media.title(), "The Big Lebowski");
        assert_eq!(hits[0].media.year(), Some(1998));
    }

    #[test]
    fn tmdb_ranks_by_position_since_popularity_is_unbounded() {
        let json =
            r#"{"results":[{"id":1,"title":"A"},{"id":2,"title":"B"},{"id":3,"title":"C"}]}"#;
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
    fn catalogues_are_asked_in_order_and_the_first_answer_wins() {
        // Not merged: two catalogues that both know a show would return it
        // twice, and a list offering the same programme twice is a worse
        // answer than one. The order is the caller's preference.
        struct One(&'static str, f32);
        impl Catalogue for One {
            fn name(&self) -> &'static str {
                "one"
            }
            fn prefix(&self) -> &'static str {
                "one"
            }
            fn search(&self, _: &str, _: MediaKind, _: Option<u32>) -> Result<Vec<CatalogueHit>> {
                Ok(vec![CatalogueHit {
                    poster: None,
                    media: Media::Movie { title: self.0.into(), year: None, provider_id: None },
                    score: self.1,
                    detail: None,
                }])
            }
            fn episodes(&self, _: &str, _: u32) -> Result<Vec<Episode>> {
                Ok(vec![])
            }
        }
        let c = Catalogues(vec![Box::new(One("preferred", 0.2)), Box::new(One("second", 0.9))]);
        let hits = c.search("x", MediaKind::Movie, None).unwrap();
        assert_eq!(hits.len(), 1, "the second catalogue must not be asked");
        assert_eq!(hits[0].media.title(), "preferred");
    }

    #[test]
    fn an_id_is_only_ever_offered_back_to_whoever_minted_it() {
        // TVmaze's 1633 and TMDB's 1633 are different shows, so an id carries
        // its origin and is routed by it.
        assert_eq!(strip_prefix("tvmaze:1633", "tvmaze").unwrap(), "1633");
        assert!(strip_prefix("tmdb:1633", "tvmaze").is_err());
        // ids from before they were tagged are assumed to be ours
        assert_eq!(strip_prefix("1633", "tvmaze").unwrap(), "1633");
    }

    #[test]
    fn one_catalogue_failing_does_not_stop_the_others() {
        struct Broken;
        impl Catalogue for Broken {
            fn name(&self) -> &'static str {
                "broken"
            }
            fn prefix(&self) -> &'static str {
                "broken"
            }
            fn search(&self, _: &str, _: MediaKind, _: Option<u32>) -> Result<Vec<CatalogueHit>> {
                Err(Error("network down".into()))
            }
            fn episodes(&self, _: &str, _: u32) -> Result<Vec<Episode>> {
                Err(Error("down".into()))
            }
        }
        let http = FakeHttp::new().on("/search/shows", SEARCH);
        let c = Catalogues(vec![Box::new(Broken), Box::new(TvMaze { http: &http })]);
        assert_eq!(c.search("Parks", MediaKind::Series, None).unwrap().len(), 2);
    }

    #[test]
    fn every_catalogue_failing_is_reported() {
        struct Broken;
        impl Catalogue for Broken {
            fn name(&self) -> &'static str {
                "broken"
            }
            fn prefix(&self) -> &'static str {
                "broken"
            }
            fn search(&self, _: &str, _: MediaKind, _: Option<u32>) -> Result<Vec<CatalogueHit>> {
                Err(Error("network down".into()))
            }
            fn episodes(&self, _: &str, _: u32) -> Result<Vec<Episode>> {
                Err(Error("down".into()))
            }
        }
        let c = Catalogues(vec![Box::new(Broken)]);
        assert!(c.search("x", MediaKind::Series, None).is_err());
    }
}

#[cfg(test)]
mod detail_tests {
    use super::*;

    /// The real shape of the problem: a search for a presenter's name returns
    /// a dozen programmes, and what separates them is who broadcast them.
    const BEAR: &str = r#"[
      {"score":0.89,"show":{"id":1,"name":"I Survived Bear Grylls","type":"Reality",
        "premiered":"2023-05-18","ended":null,"status":"To Be Determined",
        "network":{"name":"TBS"}}},
      {"score":0.89,"show":{"id":2,"name":"Bear Grylls: Breaking Point","type":"Reality",
        "premiered":"2015-03-02","ended":"2015-04-06","status":"Ended",
        "network":{"name":"Discovery"}}},
      {"score":0.89,"show":{"id":3,"name":"Bear Grylls: Mission Survive","type":"Reality",
        "premiered":"2015-02-20","ended":"2016-04-07","status":"Ended",
        "network":{"name":"ITV1"}}},
      {"score":0.5,"show":{"id":4,"name":"Streamed Thing","type":"Documentary",
        "premiered":"2020-01-01","status":"Running","webChannel":{"name":"Netflix"}}}
    ]"#;

    #[test]
    fn similar_shows_are_told_apart_by_who_broadcast_them() {
        let hits = parse_tvmaze_search(BEAR, None).unwrap();
        let details: Vec<&str> = hits.iter().filter_map(|h| h.detail.as_deref()).collect();
        assert_eq!(details[0], "TBS · Reality");
        assert_eq!(details[1], "Discovery · Reality");
        assert_eq!(details[2], "ITV1 · Reality · 2015-2016");
    }

    #[test]
    fn a_streaming_service_stands_in_for_a_network() {
        // a show is on one or the other, never both
        let hits = parse_tvmaze_search(BEAR, None).unwrap();
        assert_eq!(hits[3].detail.as_deref(), Some("Netflix · Documentary · ongoing"));
    }

    #[test]
    fn a_run_within_one_year_is_left_to_the_title() {
        // the title already shows the premiere year; "2015-2015" adds nothing
        let d = describe_show(
            Some("Discovery"),
            Some("Reality"),
            Some("2015-03-02"),
            Some("2015-04-06"),
            Some("Ended"),
        );
        assert_eq!(d.as_deref(), Some("Discovery · Reality"));
    }

    #[test]
    fn a_show_with_nothing_known_about_it_gets_no_line() {
        assert_eq!(describe_show(None, None, None, None, None), None);
    }

    #[test]
    fn missing_parts_are_dropped_rather_than_left_blank() {
        assert_eq!(
            describe_show(None, Some("Scripted"), Some("2009-04-09"), Some("2015-02-24"), None)
                .as_deref(),
            Some("Scripted · 2009-2015")
        );
        assert_eq!(describe_show(Some("NBC"), None, None, None, None).as_deref(), Some("NBC"));
    }

    #[test]
    fn a_search_result_carries_no_reasons_about_this_disc() {
        // "searched for X" was the same on every row and restated the box the
        // user had just typed into
        let http = FakeHttp::new().on("/search/shows", BEAR);
        let cat = TvMaze { http: &http };
        let found = crate::identify::search(&cat, "Bear Grylls", None).unwrap();
        assert!(found.iter().all(|c| c.reasons.is_empty()));
        assert!(found.iter().all(|c| c.detail.is_some()));
    }
}

/// Wikidata, which needs no key.
///
/// The film gap is real: TVmaze is television only, and TMDB - which is what a
/// media server uses, and the better answer when a key is configured - needs
/// one. Wikidata needs nothing, and for a film it carries everything the naming
/// actually depends on: the title, the year, and the runtime.
///
/// The runtime is the valuable part. It is evidence rather than description: a
/// disc whose longest title runs 117 minutes really is that film, and a name
/// match alone cannot say so. Search here ranks by how well the label matches,
/// not by how well known the work is, so `The Big Lebowski: A XXX Parody` comes
/// back beside the film it parodies and only the runtime tells them apart.
pub struct Wikidata<'a> {
    pub http: &'a dyn Http,
}

const WIKIDATA: &str = "https://www.wikidata.org/w/api.php";

/// `instance of` values that mean "a film".
const FILM_CLASSES: &[&str] = &[
    "Q11424",   // film
    "Q24869",   // feature film
    "Q506240",  // television film
    "Q202866",  // animated film
    "Q226730",  // silent film
    "Q1054574", // romantic comedy film... and other genre subclasses appear too
];

impl Wikidata<'_> {
    pub fn search_url(query: &str) -> String {
        format!(
            "{WIKIDATA}?action=wbsearchentities&search={}&language=en&uselang=en\
             &type=item&format=json&limit=10",
            encode(query)
        )
    }

    pub fn entities_url(ids: &[String]) -> String {
        format!(
            "{WIKIDATA}?action=wbgetentities&ids={}&props=claims|labels|sitelinks\
             &sitefilter=enwiki&languages=en&format=json",
            ids.join("%7C")
        )
    }
}

/// The article images for several Wikipedia titles at once.
///
/// One request however many candidates there are, which is why the titles are
/// collected first rather than asked for as each hit is built.
///
/// `pilicense=any` is the whole point. A film poster is copyrighted, so it is
/// not on Commons and cannot be: Wikipedia hosts it under fair use instead.
/// Left at the default this answers "no image" for every film there is.
pub fn wikipedia_images_url(titles: &[String]) -> String {
    format!(
        "https://en.wikipedia.org/w/api.php?action=query&prop=pageimages\
         &piprop=thumbnail&pithumbsize=342&pilicense=any&titles={}&format=json&redirects=1",
        titles.iter().map(|t| encode(t)).collect::<Vec<_>>().join("%7C")
    )
}

/// Article title to poster, from what that request answered.
pub fn parse_wikipedia_images(json: &str) -> Vec<(String, String)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // A title that redirects is answered under the target's name, so the
    // mapping back has to be followed or the picture belongs to nobody.
    let mut redirects: Vec<(String, String)> = Vec::new();
    if let Some(list) = v["query"]["redirects"].as_array() {
        for r in list {
            if let (Some(from), Some(to)) = (r["from"].as_str(), r["to"].as_str()) {
                redirects.push((from.to_string(), to.to_string()));
            }
        }
    }
    if let Some(pages) = v["query"]["pages"].as_object() {
        for page in pages.values() {
            let (Some(title), Some(src)) =
                (page["title"].as_str(), page["thumbnail"]["source"].as_str())
            else {
                continue;
            };
            // The API appends its own tracking parameters to the URL.
            let url = src.split('?').next().unwrap_or(src).to_string();
            out.push((title.to_string(), url.clone()));
            for (from, to) in &redirects {
                if to == title {
                    out.push((from.clone(), url.clone()));
                }
            }
        }
    }
    out
}

/// Where a file named by a Wikidata claim actually lives.
///
/// Wikidata names a file on Commons rather than linking to it. Special:FilePath
/// resolves the name, and scales the picture on the way rather than handing
/// back a poster scan the size of a wall.
pub fn commons_image_url(file: &str) -> String {
    format!("https://commons.wikimedia.org/wiki/Special:FilePath/{}?width=342", encode(file))
}

/// Candidate ids and their one-line descriptions, from a label search.
pub fn parse_wikidata_search(json: &str) -> Vec<(String, String)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    v.get("search")
        .and_then(|s| s.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|i| {
                    Some((
                        i.get("id")?.as_str()?.to_string(),
                        i.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Turn fetched claims into film hits, dropping anything that is not a film.
///
/// A search for a film's title also returns its soundtrack album, its
/// characters and its novelisation. Checking `instance of` rather than reading
/// the description keeps those out without guessing from prose.
/// The Wikipedia article each entity is about, in the order the hits come out.
///
/// Taken from the item rather than searched for by name, which is the point of
/// it: "Cloudy with a Chance of Meatballs" is a picture book, and the film is
/// filed under "Cloudy with a Chance of Meatballs (film)". The item knows
/// which article is its own; a title does not.
pub fn parse_wikidata_articles(json: &str, order: &[String]) -> Vec<Option<String>> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return vec![None; order.len()];
    };
    order
        .iter()
        .map(|qid| v["entities"][qid]["sitelinks"]["enwiki"]["title"].as_str().map(str::to_string))
        .collect()
}

/// Each entity's logo, for when nothing better can be found.
///
/// Not a poster - it is the film's wordmark on a transparent background - but
/// it is the film's own and it beats a generic icon.
pub fn parse_wikidata_logos(json: &str, order: &[String]) -> Vec<Option<String>> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return vec![None; order.len()];
    };
    order
        .iter()
        .map(|qid| {
            v["entities"][qid]["claims"]["P154"][0]["mainsnak"]["datavalue"]["value"]
                .as_str()
                .map(commons_image_url)
        })
        .collect()
}

pub fn parse_wikidata_entities(
    json: &str,
    order: &[String],
    descriptions: &[(String, String)],
) -> Vec<CatalogueHit> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(entities) = v.get("entities").and_then(|e| e.as_object()) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for (rank, qid) in order.iter().enumerate() {
        let Some(e) = entities.get(qid) else { continue };
        let claims = e.get("claims");
        let ids_of = |property: &str| -> Vec<String> {
            claims
                .and_then(|c| c.get(property))
                .and_then(|c| c.as_array())
                .map(|statements| {
                    statements
                        .iter()
                        .filter_map(|s| {
                            s.get("mainsnak")?
                                .get("datavalue")?
                                .get("value")?
                                .get("id")?
                                .as_str()
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let first = |property: &str, field: &str| -> Option<String> {
            claims?.get(property)?.as_array()?.iter().find_map(|s| {
                s.get("mainsnak")?
                    .get("datavalue")?
                    .get("value")?
                    .get(field)
                    .map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string()))
            })
        };

        // A claim whose value is a bare string rather than an object. P18 is
        // one: it names a file on Commons rather than describing it.
        let plain = |property: &str| -> Option<String> {
            claims?.get(property)?.as_array()?.iter().find_map(|s| {
                s.get("mainsnak")?.get("datavalue")?.get("value")?.as_str().map(str::to_string)
            })
        };

        if !ids_of("P31").iter().any(|c| FILM_CLASSES.contains(&c.as_str())) {
            continue;
        }
        let title = e
            .get("labels")
            .and_then(|l| l.get("en"))
            .and_then(|l| l.get("value"))
            .and_then(|l| l.as_str())
            .unwrap_or_default()
            .to_string();
        if title.is_empty() {
            continue;
        }

        // "+1998-02-26T00:00:00Z"
        let year = first("P577", "time")
            .and_then(|t| t.trim_start_matches('+').get(..4).map(str::to_string))
            .and_then(|y| y.parse::<u32>().ok());
        let runtime = first("P2047", "amount")
            .and_then(|a| a.trim_start_matches('+').parse::<f64>().ok())
            .map(|m| m.round() as u32);

        // Wikidata's own description reads well and already names the director:
        // "1998 film by Joel Coen, Ethan Coen".
        let described = descriptions
            .iter()
            .find(|(id, _)| id == qid)
            .map(|(_, d)| d.clone())
            .filter(|d| !d.is_empty());
        let detail = match (described, runtime) {
            (Some(d), Some(m)) => Some(format!("{d} \u{b7} {m} min")),
            (Some(d), None) => Some(d),
            (None, Some(m)) => Some(format!("{m} min")),
            (None, None) => None,
        };

        out.push(CatalogueHit {
            media: Media::Movie { title, year, provider_id: Some(format!("wikidata:{qid}")) },
            // Ranked by the search's own ordering, which is label similarity.
            // It is not a confidence in the disc; the runtime check is.
            score: 1.0 - (rank as f32 / order.len().max(1) as f32) * 0.5,
            // P3383 is "film poster" and P154 is "logo" - both say what they
            // depict. P18 is "image of the subject" and promises nothing:
            // asking it for Kung Fu Panda answers with a photograph of a
            // Megabus, which is what Wikidata has on the film's item. A
            // picture that is confidently the wrong thing is worse than the
            // kind icon, so only the two that mean something are read.
            // P3383 is "film poster" and P154 is "logo" - both say what they
            // depict. P18 is "image of the subject" and promises nothing:
            // asking it for Kung Fu Panda answers with a photograph of a
            // Megabus, which is what Wikidata has on the film's item.
            //
            // P3383 is almost always empty, though, and for a reason that will
            // not change: a film poster is copyrighted, so it cannot be on
            // Commons and Wikidata cannot name it. Star Wars and The Matrix
            // both have nothing there and a logo in P154, which is why every
            // film came out with its logo. The poster is filled in afterwards
            // from Wikipedia, which hosts it under fair use; the logo is what
            // is left when even that has none.
            poster: plain("P3383").as_deref().map(commons_image_url),
            detail,
        });
    }
    out
}

impl Catalogue for Wikidata<'_> {
    fn name(&self) -> &'static str {
        "Wikidata"
    }

    fn prefix(&self) -> &'static str {
        "wikidata"
    }

    fn search(
        &self,
        query: &str,
        kind: MediaKind,
        _season: Option<u32>,
    ) -> Result<Vec<CatalogueHit>> {
        if kind == MediaKind::Series {
            // Wikidata knows series but rarely their episode lists, and an
            // episode list is the whole reason a series needs a catalogue.
            return Ok(Vec::new());
        }
        let found = parse_wikidata_search(&self.http.get(&Self::search_url(query))?);
        if found.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = found.iter().map(|(id, _)| id.clone()).collect();
        let claims = self.http.get(&Self::entities_url(&ids))?;
        let mut hits = parse_wikidata_entities(&claims, &ids, &found);

        // One request for every candidate's poster, and only when one is
        // missing - which, for films, is nearly always.
        let articles = parse_wikidata_articles(&claims, &ids);
        let logos = parse_wikidata_logos(&claims, &ids);
        let wanted: Vec<String> = hits
            .iter()
            .zip(&articles)
            .filter(|(h, _)| h.poster.is_none())
            .filter_map(|(_, a)| a.clone())
            .collect();
        if !wanted.is_empty()
            && let Ok(body) = self.http.get(&wikipedia_images_url(&wanted))
        {
            let images = parse_wikipedia_images(&body);
            for ((hit, article), logo) in hits.iter_mut().zip(&articles).zip(&logos) {
                if hit.poster.is_some() {
                    continue;
                }
                let from_wikipedia = article
                    .as_ref()
                    .and_then(|a| images.iter().find(|(t, _)| t == a))
                    .map(|(_, url)| url.clone());
                // A logo is not a poster, but it is the film's own mark and
                // beats a generic icon when there is nothing else.
                hit.poster = from_wikipedia.or_else(|| logo.clone());
            }
        }
        Ok(hits)
    }

    fn episodes(&self, _provider_id: &str, _season: u32) -> Result<Vec<Episode>> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod wikidata_tests {
    use super::*;

    /// What a real search for "The Big Lebowski" returns: the film, its
    /// soundtrack, a character, and a parody that is also genuinely a film.
    const SEARCH: &str = r#"{"search":[
      {"id":"Q337078","label":"The Big Lebowski","description":"1998 film by Joel Coen, Ethan Coen"},
      {"id":"Q55716932","label":"Jeffrey Lebowski","description":"fictional character"},
      {"id":"Q16531362","label":"The Big Lebowski","description":"album by various artists"},
      {"id":"Q2409741","label":"The Big Lebowski: A XXX Parody","description":"2010 Porn movie"}
    ]}"#;

    const ENTITIES: &str = r#"{"entities":{
      "Q337078":{"labels":{"en":{"value":"The Big Lebowski"}},"claims":{
        "P31":[{"mainsnak":{"datavalue":{"value":{"id":"Q11424"}}}}],
        "P577":[{"mainsnak":{"datavalue":{"value":{"time":"+1998-02-26T00:00:00Z"}}}}],
        "P3383":[{"mainsnak":{"datavalue":{"value":"Big Lebowski poster.jpg"}}}],
        "P18":[{"mainsnak":{"datavalue":{"value":"A bus, for some reason.jpg"}}}],
        "P2047":[{"mainsnak":{"datavalue":{"value":{"amount":"+117"}}}}]}},
      "Q55716932":{"labels":{"en":{"value":"Jeffrey Lebowski"}},"claims":{
        "P31":[{"mainsnak":{"datavalue":{"value":{"id":"Q15632617"}}}}]}},
      "Q16531362":{"labels":{"en":{"value":"The Big Lebowski"}},"claims":{
        "P31":[{"mainsnak":{"datavalue":{"value":{"id":"Q482994"}}}}],
        "P2047":[{"mainsnak":{"datavalue":{"value":{"amount":"+3111"}}}}]}},
      "Q2409741":{"labels":{"en":{"value":"The Big Lebowski: A XXX Parody"}},"claims":{
        "P31":[{"mainsnak":{"datavalue":{"value":{"id":"Q11424"}}}}],
        "P577":[{"mainsnak":{"datavalue":{"value":{"time":"+2010-01-01T00:00:00Z"}}}}],
        "P2047":[{"mainsnak":{"datavalue":{"value":{"amount":"+155"}}}}]}}
    }}"#;

    fn hits() -> Vec<CatalogueHit> {
        let found = parse_wikidata_search(SEARCH);
        let ids: Vec<String> = found.iter().map(|(i, _)| i.clone()).collect();
        parse_wikidata_entities(ENTITIES, &ids, &found)
    }

    #[test]
    fn a_wikidata_film_carries_its_picture_without_another_request() {
        // The entities call already asks for every candidate's claims, and
        // P18 is among them - so this costs nothing, which is not what I said
        // when I first left it out.
        let h = hits();
        let big = h.iter().find(|h| h.media.title() == "The Big Lebowski").expect("it is there");
        let url = big.poster.as_deref().expect("P18 was in the claims");
        assert!(url.starts_with("https://commons.wikimedia.org/wiki/Special:FilePath/"), "{url}");
        assert!(url.contains("Big%20Lebowski%20poster.jpg"), "{url}");
        assert!(url.ends_with("?width=342"), "the full scan is a poster the size of a wall");
        assert!(!url.contains("bus"), "P18 promises nothing about what it depicts");
    }

    /// Entities with no poster claim, an article each, and a logo on one.
    const NO_POSTER: &str = r#"{"entities":{
      "Q337078":{"claims":{
        "P31":[{"mainsnak":{"datavalue":{"value":{"id":"Q11424"}}}}],
        "P577":[{"mainsnak":{"datavalue":{"value":{"time":"+1998-03-06T00:00:00Z"}}}}],
        "P154":[{"mainsnak":{"datavalue":{"value":"Lebowski logo.svg"}}}]},
        "sitelinks":{"enwiki":{"title":"The Big Lebowski"}},
        "labels":{"en":{"value":"The Big Lebowski"}}},
      "Q2409741":{"claims":{
        "P31":[{"mainsnak":{"datavalue":{"value":{"id":"Q11424"}}}}],
        "P577":[{"mainsnak":{"datavalue":{"value":{"time":"+2001-01-01T00:00:00Z"}}}}]},
        "sitelinks":{"enwiki":{"title":"The Big Lebowski: A XXX Parody"}},
        "labels":{"en":{"value":"The Big Lebowski: A XXX Parody"}}}}}"#;

    const IMAGES: &str = r#"{"query":{"pages":{"1":{"title":"The Big Lebowski",
      "thumbnail":{"source":"https://upload.wikimedia.org/wikipedia/en/3/35/Biglebowskiposter.jpg?utm_source=x"}}}}}"#;

    #[test]
    fn a_poster_comes_from_wikipedia_when_wikidata_has_none() {
        // P3383 is empty for practically every film - a poster is copyrighted,
        // so Commons cannot hold it and Wikidata cannot name it. Wikipedia
        // hosts it under fair use, which is why every film used to come back
        // wearing its logo.
        let http = FakeHttp::new()
            .on("wbsearchentities", SEARCH)
            .on("wbgetentities", NO_POSTER)
            .on("prop=pageimages", IMAGES);
        let w = Wikidata { http: &http };
        let found = w.search("The Big Lebowski", MediaKind::Movie, None).unwrap();
        let big = found.iter().find(|h| h.media.title() == "The Big Lebowski").expect("there");
        let url = big.poster.as_deref().expect("a poster");
        assert!(url.contains("Biglebowskiposter"), "{url}");
        assert!(!url.contains("utm_"), "the API's own tracking is not part of the picture: {url}");
    }

    #[test]
    fn the_article_is_asked_for_with_the_licence_a_poster_needs() {
        // Left at the default, the images request answers "no image" for every
        // film there is, because a poster is never freely licensed.
        let http = FakeHttp::new()
            .on("wbsearchentities", SEARCH)
            .on("wbgetentities", NO_POSTER)
            .on("prop=pageimages", IMAGES);
        Wikidata { http: &http }.search("The Big Lebowski", MediaKind::Movie, None).unwrap();
        let asked = http.requested();
        let images = asked.iter().find(|u| u.contains("pageimages")).expect("it asked");
        assert!(images.contains("pilicense=any"), "{images}");
    }

    #[test]
    fn a_logo_stands_in_when_even_wikipedia_has_no_picture() {
        // Not a poster - a wordmark on a transparent background - but it is
        // the film's own and it beats a generic icon.
        let http = FakeHttp::new()
            .on("wbsearchentities", SEARCH)
            .on("wbgetentities", NO_POSTER)
            .on("prop=pageimages", r#"{"query":{"pages":{}}}"#);
        let w = Wikidata { http: &http };
        let found = w.search("The Big Lebowski", MediaKind::Movie, None).unwrap();
        let big = found.iter().find(|h| h.media.title() == "The Big Lebowski").expect("there");
        assert!(big.poster.as_deref().is_some_and(|u| u.contains("logo")), "{:?}", big.poster);
    }

    #[test]
    fn a_film_with_no_picture_anywhere_still_says_nothing_rather_than_guessing() {
        let http = FakeHttp::new()
            .on("wbsearchentities", SEARCH)
            .on("wbgetentities", NO_POSTER)
            .on("prop=pageimages", r#"{"query":{"pages":{}}}"#);
        let w = Wikidata { http: &http };
        let found = w.search("The Big Lebowski", MediaKind::Movie, None).unwrap();
        let parody = found.iter().find(|h| h.media.title().contains("Parody")).expect("there");
        assert_eq!(parody.poster, None);
    }

    #[test]
    fn the_urls_are_built_without_a_space_in_them() {
        // These are written across two lines with a continuation, and a
        // continuation that loses its backslash puts a space and an indent
        // into the middle of a query string.
        let e = Wikidata::entities_url(&["Q337078".into()]);
        assert!(!e.contains(' '), "{e}");
        assert!(e.contains("props=claims|labels|sitelinks"), "{e}");
        assert!(e.contains("sitefilter=enwiki"), "{e}");

        let i = wikipedia_images_url(&["The Big Lebowski".into(), "Blade Runner".into()]);
        assert!(!i.contains(' '), "{i}");
        assert!(i.contains("pilicense=any") && i.contains("redirects=1"), "{i}");
        // one request for all of them, not one each
        assert_eq!(i.matches("titles=").count(), 1, "{i}");
        assert!(i.contains("%7C"), "the titles are joined into one request: {i}");
    }

    #[test]
    fn the_article_is_taken_from_the_item_and_not_from_the_title() {
        // "Cloudy with a Chance of Meatballs" is a picture book; the film is
        // filed under "(film)". Searching Wikipedia by name gets the book.
        let articles = parse_wikidata_articles(NO_POSTER, &["Q337078".into(), "Q2409741".into()]);
        assert_eq!(articles[0].as_deref(), Some("The Big Lebowski"));
    }

    #[test]
    fn a_film_wikidata_has_no_picture_for_says_nothing_rather_than_guessing() {
        let h = hits();
        let parody =
            h.iter().find(|h| h.media.title().contains("XXX Parody")).expect("it is there");
        assert_eq!(parody.poster, None);
    }

    #[test]
    fn a_film_comes_back_with_its_year() {
        let h = hits();
        assert_eq!(h[0].media.title(), "The Big Lebowski");
        assert_eq!(h[0].media.year(), Some(1998));
        assert_eq!(h[0].media.provider_id().as_deref(), Some("wikidata:Q337078"));
    }

    #[test]
    fn things_that_are_not_films_are_dropped() {
        // a search for a film's title also finds its soundtrack and its
        // characters; checking `instance of` keeps them out without having to
        // read the description as prose
        let found = hits();
        let titles: Vec<&str> = found.iter().map(|h| h.media.title()).collect();
        assert!(!titles.contains(&"Jeffrey Lebowski"), "{titles:?}");
        assert_eq!(
            titles.iter().filter(|t| **t == "The Big Lebowski").count(),
            1,
            "the soundtrack album survived: {titles:?}"
        );
    }

    #[test]
    fn the_runtime_is_carried_because_it_is_evidence() {
        // Search ranks by label similarity, not fame, so a parody sits beside
        // the film it parodies. 117 minutes against 155 is what separates them,
        // and only the disc can say which is right.
        let h = hits();
        assert!(h[0].detail.as_deref().unwrap().contains("117 min"), "{:?}", h[0].detail);
        let parody = h.iter().find(|x| x.media.title().contains("XXX")).unwrap();
        assert!(parody.detail.as_deref().unwrap().contains("155 min"));
    }

    #[test]
    fn the_description_names_the_director() {
        assert!(hits()[0].detail.as_deref().unwrap().starts_with("1998 film by Joel Coen"));
    }

    #[test]
    fn a_series_is_declined_rather_than_guessed_at() {
        // Wikidata knows series but rarely their episode lists, and an episode
        // list is the whole reason a series needs a catalogue at all.
        let http = FakeHttp::new();
        let w = Wikidata { http: &http };
        assert!(w.search("Parks and Recreation", MediaKind::Series, Some(7)).unwrap().is_empty());
        assert!(http.requested().is_empty(), "it should not have asked");
    }

    #[test]
    fn a_film_search_makes_exactly_two_requests() {
        let http = FakeHttp::new().on("wbsearchentities", SEARCH).on("wbgetentities", ENTITIES);
        let w = Wikidata { http: &http };
        let found = w.search("The Big Lebowski", MediaKind::Movie, None).unwrap();
        assert_eq!(found.len(), 2, "the film and the parody, nothing else");
        assert_eq!(http.requested().len(), 2);
    }

    #[test]
    fn nothing_found_makes_no_second_request() {
        let http = FakeHttp::new().on("wbsearchentities", r#"{"search":[]}"#);
        let w = Wikidata { http: &http };
        assert!(w.search("zzzz", MediaKind::Movie, None).unwrap().is_empty());
        assert_eq!(http.requested().len(), 1);
    }
}
