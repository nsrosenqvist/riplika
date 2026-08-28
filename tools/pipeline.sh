#!/bin/bash
# Transcode ripped titles, OCR their subtitles, embed them, and tag the result.
#
# Expects a mapping file of lines:
#   <source-stem>|<episode label>|<part number>|<title>|<air date>|<extended yes/no>
#
# Usage: pipeline.sh <rip-dir> <out-dir> <map> <glyph-table> <season> <total>
#                    [video] [audio] [encode.sh flags...]
#
# Note: subtitles are recognised from the first subtitle track of the encode,
# so if --languages reorders them, the glyph table must match whichever
# language ends up first.
set -u
RIP="$1"; OUT="$2"; MAP="$3"; TABLE="$4"; SEASON="$5"; TOTAL="$6"
shift 6
VQ="${1:-medium}"; [ $# -gt 0 ] && shift
AQ="${1:-high}";   [ $# -gt 0 ] && shift
ENC_ARGS=("$@")          # e.g. --dual-audio --languages english,swedish
RIPPER="$(dirname "$0")/../target/release/ripper"
mkdir -p "$OUT/extras"

# Read the mapping on fd 3: HandBrake and ffmpeg both consume stdin, and would
# otherwise swallow the rest of the list after the first iteration.
while IFS='|' read -r stem label part title date ext <&3; do
  [ -z "${stem:-}" ] && continue
  src="$RIP/$stem.mkv"
  [ -f "$src" ] || { echo "MISSING $src"; continue; }
  if [ "$ext" = "yes" ]; then
    dest="$OUT/extras/Parks and Recreation - S${SEASON}E${label} - ${title} - Extended Cut.mp4"
    disp="$title (Extended Cut)"
  else
    dest="$OUT/Parks and Recreation - S${SEASON}E${label} - ${title}.mp4"
    disp="$title"
  fi

  echo "### $stem -> $(basename "$dest")"
  "$(dirname "$0")/encode.sh" "$src" "$dest.tmp.mp4" "$VQ" "$AQ" \
      ${ENC_ARGS[@]+"${ENC_ARGS[@]}"} </dev/null 2>&1 | sed 's/^/    /' \
      || { echo "  TRANSCODE FAILED"; continue; }

  # recognise the English VobSub and embed it as a default text track
  srt="$dest.srt"
  if "$RIPPER" ocr "$dest.tmp.mp4" --table "$TABLE" --stream 0 -o "$srt" >/dev/null 2>&1; then
    ffmpeg -v error -y -nostdin -i "$dest.tmp.mp4" -i "$srt" \
      -map 0:v:0 -map 0:a -map 1:0 -map 0:s -dn -map_chapters 0 \
      -c copy -c:s:0 mov_text -metadata:s:s:0 language=eng -metadata:s:s:0 title="English" \
      -disposition:s:0 default -disposition:s:1 0 -disposition:s:2 0 \
      -movflags +faststart "$dest" && rm -f "$dest.tmp.mp4"
  else
    echo "  SUBTITLE OCR FAILED - keeping transcode without a text track"
    mv -f "$dest.tmp.mp4" "$dest"
  fi
  rm -f "$srt"

  # metadata, matching the rest of the library
  # -map 0 keeps every stream: without it ffmpeg's default selection drops the
  # text track that was just embedded
  ffmpeg -v error -y -nostdin -i "$dest" -map 0:v -map 0:a -map 0:s -dn -c copy \
    -metadata title="$disp" \
    -metadata show="Parks and Recreation" \
    -metadata season_number="$((10#$SEASON))" \
    -metadata episode_sort="$((10#$part))" \
    -metadata episode_id="$disp" \
    -metadata date="$date" \
    -metadata media_type=10 \
    "$dest.tagged.mp4" && mv -f "$dest.tagged.mp4" "$dest"

  echo "  done: $(ffprobe -v error -show_entries format=duration -of csv=p=0 "$dest")s"
done 3< "$MAP"
echo "PIPELINE COMPLETE"
