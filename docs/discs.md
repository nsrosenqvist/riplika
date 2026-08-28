# Reading a disc

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

## Why Blu-ray is different

`libbluray` reads the structure but does no decryption. That needs `libaacs`,
which is free software shipping no keys - you supply a `KEYDB.cfg` of volume
unique keys assembled by third parties, which is always incomplete and always
behind - and `libbdplus`, which needs conversion tables, for the BD+ titles that
most studios use. AACS 2.0 on UHD discs is out of reach entirely. MakeMKV
implements both and is maintained against new releases, and nothing free is
close. This is a real gap, not a packaging inconvenience.

## What MakeMKV still does that the free path cannot

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

## What of MakeMKV's fault tolerance is reimplemented

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

## Rescuing a damaged disc

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

## What is still not reimplemented

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

## Two things that make the free DVD path work

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
