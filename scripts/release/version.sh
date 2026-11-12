#!/bin/sh
# The version a release tag names (OWL-540). Prints it when TAG is
# `v<version>`, <version> being the version of Cargo.toml's
# `[workspace.package]`, and CHANGELOG.md has a `## <version>` section, which
# the release's notes are taken from; refuses otherwise. The release
# workflow's first job runs it, and a test runs it on the repository, so a
# version bump without its changelog section fails in CI, not at the tag.
#
#   sh scripts/release/version.sh TAG [ROOT]
#
# ROOT is the repository's root, the current folder when not given.

set -eu

if [ "$#" -lt 1 ] || [ "$#" -gt 2 ]; then
    echo "usage: version.sh TAG [ROOT]" >&2
    exit 2
fi
tag=$1
root=${2:-.}

version=$(awk '
    /^\[/ { section = $0; next }
    section == "[workspace.package]" && $1 == "version" && $2 == "=" {
        gsub(/"/, "", $3)
        print $3
        exit
    }' "$root/Cargo.toml")
if [ -z "$version" ]; then
    echo "error: $root/Cargo.toml has no version under [workspace.package]" >&2
    exit 1
fi
if [ "$tag" != "v$version" ]; then
    echo "error: the tag $tag is not v$version, the workspace's version in Cargo.toml" >&2
    exit 1
fi
if ! grep -qxF "## $version" "$root/CHANGELOG.md"; then
    echo "error: CHANGELOG.md has no '## $version' section, which the release's notes come from" >&2
    exit 1
fi
printf '%s\n' "$version"
