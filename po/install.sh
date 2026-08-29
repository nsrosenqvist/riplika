#!/bin/sh
# Compile the catalogues into a locale tree the binary can find.
#
# $1 is the prefix; without one they land beside the binary, which is where
# i18n::init looks when the application is being run from a build directory.
set -eu
cd "$(dirname "$0")/.."
prefix="${1:-target/release}"

while read -r lang; do
  [ -n "$lang" ] || continue
  dir="$prefix/locale/$lang/LC_MESSAGES"
  mkdir -p "$dir"
  msgfmt "po/$lang.po" -o "$dir/riplika.mo"
  echo "  $dir/riplika.mo"
done < po/LINGUAS
