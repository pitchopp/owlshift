#!/bin/sh
# The third-party notices of a release (OWL-548). For each TARGET, writes
# OUT/<target>/THIRD-PARTY-NOTICES.txt: the licence of every crate linked
# into `owlshift` for that target, rendered by cargo-about from about.toml
# and notices.hbs beside this script, then the licence texts of the fonts the
# binary carries (crates/owlshift-runner/src/ui/assets/fonts). cargo-about
# refuses a crate whose licence `accepted` does not allow, and then this
# script fails and writes no file for that target.
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
    mv "$part" "$file"
    echo "$file"
done
