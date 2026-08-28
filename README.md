# ripper

Deterministic subtitle recognition for DVD rips.

DVD subtitles are not photographed text — they are a rendered bitmap font, so
every `e` on a disc is the *same pixels*. `ripper` exploits that: it segments the
subtitle bitmaps into individual glyphs, labels the few hundred distinct shapes
once, and then decodes everything else by exact lookup. No statistical OCR, no
per-image tuning, and the same input always produces the same output.

Cue timings come straight from the subtitle stream and are never re-derived, so
output is sample-accurate with the source by construction.

## Why not just run Tesseract

Tesseract guesses shapes, so it confuses characters that *look* similar — `is`
becomes `ts`, `if` becomes `lf`, `to` becomes `fo` — and the errors move around
when you change the rendering. Exact matching cannot make those mistakes,
because `I` and `t` are different bitmaps.

Measured on 122 episodes (62,590 cues, 1.9M glyph instances) against a
Tesseract-derived reference that had already been hand-corrected:

| | |
|---|---|
| cue timings matching the source stream | 62,590 / 62,590 (100%) |
| cues where the text matches exactly | 94.39% |
| character accuracy | 99.02% |
| glyphs it could not recognise | 0 |
| runtime, all 122 episodes | ~50 s |

Of the cues that disagree, **16.5% are ones where ripper produces valid English
and the reference does not** (`he fell in` vs `he tell in`, `Go on` vs `Goon`,
`a lot of` vs `a fot of`), 82.9% differ only in punctuation or spacing, and
0.6% — 20 cues in 62,590 — are cases where the reference looks better.

## Install

Needs `ffmpeg`, `ffprobe` and `mkvextract` on `PATH` for reading subtitles out
of video containers. A `.idx`/`.sub` pair can be read with no external tools.

```
cargo build --release
```

## Use

Building a glyph table is a one-time cost per release font. If you already have
trusted subtitles for a few episodes, labels can be voted from them
automatically; otherwise the table comes out unlabelled and you fill it in from
the review page.

```sh
# 1. observe glyphs, and (optionally) vote labels from known-good SRTs
ripper build /media/*.mp4 --table glyphs.json --reference ./known-good/

# 2. review whatever is unlabelled or uncertain
ripper sheet --table glyphs.json --out glyphs.html
#    ... open it, fix any wrong labels, press "Copy corrections", save as JSON
ripper label --table glyphs.json corrections.json

# 3. decode
ripper ocr episode.mp4 --table glyphs.json -o episode.srt

# 4. check against a reference, if you have one
ripper verify episode.srt reference.srt

# diagnosing a bad cue
ripper inspect episode.mp4 --at 673473 --table glyphs.json
```

`build` is incremental: point it at more files and it extends the existing
table, so a table grows to cover a whole series.

## How it works

```
.idx/.sub ──▶ SPU decode ──▶ glyph segmentation ──▶ table lookup ──▶ SRT
             (RLE bitmap,     (components, mark      (exact match,
              palette,         merging, line          ambiguity classes)
              timings)         grouping, spacing)
```

Some details that turned out to matter, each of which silently corrupts output
if you get it wrong:

- **The alpha and palette nibbles in an SPU are stored for colour 3,2,1,0.**
  Read them in index order and the background counts as ink.
- **Match on the glyph *fill*, not fill plus outline.** Adjacent characters'
  outlines touch, so including them merges letters into blobs.
- **Marks must be rejoined to their stems** — the dot of an `i`, the two dots of
  a colon. An italic face shears the mark sideways, so plain bounding-box
  overlap is not enough.
- **A line with no ascenders groups its i-dots as a separate line**, leaving the
  stems reading as `1`. Real line spacing is several times the dot-to-stem gap,
  so the two cases separate cleanly.
- **Word gaps must be measured, not assumed.** The distribution is bimodal but
  has a long tail from dialogue dashes; k-means gets dragged into the tail and
  swallows every space on the disc. An Otsu split of the clipped histogram is
  stable.
- **Per-glyph spacing beats one global threshold.** The tail of an `f` or `y`
  eats into the following gap, so a single number turns `if you` into `ifyou`.
  `build` learns a threshold per glyph from the reference.

### Does the wordlist earn its keep

Measured against the 122-episode reference, decoding with and without one:

| wordlist | exact cues | characters |
|---|---|---|
| generic English, 54k words | **94.39%** | **99.02%** |
| domain-specific, 9.5k words built from held-out episodes | 94.33% | 98.87% |
| none | 93.48% | 98.91% |

It changes the answer on 739 cues and is right on 586 of them - a 9:1 win rate,
worth about 520 cues. Coverage matters more than domain fit: a wordlist built
from this very show, but only a sixth the size, scored slightly *worse* than a
generic one, and merging the two added almost nothing.

The one way it used to hurt was word splitting: `you're` is not in the
dictionary but `you` and `re` both are, so it came apart into `you 're`.
Splitting now refuses to cross an apostrophe.

