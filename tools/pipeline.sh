#!/bin/bash
# Transcode ripped titles, recognise their subtitles, embed them, and tag.
#
# Expects a mapping file of lines:
#   <source-stem>|<episode label>|<part number>|<title>|<air date>|<extended yes/no>
#
# Usage: pipeline.sh <rip-dir> <out-dir> <map> <glyph-table> <season> <total>
#                    [video] [audio] [encode.sh flags...]
#
# Every subtitle track that survives encoding is recognised on its own terms:
# its language decides the ambiguity rules and which wordlist is used. One glyph
# table covers a whole disc - the language tracks on a disc share a font - but
# it has to carry labels for the characters each language actually uses.
#
# Wordlists are looked up as <words-dir>/<code>.txt, e.g. words/swe.txt. Set the
# directory with RIPPER_WORDS; without a match the language falls back to
# structural rules, which is fine but less accurate.
set -u
RIP="$1"; OUT="$2"; MAP="$3"; TABLE="$4"; SEASON="$5"; TOTAL="$6"
shift 6
VQ="${1:-medium}"; [ $# -gt 0 ] && shift
AQ="${1:-high}";   [ $# -gt 0 ] && shift
ENC_ARGS=("$@")          # e.g. --dual-audio --languages english,swedish
HERE="$(dirname "$0")"
RIPPER="$HERE/../target/release/ripper"
WORDS="${RIPPER_WORDS:-$HERE/../work/words}"
mkdir -p "$OUT/extras"

# Read the mapping on fd 3: ffmpeg and the encoder both consume stdin, and would
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
  tmp="$dest.tmp.mp4"
  "$HERE/encode.sh" "$src" "$tmp" "$VQ" "$AQ" \
      ${ENC_ARGS[@]+"${ENC_ARGS[@]}"} </dev/null 2>&1 | sed 's/^/    /' \
      || { echo "  TRANSCODE FAILED"; continue; }

  # One pass per subtitle track, each in its own language
  mapfile -t sublangs < <(ffprobe -v error -select_streams s \
      -show_entries stream_tags=language -of csv=p=0 "$tmp" \
      | sed 's/,*$//')
  srts=()
  srtlangs=()
  for i in "${!sublangs[@]}"; do
    code="${sublangs[$i]}"
    [ -z "$code" ] && code="und"
    args=(--stream "$i" --lang "$code")
    wl="$WORDS/$code.txt"
    if [ -f "$wl" ]; then
      args+=(--words "$wl")
      note="wordlist $code.txt"
    else
      note="no wordlist"
    fi
    out="$dest.$code.srt"
    if msg=$("$RIPPER" ocr "$tmp" --table "$TABLE" "${args[@]}" -o "$out" 2>&1); then
      unk=$(printf '%s' "$msg" | grep -oE "[0-9]+ unknown" | grep -oE "^[0-9]+" || echo 0)
      cues=$(printf '%s' "$msg" | grep -oE "^[0-9]+ cues" | grep -oE "^[0-9]+" || echo 0)
      srts+=("$out"); srtlangs+=("$code")
      if [ "${unk:-0}" -gt 0 ]; then
        echo "    subs $code: $cues cues, $note, $unk unrecognised glyphs - table may be missing this language"
      else
        echo "    subs $code: $cues cues, $note"
      fi
    else
      echo "    subs $code: recognition failed, leaving the bitmap track only"
      rm -f "$out"
    fi
  done

  if [ "${#srts[@]}" -gt 0 ]; then
    # video, audio, then a text track per language, then the bitmaps
    ins=(); maps=(-map 0:v:0 -map 0:a); codecs=(); meta=(); dispo=()
    for n in "${!srts[@]}"; do
      ins+=(-i "${srts[$n]}")
      maps+=(-map "$((n + 1)):0")
      codecs+=(-c:s:$n mov_text)
      meta+=(-metadata:s:s:$n "language=${srtlangs[$n]}")
      [ "$n" = 0 ] && dispo+=(-disposition:s:0 default) || dispo+=(-disposition:s:$n 0)
    done
    nsrt=${#srts[@]}
    nbmp=$(ffprobe -v error -select_streams s -show_entries stream=index -of csv=p=0 "$tmp" | grep -c . || true)
    maps+=(-map 0:s)
    for ((b = 0; b < nbmp; b++)); do dispo+=(-disposition:s:$((nsrt + b)) 0); done

    if ffmpeg -v error -y -nostdin -i "$tmp" "${ins[@]}" \
        "${maps[@]}" -dn -map_chapters 0 -c copy "${codecs[@]}" \
        "${meta[@]}" "${dispo[@]}" -movflags +faststart "$dest"; then
      rm -f "$tmp"
    else
      echo "  EMBED FAILED - keeping the transcode as-is"
      mv -f "$tmp" "$dest"
    fi
  else
    mv -f "$tmp" "$dest"
  fi
  rm -f "$dest".*.srt

  # metadata, matching the rest of the library
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
