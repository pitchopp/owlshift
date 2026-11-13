#!/bin/sh
# Verifies a published release and renders its Homebrew formula (OWL-549).
# `.github/workflows/formula.yml` runs it once the maintainer publishes a
# release, with a read-only token; formula-commit.sh then commits the
# formula on main. Nobody reviews that commit, so this script is the guard:
#
# - it downloads the release's SHA256SUMS and SHA256SUMS.minisig, and
#   requires the signature to verify with minisign.pub, the public key at the
#   repository's root, and its trusted comment to be exactly
#   `owlshift VERSION`, which release.yml signs, so another release's files
#   cannot stand in for this one;
# - it renders the formula with homebrew-formula.sh, downloads the two macOS
#   archives at the very URLs the formula names, and requires each checksum
#   to be the one SHA256SUMS lists.
#
# Any failure exits 1 and leaves no formula.
#
#   sh scripts/release/formula-verify.sh VERSION DIR
#
# DIR receives the downloads and, last, owlshift.rb. OWLSHIFT_RELEASES_URL is
# where the releases are, each under v<version>/, as for install.sh (a copy
# of a release served from files, in the tests); the formula always names
# GitHub's.

set -eu

fail() {
    printf 'formula-verify: error: %s\n' "$*" >&2
    exit 1
}

if [ "$#" -ne 2 ]; then
    echo "usage: formula-verify.sh VERSION DIR" >&2
    exit 2
fi
version=$1
dir=$2
if ! printf '%s\n' "$version" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+'; then
    fail "'$version' is not a version <major>.<minor>.<patch>"
fi
scripts=$(dirname "$0")
key="$scripts/../../minisign.pub"
releases=${OWLSHIFT_RELEASES_URL:-https://github.com/pitchopp/owlshift/releases/download}
base="$releases/v$version"
named="https://github.com/pitchopp/owlshift/releases/download/v$version"

# Downloads URL $1 into file $2: https only, or a local file for a copy of a
# release. A release's files can take a moment to be served once it is
# published, hence the retries, a 404 included.
download() {
    curl --proto '=https,file' --tlsv1.2 -fsSL --retry 5 --retry-all-errors --retry-delay 5 \
        -o "$2" "$1" || fail "could not download $1"
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    else
        shasum -a 256 "$1" | awk '{ print $1 }'
    fi
}

mkdir -p "$dir"
rm -f "$dir/owlshift.rb" "$dir/owlshift.rb.part"
download "$base/SHA256SUMS" "$dir/SHA256SUMS"
download "$base/SHA256SUMS.minisig" "$dir/SHA256SUMS.minisig"

# `-Q` prints the trusted comment alone once the signature verifies.
if ! comment=$(minisign -V -Q -p "$key" -m "$dir/SHA256SUMS"); then
    fail "the signature of SHA256SUMS does not verify with minisign.pub"
fi
if [ "$comment" != "owlshift $version" ]; then
    fail "SHA256SUMS is signed as '$comment', not 'owlshift $version'"
fi

sh "$scripts/homebrew-formula.sh" "$version" "$dir/SHA256SUMS" > "$dir/owlshift.rb.part" ||
    fail "could not render the formula"
for target in aarch64-apple-darwin x86_64-apple-darwin; do
    archive="owlshift-$version-$target.tar.gz"
    grep -Fq "url \"$named/$archive\"" "$dir/owlshift.rb.part" ||
        fail "the formula does not name $named/$archive"
    # homebrew-formula.sh rendered only if SHA256SUMS lists it exactly once.
    expected=$(awk -v name="$archive" '$2 == name { print $1 }' "$dir/SHA256SUMS")
    download "$base/$archive" "$dir/$archive"
    actual=$(sha256_of "$dir/$archive")
    if [ "$actual" != "$expected" ]; then
        fail "$archive has the checksum $actual, not $expected as SHA256SUMS lists"
    fi
done
mv "$dir/owlshift.rb.part" "$dir/owlshift.rb"
