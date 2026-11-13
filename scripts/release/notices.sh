#!/bin/sh
# The third-party notices of a release (OWL-548). For each TARGET, writes
# OUT/<target>/THIRD-PARTY-NOTICES.txt: the licence of every crate linked
# into `owlshift` for that target, rendered by cargo-about from about.toml
# and notices.hbs beside this script, then the licence texts of the fonts the
# binary carries (crates/owlshift-runner/src/ui/assets/fonts), then (OWL-551)
# the notice of the Rust standard library, as plain text converted by
# html2text.py from the toolchain's share/doc/rust/COPYRIGHT-library.html,
# and, for the Linux musl targets, musl's COPYRIGHT (musl-COPYRIGHT beside
# this script). cargo-about refuses a crate whose licence `accepted` does not
# allow, and a missing or empty std notice is refused too: this script then
# fails and writes no file for that target.
#
# Checked 2026-11-13 on the pinned 1.98.1 `minimal` profile: the `rustc`
# component installs COPYRIGHT-library.html. Rust 1.98.1 builds its musl
# targets with musl 1.2.5 (src/ci/docker/scripts/musl.sh); re-check
# musl-COPYRIGHT when the toolchain moves. Not covered: LLVM's libunwind,
# which the musl targets' self-contained objects may link.
#
#   sh scripts/release/notices.sh OUT [TARGET...]
#
# With no TARGET, the targets the release builds. Run from anywhere, after
# `cargo fetch --locked`: cargo-about runs offline (`--frozen`), so the
# notices depend on Cargo.lock alone. Needs cargo-about 0.9.2.

set -eu

# The release's targets: the build matrix of .github/workflows/release.yml,
# which a test keeps equal to this list.
TARGETS="aarch64-apple-darwin x86_64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl"

if [ "$#" -lt 1 ]; then
    echo "usage: notices.sh OUT [TARGET...]" >&2
    exit 2
fi
out=$1
shift
if [ "$#" -eq 0 ]; then
    # shellcheck disable=SC2086 # a list of words
    set -- $TARGETS
fi

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
fonts="$root/crates/owlshift-runner/src/ui/assets/fonts"
musl="$here/musl-COPYRIGHT"

# The Rust standard library's notice, converted once for every target. Both
# rustc calls run from the repository root, so rust-toolchain.toml picks the
# pinned toolchain, the one whose std is linked, wherever the script is run.
std_html="$(cd "$root" && rustc --print sysroot)/share/doc/rust/COPYRIGHT-library.html"
std_version=$(cd "$root" && rustc --version)
std_text=$(mktemp "${TMPDIR:-/tmp}/owlshift-std-notice.XXXXXX")
part=
trap 'rm -f "$std_text" ${part:+"$part"}' EXIT
if ! python3 -I "$here/html2text.py" "$std_html" > "$std_text"; then
    echo "error: the Rust standard library notice could not be read from $std_html" >&2
    exit 1
fi

for target in "$@"; do
    mkdir -p "$out/$target"
    file="$out/$target/THIRD-PARTY-NOTICES.txt"
    part="$file.part"
    rm -f "$file" "$part"
    if ! cargo about generate --frozen --fail \
        -c "$here/about.toml" \
        -m "$root/crates/owlshift-cli/Cargo.toml" \
        --target "$target" \
        -o "$part" \
        "$here/notices.hbs"; then
        rm -f "$part"
        echo "error: the notices of $target could not be written: a crate's licence is not accepted (scripts/release/about.toml) or cannot be read" >&2
        exit 1
    fi
    found=0
    for licence in "$fonts"/OFL-*.txt; do
        [ -f "$licence" ] || continue
        found=1
        {
            echo
            echo '================================================================================'
            echo "SIL Open Font License 1.1 ($(basename "$licence"))"
            echo
            cat "$licence"
        } >> "$part"
    done
    if [ "$found" -eq 0 ]; then
        rm -f "$part"
        echo "error: $fonts holds no OFL-*.txt, the licence of the fonts the binary carries" >&2
        exit 1
    fi
    {
        echo
        echo '================================================================================'
        echo "The Rust standard library (COPYRIGHT-library.html of $std_version)"
        echo
        cat "$std_text"
    } >> "$part"
    case $target in
    *-linux-musl)
        {
            echo
            echo '================================================================================'
            echo "musl libc (scripts/release/musl-COPYRIGHT)"
            echo
            cat "$musl"
        } >> "$part"
        ;;
    esac
    mv "$part" "$file"
    part=
    echo "$file"
done
