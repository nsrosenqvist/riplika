# Riplika

Turn a DVD into a tidy, watchable library — named, tagged, and with readable subtitles — without doing it by hand.

Put a disc in. Riplika works out what it is, reads only the parts worth keeping, encodes them, turns the subtitles into text, and writes files your media server will understand:

```
Season 06/
  Parks and Recreation - S06E01 - London.mp4
  Parks and Recreation - S06E02 - London.mp4
  extras/
    Parks and Recreation - S06E04 - Doppelgangers - Extended Cut.mp4
```

There is a window and a command line. Both do the same things.

## What it does

**Reads the disc.** DVDs need nothing proprietary — libdvdread and libdvdcss do the work through ffmpeg. MakeMKV is used for Blu-ray, and for DVDs the free path cannot manage, such as a disc whose region does not match the drive.

**Works out what it is.** The volume label says what a disc claims to be; the disc's own structure says what is actually on it. A "play all" title replays the episodes back to back, so decomposing it recovers which titles are episodes and what order they belong in — no guessing. That guess is then checked against a catalogue, and shown to you before anything happens.

**Encodes it.** Crop, inverse telecine, frame rate and pixel aspect are measured rather than assumed. Three quality tiers; the middle one is about 170 MB for a 21-minute episode and is hard to tell from the disc.

**Reads the subtitles.** DVD subtitles are pictures. Riplika turns them into text, exactly rather than statistically — 99% character accuracy, and cue timings that match the source by construction. A text subtitle also stops your media server burning the picture into the video and re-encoding it every time you press play.

## Getting started

You need `ffmpeg` and `ffprobe`. For Blu-ray you also need MakeMKV. On Arch:

```sh
sudo pacman -S ffmpeg libdvdcss libdvdread libdvdnav
cargo build --release
```

Then:

```sh
riplika drives          # what is connected, and what is in it
riplika scan            # what is on the disc, without reading it
riplika identify        # what the disc appears to be, and why

riplika rip --season 6 --disc 1 -o ~/Videos
```

Or open the window:

```sh
riplika-gui
```

`--dry-run` prints what would be produced and stops without reading anything. Worth doing once per disc.

### Subtitles need a glyph table

Turning subtitle pictures into text needs a table of what the letters look like, built once per release font and then reused. Without one, subtitles are left as pictures and Riplika says so.

```sh
riplika build /path/to/*.mkv --table glyphs.json   # collect the shapes
riplika sheet --table glyphs.json --out sheet.html # label them by eye, once
riplika label --table glyphs.json corrections.json
```

See [docs/subtitles.md](docs/subtitles.md) — it is the most interesting part of the project, and explains why this beats running OCR over the pictures.

## Documentation

| | |
|---|---|
| [Reading a disc](docs/discs.md) | which software reads it, damaged discs, Flatpak |
| [Working out what a disc is](docs/identifying.md) | catalogues, episode mapping, what to take |
| [Encoding](docs/transcoding.md) | quality tiers and the decisions behind them |
| [Subtitles](docs/subtitles.md) | deterministic recognition, and how well it works |
| [Configuration](docs/configuration.md) | where things live, preferences |
| [AGENTS.md](AGENTS.md) | architecture, for anyone changing the code |

## Status

Works, and is used. The subtitle recogniser has been measured over 122 episodes; the disc handling has been run against real discs including damaged ones. The window is functional but has not had a designer near it.

Known gaps: no Blu-ray without MakeMKV, and there is no free alternative worth pretending otherwise about. The Flatpak manifest has never been built.
