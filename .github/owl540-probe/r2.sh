#!/bin/sh
# OWL-540 probe, never merged: live check R2 on Linux, as a user that is not
# root. $1 is the target, $2 a folder holding v0.1.0/ (the release's
# archives and SHA256SUMS), $3 `pass` when the confined run must pass, or
# `git-too-old` when the system's git is below Owlshift's prerequisite.
set -eu
target=$1
release=$2
gate=$3
version=0.1.0
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)

echo "== system"
head -n 4 /etc/os-release
uname -srm
(ldd --version 2>&1 || true) | head -n 1
git --version
bwrap --version || true

echo "== installed by the install script"
OWLSHIFT_RELEASES_URL="file://$release" OWLSHIFT_VERSION=$version OWLSHIFT_INSTALL_DIR="$HOME/owl540-bin" \
    sh "$repo/install.sh"
bin=$HOME/owl540-bin/owlshift
file "$bin" || true
ldd "$bin" 2>&1 || true
"$bin" --version

echo "== a tampered archive"
tampered=$(mktemp -d)
mkdir "$tampered/v$version"
cp "$release/v$version/"* "$tampered/v$version/"
printf 'x' >> "$tampered/v$version/owlshift-$version-$target.tar.gz"
if OWLSHIFT_RELEASES_URL="file://$tampered" OWLSHIFT_VERSION=$version OWLSHIFT_INSTALL_DIR="$HOME/owl540-tampered" \
    sh "$repo/install.sh"; then
    echo "PROBE: the tampered archive was installed"
    exit 1
fi
if [ -e "$HOME/owl540-tampered" ]; then
    echo "PROBE: something was installed from the tampered archive"
    exit 1
fi
echo "PROBE: the tampered archive was refused, nothing installed"

echo "== a confined run"
case $gate in
    pass)
        sh "$here/gate-probe.sh" "$bin"
        ;;
    git-too-old)
        if sh "$here/gate-probe.sh" "$bin"; then
            echo "PROBE: the gate ran with this git"
        else
            echo "PROBE: refused as expected, git below 2.39"
        fi
        ;;
esac
echo "PROBE: R2 passed for $target"
