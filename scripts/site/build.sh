#!/usr/bin/env bash
# Writes the public site to serve into FOLDER (OWL-576): the files of site/,
# the web UI's three font files with their two licence texts in fonts/, and
# the two lockups, the mark and the social preview in brand/. The repository
# keeps one copy of each font and logo; this script copies them beside the
# page. See docs/design/build-plan.md, "The public site", "How it is built".
#
# Then (OWL-607) the documentation in docs/: the renderer of scripts/site/docs
# turns the repository's Markdown pages into the site's, in the page's frame,
# so the build needs the toolchain pinned in rust-toolchain.toml ("The
# documentation").

set -eu

PROGRAM=${0##*/}

if [ "$#" -ne 1 ] || [ "$1" = "--help" ] || [ "$1" = "-h" ]; then
    echo "Usage: $PROGRAM FOLDER" >&2
    echo "Writes the public site into FOLDER, which must be new or empty." >&2
    exit 2
fi

out=$1
root=$(cd "$(dirname "$0")/../.." && pwd)
fonts="$root/crates/owlshift-runner/src/ui/assets/fonts"
brand="$root/assets/brand"

if [ -e "$out" ] && [ ! -d "$out" ]; then
    echo "$PROGRAM: $out exists and is not a folder" >&2
    exit 1
fi
if [ -d "$out" ] && [ -n "$(ls -A "$out")" ]; then
    echo "$PROGRAM: $out is not empty; give a new or empty folder" >&2
    exit 1
fi

mkdir -p "$out/fonts" "$out/brand"
cp -R "$root/site/." "$out/"

for file in \
    BricolageGrotesque-latin.woff2 \
    JetBrainsMono-Regular-latin.woff2 \
    JetBrainsMono-Medium-latin.woff2 \
    OFL-BricolageGrotesque.txt \
    OFL-JetBrainsMono.txt; do
    cp "$fonts/$file" "$out/fonts/$file"
done

for file in \
    owlshift-lockup.svg \
    owlshift-lockup-on-dark.svg \
    owlshift-mark.svg \
    owlshift-social.png; do
    cp "$brand/$file" "$out/brand/$file"
done

# From the repository's root, so rustup picks the pinned toolchain; the
# folder is passed whole, since the renderer runs from there.
out=$(cd "$out" && pwd)
(cd "$root" && cargo run --quiet --locked -p owlshift-site-docs -- "$root" "$out")

echo "Site written to $out"
