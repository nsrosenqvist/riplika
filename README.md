# Riplika

Turn a disc into a tagged, subtitled library. Four stages:

```
rip ───▶ identify ───▶ transcode ───▶ subtitles
MakeMKV   label +       ffmpeg         VobSub bitmaps
          catalogue +   one pass       to text, per language
          disc structure
```

There is a command line and a GTK/libadwaita window; both are thin shells over
one library, so they cannot disagree about what anything means.

## Layout

| crate | what it is |
|---|---|
| `riplika-core` | the whole pipeline as a library |
| `riplika-cli` | `riplika`, the terminal front end |
| `riplika-gui` | `riplika-gui`, the window |

Two rules shape `riplika-core`, and both come from bugs that shipped in the shell
scripts this replaces.

**Deciding is separate from doing.** Nothing that talks to ffmpeg or MakeMKV
also decides what to ask them. A planner turns state into an argv vector and a
runner executes it, so a test can assert on the exact arguments with no disc in
the drive. That is what catches the arguments that are wrong but not *invalid* —
a missing `-map` that silently drops the subtitle track, a `-disposition` index
one too high — which is precisely the class of bug that a shell script cannot be
tested for and that cost the most time here.

**The outside world is behind a trait.** `Runner`, `Prober`, `Ripper`,
`Catalogue` and `Fs` all have fake implementations, so the entire pipeline runs
end to end in milliseconds with no hardware, no ffmpeg and no network. A disc
with two episodes and a play-all is a few lines of test data.

```
core/src/
  host.rs        Command, Runner, Fs - the only way out of the process
  model.rs       the shared vocabulary: discs, titles, tracks, roles, settings
  lang.rs        ISO 639 matching and language preference order
  media.rs       ffprobe, as one JSON call
  naming.rs      what a file is called and where it goes
  rip/           MakeMKV: enumerate a disc, then read it
  identify/      volume label, disc structure, and the catalogues
  transcode/     what to measure, and the ffmpeg command to build
  subs/          bitmap subtitles to text
  job.rs         the four stages in order, reporting as it goes
```

`job.rs` deliberately exposes `rip`, `organise` and `produce` separately rather
than only a single `run`. The window has to stop between the second and the
third to show what it identified and let you correct it; a single call that did
everything would work for a script and be useless for a window.

## Install

