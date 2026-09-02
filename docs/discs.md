# Reading a disc

## Reading discs

MakeMKV is proprietary, so it is worth knowing exactly what it is needed for.

| | DVD | Blu-ray |
|---|---|---|
| free software | **yes**, and it is the default | **no** |
| what does it | libdvdread + libdvdnav + libdvdcss, through ffmpeg's `dvdvideo` demuxer | MakeMKV |

`--reader dvd` needs nothing beyond an ffmpeg built `--enable-libdvdnav --enable-libdvdread`, which is the usual packaging. `--reader makemkv` is still there. `auto`, the default, looks for a `VIDEO_TS` directory and picks the free path when it finds one.

**Blu-ray has never been tested here.** What follows about it is why it is hard, not a claim that Riplika handles it. Nobody has put one in the drive, so treat the Blu-ray column as an explanation of the landscape.

Verified on a Parks and Recreation season 7 disc: the free reader finds the same titles MakeMKV does - seven 21-minute episodes, the 43-minute and 107-minute play-alls, the 27-minute extended cut and the extras.

## Why Blu-ray is different

`libbluray` reads the structure but does no decryption. That needs `libaacs`, which is free software shipping no keys - you supply a `KEYDB.cfg` of volume unique keys assembled by third parties, which is always incomplete and always behind - and `libbdplus`, which needs conversion tables, for the BD+ titles that most studios use. AACS 2.0 on UHD discs is out of reach entirely. MakeMKV implements both and is maintained against new releases, and nothing free is close. This is a real gap, not a packaging inconvenience.

## What MakeMKV still does that the free path cannot

Two things, and both are real.

**Region.** libdvdcss does the player-key exchange with the drive, and an RPC-2 drive can refuse it when the disc's region does not match the region the drive is set to. A drive can only be set to one region, and only a few times. MakeMKV talks to the drive itself and does not care, which is why it is the one that copes with a shelf holding both Region 1 and Region 2 discs.

**Damage.** MakeMKV retries unreadable sectors and carries on past the ones it cannot get. libdvdread mostly gives up. That covers scratched discs and also the deliberately corrupt sectors some copy protections write to break naive rippers.

Losing either of those *silently* would be worse than not having the free path at all, because both fail the same way, with a scan that succeeds and returns fewer titles than the disc holds. So `auto` watches for the signature - `Error cracking CSS key`, unreadable sectors - abandons the scan the moment it appears, and hands the disc to MakeMKV:

```
reader: ffmpeg dvdvideo (makemkv in reserve)
  the free reader could not read this disc fully:
    libdvdcss could not decrypt parts of the disc (/VIDEO_TS/VTS_06_1.VOB) -
    titles in those parts are missing from this scan
  handing it to makemkv, which works around this
```

With `--reader dvd` there is no fallback, so it stops with the same complaint rather than pretending. Keep MakeMKV installed; the free path just means it is not on the critical path for an ordinary DVD.

## What of MakeMKV's fault tolerance is reimplemented

Some of it, and the parts that are reachable are worth having. What follows is what the free reader now does; the honest ceiling is in the next section.

**Every decryption method, not just one.** libdvdcss has three, and they are answers to different problems rather than alternatives:

| method | what it does | what defeats it |
|---|---|---|
| `key` | asks the drive to do the CSS handshake | an RPC-2 drive whose region does not match the disc |
| `disc` | cracks the disc key without the drive | discs resistant to cracking |
| `title` | cracks each title key from the data | slowest; same resistance |

Only the first needs the drive's cooperation, so `disc` is what reads a Region 1 disc in a drive set to Region 2 - the case a mixed-region shelf runs into. Each is tried in turn, and a failing attempt is abandoned after a single probe, so the cost of trying is small. The one that worked is recorded.

**Checking the length of what came back.** This is the most valuable of the three, because the failure it catches is silent. A damaged sector does not usually make ffmpeg fail - it makes it stop, exit zero, and leave a file that plays and is merely missing its ending. Every ripped title is measured against the duration the scan reported, and anything more than 2% short is treated as damage rather than accepted.

**Salvaging a title a chapter at a time.** When every method stops early, the title is read again chapter by chapter and whatever survives is joined back together, so one scratch costs one chapter rather than the whole episode:

