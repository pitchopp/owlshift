#!/bin/sh
# OWL-647 on Linux: owl-647.sh's runs, in Docker, with a Linux build of this checkout's `owlshift` whose sandbox is
# bwrap. The official image `swift:6.4.3-noble` (Ubuntu 24.04, the Swift tool chain in /usr) with bubblewrap, git and
# a C tool chain; a non-root user `owl` (uid 1001) whose rustup and tool chain, from rustup's own installer, build
# `owlshift` from this checkout, mounted read-only, into a volume of its own. The two options bwrap needs inside
# Docker: `--security-opt seccomp=unconfined --security-opt systempaths=unconfined`. The rig of owl-628-linux.sh, with
# Swift's image as its base. As owl-647.sh says, the checkout built is one whose `stack::swift` writes the note's
# lines (a local change, as on macOS): built as this ticket left it, `init` writes a note and no line, and the runs have
# nothing to check.
#
# On Linux the private home lets a tool write its default cache into a folder lost with the command (OWL-617,
# OWL-624), so the run with `CLANG_MODULE_CACHE_PATH` removed is expected green there.
#
# Usage, Docker running: [RUNS=N] [TAIL=N] sh docs/design/live-checks/owl-647-linux.sh   (network needed)
# It prints results only. The volumes `owl647-cargo` and `owl647-target` keep the build between runs: remove them
# with `docker volume rm owl647-cargo owl647-target`.
set -eu
repo=$(cd "$(dirname "$0")/../../.." && pwd -P)
image=owlshift-owl647-rig
docker build -q -t "$image" - <<'EOF' >/dev/null
FROM swift:6.4.3-noble
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
      bubblewrap ca-certificates curl git build-essential >/dev/null \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -m -u 1001 owl
USER owl
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain 1.98.1 >/dev/null 2>&1 \
    && mkdir -p /home/owl/.cargo/registry /home/owl/target
EOF
docker run --rm -i --security-opt seccomp=unconfined --security-opt systempaths=unconfined -e RUNS -e TAIL \
  -v "$repo:/src:ro" -v owl647-cargo:/home/owl/.cargo/registry -v owl647-target:/home/owl/target "$image" \
  bash -c 'cd /src && CARGO_TARGET_DIR=/home/owl/target /home/owl/.cargo/bin/cargo build -q -p owlshift-cli --locked \
    2>&1 | tail -5 && cd && bash /src/docs/design/live-checks/owl-647.sh /home/owl/target/debug/owlshift'
