# Riplika

Riplika reads a disc and writes files a media server can use, with the naming
and tagging already done.

Doing the same thing by hand means a rip, then a transcode, then replacing the
subtitle pictures with text so the server does not re-encode the film on every
play. Little of that varies between discs, which is what makes it worth
automating.

```
Season 06/
  Parks and Recreation - S06E01 - London.mp4
  Parks and Recreation - S06E02 - London.mp4
  extras/
    Parks and Recreation - S06E04 - Doppelgangers - Extended Cut.mp4
```

There is a window and a command line, and both do the same things.

Quality is High, Medium or Low, and the rest of what another tool would ask you
is measured from the disc instead. The narrow set of choices is deliberate.
Anyone who wants to sit over an encode and tune it is better served by the tools
that already do that well.

## Three kinds of disc

Which pipeline runs is decided by asking the drive, not by asking you.

| In the drive | What comes out | Named by |
| --- | --- | --- |
| DVD or Blu-ray | Encoded video with text subtitles | TMDB, TVmaze or Wikidata |
| Music CD | FLAC or MP3, tagged, with cover art | MusicBrainz, or the disc's own CD-Text |
| Game disc | A raw image, or a cue sheet and one file per track | Redump datfiles |

### Video

DVDs need nothing proprietary, since libdvdread and libdvdcss do the reading through ffmpeg. MakeMKV covers Blu-ray, and DVDs the free path cannot manage, such as a disc whose region does not match the drive.

The volume label says what a disc claims to be, while its structure says what is actually on it. A "play all" title replays the episodes back to back, so decomposing it recovers which titles are episodes and what order they belong in. That reading is checked against a catalogue and shown to you before anything is read.

A disc holding one long title is a film rather than a season, which the identification weighs alongside the name, and the pages then stop asking a film which season it is.

Crop and inverse telecine are measured rather than assumed, as are frame rate and pixel aspect. Of the three quality tiers, the middle one comes to about 170 MB for a 21-minute episode and is hard to tell from the disc.

DVD subtitles are pictures, and Riplika turns them into text exactly rather than statistically, at 99% character accuracy, with cue timings that match the source by construction. A text subtitle also stops a media server burning the picture into the video and re-encoding it on every play.

### Music

The table of contents gives a disc id, which MusicBrainz answers with the pressing rather than a guess at the album. A disc it has never seen falls back to CD-Text, and a disc neither of them knows can be searched for by name. Cover art comes from the Cover Art Archive and is embedded as a front cover.

Audio is read with cdparanoia, which re-reads and checks rather than trusting the first answer. FLAC and MP3 are both available at three quality tiers, with the tag vocabulary each format expects. Filenames follow a pattern you can set, and a slash in that pattern makes a folder.

### Games

A game disc is copied rather than encoded, at the 2352 bytes a sector actually holds. A disc with audio tracks becomes one file per track and a cue sheet, because that is what such a disc is, and a flat image of one matches no database.

Redump datfiles name the result from what it hashes to. When nothing matches exactly, Riplika says which disc it came closest to and which tracks disagreed, so a damaged read can be told apart from a pressing nobody has catalogued yet. Datfiles are fetched as soon as a game disc is recognised.

Two checks run while a disc is being dumped. The drive's C2 error pointers say which audio sectors it had to guess at, since audio carries no error correction a host can verify. Data sectors carry their own error detection, which is checked directly, and a track that fails it is a bad read rather than an unknown disc.

## Getting started

The Flatpak carries everything except MakeMKV:

```sh
flatpak-builder --user --install --force-clean build packaging/com.nsrosenqvist.Riplika.yml
```

For a native build you need ffmpeg, cdparanoia, and MakeMKV if you want Blu-ray. On Arch:

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
What is still needed is at least one screenshot: Flathub requires one for any
graphical application, linked directly and, if it lives in this repository,
from a tag rather than a branch. `data/com.nsrosenqvist.Riplika.metainfo.xml`
says where it goes.

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

Riplika works and is in use. The subtitle recogniser has been measured over 122 episodes. Disc handling has been run against real discs, including damaged ones: a PC CD-ROM matched Redump byte for byte, and a scratched PlayStation disc was caught by its own C2 flags rather than passing as a good dump. The Flatpak builds in CI on every push. The window is functional but has not had a designer near it.

Known gaps: Blu-ray needs MakeMKV, and no free reader is worth pretending otherwise about. Inside the Flatpak, a library configured somewhere other than the three default folders is not reachable, because the sandbox has no way for the application to ask for it.

## Licence

Riplika is MIT licensed; see [LICENSE](LICENSE).