```
title 41 is damaged; salvaging by chapter
title 41 recovered without chapter 3
```

That is a coarse version of what `ddrescue` does. The granularity is chapters because that is the smallest unit the demuxer can be asked for; MakeMKV works at the sector level and loses correspondingly less.

## Rescuing a damaged disc

`riplika rescue` is GNU ddrescue's algorithm applied to a DVD. Its central insight is not obvious and is worth stating: **read the easy data first**. A failing disc may not survive an hour of retries, and every unreadable sector costs seconds while the drive retries internally - so working on the damage before the good 99% is secured is a way to end up with nothing.

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

**It decrypts as it reads.** A raw image of a CSS disc is encrypted, and decrypting it afterwards means cracking the keys - which failed for six video title sets of the disc this was built against. Reading through libdvdcss uses the drive's key exchange while the drive is still in front of it, and `DVDCSS_READ_DECRYPT` clears the scrambling bits, so the image needs no keys. libdvdcss is loaded at runtime, so everything else still works without it.

**It reads only what you ask for.** The video title set IFOs give each program chain's cell extents, so a rescue can cover the episodes and skip the menus. The play-alls share the episodes' sectors, so they cost nothing:

```
chain  1:  43m01   1.57 GB     <- play-all
chain  2:  21m30   0.79 GB     <- episode
...
chain  9: 107m42   3.67 GB     <- play-all
episodes (chains 2-8): 1 run, 5.24 GB
```

Verified end to end against a real disc: rescuing one episode reads 0.79 GB, writes a 752 MB sparse image spanning the disc's full 7.7 GB address space, and that image demuxes to the right title - `mpeg2video`, `ac3` English, `dvd_subtitle` English and Spanish, 1290.73 s against the disc\'s 21m30.

**It is resumable.** A map file is written beside the image in ddrescue's own format, recording the state of every sector. Stop it, clean the disc, run it again: only what is still missing is attempted. That is the difference between a scratched disc being a lost cause and being a second attempt.

**What is not encrypted must not be decrypted.** `DVDCSS_READ_DECRYPT` descrambles whatever it is handed, and a DVD sector's payload starts at byte 128 - so decrypting the volume descriptors leaves their first 128 bytes intact and turns the rest into noise. The image then still mounts as a filesystem and still will not open as a DVD, which is a confusing way to fail. Only the video objects are read with decryption.

**Holes become padding, not zeros.** Unrecoverable sectors are filled with MPEG program-stream padding packets, which a demuxer skips. Zeros where a packet header belongs make it treat the rest of the stream as corrupt, so the choice is between losing a moment and losing the remainder of the file.

## What is still not reimplemented

- **Making the drive try harder.** MakeMKV sets the drive's read-retry behaviour over raw SCSI (`MODE SELECT`), which often turns an unreadable sector into a slow one. Doable through `SG_IO`, untestable here, and easy to get wrong in ways that hang a drive.
- **Protection-specific workarounds.** Arccos and RipGuard deliberately write unreadable sectors and bogus structures. Handling them means recognising each scheme, which is reverse engineering per title, and is MakeMKV's real moat.
- **AACS and BD+.** Not a fault-tolerance question - there is simply no free implementation with keys. See above.

MakeMKV remains better on a disc whose damage is deliberate rather than accidental, and it asks the drive to try harder than we know how to. For ordinary damage - a scratch, a smudge, a disc that reads on the third attempt - the rescue path now covers it.

## Two things that make the free DVD path work

Both were found by running it against a real disc, and both fail in the same dangerous way, with a scan that succeeds and returns a season holding no episodes.

**`DVDCSS_METHOD=key`.** libdvdcss defaults to cracking title keys by brute force. On this disc that failed for exactly the VTSs holding the episodes:

```
libdvdnav: Error cracking CSS key for /VIDEO_TS/VTS_06_1.VOB (0x000651ea)
```

The scan then returns the extras and nothing else, which looks like a disc that simply has no episodes on it. Asking for the proper player-key exchange with the drive decrypts everything, and is faster.