### Genuine ambiguity

Some characters are drawn *identically*. In this DVD's face, capital `I` and
lowercase `l` are the same 3×21 bar — 91,397 instances of one bitmap that is
sometimes one letter and sometimes the other. No image matching can separate
them, because the distinction is not in the picture.

`build` detects this (two labels each holding a real share of the votes) and
records an ambiguity class, `"l|I"`. At decode time the word is resolved from
context: a wordlist plus structural rules covering `I` and its contractions,
acronyms, and position within the word. Where the geometry shows a gap that only
just missed the space threshold, a word may also be split (`Ijust` → `I just`) —
but only there, so `Pawnee` never becomes `Paw nee`.

## Other languages

The method is script-agnostic — it matches bitmaps — but two things are not: the
wordlist, and the rules for resolving genuinely ambiguous glyphs.

Verified on Swedish (*Frozen*, Region 2 DVD) and Spanish (the Spanish track of
the Parks and Recreation discs). Both compose their diacritics into single
glyphs, so `å ä ö é` and `á é í ó ú ñ ¿ ¡` are ordinary table entries:

```
Född ur kall midvinters köld, ur karga bergens dimma
En kraft båd' hård och skön har skapt denna frusna härskarinna
Frukta hennes själ — Hon älskar dig ihjäl
```

Scored against the aspell Swedish wordlist, **97.2% of the 6,471 output words
are dictionary-valid**. Almost all of the remainder are legitimate: place names
(*Arendal*, *Vessleby*), colloquial forms (*Va*, *sånt*, *sommarn*) and Swedish
compounds (*handelspartner*, *sommarrea*) that no wordlist carries.

Confirmed across two films and four languages — English, Swedish, Finnish and
Icelandic, eight subtitle tracks in all. Icelandic exercises the widest
character set and comes through intact: `ð þ æ ý á é í ó ú ö` and their
capitals, all composed as single glyphs.

```
Það frosna afl í erg og gríð          (is)
og ég vil hlýtt knús.                 (is)
Varokaa / Iskekää                     (fi)
Han vill vara smart, men det är töntigt.   (sv)
```

**All the language tracks on one disc share a font.** Frozen's Swedish table
covered 86-99% of the glyph instances on its English, Finnish and Icelandic
tracks, so one table per *disc* serves every language on it - only the
language-specific letters need adding.

Two things to set for a non-English language:

- **`--lang`.** English-only rules — a lone ambiguous bar being the pronoun `I`,
  `I'm`/`I'll` being likely — are wrong elsewhere. Swedish `i` is a lowercase
  preposition, so the English rule would capitalise every one. Any `--lang`
  other than `en` turns those rules off.
- **`--words`.** On Arch: `pacman -S aspell-sv`, then
  `aspell -d sv dump master | aspell -l sv expand | tr ' ' '\n' | sort -u > sv.txt`.
  Without one, ambiguous glyphs fall back to structural rules, which is fine -
  but a *mismatched* wordlist is worse than none, so the English default is only
  loaded for `--lang en`. (`vii` and `alia` are English words while `vil` and
  `alla` are not, which turned Icelandic `ég vil` into `ég viI` until the
  fallback was removed.)

### Umlauts need both dots

An umlaut is *two* marks. Once the first joins its letter, the second no longer
sits above anything — it overlaps the merged glyph — so a naive stacking test
drops it and `Född` comes out as `Fö.dd`. A ring (`å`) is a single mark and never
hits this, which is what made the bug easy to miss. `merge_diacritics` handles
it; the same pass covers Spanish `ñ` and any other stacked mark.

### Check your labels

Labelling by eye is the one manual step, and the mistake it invites is case:
`o` and `O` are the same shape at different sizes, and a contact sheet that
scales every glyph into one cell hides exactly that. `ripper check` compares
each label against the table's own x-height and cap-height and flags the
mismatches - it caught an `o` labelled `O` that had corrupted 789 instances.

```sh
ripper check --table glyphs.json
```

### Tables do not transfer between releases

Measured: of Frozen's 110 glyphs, **1** matches the Parks and Recreation table;
of Cloudy with a Chance of Meatballs' 139, **2** match Frozen's. Different
studios use different subtitle faces (21px cap height vs 22px here), so a table
is per-release. Labelling a fresh one from the review sheet takes a few minutes
and needs no knowledge of the language — only of the alphabet.

## Status

Prototype, and deliberately scoped to the recognition stage. Ripping,
title identification and transcoding are not here yet.

Known rough edges:

- `%` segments into two or three components; the pieces are labelled so the
  output is right, but it is a hack.
- A handful of rare merged punctuation pairs (one or two instances each across
  1.9M glyphs) carry best-guess labels.
- The wordlist is `/usr/share/dict/cracklib-small` by default, which is a
  password dictionary — serviceable, but a real English wordlist would resolve
  ambiguity better. Override with `--words`.
