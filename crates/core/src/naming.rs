//! Turning what a title *is* into where it goes.
//!
//! Names are built for the strictest filesystem in the chain, not the local
//! one. These files end up on a NAS and are read over SMB, where a colon is
//! not a legal character - so `Ron & Tammy: Part Two` written happily on ext4
//! becomes a file Windows clients cannot open, and the failure surfaces days
//! later somewhere else entirely. Sanitising happens here, once, for everyone.

use crate::model::{Container, Item, Media, Role, Tags};
use std::path::{Path, PathBuf};

/// How episode filenames are built, when nothing else is said.
pub const DEFAULT_EPISODE_TEMPLATE: &str = "{show} - S{season}E{episode} - {title}";

/// The fields a template can use.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Fields {
    pub show: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub title: String,
    pub year: Option<u32>,
    pub date: Option<String>,
}

/// Every token, with a line about it, for showing beside the field.
pub const TOKENS: &[(&str, &str)] = &[
    ("{show}", "the programme's name"),
    ("{season}", "season number, two digits"),
    ("{episode}", "episode number, two digits"),
    ("{title}", "this episode's title"),
    ("{year}", "the year it first aired"),
    ("{date}", "the date it first aired"),
];

/// Something a template can be filled from.
///
/// Two kinds of thing get named here and they share nothing but the syntax: an
/// episode knows about seasons, a track knows about discs, and neither has any
/// use for the other's words. The renderer holds the syntax and each source
/// answers only for its own tokens.
pub trait Tokens {
    /// The value for one token, padded where padding applies. `None` means
    /// this source has no such token, and it is left standing in the output.
    fn value(&self, name: &str, width: usize) -> Option<String>;
}

impl Tokens for Fields {
    fn value(&self, name: &str, width: usize) -> Option<String> {
        let number = |v: Option<u32>| v.map(|n| format!("{n:0width$}")).unwrap_or_default();
        Some(match name {
            "show" => self.show.clone(),
            "title" => self.title.clone(),
            "season" => number(self.season),
            "episode" => number(self.episode),
            "year" => self.year.map(|y| y.to_string()).unwrap_or_default(),
            "date" => self.date.clone().unwrap_or_default(),
            _ => return None,
        })
    }
}

/// Fill a template.
///
/// Numbers are padded to two digits, which is what every media server expects
/// and what the whole library already uses; `{season:3}` asks for more. An
/// unknown token is left alone rather than silently dropped, so a typo shows up
/// in the preview as itself instead of as a gap.
pub fn render(template: &str, f: &dyn Tokens) -> String {
    let mut out = String::with_capacity(template.len() + 32);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            // an unclosed brace is a typo, not an instruction
            out.push_str(&rest[open..]);
            return out;
        };
        let token = &rest[open + 1..open + close];
        rest = &rest[open + close + 1..];

        let (name, width) = match token.split_once(':') {
            Some((n, w)) => (n, w.parse::<usize>().unwrap_or(2)),
            None => (token, 2),
        };
        match f.value(name, width) {
            Some(value) => out.push_str(&value),
            None => {
                out.push('{');
                out.push_str(token);
                out.push('}');
            }
        }
    }
    out.push_str(rest);
    out
}

/// What a template produces for a made-up episode, for showing as you type.
pub fn preview(template: &str, container: Container) -> String {
    let f = Fields {
        show: "Parks and Recreation".into(),
        season: Some(6),
        episode: Some(4),
        title: "Doppelgangers".into(),
        year: Some(2013),
        date: Some("2013-10-17".into()),
    };
    format!("{}.{}", sanitize(&render(template, &f)), container.extension())
}

/// How track filenames are built, when nothing else is said.
///
/// Deliberately bare: the artist and album are in the tags, and every music
/// server reads those rather than the name. Anyone who wants them in the name
/// has `{artist}` and `{album}` to say so.
pub const DEFAULT_TRACK_TEMPLATE: &str = "{track} - {title}";

/// The fields a music template can use.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MusicFields {
    pub albumartist: String,
    /// Who performed this track, which on a compilation is not the album's
    /// artist.
    pub artist: String,
    pub album: String,
    pub title: String,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub year: Option<u32>,
    pub date: Option<String>,
}

