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
| cues where the text matches exactly | 94.23% |
| character accuracy | 98.94% |
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
