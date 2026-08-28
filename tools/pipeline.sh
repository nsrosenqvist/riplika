#!/bin/bash
# Transcode ripped titles, OCR their subtitles, embed them, and tag the result.
#
# Expects a mapping file of lines:
#   <source-stem>|<episode label>|<part number>|<title>|<air date>|<extended yes/no>
set -u
RIP="$1"; OUT="$2"; MAP="$3"; TABLE="$4"; SEASON="$5"; TOTAL="$6"
RIPPER="$(dirname "$0")/../target/release/ripper"
mkdir -p "$OUT/extras"

while IFS='|' read -r stem label part title date ext; do
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
  HandBrakeCLI -i "$src" -o "$dest.tmp.mp4" -e x264 -q 20 --encoder-profile main \
      --detelecine -a 1 -E av_aac -B 160 --mixdown stereo -s 1,2 --optimize \
      >/dev/null 2>&1 || { echo "  TRANSCODE FAILED"; continue; }

  # recognise the English VobSub and embed it as a default text track
  srt="$dest.srt"
  if "$RIPPER" ocr "$dest.tmp.mp4" --table "$TABLE" --stream 0 -o "$srt" >/dev/null 2>&1; then
    ffmpeg -v error -y -i "$dest.tmp.mp4" -i "$srt" \
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
  ffmpeg -v error -y -i "$dest" -c copy \
    -metadata title="$disp" \
    -metadata show="Parks and Recreation" \
    -metadata season_number="$((10#$SEASON))" \
    -metadata episode_sort="$((10#$part))" \
    -metadata episode_id="$disp" \
    -metadata date="$date" \
    -metadata media_type=10 \
    "$dest.tagged.mp4" && mv -f "$dest.tagged.mp4" "$dest"

  echo "  done: $(ffprobe -v error -show_entries format=duration -of csv=p=0 "$dest")s"
done < "$MAP"
echo "PIPELINE COMPLETE"