impl Tokens for MusicFields {
    fn value(&self, name: &str, width: usize) -> Option<String> {
        let number = |v: Option<u32>| v.map(|n| format!("{n:0width$}")).unwrap_or_default();
        Some(match name {
            "title" => self.title.clone(),
            "artist" => self.artist.clone(),
            "albumartist" => self.albumartist.clone(),
            "album" => self.album.clone(),
            "track" => number(self.track),
            "disc" => number(self.disc),
            "year" => self.year.map(|y| y.to_string()).unwrap_or_default(),
            "date" => self.date.clone().unwrap_or_default(),
            _ => return None,
        })
    }
}

/// Every music token, with a line about it, for showing beside the field.
pub const MUSIC_TOKENS: &[(&str, &str)] = &[
    ("{track}", "track number, two digits"),
    ("{title}", "this track's title"),
    ("{artist}", "who performed this track"),
    ("{albumartist}", "who the album is credited to"),
    ("{album}", "the album's title"),
    ("{disc}", "disc number, in a set"),
    ("{year}", "the year it was released"),
    ("{date}", "the date it was released"),
];

/// What a music template produces for a made-up track, for showing as you type.
pub fn music_preview(template: &str, extension: &str) -> String {
    let f = MusicFields {
        albumartist: "Shawn McDonald".into(),
        artist: "Shawn McDonald".into(),
        album: "Roots".into(),
        title: "Slow Down".into(),
        track: Some(8),
        disc: Some(1),
        year: Some(2008),
        date: Some("2008-03-11".into()),
    };
    // Shown as the path it will make, slashes and all, so a template that
    // lays out folders can be seen doing it before a disc is ripped with it.
    if template.contains('/') {
        let mut parts = render_path(template, &f);
        if let Some(name) = parts.pop() {
            parts.push(sanitize(&format!("{name}.{extension}")));
            return parts.join("/");
        }
    }
    format!("{}.{extension}", sanitize(&render(template, &f)))
}

/// Characters Windows and SMB reject outright.
const ILLEGAL: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];

/// Names DOS reserved, still refused by Windows with any extension.
const RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Make one path component safe everywhere.
///
/// A colon becomes ` - ` rather than being dropped, because it is nearly always
/// separating a title from a subtitle and the dash preserves that reading.
/// Fill a template that may name directories as well as a file.
///
/// Splitting happens *before* the fields are filled in, which is the whole
/// safety of it: a slash in a value - AC/DC, He/She - cannot then invent a
/// directory, because by the time the value arrives its segment has already
/// been decided and the slash is sanitised into the name like any other
/// character a filesystem will not take.
///
/// Segments that come out empty are dropped, as are `.` and `..`: a template
/// that renders to nothing for a field somebody left blank should shorten the
/// path, not put a nameless directory in it or climb out of the library.
pub fn render_path(template: &str, f: &dyn Tokens) -> Vec<String> {
    template
        .split('/')
        .filter_map(|segment| {
            // Emptiness is decided before sanitising, not after: sanitize()
            // answers "untitled" for a name it cannot make anything of, which
            // is right for a file and wrong for a folder - a template with
            // {year} in it and a release with no date would otherwise file the
            // record under a directory called "untitled".
            let filled = render(segment, f);
            if filled.trim().is_empty() {
                return None;
            }
            let clean = sanitize(&filled);
            (clean != "." && clean != "..").then_some(clean)
        })
        .collect()
}

pub fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            ':' => out.push_str(" - "),
            c if ILLEGAL.contains(&c) => out.push('-'),
            // Control characters are illegal in SMB share names too
            c if (c as u32) < 0x20 => out.push(' '),
            c => out.push(c),
        }
    }
    // collapse the runs the substitutions above can produce
    let mut collapsed = String::with_capacity(out.len());
    let mut prev_space = false;
    for ch in out.chars() {
        let is_space = ch == ' ';
        if !(is_space && prev_space) {
            collapsed.push(ch);
        }
        prev_space = is_space;
    }
    // Windows silently strips trailing dots and spaces, so a name ending in one
    // does not round trip - two files can collide after the strip.
    let trimmed = collapsed.trim().trim_end_matches('.').trim_end().to_string();

    let stem = trimmed.split('.').next().unwrap_or("").to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        return format!("_{trimmed}");
    }
    // A name made only of substituted punctuation reads as "---", which is
    // not a name; it also collides with every other such title.
    if !trimmed.chars().any(char::is_alphanumeric) {
        return "untitled".into();
    }
    trimmed
}

/// `S02E07`, or `S02E07-E08` for a double episode.
pub fn episode_code(season: u32, number: u32) -> String {
    format!("S{season:02}E{number:02}")
}