Needs `ffmpeg`, `ffprobe` and `mkvextract` on `PATH`. ffmpeg must be built
`--enable-libdvdnav --enable-libdvdread` to read DVDs, which is the usual
packaging. `makemkvcon` is needed only for Blu-ray - see [Reading
discs](#reading-discs). A `.idx`/`.sub` pair can be read with no external tools.

```
cargo build --release
```

## Use

```sh
riplika drives                     # what is connected, and what is loaded
riplika scan                       # titles on the disc, without ripping it
riplika identify                   # what the disc appears to be, and why

# --reader picks who reads the disc: auto (default), dvd, or makemkv.
# "dvd" needs nothing proprietary; "makemkv" is the only one that does Blu-ray.
riplika scan --reader dvd

# the whole pipeline
riplika rip --languages english,swedish --video medium --audio high \
            --table glyphs.json --words ./words -o ~/Videos

# an already-ripped folder, skipping the disc
riplika process ~/rips/parks-s7d1 --title "Parks and Recreation" --season 7 \
                --disc 1 --dry-run
```

`--dry-run` prints the plan - which title becomes which episode, and what each
one will be called - and stops. Worth doing once per disc.

Set `TMDB_API_KEY` to add film lookup; TV works with no key at all via TVmaze.

## Reading discs

MakeMKV is proprietary, so it is worth knowing exactly what it is needed for.

| | DVD | Blu-ray |
|---|---|---|
| free software | **yes**, and it is the default | **no** |
| what does it | libdvdread + libdvdnav + libdvdcss, through ffmpeg's `dvdvideo` demuxer | MakeMKV |

`--reader dvd` needs nothing beyond an ffmpeg built `--enable-libdvdnav
--enable-libdvdread`, which is the usual packaging. `--reader makemkv` is still
there and is still the only option for Blu-ray. `auto`, the default, looks for a
`VIDEO_TS` directory and picks the free path when it finds one.

Verified on a Parks and Recreation season 7 disc: the free reader finds the same
titles MakeMKV does - seven 21-minute episodes, the 43-minute and 107-minute
play-alls, the 27-minute extended cut and the extras.

### Why Blu-ray is different

`libbluray` reads the structure but does no decryption. That needs `libaacs`,
which is free software shipping no keys - you supply a `KEYDB.cfg` of volume
unique keys assembled by third parties, which is always incomplete and always
behind - and `libbdplus`, which needs conversion tables, for the BD+ titles that
most studios use. AACS 2.0 on UHD discs is out of reach entirely. MakeMKV
implements both and is maintained against new releases, and nothing free is
close. This is a real gap, not a packaging inconvenience.

### What MakeMKV still does that the free path cannot

Two things, and both are real.

**Region.** libdvdcss does the player-key exchange with the drive, and an RPC-2
drive can refuse it when the disc's region does not match the region the drive
is set to. A drive can only be set to one region, and only a few times. MakeMKV
talks to the drive itself and does not care, which is why it is the one that
copes with a shelf holding both Region 1 and Region 2 discs.

**Damage.** MakeMKV retries unreadable sectors and carries on past the ones it
cannot get. libdvdread mostly gives up. That covers scratched discs and also the
deliberately corrupt sectors some copy protections write to break naive rippers.

Losing either of those *silently* would be worse than not having the free path
at all, because both fail the same way: the scan succeeds and quietly returns
fewer titles. So `auto` watches for the signature - `Error cracking CSS key`,
unreadable sectors - abandons the scan the moment it appears, and hands the disc
to MakeMKV:

```
reader: ffmpeg dvdvideo (makemkv in reserve)
  the free reader could not read this disc fully:
    libdvdcss could not decrypt parts of the disc (/VIDEO_TS/VTS_06_1.VOB) -
    titles in those parts are missing from this scan
  handing it to makemkv, which works around this
```

With `--reader dvd` there is no fallback, so it stops with the same complaint
rather than pretending. Keep MakeMKV installed; the free path just means it is
not on the critical path for an ordinary DVD.

### What of MakeMKV's fault tolerance is reimplemented

Some of it, and the parts that are reachable are worth having. What follows is
what the free reader now does; the honest ceiling is in the next section.

**Every decryption method, not just one.** libdvdcss has three, and they are
answers to different problems rather than alternatives:

| method | what it does | what defeats it |
|---|---|---|
| `key` | asks the drive to do the CSS handshake | an RPC-2 drive whose region does not match the disc |
| `disc` | cracks the disc key without the drive | discs resistant to cracking |
| `title` | cracks each title key from the data | slowest; same resistance |

Only the first needs the drive's cooperation, so `disc` is what reads a Region 1
disc in a drive set to Region 2 - the case a mixed-region shelf runs into. Each
is tried in turn, and a failing attempt is abandoned after a single probe, so
the cost of trying is small. The one that worked is recorded.

**Checking the length of what came back.** This is the most valuable of the
three, because the failure it catches is silent. A damaged sector does not
usually make ffmpeg fail - it makes it stop, exit zero, and leave a file that
plays and is merely missing its ending. Every ripped title is measured against
the duration the scan reported, and anything more than 2% short is treated as
damage rather than accepted.

**Salvaging a title a chapter at a time.** When every method stops early, the
title is read again chapter by chapter and whatever survives is joined back
together, so one scratch costs one chapter rather than the whole episode:

```
title 41 is damaged; salvaging by chapter
title 41 recovered without chapter 3
```

That is a coarse version of what `ddrescue` does. The granularity is chapters
because that is the smallest unit the demuxer can be asked for; MakeMKV works at
the sector level and loses correspondingly less.

### Rescuing a damaged disc

`riplika rescue` is GNU ddrescue's algorithm applied to a DVD. Its central
insight is not obvious and is worth stating: **read the easy data first**. A
failing disc may not survive an hour of retries, and every unreadable sector
costs seconds while the drive retries internally - so working on the damage
before the good 99% is secured is a way to end up with nothing.

```sh
# the seven episodes only - 5.24 GB of an 8.24 GB disc
riplika rescue /dev/sr0 disc.iso --vts 11 --chains 2-8

# everything
riplika rescue /dev/sr0 disc.iso
```

Four passes, narrowing each time:

| pass | reads | purpose |
|---|---|---|
| copy | 128 sectors | sweep up everything easy; on error, skip well ahead |
| trim | 8 sectors | approach the damage from both ends to find its real extent |
| scrape | 1 sector | read what is left individually |
| retry | 1 sector | go round the remaining bad sectors again - drives are not deterministic |

**It decrypts as it reads.** A raw image of a CSS disc is encrypted, and
decrypting it afterwards means cracking the keys - which failed for six video
title sets of the disc this was built against. Reading through libdvdcss uses
the drive's key exchange while the drive is still in front of it, and
`DVDCSS_READ_DECRYPT` clears the scrambling bits, so the image needs no keys.
libdvdcss is loaded at runtime, so everything else still works without it.

**It reads only what you ask for.** The video title set IFOs give each program
chain's cell extents, so a rescue can cover the episodes and skip the menus. The
play-alls share the episodes' sectors, so they cost nothing:

```
chain  1:  43m01   1.57 GB     <- play-all
chain  2:  21m30   0.79 GB     <- episode
...
chain  9: 107m42   3.67 GB     <- play-all
episodes (chains 2-8): 1 run, 5.24 GB
```

Verified end to end against a real disc: rescuing one episode reads 0.79 GB,
writes a 752 MB sparse image spanning the disc's full 7.7 GB address space, and
that image demuxes to the right title - `mpeg2video`, `ac3` English,
`dvd_subtitle` English and Spanish, 1290.73 s against the disc\'s 21m30.

**It is resumable.** A map file is written beside the image in ddrescue's own
format, recording the state of every sector. Stop it, clean the disc, run it
again: only what is still missing is attempted. That is the difference between
a scratched disc being a lost cause and being a second attempt.

**What is not encrypted must not be decrypted.** `DVDCSS_READ_DECRYPT`
descrambles whatever it is handed, and a DVD sector's payload starts at byte
128 - so decrypting the volume descriptors leaves their first 128 bytes intact
and turns the rest into noise. The image then still mounts as a filesystem and
still will not open as a DVD, which is a genuinely confusing way to fail. Only
the video objects are read with decryption.

**Holes become padding, not zeros.** Unrecoverable sectors are filled with MPEG
program-stream padding packets, which a demuxer skips. Zeros where a packet
header belongs make it treat the rest of the stream as corrupt, so the choice is
between losing a moment and losing the remainder of the file.

### What is still not reimplemented

- **Making the drive try harder.** MakeMKV sets the drive's read-retry
  behaviour over raw SCSI (`MODE SELECT`), which often turns an unreadable
  sector into a slow one. Doable through `SG_IO`, untestable here, and easy to
  get wrong in ways that hang a drive.
- **Protection-specific workarounds.** Arccos and RipGuard deliberately write
  unreadable sectors and bogus structures. Handling them means recognising each
  scheme, which is reverse engineering per title, and is MakeMKV's real moat.
- **AACS and BD+.** Not a fault-tolerance question - there is simply no free
  implementation with keys. See above.

MakeMKV remains better on a disc whose damage is deliberate rather than
accidental, and it asks the drive to try harder than we know how to. For
ordinary damage - a scratch, a smudge, a disc that reads on the third attempt -
the rescue path now covers it.

### Two things that make the free DVD path work

Both were found by running it against a real disc, and both fail in the same
dangerous way - a scan that succeeds and quietly returns a season with no
episodes in it.

**`DVDCSS_METHOD=key`.** libdvdcss defaults to cracking title keys by brute
force. On this disc that failed for exactly the VTSs holding the episodes:

```
libdvdnav: Error cracking CSS key for /VIDEO_TS/VTS_06_1.VOB (0x000651ea)
```

The scan then returns the extras and nothing else, which looks like a disc that
simply has no episodes on it. Asking for the proper player-key exchange with the
drive decrypts everything, and is faster.

**The title count comes from the disc.** DVD title numbering is not contiguous.
This disc has content at titles 2-19 and again at 39-58, with a seventeen-title
hole in between, so any stop-after-N-empty-titles rule gives up in the hole -
losing every episode, since they are at 41-47. Probing all 99 instead is safe
but slow. So `rip/iso.rs` walks ISO 9660 to `VIDEO_TS.IFO` and reads the title
table, which costs three sector reads and gives an exact count:

```
number of titles: 58
  title 41: chapters=5  vts=11 ttn=2      <- episode
  title 48: chapters=21 vts=11 ttn=9      <- the play-all
```

That table also carries chapter counts, and the demuxer reports chapter
*durations* - which MakeMKV's scan does not. Those are what decompose a
play-all, so a disc can in principle be sorted out before anything is read, and
the play-all title never ripped at all. On this disc that is two and a half
hours of redundant reading.

## Flatpak

`packaging/com.nsrosenqvist.Riplika.yml` builds the window against
`org.gnome.Platform`, bundling libdvdcss, libdvdread, libdvdnav, ffmpeg and
mkvextract.

Two things about it are worth knowing before you rely on it.

**It is DVD-only.** MakeMKV cannot be bundled - it is proprietary, so
redistributing it is not ours to do - and reaching the host's copy would need
`--talk-name=org.freedesktop.Flatpak`, which lets the application run anything
at all on the host. Claiming to be sandboxed after that would be a lie. The
Flatpak is coherent precisely because the DVD path needs nothing proprietary;
for Blu-ray, use the native build.

**`--device=all` is required.** Flatpak has no narrower permission for an
optical drive: `--device=dri` is the GPU and there is nothing for `/dev/sr0`
alone. Reading a disc means granting access to devices generally.

I have not built it. `flatpak-builder` is not installed here, so the manifest is
structurally right and its checksums are real and verified against upstream's
published sums, but it has never been through a build. Expect the ffmpeg and
mkvtoolnix modules to need adjusting.

```sh
flatpak install org.gnome.Platform//50 org.gnome.Sdk//50 \
                org.freedesktop.Sdk.Extension.rust-stable//25.08
python3 flatpak-cargo-generator.py Cargo.lock -o packaging/cargo-sources.json
flatpak-builder --install --user build packaging/com.nsrosenqvist.Riplika.yml
```

## Preferences

The window keeps settings in `$XDG_CONFIG_HOME/riplika/preferences.json`, not
GSettings - a schema has to be compiled into a system directory before the
application will start, which is a poor trade for a dozen values. A missing or
corrupt file falls back to defaults rather than refusing to launch; losing
preferences should cost a re-tick, not a launch.

The split is between policy and per-disc choice. Preferences hold what is true
of the whole library - preferred languages, whether commentary is wanted, where
the glyph table lives. The rip page holds what differs between discs: quality,
the output folder, and which of *this* disc's languages to take.

**Preferred languages** decide what starts ticked. The rip page lists the
languages the disc actually carries - taken from the scan, so there is nothing
to spell - with the preferred ones ticked and moved to the top. Order is the
order you switch them on, and the first one becomes the default track. A
language you want that is not on the disc simply does not appear; a language on
the disc that you have not asked for is still offered, just unticked.

**The MakeMKV fallback** can only be switched on if `makemkvcon` is installed.
When it is missing the row is insensitive and says so, rather than being a live
control whose promise would be broken forty minutes into a disc. The choice is
honoured by both front ends through `rip::Auto`, so the window and the command
line cannot drift apart about when MakeMKV gets involved.

## Identification

A DVD carries no usable identifier, and there is no database keyed by disc.
Redump and the DVD-Video hash registries cover games and preservation, not
retail television, and neither can answer "which episodes are on this disc". So
two independent kinds of evidence are combined, and both are shown:

- **The volume label.** `PARKS_AND_RECREATION_S7D1` is the single most
  informative thing on a disc and it costs nothing to read. It is also capped at
  32 characters, so it truncates, and every authoring house has its own
  conventions - it can only ever be a hypothesis to search with.
- **The disc's own structure.** How many episode-length titles there are, how
  long they run, and how they group under the "play all" title. A play-all
  replays the episodes back to back, so its chapter list is theirs
  concatenated - decomposing it recovers both which titles are episodes and what
  order they belong in, with no network and no guessing.

A candidate is only trusted when the two agree, and the reasons are carried
along so a wrong guess is visible rather than mysterious. Which disc of a season
you are holding is genuinely ambiguous from one disc - a season split 5/5/4 and
one split 4/4/6 look identical from disc two - so episode numbering prefers what
is already in the output folder, then falls back to a guess it tells you about.

## Transcoding

HandBrake is a wrapper around the same libx264; encoding a title both ways with
matching settings produces byte-identical x264 parameter strings. What HandBrake
actually supplies is the preprocessing decisions, so those are made explicitly:

| | |
|---|---|
| crop | detected from the middle of the film, not the opening titles |
| inverse telecine | applied only when the duplicate frames are really there |
| frame rate | pinned constant |
| pixel aspect | snapped to the four ratios a DVD can have |
| subtitles | recognised per language, bitmaps dropped once they are |
| faststart | moov atom at the front |

Three tiers each for picture and sound. DVD is 720x480 MPEG-2 at 4-6 Mb/s, so
the useful range is narrow: by CRF 18 the encode is transparent against the
source and further bits mostly track MPEG-2's own noise, while below CRF 23 SD
detail goes quickly.

| tier | picture | sound |
|---|---|---|
| high | CRF 18, ~240 MB an episode | original AC3, untouched |
| medium | CRF 20, ~170 MB - the sweet spot | AAC 160k stereo |
| low | CRF 23, ~107 MB | AAC 96k stereo |

`--audio high` keeps the original 5.1 with no downmix and no second lossy
generation. The cost is browsers, which cannot decode AC3 at all, so a web
client makes the server transcode; `--dual-audio` adds an AAC stereo track
beside the original for exactly that case.

`--languages english,swedish` filters audio and subtitles together, and the
order is meaningful: the first language listed ends up first *and* carries the
default flag, because players go by the flag rather than the order. A subtitle
filter matching nothing is honoured exactly - a subtitle you cannot read is
worse than none - while an audio filter matching nothing keeps everything, since
a file with no audio is simply broken.

### Things that were wrong before, and the tests that hold them down

Each of these produced a file that looked correct:

- **A blanket `-map 0`** carried the DVD's `bin_data` stream through and
  accumulated a duplicate on every pass. Mapping is explicit, `-dn` is set, and
  chapters come over separately via `-map_chapters 0`.
- **`-c copy` with no subtitle `-map`** wrote files with no subtitles in them.
- **ffmpeg reads stdin** looking for keypresses, so inside a loop that reads a
  list it eats the rest of the list. One run processed one episode out of eight
  and reported success. Every child now gets a closed stdin, in one place.
- **`fieldmatch,decimate` applied blind** threw away one real frame in five
  (24,772 kept of 30,964) because the source was *soft* telecine, already
  resolved to 23.976 by the decoder. Detection measures what the decoder
  produces rather than what the container declares.
- **Writing straight to the destination** left a truncated file at the final
  path when a run was interrupted, which the next run counted as a finished
  episode and skipped. Encoding goes to `.part` and renames on success.
- **Frames captured as text.** Perceptual hashing read raw greyscale through a
  UTF-8 conversion, which replaces invalid bytes with U+FFFD and changes the
  length, so no extended cut ever matched. Frames go to a file.

## Subtitles

Deterministic recognition, not OCR.

DVD subtitles are not photographed text - they are a rendered bitmap font, so
every `e` on a disc is the *same pixels*. `riplika` exploits that: it segments the
subtitle bitmaps into individual glyphs, labels the few hundred distinct shapes
once, and then decodes everything else by exact lookup. No statistical OCR, no
per-image tuning, and the same input always produces the same output.

Cue timings come straight from the subtitle stream and are never re-derived, so
output is sample-accurate with the source by construction.

Recognition runs on the *rip*, before encoding, so the SRTs already exist when
ffmpeg starts and can be extra inputs to the one pass that was always necessary.
The shell version needed three passes an episode; two of them existed only
because it recognised from the transcode instead.

Once a track is recognised its bitmap is redundant, and a client that selects a
bitmap track forces the server to burn it into the picture and re-encode - so
bitmaps are dropped. `--keep-bitmap-subs` retains them. A track whose
recognition *failed* keeps its bitmap either way: losing the text form of a
language is a nuisance, losing the language is not.

### Why not just run Tesseract

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

Of the cues that disagree, **16.5% are ones where riplika produces valid English
and the reference does not** (`he fell in` vs `he tell in`, `Go on` vs `Goon`,
`a lot of` vs `a fot of`), 82.9% differ only in punctuation or spacing, and
0.6% — 20 cues in 62,590 — are cases where the reference looks better.

### Building a glyph table

Building one is a one-time cost per release font. If you already have
trusted subtitles for a few episodes, labels can be voted from them
automatically; otherwise the table comes out unlabelled and you fill it in from
the review page.

```sh
# 1. observe glyphs, and (optionally) vote labels from known-good SRTs
riplika build /media/*.mp4 --table glyphs.json --reference ./known-good/

# 2. review whatever is unlabelled or uncertain
riplika sheet --table glyphs.json --out glyphs.html
#    ... open it, fix any wrong labels, press "Copy corrections", save as JSON
riplika label --table glyphs.json corrections.json

# 3. decode
riplika ocr episode.mp4 --table glyphs.json -o episode.srt

# 4. check against a reference, if you have one
riplika verify episode.srt reference.srt

# diagnosing a bad cue
riplika inspect episode.mp4 --at 673473 --table glyphs.json
```

`build` is incremental: point it at more files and it extends the existing
table, so a table grows to cover a whole series.

### How it works

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

### Other languages

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
scales every glyph into one cell hides exactly that. `riplika check` compares
each label against the table's own x-height and cap-height and flags the
mismatches - it caught an `o` labelled `O` that had corrupted 789 instances.

```sh
riplika check --table glyphs.json
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
- Film lookup needs a `TMDB_API_KEY`; without one only television resolves.
- The window has not been through a proper design pass. It works, it is not
  pretty.
