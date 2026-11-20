#!/bin/sh
# Install a locally built BaiCode CLI for the current user.
set -eu
if [ "$#" -ne 1 ]; then
  echo 'Usage: install.sh /path/to/baic' >&2
  exit 2
fi
case "$1" in
  /*) source=$1 ;;
  *) source=$PWD/$1 ;;
esac
if [ ! -f "$source" ] || [ ! -x "$source" ]; then
  echo 'Expected an executable BaiCode binary.' >&2
  exit 1
fi
version=$("$source" --version)
case "$version" in
  'baic '*) ;;
  *) echo 'The supplied binary did not identify itself as baic.' >&2; exit 1 ;;
esac
bin_dir=${BAICODE_INSTALL_DIR:-"$HOME/.local/bin"}
mkdir -p "$bin_dir"
dest="$bin_dir/baic"
if [ -d "$dest" ]; then
  echo "Destination is a directory: $dest" >&2
  exit 1
fi
tmp=$(mktemp "$bin_dir/.baic-install.XXXXXXXX")
trap 'rm -f "$tmp"' EXIT
cp "$source" "$tmp"
chmod 755 "$tmp"
mv -f "$tmp" "$dest"
echo "Installed $version at $dest"
