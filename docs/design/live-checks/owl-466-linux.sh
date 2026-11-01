#!/bin/sh
# OWL-466, live check U1 of docs/design/build-plan.md, "P8 · Local web UI", on Linux: runs the ignored test
# crates/owlshift-runner/tests/ui_token_live.rs under bwrap in a Linux container (Docker), as OWL-101 ran its checks:
# Debian bookworm, bubblewrap, a non-root user `owl` (uid 1001) whose home is /home/owl, and the two options bwrap
# needs inside Docker (`--security-opt seccomp=unconfined --security-opt systempaths=unconfined`, not --privileged).
# The data directory goes under /home/owl/.local/share, then under /srv, which `owl` owns and nothing closes.
#
# Usage, from a checkout of the branch, Docker running:
#   sh docs/design/live-checks/owl-466-linux.sh
# The checkout is mounted read-only; the build goes to the Docker volumes owl466-target and owl466-cargo (remove them
# with `docker volume rm owl466-target owl466-cargo`). No credential enters the container; it prints results only.
set -eu
here=$(cd "$(dirname "$0")/../../.." && pwd)
toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$here/rust-toolchain.toml")
image=owlshift-owl466-rig
docker build -q -t "$image" --build-arg "TOOLCHAIN=$toolchain" - <<'EOF' >/dev/null
FROM debian:bookworm-slim
ARG TOOLCHAIN
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
      bubblewrap ca-certificates curl gcc git libc6-dev pkg-config >/dev/null \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -m -u 1001 owl && mkdir -p /srv /target /cargo/registry && chown -R owl /srv /target /cargo
USER owl
ENV CARGO_HOME=/cargo RUSTUP_HOME=/home/owl/.rustup PATH=/cargo/bin:/usr/local/bin:/usr/bin:/bin
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain "$TOOLCHAIN" --no-modify-path
EOF
docker run --rm \
  --security-opt seccomp=unconfined --security-opt systempaths=unconfined \
  -v "$here:/src:ro" -v owl466-target:/target -v owl466-cargo:/cargo/registry \
  -e CARGO_TARGET_DIR=/target -e OWLSHIFT_LIVE_UI_TOKEN=1 -w /src "$image" sh -c '
    echo "kernel=$(uname -r) arch=$(uname -m) $(. /etc/os-release; echo "$PRETTY_NAME") $(bwrap --version) user=$(id -un) uid=$(id -u)"
    cargo test -q -p owlshift-runner --test ui_token_live --locked -- --ignored --nocapture 2>&1 | grep -v "^\s*Compiling"
  '