**The title count comes from the disc.** DVD title numbering is not contiguous. This disc has content at titles 2-19 and again at 39-58, with a seventeen-title hole in between, so any stop-after-N-empty-titles rule gives up in the hole - losing every episode, since they are at 41-47. Probing all 99 instead is safe but slow. So `rip/iso.rs` walks ISO 9660 to `VIDEO_TS.IFO` and reads the title table, which costs three sector reads and gives an exact count:

```
number of titles: 58
  title 41: chapters=5  vts=11 ttn=2      <- episode
  title 48: chapters=21 vts=11 ttn=9      <- the play-all
```

That table also carries chapter counts, and the demuxer reports chapter *durations* - which MakeMKV's scan does not. Those are what decompose a play-all, so a disc can in principle be sorted out before anything is read, and the play-all title never ripped at all. On this disc that is two and a half hours of redundant reading.


## Telling what is in the drive

The volume recognition sequence starts at sector 16 and runs until a terminator. A disc with an ISO 9660 bridge puts `CD001` in the first of those sectors, which is where a DVD-Video's `VIDEO_TS` directory is found; a pressed PC-DVD often has no ISO 9660 at all and puts UDF's `BEA01` there instead. Reading only sector 16 and requiring `CD001` therefore called The Sims 3 expansion an empty drive - the window said "no disc" with a disc in it, and there was no way off the landing page.

The whole sequence is read now. An ISO descriptor anywhere in it is used as before; failing that, a UDF marker means there is a filesystem here, and one this does not read is exactly what a data disc is. A disc with neither is still an empty drive, which is the distinction worth keeping: the point is not to call every unreadable disc a data disc.

**What such a disc is called comes from UDF too.** The anchor at sector 256 points at the volume descriptor sequence, and the logical volume descriptor in it holds the name the desktop mounts the disc under; the primary descriptor carries one as well, as a fallback. Without reading it a Sims 3 expansion arrived as "unnamed disc" with its name printed on the box - and on the mount point three lines up in `mount`, because the kernel reads UDF perfectly well. Names are `dstring`s: a compression byte, the characters, and the used length in the last byte of the field, one byte a character or two big-endian.

## Opening automatically when a disc goes in

A desktop can offer an application when a disc is inserted, and the mechanism is the same one that opens a file: the volume is mounted and matched to a content type, and applications that declare it are offered.

Riplika declares the ones it can actually read:

```
MimeType=x-content/video-dvd;x-content/video-vcd;x-content/video-svcd;x-content/audio-cdda;
Exec=riplika-gui %u
```

The `%u` is the important half. What the desktop hands over is the *mount point* - `file:///run/media/someone/PARKS_AND_RECREATION` - because that is what it knows about, while everything here works from a device. So the window reads the kernel's mount table and works back, then selects that drive rather than guessing, which is the only way to be right on a machine with two.

Both encodings have to be undone to get there, and they are not the same encoding: the URI percent-encodes a space as `%20` and the mount table escapes it in octal as `\040`.

To register it after building:

```sh
install -Dm755 target/release/riplika-gui ~/.local/bin/riplika-gui
install -Dm644 data/com.nsrosenqvist.Riplika.desktop ~/.local/share/applications/
update-desktop-database ~/.local/share/applications
```

**Do this or install the Flatpak, not both.** Both carry the same application id, and `$XDG_DATA_HOME` is searched before the Flatpak's export directory, so the file written above wins every launch from the desktop - silently, and for as long as it exists. A native build left over from an earlier afternoon is then what the icon starts, which looks like a bug that was fixed coming back. Remove `~/.local/share/applications/com.nsrosenqvist.Riplika.desktop` to hand the id back to the Flatpak.

Then it appears in GNOME Settings under Removable Media, or in the prompt a desktop shows when a disc goes in, beside whatever else is installed:

```
$ gio mime x-content/video-dvd
Registered applications:
	com.nsrosenqvist.Riplika.desktop
	org.videolan.VLC-opendvd.desktop
	fr.handbrake.ghb.desktop
```

## What a datfile can and cannot answer

A Redump entry carries a name, a category, a description, and one `rom` element per file with its size, CRC-32, MD5 and SHA-1. That is all of it. There is no volume label and no serial, so a disc cannot be looked up before it is read - `Sims3SP01` matches nothing, and the only question the database answers is "which disc has these hashes". That is why a game is named after the dump rather than before it, and why the identification page for one offers no search box.

