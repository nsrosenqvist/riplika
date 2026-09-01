<div align="center">

<img src="data/icons/hicolor/scalable/apps/com.nsrosenqvist.Riplika.svg" width="120" alt="">

# Riplika

**Put a disc in and get library files out, named and tagged.**

</div>

![Riplika, having identified a disc](data/hero.png)

Doing this by hand takes a rip, a transcode, and then work on the subtitles, which arrive as pictures and have to become text before a media server will stop re-encoding the film on every play. Little of it changes from disc to disc.

Riplika asks the drive what it is holding and runs the pipeline that suits it.

| | Produces | Named from |
|---|---|---|
| **DVD** | Encoded video, subtitles as text | TMDB, TVmaze or Wikidata, checked against the disc's own structure |
| **Music CD** | FLAC or MP3, tagged, with cover art | MusicBrainz, or the disc's CD-Text |
| **Game disc** | A raw image, or a cue sheet and one file per track | Redump datfiles, matched on what it hashes to |

## What it does

**Subtitles become text, exactly.** DVD subtitles are pictures. Riplika matches the bitmaps against a table of known letters, which gives 99% character accuracy and cue timings taken from the source. This is the most interesting part of the project and [has its own document](docs/subtitles.md).

**Identification is argued, and shows its reasoning.** A volume label is a hypothesis. The shape of the disc is evidence: a "play all" title decomposes into which titles are episodes and what order they run in, one long title says the disc is a film, and runtimes are checked against the catalogue. You see why before anything is read.

**Damage is reported, not written out.** A game dump is verified against the drive's C2 error pointers for audio and the sectors' own error detection for data, so a bad read is told apart from a disc nobody has catalogued. When a dump nearly matches, Riplika says which disc and which tracks disagreed.

**Nothing about the picture is guessed.** Crop and inverse telecine are measured from the video. Quality has three settings and everything else follows from the disc, which is a deliberate limit; anyone who wants to tune an encode is better served by the tools that already do that.

**A window and a command line**, doing the same things.

## Install

The Flatpak carries everything it needs:

```sh
flatpak-builder --user --install --force-clean build packaging/com.nsrosenqvist.Riplika.yml
```

For a native build you need ffmpeg and cdparanoia:

```sh
sudo pacman -S ffmpeg libdvdcss libdvdread libdvdnav cdparanoia
cargo build --release
```

```sh
riplika drives                          # what is connected, and what is in it
riplika rip --season 6 --disc 1         # the whole pipeline
riplika rip-cd --format flac
riplika rip-game
riplika-gui                             # or the window
```

`--dry-run` prints what would be produced and stops without reading anything.

## Documentation

| | |
|---|---|
| [Reading a disc](docs/discs.md) | which software reads it, damaged discs, packaging, hardware notes |
| [Working out what a disc is](docs/identifying.md) | catalogues, episode mapping, what to take |
| [Encoding](docs/transcoding.md) | quality tiers and the decisions behind them |
| [Subtitles](docs/subtitles.md) | deterministic recognition, and how well it works |
| [Configuration](docs/configuration.md) | where things live, preferences |
| [AGENTS.md](AGENTS.md) | architecture, for anyone changing the code |

## Status

Works and is in use. The subtitle recogniser has been measured over 122 episodes, and disc handling has been run against real discs, damaged ones included: a PC CD-ROM matched Redump byte for byte, and a scratched PlayStation disc was caught by its own C2 flags before it could pass as a good dump.

Blu-ray has never been tested against a disc. MakeMKV is used as a fallback where it is installed, for a disc the free reader cannot manage, and it cannot be bundled in the Flatpak.

## Licence

Riplika is MIT licensed; see [LICENSE](LICENSE).
