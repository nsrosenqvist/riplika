//! Turning what a title *is* into where it goes.
//!
//! Names are built for the strictest filesystem in the chain, not the local
//! one. These files end up on a NAS and are read over SMB, where a colon is
//! not a legal character - so `Ron & Tammy: Part Two` written happily on ext4
//! becomes a file Windows clients cannot open, and the failure surfaces days
//! later somewhere else entirely. Sanitising happens here, once, for everyone.

use crate::model::{Container, Item, Media, Role, Tags};
use std::path::{Path, PathBuf};

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
pub fn file_name(media: &Media, item: &Item, container: Container) -> String {
    let ext = container.extension();
    let stem = match (&item.role, media) {
        (Role::Episode { season, number }, Media::Series { title, .. }) => {
            format!("{title} - {} - {}", episode_code(*season, *number), item.title)
        }
        (Role::ExtendedCut { season, number }, Media::Series { title, .. }) => format!(
            "{title} - {} - {} - Extended Cut",
            episode_code(*season, *number),
            item.title
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
pub fn destination(root: &Path, media: &Media, item: &Item, container: Container) -> PathBuf {
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
    p.push(file_name(media, item, container));
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
            file_name(&series(), &i, Container::Mp4),
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
            destination(Path::new("/media"), &series(), &i, Container::Mp4),
            PathBuf::from(
                "/media/Season 03/Parks and Recreation - S03E04 - Ron and Tammy.mp4"
            )
        );
    }

    #[test]
    fn extended_cuts_land_in_extras_beside_the_season() {
        let i = item(Role::ExtendedCut { season: 3, number: 4 }, "Ron and Tammy");
        assert_eq!(
            destination(Path::new("/media"), &series(), &i, Container::Mkv),
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
        let name = file_name(&series(), &i, Container::Mp4);
        assert_eq!(name, "Deleted Scenes.mp4");
        assert!(!name.contains("S03"));
    }

    #[test]
    fn movies_are_named_with_their_year_and_no_season_directory() {
        let m = Media::Movie {
            title: "The Big Lebowski".into(),
            year: Some(1998),
            provider_id: None,
        };
        let i = item(Role::Feature, "The Big Lebowski");
        assert_eq!(
            destination(Path::new("/media"), &m, &i, Container::Mp4),
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