The datfiles for systems there is no copy of yet are fetched, rather than none at all if there is any. A fetch that got PlayStation and failed on the rest left the folder non-empty, and asking whether it was empty called the job done for good: every PC disc afterwards was checked against a database of PlayStation discs, and came back unknown for a reason that had nothing to do with the disc. Which system a datfile belongs to is read from its header and compared on the letters alone, because the name shown here is "Sony PlayStation" and the header says "Sony - PlayStation".

A finished dump is filed under the system its datfile names - "Sony - PlayStation", "IBM - PC compatible" - because that is the one grouping the database actually knows and the one an emulator's library expects. A disc that came to a single image is that file, sitting there among its neighbours. A disc that came to several gets a folder of its own first: a PlayStation disc runs to a cue sheet and however many tracks, and two of those ripped one after the other left twenty-eight files in one folder with nothing but the name at the front of each to say which disc they belonged to.

## Flatpak

`packaging/com.nsrosenqvist.Riplika.yml` builds the window against `org.gnome.Platform`, bundling libdvdcss, libdvdread, libdvdnav, ffmpeg, cdparanoia and Tesseract.

Three things about it are worth knowing before you rely on it.

**It is DVD-only.** MakeMKV cannot be bundled - it is proprietary, so redistributing it is not ours to do - and reaching the host's copy would need `--talk-name=org.freedesktop.Flatpak`, which lets the application run anything at all on the host. Claiming to be sandboxed after that would be a lie. The Flatpak is coherent precisely because the DVD path needs nothing proprietary; for Blu-ray, use the native build.

**Subtitles can be read in eighteen languages.** A disc whose lettering is not already in a glyph table has one built by reading it, and that needs Tesseract data for the language: Czech, Danish, Dutch, English, Finnish, French, German, Greek, Hungarian, Icelandic, Italian, Norwegian, Polish, Portuguese, Russian, Spanish, Swedish and Turkish are bundled. A track in any other language keeps its bitmaps rather than being read with somebody else's alphabet, which teaches the shared table nonsense. Using a table that already fits is not limited this way, and a native build reads whatever Tesseract data is installed. See [subtitles](subtitles.md#which-languages-a-disc-can-be-read-in).

**`--device=all` is required.** Flatpak has no narrower permission for an optical drive: `--device=dri` is the GPU and there is nothing for `/dev/sr0` alone. Reading a disc means granting access to devices generally.

It builds, and building it found four things wrong with it that reading it never would have:

- VideoLAN moved libdvdcss, libdvdread and libdvdnav to **meson**; the manifest assumed autotools and died on the first module.
- libdvdread's option is `libdvdcss`, not `dvdcss`. Worth setting rather than leaving to the default, too: libdvdread otherwise loads libdvdcss by name at runtime, which inside a sandbox depends on it being on the loader path.
- **The GNOME SDK carries no x264**, and it is not optional here - every quality tier is an x264 CRF, so an ffmpeg without it cannot encode anything.
- `flatpak-builder` is itself a flatpak, so `--repo=/tmp/...` writes into its own sandbox and the host never sees it.

Verified in the sandbox afterwards: the `dvdvideo` demuxer is present, `libx264` is present, and `libdvdcss.so.2` is where libdvdread expects it. Those three are what decide whether it can read a disc and encode it at all.