/// The filename for an item, without a directory.
pub fn file_name(
    media: &Media,
    item: &Item,
    container: Container,
    template: Option<&str>,
) -> String {
    let ext = container.extension();
    let episode_fields = |season: &u32, number: &u32, show: &str, year: &Option<u32>| Fields {
        show: show.to_string(),
        season: Some(*season),
        episode: Some(*number),
        title: item.title.clone(),
        year: *year,
        date: item.air_date.clone(),
    };
    let stem = match (&item.role, media) {
        (Role::Episode { season, number }, Media::Series { title, year, .. }) => render(
            template.unwrap_or(DEFAULT_EPISODE_TEMPLATE),
            &episode_fields(season, number, title, year),
        ),
        (Role::ExtendedCut { season, number }, Media::Series { title, year, .. }) => format!(
            "{} - Extended Cut",
            render(
                template.unwrap_or(DEFAULT_EPISODE_TEMPLATE),
                &episode_fields(season, number, title, year)
            )
        ),
        (Role::Feature, Media::Movie { title, year, .. }) => match year {
            Some(y) => format!("{title} ({y})"),
            None => title.clone(),
        },
        // An extra carries only its own name; prefixing it with the show would
        // make a media server try to parse it as an episode.
        (Role::Extra, _) | (Role::Feature, _) => item.title.clone(),
        (Role::PlayAll, _) => format!("{} (play-all)", item.title),
        (Role::Episode { season, number }, _) | (Role::ExtendedCut { season, number }, _) => {
            format!("{} - {}", episode_code(*season, *number), item.title)
        }
    };
    format!("{}.{ext}", sanitize(&stem))
}

/// Where an item goes under `root`.
///
/// Series get a `Season NN` directory because that is what Jellyfin, Plex and
/// Emby all expect; getting it wrong makes a season show up as loose files.
pub fn destination(
    root: &Path,
    media: &Media,
    item: &Item,
    container: Container,
    template: Option<&str>,
) -> PathBuf {
    let mut p = root.to_path_buf();
    if let Media::Series { season, .. } = media {
        let season = match &item.role {
            Role::Episode { season, .. } | Role::ExtendedCut { season, .. } => *season,
            _ => *season,
        };
        p.push(sanitize(&format!("Season {season:02}")));
    }
    if let Some(sub) = item.role.subdirectory() {
        p.push(sub);
    }
    p.push(file_name(media, item, container, template));
    p
}

