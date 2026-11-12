#!/bin/sh
# OWL-540 probe, never merged: live check R1 on a macOS runner. $1 is the
# target, $2 a folder holding the release's archives and SHA256SUMS.
set -eu
target=$1
release=$2
version=0.1.0
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
# perl's alarm bounds a run that could wait on a dialog.
bounded() { perl -e 'alarm shift; exec @ARGV' 60 "$@"; }

echo "== machine"
sw_vers
uname -m
spctl --status || true

python3 -m http.server 8765 --bind 127.0.0.1 --directory "$release" >/dev/null 2>&1 &
server=$!
trap 'kill "$server"' EXIT
sleep 2

echo "== downloaded with curl"
dl=$(mktemp -d)
archive=owlshift-$version-$target.tar.gz
curl -fsSL -o "$dl/$archive" "http://127.0.0.1:8765/$archive"
tar -xzf "$dl/$archive" -C "$dl"
bin=$dl/owlshift-$version-$target/owlshift
echo "xattr: [$(xattr "$bin" | tr '\n' ' ')]"
codesign -dv "$bin" 2>&1 || true
file "$bin"
bounded "$bin" --version
sh "$here/gate-probe.sh" "$bin"

echo "== the same binary quarantined, as a browser download would leave it (left to P11)"
cp "$bin" "$dl/quarantined"
xattr -w com.apple.quarantine "0083;$(printf %x "$(date +%s)");Safari;" "$dl/quarantined"
spctl -a -vv -t execute "$dl/quarantined" 2>&1 || true
status=0
bounded "$dl/quarantined" --version 2>&1 || status=$?
echo "quarantined run: exit $status"

echo "== installed through the tap"
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_INSTALL_CLEANUP=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_ENV_HINTS=1
brew tap pitchopp/owlshift https://github.com/pitchopp/owlshift
tap=$(brew --repository pitchopp/owlshift)
mkdir -p "$tap/Formula"
render() {
    sh "$repo/scripts/release/homebrew-formula.sh" "$version" "$release/SHA256SUMS" |
        sed "s#https://github.com/pitchopp/owlshift/releases/download/v$version#$1#" > "$tap/Formula/owlshift.rb"
    git -C "$tap" add Formula/owlshift.rb
    git -C "$tap" -c user.name=probe -c user.email=probe@example.invalid commit -q -m "owlshift $version (probe)"
}
render http://127.0.0.1:8765
cat "$tap/Formula/owlshift.rb"
brew --version
if brew install owlshift; then
    echo "PROBE: unexpected, an untrusted tap's formula installed"
    exit 1
fi
echo "PROBE: refused before trust, as expected"
brew trust --formula pitchopp/owlshift/owlshift
if ! brew install owlshift; then
    echo "PROBE: brew refused the local http URL, trying file://"
    render "file://$release"
    brew install owlshift
fi
brew test owlshift
installed=$(brew --prefix)/bin/owlshift
ls -l "$installed"
real=$(cd "$(dirname "$installed")" && cd "$(dirname "$(readlink "$installed")")" && pwd)/owlshift
echo "xattr: [$(xattr "$real" | tr '\n' ' ')]"
codesign -dv "$real" 2>&1 || true
bounded "$installed" --version
sh "$here/gate-probe.sh" "$installed"
echo "PROBE: R1 passed for $target"
