#!/bin/sh
# OWL-540 probe, never merged: R2 inside a container, as root first to
# install what a user's machine has, then as a user that is not root.
set -eu
apt-get update -qq >/dev/null
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq ca-certificates curl git bubblewrap file >/dev/null
useradd -m probe
su - probe -c "sh /probe/.github/owl540-probe/r2.sh $1 /release $2"
