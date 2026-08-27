//! Tests for the parts where a regression would be silent: the ambiguity
//! resolver, mark merging, spacing, and SRT round-tripping.

use crate::resolve::{Resolver, Slot};
use crate::segment::{SegOpts, Glyph, Line};
use crate::srt;

fn amb() -> Slot {
    // the l/I bar, most-frequent reading first, as the table stores it
    Slot::Ambiguous(vec!["l".into(), "I".into()])
}

fn fixed(s: &str) -> Slot {
    Slot::Fixed(s.into())
}

fn word(r: &Resolver, slots: &[Slot]) -> String {
    r.resolve_word(slots)
}

#[test]
fn timestamps_round_trip() {
    for ms in [0u64, 1, 999, 1000, 61_001, 3_661_123, 9_999_999] {
        assert_eq!(srt::parse_ts(&srt::fmt_ts(ms)), Some(ms), "ms = {ms}");
    }
}

#[test]
fn srt_round_trips_through_parse() {
    let cues = vec![
        srt::Cue { start_ms: 1101, end_ms: 2591, text: "I have some\nnews".into() },
        srt::Cue { start_ms: 2669, end_ms: 4603, text: "Li'l Sebastian.".into() },
    ];
    assert_eq!(srt::parse(&srt::write(&cues)), cues);
}

#[test]
fn resolves_standalone_i() {
    let r = Resolver::load(None);
    assert_eq!(word(&r, &[amb()]), "I");
}

#[test]
fn resolves_contractions() {
    let r = Resolver::load(None);
    // I'm  -  the bar, apostrophe, m
    assert_eq!(word(&r, &[amb(), fixed("'"), fixed("m")]), "I'm");
    // I'll  -  bar, apostrophe, bar, bar
    assert_eq!(word(&r, &[amb(), fixed("'"), amb(), amb()]), "I'll");
    // We'll  -  the ambiguous pair must not turn the word into shouting
    assert_eq!(
        word(&r, &[fixed("W"), fixed("e"), fixed("'"), amb(), amb()]),
        "We'll"
    );
}

#[test]
fn resolves_by_dictionary() {
    let r = Resolver::load(None);
    // "look" not "Iook"
    assert_eq!(
        word(&r, &[amb(), fixed("o"), fixed("o"), fixed("k")]),
        "look"
    );
    // "All" not "AII" - one leading capital is a sentence start, not an acronym
    assert_eq!(word(&r, &[fixed("A"), amb(), amb()]), "All");
    // "It" not "lt"
    assert_eq!(word(&r, &[amb(), fixed("t")]), "It");
}

#[test]
fn keeps_capitals_inside_an_acronym() {
    let r = Resolver::load(None);
    // IOW - settled letters are all capitals, so the bar is a capital too
    assert_eq!(word(&r, &[amb(), fixed("O"), fixed("W")]), "IOW");
}

#[test]
fn resolves_lowercase_l_after_apostrophe() {
    let r = Resolver::load(None);
    // Li'l - not a dictionary word, so the structural rule decides
    assert_eq!(
        word(&r, &[fixed("L"), fixed("i"), fixed("'"), amb()]),
        "Li'l"
    );
}

/// Build a glyph from an ASCII picture, '#' meaning ink.
fn glyph(x: i32, y: i32, rows: &[&str]) -> Glyph {
    let h = rows.len() as i32;
    let w = rows[0].len() as i32;
    let mut bits = Vec::with_capacity((w * h) as usize);
    for r in rows {
        assert_eq!(r.len() as i32, w, "ragged glyph picture");
        bits.extend(r.chars().map(|c| (c == '#') as u8));
    }
    Glyph { x, y, w, h, bits }
}

#[test]
fn glyph_key_depends_on_shape_and_size() {
    let a = glyph(0, 0, &["##", "##"]);
    let b = glyph(9, 9, &["##", "##"]);
    let c = glyph(0, 0, &["##", "#."]);
    let d = glyph(0, 0, &["####"]);
    assert_eq!(a.key(), b.key(), "position must not affect identity");
    assert_ne!(a.key(), c.key(), "different ink must differ");
    assert_ne!(a.key(), d.key(), "different dimensions must differ");
}

#[test]
fn space_threshold_scales_with_line_height() {
    let opts = SegOpts::default();
    let tall = Line { glyphs: vec![], top: 0, bottom: 39 };
    let short = Line { glyphs: vec![], top: 0, bottom: 9 };
    assert!(
        crate::segment::space_threshold(&tall, &opts)
            > crate::segment::space_threshold(&short, &opts)
    );
    assert!(crate::segment::space_threshold(&short, &opts) >= 2);
}

#[test]
fn gaps_are_measured_between_bounding_boxes() {
    let line = Line {
        glyphs: vec![
            glyph(0, 0, &["#"]),
            glyph(5, 0, &["#"]),
            glyph(6, 0, &["#"]),
        ],
        top: 0,
        bottom: 0,
    };
    assert_eq!(crate::segment::gaps(&line), vec![4, 0]);
}


#[test]
fn english_rules_do_not_apply_to_other_languages() {
    let en = Resolver::load_lang(None, "en");
    let sv = Resolver::load_lang(None, "sv");
    // English: the lone bar is the pronoun "I"
    assert_eq!(word(&en, &[amb()]), "I");
    // Elsewhere that rule must not fire - Swedish's lone "i" is lowercase, so
    // capitalising every bar would be wrong
    assert_eq!(word(&sv, &[amb()]), "l");
}
