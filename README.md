# Riplika

Riplika reads a disc and writes files a media server can use, with the naming
and tagging already done.

By hand the same disc takes a rip, a transcode, and then work on the subtitles,
which arrive as pictures and have to become text before a media server will stop
re-encoding the film on every play. Little of that changes from disc to disc,
and that is the case for automating it.

```
Season 06/
  Parks and Recreation - S06E01 - London.mp4
  Parks and Recreation - S06E02 - London.mp4
  extras/
    Parks and Recreation - S06E04 - Doppelgangers - Extended Cut.mp4
```

There is a window and a command line, and both do the same things. Quality has
three settings and everything else is measured from the disc, which is a
deliberate limit. Anyone who wants to sit over an encode and tune it is better
served by the tools that already do that well.

## Three kinds of disc

The drive is asked what it is holding, and the answer decides which pipeline runs.

| In the drive | What comes out | Named by |
| --- | --- | --- |
| DVD | Encoded video with text subtitles | TMDB, TVmaze or Wikidata |
| Music CD | FLAC or MP3, tagged, with cover art | MusicBrainz, or the disc's own CD-Text |
| Game disc | A raw image, or a cue sheet and one file per track | Redump datfiles |

### Video

DVDs need nothing proprietary. libdvdread and libdvdcss do the reading through ffmpeg, and MakeMKV is used as a fallback where it is installed, for a disc the free reader cannot manage.

The volume label says what a disc claims to be, and its structure says what is on it. A "play all" title replays the episodes back to back, so decomposing it recovers which titles are episodes and what order they belong in. What that produces is checked against a catalogue and shown to you before anything is encoded.

A disc holding one long title is a film, and the identification weighs that alongside the name. The pages after it then stop asking a film which season it is.

The encoder settings are measured from the video itself, including the crop and whether the picture needs inverse telecine. Of the three quality tiers, the middle one comes to about 170 MB for a 21-minute episode and is hard to tell from the disc.

DVD subtitles arrive as pictures, and Riplika matches those bitmaps against a table of known letters, which gives 99% character accuracy and cue timings taken straight from the source. A text subtitle also stops a media server burning the picture into the video and re-encoding it on every play.

### Music

The table of contents gives a disc id, and MusicBrainz answers that with the pressing itself. A disc it has never seen falls back to CD-Text, and a disc neither knows can be searched for by name. Cover art comes from the Cover Art Archive and is embedded as a front cover.

Audio is read with cdparanoia, which re-reads and checks its own work. FLAC and MP3 are both available at three quality tiers, with the tag vocabulary each format expects. Filenames follow a pattern you can set, and a slash in that pattern makes a folder.

### Games

A game disc is copied whole, at the 2352 bytes a sector actually holds. A disc with audio tracks becomes one file per track and a cue sheet, since a flat image of such a disc matches no database.

Redump datfiles name the result from what it hashes to. When nothing matches exactly, Riplika says which disc it came closest to and which tracks disagreed, so a damaged read can be told apart from a pressing nobody has catalogued yet. Datfiles are fetched as soon as a game disc is recognised.

Audio and data are checked in different ways while a disc is dumped. The drive's C2 error pointers say which audio sectors it had to guess at, since audio carries no error correction a host can verify. Data sectors carry their own error detection, so a track failing that check is known to be a bad read.

## Getting started

The Flatpak carries everything it needs:

```sh
flatpak-builder --user --install --force-clean build packaging/com.nsrosenqvist.Riplika.yml
```

For a native build you need ffmpeg and cdparanoia. On Arch:

```sh
sudo pacman -S ffmpeg libdvdcss libdvdread libdvdnav cdparanoia
cargo build --release
```

Then:

```sh
riplika drives          # what is connected, and what is in it
riplika disc            # what kind of disc this is
riplika scan            # what is on it, without reading it
riplika identify        # what it appears to be, and why

riplika rip --season 6 --disc 1 -o ~/Videos
riplika rip-cd --format flac
riplika rip-game
```

Or open the window:

```sh
riplika-gui
```

`--dry-run` prints what would be produced and stops without reading anything, which is worth doing once per disc.

### Subtitles need a glyph table

Turning subtitle pictures into text needs a table of what the letters look like, built once per release font and reused after that. Without one, subtitles stay as pictures and Riplika says so.

```sh
riplika build /path/to/*.mkv --table glyphs.json   # collect the shapes
riplika sheet --table glyphs.json --out sheet.html # label them by eye, once
riplika label --table glyphs.json corrections.json
```

See [docs/subtitles.md](docs/subtitles.md), which explains why this beats running OCR over the pictures.

### Publishing

The release workflow builds a `.flatpak` bundle and attaches it to the tag, so
anyone can install a release without a repository being involved:

```sh
flatpak install ./riplika-0.3.0.flatpak
```

That needs the GNOME 50 runtime, which comes from Flathub, so a machine with
no remotes configured has to add Flathub once first.

For a Flathub submission, the same tag also carries a manifest with its source
pinned to that tag, and the cargo source list it refers to. Both are generated
from the manifest in `packaging/`, so there is no second copy to keep in step.
The screenshots a software centre shows are served from this repository at the
tag, since a branch moves and what Flathub keeps is whatever it fetched at the
time. `./release.sh` rewrites those URLs to the tag it is making.

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

Riplika works and is in use. The subtitle recogniser has been measured over 122 episodes, and disc handling has been run against real discs, damaged ones included. A PC CD-ROM matched Redump byte for byte. A scratched PlayStation disc was caught by its own C2 flags before it could pass as a good dump. The Flatpak builds in CI on every push, and the window is functional but has not had a designer near it.

Known gaps: Blu-ray is untested, so nothing here claims to handle it. Inside the Flatpak, a library configured somewhere other than the three default folders is not reachable, because the sandbox has no way for the application to ask for it, and MakeMKV cannot be bundled so the option to use it is not shown there.

## Licence

Riplika is MIT licensed; see [LICENSE](LICENSE).
