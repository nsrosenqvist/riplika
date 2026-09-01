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
# Both formats the settings screen offers. FLAC is ffmpeg's own; MP3 needs a
# library that has to be built, and without it the rip fails at the encode.
check "ffmpeg can encode MP3"          bash -c \
  "flatpak run --command=ffmpeg $APP -hide_banner -encoders 2>/dev/null | grep -q libmp3lame"
check "ffmpeg can encode FLAC"         bash -c \
  "flatpak run --command=ffmpeg $APP -hide_banner -encoders 2>/dev/null | grep -qE ' flac'"
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
# A music rip shells out to this. Without it every track fails with "no such
# file or directory" at the point the disc is already spinning, which is how
# it shipped.
check "cdparanoia is present"          bash -c \
  "flatpak run --command=cdparanoia $APP -V 2>&1 | grep -q 'cdparanoia III'"
check "mkvextract is NOT needed"       bash -c \
  "! flatpak run --command=sh $APP -c 'command -v mkvextract' >/dev/null 2>&1"
# A disc whose subtitle face is not already in a glyph table is labelled by
# reading it. Without these it keeps its subtitles as pictures, which is what
# The Lion King did on every build before they were bundled.
check "tesseract is present"           bash -c \
  "flatpak run --command=tesseract $APP --version 2>&1 | grep -q '^tesseract'"
check "it can find its language data"  bash -c \
  "flatpak run --command=tesseract $APP --list-langs 2>&1 | grep -qx eng"
check "and the languages discs use"    bash -c \
  "flatpak run --command=tesseract $APP --list-langs 2>&1 | grep -qx swe"
echo "--- the application ---"
check "the CLI runs"                   bash -c \
  "flatpak run --command=riplika $APP --version | grep -q riplika"
check "the English catalogue shipped"  bash -c \
  "flatpak run --command=sh $APP -c 'test -f /app/share/locale/en/LC_MESSAGES/riplika.mo'"
check "the desktop entry shipped"      bash -c \
  "flatpak run --command=sh $APP -c 'test -f /app/share/applications/$APP.desktop'"
check "it declares the DVD mime type"  bash -c \
  "flatpak run --command=sh $APP -c 'grep -q x-content/video-dvd /app/share/applications/$APP.desktop'"
# Two halves of one thing. Flatpak exports only icons named after the app id,
# and the entry's Icon= has to be that same name - either half wrong and the
# application launches with a blank square where its icon should be.
# Each library it can be asked to write to. A missing one does not fail at
# startup: it fails at the end of a rip, on the directory it cannot create.
check "it can write to Videos"         bash -c \
  "flatpak run --command=sh $APP -c 'mkdir -p ~/Videos && test -w ~/Videos'"
check "it can write to Music"          bash -c \
  "flatpak run --command=sh $APP -c 'mkdir -p ~/Music && test -w ~/Music'"
check "it can write to Games"          bash -c \
  "flatpak run --command=sh $APP -c 'mkdir -p ~/Games && test -w ~/Games'"
# What a software centre reads. Missing, the application installs and runs and
# shows up with no name, description or licence beside it.
check "the metainfo shipped"           bash -c \
  "flatpak run --command=sh $APP -c 'test -f /app/share/metainfo/$APP.metainfo.xml'"
# Validated on the host: the runtime carries no appstreamcli, and the file
# inside the sandbox is a copy of this one anyway.
check "the metainfo is valid"          appstreamcli validate --no-net \
  "$(dirname "$0")/../data/$APP.metainfo.xml"
check "the icon shipped"               bash -c \
  "flatpak run --command=sh $APP -c 'test -f /app/share/icons/hicolor/scalable/apps/$APP.svg'"
check "the entry points at that icon"  bash -c \
  "flatpak run --command=sh $APP -c 'grep -qx Icon=$APP /app/share/applications/$APP.desktop'"
echo
echo "  $pass passed, $fail failed"
exit $fail
