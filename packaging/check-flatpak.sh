#!/bin/bash
# Test the flatpak the way it will actually be used.
set -u
APP=com.nsrosenqvist.Riplika
pass=0; fail=0
check() {
  local what="$1"; shift
  if "$@" >/dev/null 2>&1; then printf "  PASS  %s\n" "$what"; pass=$((pass+1))
  else printf "  FAIL  %s\n" "$what"; fail=$((fail+1)); fi
}
echo "--- the tools it drives ---"
check "ffmpeg has the dvdvideo demuxer" bash -c \
  "flatpak run --command=ffmpeg $APP -hide_banner -demuxers 2>/dev/null | grep -q dvdvideo"
check "ffmpeg has libx264"             bash -c \
  "flatpak run --command=ffmpeg $APP -hide_banner -encoders 2>/dev/null | grep -q ' libx264'"
check "ffprobe is present"             bash -c \
  "flatpak run --command=ffprobe $APP -version >/dev/null 2>&1"
check "libdvdcss is present"           bash -c \
  "flatpak run --command=sh $APP -c 'test -e /app/lib/libdvdcss.so.2'"
# Not the same question. The manifest links libdvdcss into libdvdread rather
# than letting it be dlopened by name, because inside a sandbox that depends on
# the loader path. If that ever silently reverts, the library is still present
# and an encrypted disc is still unreadable.
check "libdvdread links libdvdcss"     bash -c \
  "flatpak run --command=sh $APP -c 'ldd /app/lib/libdvdread.so.* | grep -q libdvdcss'"
check "mkvextract is NOT needed"       bash -c \
  "! flatpak run --command=sh $APP -c 'command -v mkvextract' >/dev/null 2>&1"
echo "--- the application ---"
check "the CLI runs"                   bash -c \
  "flatpak run --command=riplika $APP --version | grep -q riplika"
check "the English catalogue shipped"  bash -c \
  "flatpak run --command=sh $APP -c 'test -f /app/share/locale/en/LC_MESSAGES/riplika.mo'"
check "the desktop entry shipped"      bash -c \
  "flatpak run --command=sh $APP -c 'test -f /app/share/applications/$APP.desktop'"
check "it declares the DVD mime type"  bash -c \
  "flatpak run --command=sh $APP -c 'grep -q x-content/video-dvd /app/share/applications/$APP.desktop'"
echo
echo "  $pass passed, $fail failed"
exit $fail
