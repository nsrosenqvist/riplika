#!/bin/bash
# Transcode a ripped DVD title to MP4 with ffmpeg.
#
# HandBrake is a wrapper around the same libx264; encoding a title both ways
# with matching settings produces byte-identical x264 parameter strings. What
# HandBrake actually supplies is the preprocessing decisions, so this makes
# those explicit:
#
#   crop      detected from the picture, not assumed
#   IVTC      applied only when the source really is telecined
#   frame rate pinned constant
#   pixel aspect snapped to the DVD ratios
#   subs      every track carried over
#   faststart moov atom at the front, HandBrake's "web optimized"
#
# Usage: encode.sh <input> <output> [video: high|medium|low] [audio: high|medium|low]
#                  [--dual-audio] [--languages english,swedish]
set -euo pipefail
IN="$1"; OUT="$2"; VQ="${3:-medium}"; AQ="${4:-high}"
DUAL=0
LANGS=""
_prev=""
for arg in "$@"; do
  [ "$arg" = "--dual-audio" ] && DUAL=1
  [ "$_prev" = "--languages" ] && LANGS="$arg"
  _prev="$arg"
done
HERE="$(dirname "$0")"

# Video tiers. DVD is 720x480 MPEG-2 at 4-6 Mb/s, so the useful range is narrow:
# by CRF 18 the encode is transparent against the source and further bits mostly
# track MPEG-2's own noise, while below CRF 23 SD detail goes quickly.
case "$VQ" in
  high)   CRF=18 ;;   # ~1.5 Mb/s, ~240 MB per 21-minute episode
  medium) CRF=20 ;;   # ~1.0 Mb/s, ~170 MB  - the sweet spot
  low)    CRF=23 ;;   # ~0.7 Mb/s, ~107 MB
  *) echo "video quality must be high, medium or low" >&2; exit 2 ;;
esac

# Audio tiers. "high" keeps the stream untouched, which on a DVD means the
# original AC3 5.1: no downmix and no second lossy generation. The cost is
# browser playback - nothing web-based decodes AC3, so a browser client makes
# the server transcode audio, while TV and desktop apps play it directly.
case "$AQ" in
  high)   AOPTS=(-c:a copy) ;;
  medium) AOPTS=(-c:a aac -b:a 160k -ac 2) ;;
  low)    AOPTS=(-c:a aac -b:a 96k  -ac 2) ;;
  *) echo "audio quality must be high, medium or low" >&2; exit 2 ;;
esac

# How many audio tracks the source has, so the extra stereo track can be given
# the right output index
# Which audio and subtitle tracks to keep. Listing languages in preference
# order also orders the output, so the first one asked for becomes the default.
AMAPS=()
SMAPS=()
if [ -n "$LANGS" ]; then
  echo "  languages: $LANGS"
  while read -r i; do AMAPS+=(-map "0:a:$i"); done \
    < <(python3 "$HERE/langmap.py" "$IN" a "$LANGS")
  while read -r i; do SMAPS+=(-map "0:s:$i"); done \
    < <(python3 "$HERE/langmap.py" "$IN" s "$LANGS")
else
  AMAPS=(-map 0:a)
  SMAPS=(-map "0:s?")
