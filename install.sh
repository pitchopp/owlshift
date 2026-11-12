#!/bin/sh
# Installs Owlshift on Linux from a release on GitHub (OWL-540):
#
#   curl -fsSL https://raw.githubusercontent.com/pitchopp/owlshift/main/install.sh | sh
#
# It downloads the archive of this machine's target and the release's
# SHA256SUMS, refuses the archive unless its SHA-256 checksum is the one
# SHA256SUMS lists, then puts `owlshift` in ~/.local/bin by an atomic rename,
# which also replaces a binary that is running. The checksum guards against a
# damaged download, not against someone able to replace the release's files:
# what says the files are Owlshift's is the signature of SHA256SUMS,
# SHA256SUMS.minisig, checked with `minisign -Vm SHA256SUMS -p minisign.pub`
# (the public key at the repository's root). On macOS, use the tap instead:
# `brew tap pitchopp/owlshift https://github.com/pitchopp/owlshift`, then
# `brew install owlshift`.
#
# Settings, from the environment:
#   OWLSHIFT_VERSION       the version to install, 0.1.0 or v0.1.0; the latest
#                          published release when unset
#   OWLSHIFT_INSTALL_DIR   where to put `owlshift`; ~/.local/bin when unset
#   OWLSHIFT_RELEASES_URL  where the releases are, each under v<version>/;
#                          https://github.com/pitchopp/owlshift/releases/download
#                          when unset
#
# Everything runs from `main`, called on the last line, so a download of
# this script cut short runs nothing.

set -eu

REPOSITORY=https://github.com/pitchopp/owlshift

say() {
    printf 'owlshift install: %s\n' "$*"
}

fail() {
    printf 'owlshift install: error: %s\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || fail "this script needs $1, which is not installed"
}

# Downloads URL $1 into file $2: https only, or a local file for a copy of a
# release.
download() {
    curl --proto '=https,file' --tlsv1.2 -fsSL -o "$2" "$1" || fail "could not download $1"
}

sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    else
        shasum -a 256 "$1" | awk '{ print $1 }'
    fi
}

# The version of the latest published release, read from where GitHub
# redirects its "latest" page: .../releases/tag/v<version>.
latest_version() {
    url=$(curl --proto '=https' --tlsv1.2 -fsSLI -o /dev/null -w '%{url_effective}' \
        "$REPOSITORY/releases/latest") || fail "could not reach $REPOSITORY/releases/latest"
    case $url in
        */releases/tag/v*) printf '%s\n' "${url##*/releases/tag/v}" ;;
        *) fail "$REPOSITORY has no published release yet" ;;
    esac
}

main() {
    if [ "$(uname -s)" != Linux ]; then
        fail "this script installs Owlshift on Linux; on macOS, run: brew tap pitchopp/owlshift $REPOSITORY && brew install owlshift"
    fi
    machine=$(uname -m)
    case $machine in
        x86_64 | amd64) target=x86_64-unknown-linux-musl ;;
        aarch64 | arm64) target=aarch64-unknown-linux-musl ;;
        *) fail "no release binary for this machine ($machine); build from source: cargo install --git $REPOSITORY --locked owlshift-cli" ;;
    esac
    need curl
    need tar
    need awk
    if ! command -v sha256sum >/dev/null 2>&1 && ! command -v shasum >/dev/null 2>&1; then
        fail "this script needs sha256sum or shasum, and neither is installed"
    fi

    version=${OWLSHIFT_VERSION:-}
    if [ -z "$version" ]; then
        version=$(latest_version)
    fi
    version=${version#v}
    # A version such as 1.2.3, with an optional pre-release or build suffix:
    # nothing else reaches a URL or a path.
    if ! printf '%s\n' "$version" | grep -Eqx '[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?'; then
        fail "\"$version\" is not a release version such as 0.1.0"
    fi
    releases=${OWLSHIFT_RELEASES_URL:-$REPOSITORY/releases/download}
    base=${releases%/}/v$version
    dir=${OWLSHIFT_INSTALL_DIR:-$HOME/.local/bin}
    name=owlshift-$version-$target
    archive=$name.tar.gz

    work=$(mktemp -d)
    staged=
    trap 'rm -rf "$work"; if [ -n "$staged" ]; then rm -f "$staged"; fi' EXIT
    trap 'exit 1' HUP INT TERM

    say "downloading Owlshift $version for $target"
    download "$base/$archive" "$work/$archive"
    download "$base/SHA256SUMS" "$work/SHA256SUMS"

    # Exactly one line naming the archive, with 64 lowercase hexadecimal
    # digits (scripts/release/homebrew-formula.sh reads it the same way).
    expected=$(awk -v name="$archive" '$2 == name { print $1 }' "$work/SHA256SUMS")
    if [ -z "$expected" ]; then
        fail "SHA256SUMS does not list $archive: nothing installed"
    fi
    case $expected in
        *[!0-9a-f]*) fail "SHA256SUMS does not list $archive once with a SHA-256 checksum: nothing installed" ;;
    esac
    if [ "${#expected}" -ne 64 ]; then
        fail "SHA256SUMS does not list $archive once with a SHA-256 checksum: nothing installed"
    fi
    actual=$(sha256_of "$work/$archive")
    if [ "$actual" != "$expected" ]; then
        fail "the checksum of $archive is $actual, not $expected as SHA256SUMS says: the download is damaged or is not the release's, nothing installed"
    fi

    tar -xzf "$work/$archive" -C "$work" "$name/owlshift" ||
        fail "$archive does not hold $name/owlshift: nothing installed"
    mkdir -p "$dir" || fail "could not create $dir"
    staged=$dir/.owlshift.$$
    cp "$work/$name/owlshift" "$staged" || fail "could not write in $dir"
    chmod 755 "$staged"
    mv -f "$staged" "$dir/owlshift" || fail "could not replace $dir/owlshift"
    staged=
    say "installed Owlshift $version as $dir/owlshift"
    case ":$PATH:" in
        *":$dir:"*) ;;
        *) say "$dir is not on your PATH: add it, for instance with export PATH=\"$dir:\$PATH\" in ~/.profile" ;;
    esac
}

main "$@"
