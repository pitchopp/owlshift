#!/bin/sh
# OWL-627 on Linux: owl-627.sh's runs, in Docker, with a Linux build of this checkout's `owlshift` whose sandbox is
# bwrap. Ubuntu 24.04 with bubblewrap, git, Python 3 with venv and pdm (pip, in /opt/pdm), Maven and OpenJDK 21
# headless, PHP's command line with the extensions PHPUnit needs and Composer from the distribution, the .NET SDK 10
# from Microsoft's dotnet-install script in /usr/share/dotnet; a non-root user `owl` (uid 1001) whose rustup and tool
# chain, from rustup's own installer, build `owlshift` from this checkout, mounted read-only, into a volume of its own.
# The two options bwrap needs inside Docker: `--security-opt seccomp=unconfined --security-opt systempaths=unconfined`.
# As owl-624-linux.sh, whose rig this is with the four tool chains of OWL-627 in place of OWL-624's.
#
# Written on 2026-11-22 and not run: see docs/design/records/OWL-627.md.
#
# Usage, Docker running: sh docs/design/live-checks/owl-627-linux.sh [ECOSYSTEM...]   (network needed)
# It prints results only. The volumes `owl627-cargo` and `owl627-target` keep the build between runs: remove them
# with `docker volume rm owl627-cargo owl627-target`.
set -eu
repo=$(cd "$(dirname "$0")/../../.." && pwd -P)
image=owlshift-owl627-rig
docker build -q -t "$image" - <<'EOF' >/dev/null
FROM ubuntu:24.04
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
      bubblewrap ca-certificates curl git zip unzip build-essential python3 python3-venv \
      maven openjdk-21-jdk-headless php-cli php-xml php-mbstring composer libicu74 >/dev/null \
    && rm -rf /var/lib/apt/lists/*
RUN python3 -m venv /opt/pdm && /opt/pdm/bin/pip install -q pdm && ln -s /opt/pdm/bin/pdm /usr/local/bin/pdm
RUN curl -fsSL -o /tmp/dotnet-install.sh https://dot.net/v1/dotnet-install.sh \
    && bash /tmp/dotnet-install.sh --channel 10.0 --install-dir /usr/share/dotnet >/dev/null \
    && ln -s /usr/share/dotnet/dotnet /usr/local/bin/dotnet && rm /tmp/dotnet-install.sh
RUN useradd -m -u 1001 owl
USER owl
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain 1.98.1 >/dev/null 2>&1 \
    && mkdir -p /home/owl/.cargo/registry /home/owl/target
EOF
docker run --rm -i --security-opt seccomp=unconfined --security-opt systempaths=unconfined \
  -v "$repo:/src:ro" -v owl627-cargo:/home/owl/.cargo/registry -v owl627-target:/home/owl/target "$image" \
  bash -c 'cd /src && CARGO_TARGET_DIR=/home/owl/target /home/owl/.cargo/bin/cargo build -q -p owlshift-cli --locked \
    2>&1 | tail -5 && bash /src/docs/design/live-checks/owl-627.sh /home/owl/target/debug/owlshift "$@"' owl627 "$@"