fi
NAUD=${#AMAPS[@]}
NAUD=$((NAUD / 2))   # each map is two argv entries
NSUB=${#SMAPS[@]}
NSUB=$((NSUB / 2))

# Make the preference real. ffmpeg carries the source's disposition across, so
# without this "swedish,english" would put Swedish first but leave English
# flagged default, and players would still pick English.
DISPO=()
if [ -n "$LANGS" ]; then
  for ((i = 0; i < NAUD; i++)); do
    [ "$i" = 0 ] && DISPO+=(-disposition:a:0 default) || DISPO+=(-disposition:a:$i 0)
  done
  for ((i = 0; i < NSUB; i++)); do
    [ "$i" = 0 ] && DISPO+=(-disposition:s:0 default) || DISPO+=(-disposition:s:$i 0)
  done
fi

# --dual-audio adds an AAC stereo track alongside the untouched original. AC3
# cannot be decoded by any browser, so without this a web client makes the
# server transcode audio; with it there is a track every client can take.
# The original stays first and default, so capable players still get 5.1.
if [ "$DUAL" = "1" ]; then
  if [ "$AQ" != "high" ]; then
    echo "  note: --dual-audio only applies to audio high; ignoring"
  else
    # the stereo track is derived from whichever audio ended up first
    first_a="${AMAPS[1]:-0:a:0}"
    AMAPS+=(-map "$first_a")
    # No track title: MP4 cannot carry one through ffmpeg's muxer, unlike
    # Matroska. Players tell the two apart by codec and channel count anyway.
    AOPTS=(-c:a copy
           -c:a:$NAUD aac -b:a:$NAUD 160k -ac:a:$NAUD 2
           -disposition:a:$NAUD 0)
    echo "  dual audio: original + AAC stereo fallback"
  fi
fi

echo "  video: $VQ (crf $CRF)   audio: $AQ"

# What matters is what the decoder actually produces, not what the container
# claims. This DVD reports 29.97 but decodes to 23.976 progressive film: MakeMKV
# kept the soft-telecine flags and ffmpeg resolves them. Blindly running
# fieldmatch,decimate on that throws away one real frame in five - the output
# still says 23.976 and still has the right duration, so it looks fine until you
# count frames.
measure_fps() {
  local n
  n=$(ffmpeg -nostdin -v info -t 20 -i "$1" -an -f null - 2>&1 \
        | grep -oE "frame= *[0-9]+" | tail -1 | grep -oE "[0-9]+")
  [ -n "$n" ] && python3 -c "print(f'{$n/20:.3f}')" || echo 0
}

ivtc=""
rate=""
dec_fps=$(measure_fps "$IN")
if [ "$(python3 -c "print(1 if abs($dec_fps-29.97)<0.6 else 0)")" = "1" ]; then
  # 29.97 out of the decoder: either true video, or telecine still to undo.
  # Decimation drops one frame in five only when the duplicates are really there.
  a=$(ffmpeg -nostdin -v info -t 20 -i "$IN" -an -f null - 2>&1 \
        | grep -oE "frame= *[0-9]+" | tail -1 | grep -oE "[0-9]+")
  b=$(ffmpeg -nostdin -v info -t 20 -i "$IN" -vf "fieldmatch,decimate" -an -f null - 2>&1 \
        | grep -oE "frame= *[0-9]+" | tail -1 | grep -oE "[0-9]+")
  if [ "$(python3 -c "print(1 if abs($b/$a-0.8)<0.03 else 0)")" = "1" ]; then
    ivtc="fieldmatch,decimate,"
    echo "  telecined 29.97: inverse telecine -> 23.976"
  else
    echo "  true 29.97 video: frame rate left alone"
  fi
else
  echo "  decodes at ${dec_fps} fps: frame rate left alone"
fi

# Pin a constant frame rate. Soft telecine reaches the encoder as 23.976 unique
# frames carried on a 29.97 timestamp grid, every fifth one held for two ticks;
# muxed as-is that becomes a variable-rate file that merely averages 23.976.
case "$(python3 -c "print(round($dec_fps,2))")" in
  23.98|23.97|24.0|24.00) rate="24000/1001" ;;
  29.97|30.0|30.00)       rate="30000/1001" ;;
  25.0|25.00)             rate="25" ;;
esac
[ -n "$ivtc" ] && rate="24000/1001"
[ -n "$rate" ] && echo "  constant frame rate: $rate"

# Auto-crop from a spread of the picture, not just the opening frames
crop=$(ffmpeg -nostdin -v info -ss 300 -t 60 -i "$IN" -vf cropdetect=24:2:0 \
        -frames:v 400 -an -f null - 2>&1 \
        | grep -oE "crop=[0-9]+:[0-9]+:[0-9]+:[0-9]+" | sort | uniq -c | sort -rn \
        | head -1 | grep -oE "crop=.*")
[ -n "$crop" ] && echo "  crop: $crop" || crop=""
[ -n "$crop" ] && crop="$crop,"

# Keep the source's display aspect: DVD pixels are not square
raw_sar=$(ffprobe -v error -select_streams v:0 -show_entries stream=sample_aspect_ratio \
        -of csv=p=0 "$IN" | tr -d ' ,\n' | tr ':' '/')
[ -z "$raw_sar" ] && raw_sar="1/1"
[ "$raw_sar" = "0/1" ] && raw_sar="1/1"
# Snap to the DVD pixel aspects. Passing the source's odd ratio through makes
# ffmpeg approximate it (853/720 became 77/65), which drifts the picture.
sar=$(python3 - "$raw_sar" <<'PYEOF'
import sys
from fractions import Fraction
try:
    v = float(Fraction(sys.argv[1]))
except Exception:
    v = 1.0
for name, f in (("32/27", 32/27), ("8/9", 8/9), ("64/45", 64/45), ("16/11", 16/11), ("1/1", 1.0)):
    if abs(v - f) / f < 0.01:
        print(name); break
else:
    print(sys.argv[1])
PYEOF
)
echo "  pixel aspect: $sar"

ffmpeg -nostdin -v error -y -i "$IN" \
  -map 0:v:0 "${AMAPS[@]}" ${SMAPS[@]+"${SMAPS[@]}"} -dn \
  -vf "${ivtc}${crop}setsar=${sar}" \
  ${rate:+-fps_mode cfr -r $rate} \
  -c:v libx264 -crf "$CRF" -preset medium -profile:v high -level 4.0 \
  ${DISPO[@]+"${DISPO[@]}"} \
  "${AOPTS[@]}" \
  -c:s copy \
  -movflags +faststart \
  "$OUT"

sz=$(stat -c%s "$OUT")
echo "  wrote $(basename "$OUT"): $((sz / 1048576)) MB"