It has subtitle recognition, which took removing a dependency to get. That needed `mkvextract` to split a VobSub track into its `.idx`/`.sub` pair - ffmpeg reads that format but has no muxer to write it - and MKVToolNix requires Qt for every one of its tools, not only its window. Rather than bundle Qt for one binary, the track is now read out of Matroska directly, which dropped the dependency from every build. See [subtitles](subtitles.md#reading-the-track).

### Two things about running it

Its data directory is the sandbox's own, so a glyph table installed on the host is not visible to it. Put one in `~/.var/app/com.nsrosenqvist.Riplika/data/riplika/` - or build one from inside the flatpak, which puts it there anyway.

A flatpak inherits the host's `LANG` but its runtime carries only the languages it was configured for, so an uncommon locale - `en_SE`, say - arrives with nothing to set it to. Riplika borrows an installed locale and keeps the language in `LANGUAGE`, which is enough for gettext to still read the right catalogue, so this costs nothing but a missing territory's date and number formats. `flatpak config --user --set languages "en;sv"` installs the real thing.

```sh
flatpak run org.flatpak.Builder --user --force-clean --repo=repo build packaging/com.nsrosenqvist.Riplika.yml
flatpak build-update-repo repo
flatpak remote-add --user --no-gpg-verify riplika-local "file://$PWD/repo"
flatpak install --user riplika-local com.nsrosenqvist.Riplika
```

```sh
flatpak install org.gnome.Platform//50 org.gnome.Sdk//50 \
                org.freedesktop.Sdk.Extension.rust-stable//25.08
python3 flatpak-cargo-generator.py Cargo.lock -o packaging/cargo-sources.json
flatpak-builder --install --user build packaging/com.nsrosenqvist.Riplika.yml
```

## Hardware notes

Things measured on real discs and real drives here, kept because none of them is recoverable by reading the code. Most have a matching comment where they bite; this is the list.

**A FakeRunner accepts commands real ffmpeg refuses.** Every test here drives one, which is what makes them fast and offline, and it means a command can be well-formed to the tests and rejected by ffmpeg. That is not hypothetical: files are written to a `.part` path while being made, ffmpeg picks its muxer from the extension, `.part` is not one, and every transcode failed with "Invalid argument" while the whole suite passed. Anything about *how ffmpeg reads a command* needs a real run to confirm - `riplika process` on a directory of two short files takes seconds.

- **`pkill -f riplika`** matches the shell running it. Use `pkill -x`.
- **The GUI runs jobs on worker threads inside itself**, so there is no separate process to look for, and restarting it kills a running rip.
- **An optical drive can wedge in `D` state**, uninterruptible; not even `kill -9` reaches it until the read returns. Ejecting clears it.
- **A DVD is ISO 9660 *and* UDF.** libdvdread reads the UDF. Copying only the ISO structures yields an image that mounts and will not open as a DVD.
- **`DVDCSS_READ_DECRYPT` descrambles whatever it is given**, and a sector's payload starts at byte 128, so decrypting the volume descriptors leaves their first 128 bytes intact and turns the rest into noise.
- **DVD title numbering is not contiguous.** One disc here has content at titles 2–19 and again at 39–58. Read the count from the disc, never infer it.
- **MakeMKV reports chapter *counts*; the free reader reports chapter *durations*.** Only the durations can decompose a play-all, so anything that depends on them must handle their absence.
- **A CD sector is 2352 bytes, and `/dev/sr0` gives you 2048 of them.** The drive checks the error correction, keeps the user data and discards the sync pattern, header and ECC, and the discarded part is what Redump hashes. An image of cooked sectors is a perfectly good image that matches nothing, and the failure reads as "this disc is not in the database". CDs go through `READ CD` for that reason; DVDs and Blu-rays have no such distinction.
- **A CD's length comes from its table of contents, not the block device.** On the disc measured here the ISO volume says 339,313 sectors and the lead-out says 339,463. Redump quotes the whole track.
- **MusicBrainz allows one request a second** and refuses the rest. An empty result therefore means either "never heard of it" or "never asked", and only one of those is worth telling somebody about. `Found::lookup_failed` keeps them apart.
- **Reading a CD raw is about a fifth the speed of reading it cooked** (0.9 MB/s against 4-plus on the drive here), because the drive cannot read ahead the same way. That is the price of an image that can be verified.
- **Do not benchmark the drive while something else is using it.** Doing that here produced a figure seven times too low and nearly a wrong conclusion with it.
- **A disc with audio tracks on it is not necessarily a music disc.** Which track comes *first* decides. Mixed Mode puts data first and its soundtrack after, which is a PlayStation game, and reading "has audio tracks" as "is an album" filed one as music and sent its disc id to MusicBrainz. An enhanced music CD does the opposite: audio first, data track last, in a session of its own.
- **A track's file starts at its pregap, not where the table of contents says the track starts.** The gap is usually 150 sectors and assuming that is how a ripper gets most discs right and some wrong: on the Moto Racer disc here, track two's pregap is 225. Read it from the Q subchannel.
- **The drive can repeat a subchannel answer.** At that same boundary two consecutive sectors came back with identical Q data, so the boundary looked one sector early and the pregap computed as 226, which is not a whole count of anything. The *countdown* field in the same answer was right. Trust the countdown, not the address.
- **Every drive returns audio displaced by a fixed number of samples.** A data sector carries its own address so the drive syncs exactly and a data track comes off byte-perfect; audio has no such marker. The drive here is **+669 samples**, found by searching for the shift that reproduced a known track's checksum. Uncorrected, a rip plays perfectly and matches nothing, being out by a fifteenth of a second. The correction is `read_offset` in the preferences, and defaults to zero because a wrong correction is worse than none.
- **This drive cannot read past the lead-out.** LBA 232013 reads on the Moto Racer disc, 232014 is refused. With a positive read offset the last few hundred samples of the final track lie out there, so they become silence and the last track can never match a datfile, which the tool says outright rather than reporting the disc as unknown. A drive that can overread has no such limit.
- **The drive lies about how much it sent.** The USB bridge here reports a residual of 88, 96, 128, 176, 192 or 256 bytes on transfers that plainly completed: the buffer comes back filled to the last byte and every sector's sync pattern sits at its exact stride. Cutting the answer at the residual leaves a part sector, which every caller reads as a failed read, so good two-sector reads were being thrown away, and the pregap scan was falling back to its slow, stale-answer path against a drive that had answered correctly. `scsi::read_sectors` disbelieves any residual shorter than one sector. This is also where the phantom "2640-byte C2 sector" came from, and two wrong conclusions with it.
- **C2 error pointers are how you find out an audio track is wrong.** `READ CD` with flag byte `0xFA` appends 294 bytes to each sector, one bit per byte, saying which the drive had to guess at. The sector is then 2646 bytes. On the damaged disc here, two passes over one track were byte-identical for 7568 sectors and then differed in half of the rest, and **the first sector the drive flagged was the very sector at which the passes first differed**. That is the tripwire: a track with no C2 flags at all can be read once and believed, which halves the work on a healthy disc.
- **C2 is a tripwire, not a map.** In the same measurement 2438 flagged sectors differed between passes and 7296 differing sectors were never flagged, every one of them within seven sectors of one that was. It says reliably that a disc is damaged. It does not say exactly where, so do not use it to decide which sectors to re-read.
- **Two dumps disagreeing does not mean the drive is unreliable.** That was the conclusion here for a while, and it was wrong. It was not drift (overlapping reads matched at exactly position 0), not sector damage in isolation (a sector inside a differing run read identically four times), and not speed (two passes at 4x still differed in 3.7 MB). It was a scratched disc, and C2 said so as soon as it was asked properly.
- **A data sector can be checked without a drive or a database.** It carries a CRC over its own contents - `crates/core/src/edc.rs`. That is the difference between "no datfile has this disc" and "this read is wrong", which the tool could not tell apart before and which call for opposite responses. The Cool Boarders 2 disc here turned out to be an uncatalogued pressing: identical audio to Redump's Europe entry, different data track, and all 61,036 checkable sectors of it sound.
- **The EDC polynomial is 0x8001801B written the usual way round**, and a least-significant-bit-first CRC needs it reversed, as 0xD8018001. Quoting it as written and shifting right anyway fails every sector on the disc rather than a few, which at least announces itself. Mode 1 covers bytes 0..2064, Mode 2 Form 1 covers 16..2072, and a Form 2 sector's EDC is optional - zero means there is none, and a track of those has not been verified however many sectors came back.
- **This drive reports no C2 for data sectors at all.** 55,625 of them came back unflagged on a disc scratched enough that the drive refuses whole reads elsewhere. C2 is worth asking for on audio and worth nothing on data - which is where EDC comes in.
- **A disc is only identified when every track matches.** A boundary cut one sector wrong leaves the first file perfect and shifts everything after it, so checking one track proves nothing.

### Working against a real drive

There is usually a disc in `/dev/sr0`. Useful, but:

- a full scan probes every title and takes minutes, so run it in the background
- **do not truncate the output through `tail`**. Doing that once produced a confident and completely wrong diagnosis, because extras sort last and the episodes were above the cut.
- `~/Videos` is a real library. Write to a scratch directory.