/// The metadata to write into the file.
pub fn tags(media: &Media, item: &Item) -> Tags {
    match (&item.role, media) {
        (Role::Episode { season, number }, Media::Series { title, .. })
        | (Role::ExtendedCut { season, number }, Media::Series { title, .. }) => {
            let display = if matches!(item.role, Role::ExtendedCut { .. }) {
                format!("{} (Extended Cut)", item.title)
            } else {
                item.title.clone()
            };
            Tags {
                title: Some(display.clone()),
                show: Some(title.clone()),
                season_number: Some(*season),
                episode_sort: Some(*number),
                episode_id: Some(display),
                date: item.air_date.clone(),
                // 10 = TV show. Without it an MP4 episode can be filed as a
                // movie no matter how well the filename is formed.
                media_type: Some(10),
            }
        }
        (_, Media::Movie { title, year, .. }) => Tags {
            title: Some(title.clone()),
            date: year.map(|y| y.to_string()),
            media_type: Some(9),
            ..Tags::default()
        },
        _ => Tags {
            title: Some(item.title.clone()),
            show: Some(media.title().to_string()),
            ..Tags::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn series() -> Media {
        Media::Series {
            title: "Parks and Recreation".into(),
            year: Some(2009),
            season: 3,
            provider_id: None,
        }
    }

    fn item(role: Role, title: &str) -> Item {
        Item {
            source: PathBuf::from("/rip/title_t00.mkv"),
            role,
            title: title.into(),
            air_date: Some("2011-02-03".into()),
            duration: 1_274_933,
            destination: None,
        }
    }

    #[test]
    fn a_colon_becomes_a_dash_because_smb_rejects_it() {
        // this exact episode is why: it was unopenable from Windows on the NAS
        let i = item(Role::Episode { season: 3, number: 4 }, "Ron & Tammy: Part Two");
        assert_eq!(
            file_name(&series(), &i, Container::Mp4, None),
            "Parks and Recreation - S03E04 - Ron & Tammy - Part Two.mp4"
        );
    }

    #[test]
    fn every_illegal_character_is_replaced() {
        assert_eq!(sanitize(r#"a<b>c"d/e\f|g?h*i"#), "a-b-c-d-e-f-g-h-i");
    }

    #[test]
    fn trailing_dots_and_spaces_go_because_windows_strips_them() {
        // otherwise "Episode." and "Episode" become the same file after copying
        assert_eq!(sanitize("Episode. "), "Episode");
        assert_eq!(sanitize("Episode..."), "Episode");
    }

    #[test]
    fn reserved_dos_names_are_escaped() {
        assert_eq!(sanitize("CON"), "_CON");
        assert_eq!(sanitize("nul"), "_nul");
        assert_eq!(sanitize("Constantine"), "Constantine");
    }

    #[test]
    fn substitutions_do_not_leave_double_spaces() {
        assert_eq!(sanitize("Ron & Tammy : Part Two"), "Ron & Tammy - Part Two");
    }

    #[test]
    fn an_empty_name_still_produces_a_file() {
        assert_eq!(sanitize("???"), "untitled");
    }

    #[test]
    fn episodes_land_in_a_season_directory() {
        let i = item(Role::Episode { season: 3, number: 4 }, "Ron and Tammy");
        assert_eq!(
            destination(Path::new("/media"), &series(), &i, Container::Mp4, None),
            PathBuf::from("/media/Season 03/Parks and Recreation - S03E04 - Ron and Tammy.mp4")
        );
    }

    #[test]
    fn extended_cuts_land_in_extras_beside_the_season() {
        let i = item(Role::ExtendedCut { season: 3, number: 4 }, "Ron and Tammy");
        assert_eq!(
            destination(Path::new("/media"), &series(), &i, Container::Mkv, None),
            PathBuf::from(
                "/media/Season 03/extras/Parks and Recreation - S03E04 - Ron and Tammy - Extended Cut.mkv"
            )
        );
    }

    #[test]
    fn an_extra_is_not_named_like_an_episode() {
        // a media server parses "Show - S03E04" out of a filename; an extra
        // that looked like one would be filed as a duplicate episode
        let i = item(Role::Extra, "Deleted Scenes");
        let name = file_name(&series(), &i, Container::Mp4, None);
        assert_eq!(name, "Deleted Scenes.mp4");
        assert!(!name.contains("S03"));
    }

    #[test]
    fn movies_are_named_with_their_year_and_no_season_directory() {
        let m =
            Media::Movie { title: "The Big Lebowski".into(), year: Some(1998), provider_id: None };
        let i = item(Role::Feature, "The Big Lebowski");
        assert_eq!(
            destination(Path::new("/media"), &m, &i, Container::Mp4, None),
            PathBuf::from("/media/The Big Lebowski (1998).mp4")
        );
    }

    #[test]
    fn episode_tags_mark_the_file_as_television() {
        let t = tags(&series(), &item(Role::Episode { season: 3, number: 4 }, "Ron"));
        assert_eq!(t.media_type, Some(10));
        assert_eq!(t.show.as_deref(), Some("Parks and Recreation"));
        assert_eq!(t.season_number, Some(3));
        assert_eq!(t.episode_sort, Some(4));
        assert_eq!(t.date.as_deref(), Some("2011-02-03"));
    }

    #[test]
    fn an_extended_cut_says_so_in_its_title_tag() {
        let t = tags(&series(), &item(Role::ExtendedCut { season: 3, number: 4 }, "Ron"));
        assert_eq!(t.title.as_deref(), Some("Ron (Extended Cut)"));
    }
}

#[cfg(test)]
mod template_tests {
    use super::*;

    fn fields() -> Fields {
        Fields {
            show: "Parks and Recreation".into(),
            season: Some(6),
            episode: Some(4),
            title: "Doppelgangers".into(),
            year: Some(2013),
            date: Some("2013-10-17".into()),
        }
    }

    #[test]
    fn the_default_produces_what_the_library_already_uses() {
        assert_eq!(
            render(DEFAULT_EPISODE_TEMPLATE, &fields()),
            "Parks and Recreation - S06E04 - Doppelgangers"
        );
    }

    #[test]
    fn numbers_are_padded_to_two_digits() {
        // every media server expects it, and the existing library uses it
        assert_eq!(render("S{season}E{episode}", &fields()), "S06E04");
    }

    #[test]
    fn a_wider_field_can_be_asked_for() {
        assert_eq!(render("{episode:3}", &fields()), "004");
    }

    #[test]
    fn every_token_resolves() {
        for (token, _) in TOKENS {
            let out = render(token, &fields());
            assert_ne!(out, *token, "{token} was not recognised");
            assert!(!out.is_empty(), "{token} produced nothing");
        }
    }

    #[test]
    fn an_unknown_token_is_left_alone_rather_than_dropped() {
        // a typo should be visible in the preview as itself, not as a gap the
        // user has to work out the cause of
        assert_eq!(render("{shwo} - {title}", &fields()), "{shwo} - Doppelgangers");
    }

    #[test]
    fn an_unclosed_brace_is_a_typo_not_an_instruction() {
        assert_eq!(render("{show} - {tit", &fields()), "Parks and Recreation - {tit");
    }

    #[test]
    fn a_missing_value_leaves_a_gap_rather_than_the_word_none() {
        let sparse = Fields { show: "Thing".into(), ..Fields::default() };
        assert_eq!(render("{show} {year}", &sparse), "Thing ");
    }

    #[test]
    fn the_preview_is_sanitised_like_a_real_name() {
        // what it shows must be what would actually be written
        let p = preview("{show}: {title}", Container::Mp4);
        assert!(!p.contains(':'), "{p}");
        assert!(p.ends_with(".mp4"));
    }

    #[test]
    fn a_custom_template_reaches_the_filename() {
        let media = Media::Series {
            title: "Parks and Recreation".into(),
            year: Some(2009),
            season: 6,
            provider_id: None,
        };
        let item = Item {
            source: PathBuf::from("/rip/a.mkv"),
            role: Role::Episode { season: 6, number: 4 },
            title: "Doppelgangers".into(),
            air_date: Some("2013-10-17".into()),
            duration: 0,
            destination: None,
        };
        assert_eq!(
            file_name(&media, &item, Container::Mkv, Some("{season}x{episode} {title}")),
            "06x04 Doppelgangers.mkv"
        );
    }

    #[test]
    fn an_extended_cut_still_says_so_whatever_the_template() {
        let media = Media::Series {
            title: "Parks and Recreation".into(),
            year: Some(2009),
            season: 6,
            provider_id: None,
        };
        let item = Item {
            source: PathBuf::from("/rip/a.mkv"),
            role: Role::ExtendedCut { season: 6, number: 4 },
            title: "Doppelgangers".into(),
            air_date: None,
            duration: 0,
            destination: None,
        };
        let name = file_name(&media, &item, Container::Mp4, Some("{title}"));
        assert_eq!(name, "Doppelgangers - Extended Cut.mp4");
    }

    #[test]
    fn a_music_preview_shows_the_folders_a_slash_makes() {
        assert_eq!(
            music_preview("{artist}/{album}/{track} - {title}", "flac"),
            "Shawn McDonald/Roots/08 - Slow Down.flac"
        );
    }

    #[test]
    fn a_preview_without_slashes_is_still_just_a_filename() {
        assert_eq!(music_preview("{track} - {title}", "flac"), "08 - Slow Down.flac");
    }

    #[test]
    fn a_path_template_is_split_before_it_is_filled_in() {
        // Otherwise a value containing a slash invents a directory, and a
        // value containing ../ climbs out of the library entirely.
        struct Nasty;
        impl Tokens for Nasty {
            fn value(&self, token: &str, _width: usize) -> Option<String> {
                match token {
                    "artist" => Some("AC/DC".into()),
                    "title" => Some("../../etc/passwd".into()),
                    _ => None,
                }
            }
        }
        assert_eq!(render_path("{artist}/{title}", &Nasty), vec!["AC-DC", "..-..-etc-passwd"]);
    }

    #[test]
    fn a_segment_that_renders_to_nothing_is_dropped_not_named_untitled() {
        // A known field with nothing in it - a release with no date - renders
        // empty. sanitize() would call that "untitled", which is right for a
        // file and wrong for a folder, so the segment goes instead.
        let f = MusicFields {
            albumartist: "Shawn McDonald".into(),
            artist: "Shawn McDonald".into(),
            album: "Roots".into(),
            title: "Clarity".into(),
            track: Some(1),
            disc: None,
            year: None,
            date: None,
        };
        assert_eq!(render_path("{year}/{album}/{title}", &f), vec!["Roots", "Clarity"]);
    }

    #[test]
    fn a_token_nobody_recognises_is_left_standing_even_in_a_folder_name() {
        // Same rule as everywhere else: a typo should be visible in the output
        // rather than silently swallowed into a shorter path.
        let f = MusicFields {
            albumartist: "A".into(),
            artist: "A".into(),
            album: "B".into(),
            title: "C".into(),
            track: Some(1),
            disc: None,
            year: None,
            date: None,
        };
        assert_eq!(render_path("{albm}/{title}", &f), vec!["{albm}", "C"]);
    }
}
