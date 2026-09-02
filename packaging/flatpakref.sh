#!/bin/bash
# Write the .flatpakref file that installs Riplika in one command.
#
# Not the same thing as the .flatpakrepo beside it. That one adds a remote and
# leaves you to find what is on it; this one names an application, and adding
# the remote is something it does on the way. It is what an install line in a
# README should point at:
#
#     flatpak install https://flatpak.nsrosenqvist.com/riplika.flatpakref
#
# RuntimeRepo is the part that earns it. The GNOME runtime is not served from
# here and never will be, so without that line the install fails on a runtime
# it cannot find and the answer - add Flathub first - is one the person has to
# already know. With it, flatpak goes and gets it.
#
# One per application, so this one lives with Riplika rather than describing
# the remote. Reads the public half of whatever key GNUPGHOME holds.
set -eu

BASE="${1:?usage: flatpakref.sh https://host [keyid]}"
BASE="${BASE%/}"
KEY="${2:-}"

# One line, no wrapping: the value is read to the end of the line, and gpg's
# own 64-column armour would end the field after the first of them.
GPG=$(gpg --export ${KEY:+"$KEY"} | base64 -w0)
if [ -z "$GPG" ]; then
  echo "flatpakref.sh: no public key to export" >&2
  exit 1
fi

cat <<EOF
[Flatpak Ref]
Title=Riplika
Name=com.nsrosenqvist.Riplika
Branch=master
Url=$BASE/repo/
SuggestRemoteName=nsrosenqvist
RuntimeRepo=https://dl.flathub.org/repo/flathub.flatpakrepo
IsRuntime=false
GPGKey=$GPG
EOF
