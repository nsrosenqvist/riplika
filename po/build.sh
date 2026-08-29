#!/bin/sh
# Regenerate the template from the source, and refresh every catalogue from it.
#
# Run after adding or changing a string a person reads. The template is derived,
# so it is not edited by hand; the catalogues are merged rather than replaced,
# so existing translations survive a change elsewhere in the file.
set -eu
cd "$(dirname "$0")/.."

xgettext --files-from=po/POTFILES.in --directory=. \
  --keyword=tr --keyword=tr_n:1,2 --language=Rust --from-code=UTF-8 --add-comments \
  --package-name=Riplika --package-version=0.2.0 \
  --copyright-holder="Niklas Rosenqvist" \
  --msgid-bugs-address="https://github.com/nsrosenqvist/riplika/issues" \
  -o po/riplika.pot
xgettext --language=Desktop --join-existing --omit-header \
  -o po/riplika.pot data/com.nsrosenqvist.Riplika.desktop

while read -r lang; do
  [ -n "$lang" ] || continue
  if [ -f "po/$lang.po" ]; then
    msgmerge --update --backup=none --quiet "po/$lang.po" po/riplika.pot
  else
    msginit --no-translator --locale="$lang" --input=po/riplika.pot --output-file="po/$lang.po"
  fi
  msgfmt --check --statistics "po/$lang.po" -o /dev/null
done < po/LINGUAS
