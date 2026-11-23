#!/bin/sh
# OWL-624 on Linux: owl-624.sh's runs, in Docker, with a Linux build of this checkout's `owlshift` whose sandbox is
# bwrap. Ubuntu 24.04 with bubblewrap, git, Python 3 with venv and pip, poetry and Ruby with bundler from the
# distribution, OpenJDK 21 headless, Go from go.dev in /usr/local/go and Gradle from gradle.org in /opt/gradle (used
# unconfined only, to make the fixture's wrapper); a non-root user `owl` (uid 1001) whose rustup and tool chain, from
# rustup's own installer, build `owlshift` from this checkout, mounted read-only, into a volume of its own. The two
# options bwrap needs inside Docker: `--security-opt seccomp=unconfined --security-opt systempaths=unconfined`.
#
# Run on 2026-11-23: see docs/design/records/OWL-632.md. The harness runs from the home, not from `/src`, whose `.git`
# names a host path when the checkout is a git worktree, which would fail its `git config --global`.
#
# Usage, Docker running: [TAIL=N] [CONCURRENT=1] [PY_TOOLS=pytest] sh docs/design/live-checks/owl-624-linux.sh
#   [hand|init] [ECOSYSTEM...]   (network needed; the variables are owl-624.sh's, passed into the container)
# It prints results only. The volumes `owl624-cargo` and `owl624-target` keep the build between runs: remove them
# with `docker volume rm owl624-cargo owl624-target`.
set -eu
repo=$(cd "$(dirname "$0")/../../.." && pwd -P)
image=owlshift-owl624-rig
docker build -q -t "$image" - <<'EOF' >/dev/null
FROM ubuntu:24.04
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
      bubblewrap ca-certificates curl git zip unzip build-essential python3 python3-venv python3-pip python3-poetry \
      ruby ruby-bundler openjdk-21-jdk-headless >/dev/null \
    && rm -rf /var/lib/apt/lists/*
RUN arch=$(dpkg --print-architecture) \
    && curl -fsSL "https://go.dev/dl/go1.27.4.linux-$arch.tar.gz" | tar -xz -C /usr/local
RUN curl -fsSL -o /tmp/gradle.zip https://services.gradle.org/distributions/gradle-9.9.2-bin.zip \
    && unzip -q /tmp/gradle.zip -d /opt && mv /opt/gradle-9.9.2 /opt/gradle && rm /tmp/gradle.zip
RUN useradd -m -u 1001 owl
USER owl
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain 1.98.1 >/dev/null 2>&1 \
    && mkdir -p /home/owl/.cargo/registry /home/owl/target
EOF
docker run --rm -i --security-opt seccomp=unconfined --security-opt systempaths=unconfined \
  -e TAIL -e CONCURRENT -e PY_TOOLS \
  -v "$repo:/src:ro" -v owl624-cargo:/home/owl/.cargo/registry -v owl624-target:/home/owl/target "$image" \
  bash -c 'cd /src && CARGO_TARGET_DIR=/home/owl/target /home/owl/.cargo/bin/cargo build -q -p owlshift-cli --locked \
    2>&1 | tail -5 && cd && bash /src/docs/design/live-checks/owl-624.sh /home/owl/target/debug/owlshift "$@"' owl624 "$@"
