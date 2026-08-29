//! Reading a DVD's volume label.
//!
//! `PARKS_AND_RECREATION_S7D1` is the single most informative thing on a disc,
//! and it is free - it comes back from a scan without reading any video. It is
//! also unreliable enough that it can only ever be a hypothesis: labels are
//! capped at 32 characters and get truncated, punctuation is stripped, and
//! authoring houses use their own conventions. So this parses out a *guess* to
//! search a catalogue with, and the catalogue decides.

/// What a volume label appears to say.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LabelGuess {
    /// Title with separators turned back into spaces.
    pub title: String,
    pub season: Option<u32>,
    pub disc: Option<u32>,
}

/// Trailing tokens that describe the disc rather than the work.
const NOISE: &[&str] = &[
    "DVD", "NTSC", "PAL", "R1", "R2", "R4", "WS", "FS", "SE", "CE", "VOL", "VOLUME", "SEASON",
    "SERIES", "DISC", "DISK", "SIDE", "BONUS", "EXTRA", "EXTRAS", "SET",
];

fn split_tokens(label: &str) -> Vec<String> {
    label
        .split(['_', '-', '.', ' ', '+'])
        .filter(|t| !t.is_empty())
        .map(|t| t.to_ascii_uppercase())
        .collect()
}

/// Pull `S7D1`, `S07`, `D2` out of a token. Returns (season, disc).
fn parse_combined(token: &str) -> (Option<u32>, Option<u32>) {
    let mut season = None;
    let mut disc = None;
    let bytes: Vec<char> = token.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let marker = bytes[i];
        if marker != 'S' && marker != 'D' {
            return (None, None); // not a season/disc token at all
        }
        let mut j = i + 1;
        let mut n = String::new();
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            n.push(bytes[j]);
            j += 1;
        }
        if n.is_empty() {
            return (None, None);
        }
        let v = n.parse::<u32>().ok();
        if marker == 'S' {
            season = v;
        } else {
            disc = v;
        }
        i = j;
    }
    (season, disc)
}

/// Interpret a volume label.
pub fn parse(label: &str) -> LabelGuess {
    let tokens = split_tokens(label);
    let mut guess = LabelGuess::default();
    let mut title_tokens: Vec<String> = Vec::new();
    let mut i = 0;

    while i < tokens.len() {
        let t = &tokens[i];

        // "SEASON 3" / "DISC 2" as two tokens
        let next_number = tokens.get(i + 1).and_then(|n| n.parse::<u32>().ok());
        if (t == "SEASON" || t == "SERIES") && next_number.is_some() {
            guess.season = next_number;
            i += 2;
            continue;
        }
        if (t == "DISC" || t == "DISK") && next_number.is_some() {
            guess.disc = next_number;
            i += 2;
            continue;
        }

        // "S7D1" / "S03" / "D2" as one token
        let (s, d) = parse_combined(t);
        if s.is_some() || d.is_some() {
            guess.season = guess.season.or(s);
            guess.disc = guess.disc.or(d);
            i += 1;
            continue;
        }

        if !NOISE.contains(&t.as_str()) {
            title_tokens.push(t.clone());
        }
        i += 1;
    }

    guess.title = title_tokens.iter().map(|t| title_case(t)).collect::<Vec<_>>().join(" ");
    guess
}

/// `RECREATION` to `Recreation`, leaving short all-caps words alone since they
/// are usually initialisms.
fn title_case(t: &str) -> String {
    if t.len() <= 2 || t.chars().any(|c| c.is_ascii_digit()) {
        return t.to_string();
    }
    let mut c = t.chars();
    match c.next() {
        Some(f) => f.to_string() + &c.as_str().to_ascii_lowercase(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_common_makemkv_shape_parses() {
        let g = parse("PARKS_AND_RECREATION_S7D1");
        assert_eq!(g.title, "Parks And Recreation");
        assert_eq!(g.season, Some(7));
        assert_eq!(g.disc, Some(1));
    }

    #[test]
    fn spelled_out_season_and_disc_parse() {
        let g = parse("THE_OFFICE_SEASON_3_DISC_2");
        assert_eq!(g.title, "The Office");
        assert_eq!(g.season, Some(3));
        assert_eq!(g.disc, Some(2));
    }

    #[test]
    fn leading_zeros_do_not_change_the_number() {
        assert_eq!(parse("SHOW_S02D3").season, Some(2));
        assert_eq!(parse("SHOW_S02D3").disc, Some(3));
    }

    #[test]
    fn a_disc_only_label_still_gives_a_title() {
        let g = parse("BREAKING_BAD_D4");
        assert_eq!(g.title, "Breaking Bad");
        assert_eq!(g.season, None);
        assert_eq!(g.disc, Some(4));
    }

    #[test]
    fn a_movie_label_has_no_season_at_all() {
        let g = parse("THE_BIG_LEBOWSKI");
        assert_eq!(g.title, "The Big Lebowski");
        assert_eq!(g.season, None);
        assert_eq!(g.disc, None);
    }

    #[test]
    fn disc_noise_is_stripped_from_the_title() {
        assert_eq!(parse("SOME_MOVIE_WS_NTSC_DVD").title, "Some Movie");
    }

    #[test]
    fn a_word_starting_with_s_is_not_mistaken_for_a_season() {
        // "SEINFELD" begins with S but is not S-plus-digits
        let g = parse("SEINFELD_S04D1");
        assert_eq!(g.title, "Seinfeld");
        assert_eq!(g.season, Some(4));
    }

    #[test]
    fn numbers_in_a_title_survive() {
        let g = parse("24_S01D1");
        assert_eq!(g.title, "24");
        assert_eq!(g.season, Some(1));
    }

    #[test]
    fn an_empty_label_is_not_a_crash() {
        assert_eq!(parse(""), LabelGuess::default());
    }
}
