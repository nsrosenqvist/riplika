# Encoding

## Transcoding

HandBrake is a wrapper around the same libx264; encoding a title both ways with matching settings produces byte-identical x264 parameter strings. What HandBrake actually supplies is the preprocessing decisions, so those are made explicitly:

| | |
|---|---|
| crop | detected from the middle of the film, not the opening titles |
| inverse telecine | applied only when the duplicate frames are really there |
| frame rate | pinned constant |
| pixel aspect | snapped to the four ratios a DVD can have, then scaled away |
| subtitles | recognised per language, bitmaps dropped once they are |
| faststart | moov atom at the front |

**The picture is written at the size it is meant to be seen at.** A DVD stores 720 across and means it to be shown at 1024, and every player scales it; what they do not agree on is where a subtitle goes. GNOME Videos lays one out in the stored width and stretches the picture underneath it, so the text sits a third of the way across instead of centred. VLC gets it right. The disagreement only exists because the file asks to be stretched, so the scaling is done once, here, and there is nothing left to disagree about.

Measured over two minutes of *Cloudy with a Chance of Meatballs* at CRF 20: 14.5 MB stored anamorphic against 18.4 MB square, so about a quarter more. That buys a file that is the shape it claims to be in anything that opens it. For 4:3 material the same correction is a reduction - 720 becomes 640 - and the file gets smaller.

Three tiers each for picture and sound. DVD is 720x480 MPEG-2 at 4-6 Mb/s, so the useful range is narrow: by CRF 18 the encode is transparent against the source and further bits mostly track MPEG-2's own noise, while below CRF 23 SD detail goes quickly.

| tier | picture | sound |
|---|---|---|
| high | CRF 18, ~240 MB an episode | original AC3, untouched |
| medium | CRF 20, ~170 MB - the sweet spot | AAC 160k stereo |
| low | CRF 23, ~107 MB | AAC 96k stereo |

`--audio high` keeps the original 5.1 with no downmix and no second lossy generation. The cost is browsers, which cannot decode AC3 at all, so a web client makes the server transcode; `--dual-audio` adds an AAC stereo track beside the original for exactly that case.

`--languages english,swedish` filters audio and subtitles together, and the order is meaningful: the first language listed ends up first *and* carries the default flag, because players go by the flag rather than the order. A subtitle filter matching nothing is honoured exactly - a subtitle you cannot read is worse than none - while an audio filter matching nothing keeps everything, since a file with no audio is simply broken.

## Things that were wrong before, and the tests that hold them down

Each of these produced a file that looked correct:

- **A blanket `-map 0`** carried the DVD's `bin_data` stream through and accumulated a duplicate on every pass. Mapping is explicit, `-dn` is set, and chapters come over separately via `-map_chapters 0`.
- **`-c copy` with no subtitle `-map`** wrote files with no subtitles in them.
- **ffmpeg reads stdin** looking for keypresses, so inside a loop that reads a list it eats the rest of the list. One run processed one episode out of eight and reported success. Every child now gets a closed stdin, in one place.
- **`fieldmatch,decimate` applied blind** threw away one real frame in five (24,772 kept of 30,964) because the source was *soft* telecine, already resolved to 23.976 by the decoder. Detection measures what the decoder produces rather than what the container declares.
- **Writing straight to the destination** left a truncated file at the final path when a run was interrupted, which the next run counted as a finished episode and skipped. Encoding goes to `.part` and renames on success.
- **Frames captured as text.** Perceptual hashing read raw greyscale through a UTF-8 conversion, which replaces invalid bytes with U+FFFD and changes the length, so no extended cut ever matched. Frames go to a file.
