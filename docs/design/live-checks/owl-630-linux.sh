#!/bin/sh
# OWL-630 on Linux: owl-630.sh's runs, in Docker, with a Linux build of this checkout's `owlshift` whose sandbox is
# bwrap. Ubuntu 24.04 with bubblewrap, git and python3; node 24.22.3 from nodejs.org in /opt/node, with pnpm 10.33.2
# installed there by npm; uv 0.11.7 copied from its image; a non-root user `owl` (uid 1001) whose rustup and tool
# chain, from rustup's own installer, build `owlshift` from this checkout, mounted read-only, into a volume of its own.
# The two options bwrap needs inside Docker: `--security-opt seccomp=unconfined --security-opt systempaths=unconfined`.
# The rig of owl-629-linux.sh, with uv and python3 in place of OWL-629's tool chains. owl-630.sh skips its sweep of
# removed run caches on Linux unless SWEEP=1: the private home lets an install with a variable left out succeed into a
# folder lost with the command (OWL-617, OWL-624), so those runs cannot fail there.
#
# Usage, Docker running: [SWEEP=1] sh docs/design/live-checks/owl-630-linux.sh [FIXTURE...]   (network needed)
# It prints results only. The volumes `owl630-cargo` and `owl630-target` keep the build between runs: remove them
# with `docker volume rm owl630-cargo owl630-target`.
set -eu
repo=$(cd "$(dirname "$0")/../../.." && pwd -P)
image=owlshift-owl630-rig
docker build -q -t "$image" - <<'EOF' >/dev/null
FROM ubuntu:24.04
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
      bubblewrap ca-certificates curl git xz-utils build-essential >/dev/null \
    && rm -rf /var/lib/apt/lists/*
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends python3 >/dev/null \
    && rm -rf /var/lib/apt/lists/*
RUN arch=$(dpkg --print-architecture | sed 's/amd64/x64/') \
    && curl -fsSL "https://nodejs.org/dist/v24.22.3/node-v24.22.3-linux-$arch.tar.xz" | tar -xJ -C /opt \
    && mv /opt/node-v24.22.3-linux-$arch /opt/node && export PATH=/opt/node/bin:$PATH \
    && npm install -g --silent pnpm@10.33.2 && rm -rf /root/.npm
COPY --from=ghcr.io/astral-sh/uv:0.11.7 /uv /usr/local/bin/uv
RUN useradd -m -u 1001 owl
USER owl
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain 1.98.1 >/dev/null 2>&1 \
    && mkdir -p /home/owl/.cargo/registry /home/owl/target
EOF
docker run --rm -i --security-opt seccomp=unconfined --security-opt systempaths=unconfined -e SWEEP -e TAIL \
  -v "$repo:/src:ro" -v owl630-cargo:/home/owl/.cargo/registry -v owl630-target:/home/owl/target "$image" \
  bash -c 'cd /src && CARGO_TARGET_DIR=/home/owl/target /home/owl/.cargo/bin/cargo build -q -p owlshift-cli --locked \
    2>&1 | tail -5 && cd && bash /src/docs/design/live-checks/owl-630.sh /home/owl/target/debug/owlshift "$@"' owl630 "$@"
