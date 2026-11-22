#!/bin/sh
# OWL-628 on Linux: owl-628.sh's runs, in Docker, with a Linux build of this checkout's `owlshift` whose sandbox is
# bwrap. Ubuntu 24.04 with bubblewrap, git and unzip; Erlang/OTP 28 from the Erlang Ecosystem Foundation's builds
# for Ubuntu 24.04 in /opt/otp, Elixir 1.20.5 from its release archive in /opt/elixir, Deno 2.9.8 from its release
# archive in /usr/local/bin; a non-root user `owl` (uid 1001) whose rustup and tool chain, from rustup's own
# installer, build `owlshift` from this checkout, mounted read-only, into a volume of its own. The two options bwrap
# needs inside Docker: `--security-opt seccomp=unconfined --security-opt systempaths=unconfined`. As
# owl-627-linux.sh, whose rig this is with the tool chains of OWL-628 in place of OWL-627's.
#
# Written on 2026-11-23 and not run: see docs/design/records/OWL-628.md.
#
# Usage, Docker running: sh docs/design/live-checks/owl-628-linux.sh [ECOSYSTEM...]   (network needed)
# It prints results only. The volumes `owl628-cargo` and `owl628-target` keep the build between runs: remove them
# with `docker volume rm owl628-cargo owl628-target`.
set -eu
repo=$(cd "$(dirname "$0")/../../.." && pwd -P)
image=owlshift-owl628-rig
docker build -q -t "$image" - <<'EOF' >/dev/null
FROM ubuntu:24.04
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
      bubblewrap ca-certificates curl git zip unzip build-essential libncurses6 libssl3 >/dev/null \
    && rm -rf /var/lib/apt/lists/*
RUN arch=$(uname -m) && mkdir -p /opt/otp && curl -fsSL \
      "https://github.com/erlef/otp_builds/releases/download/maint-28-latest/otp-$arch-unknown-linux-gnu-ubuntu-24.04.tar.gz" \
      | tar -xz -C /opt/otp \
    && curl -fsSL -o /tmp/elixir.zip https://github.com/elixir-lang/elixir/releases/download/v1.20.5/elixir-otp-28.zip \
    && unzip -q /tmp/elixir.zip -d /opt/elixir && rm /tmp/elixir.zip \
    && curl -fsSL -o /tmp/deno.zip "https://github.com/denoland/deno/releases/download/v2.9.8/deno-$arch-unknown-linux-gnu.zip" \
    && unzip -q /tmp/deno.zip -d /usr/local/bin && rm /tmp/deno.zip
RUN useradd -m -u 1001 owl
USER owl
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain 1.98.1 >/dev/null 2>&1 \
    && mkdir -p /home/owl/.cargo/registry /home/owl/target
EOF
docker run --rm -i --security-opt seccomp=unconfined --security-opt systempaths=unconfined \
  -v "$repo:/src:ro" -v owl628-cargo:/home/owl/.cargo/registry -v owl628-target:/home/owl/target "$image" \
  bash -c 'cd /src && CARGO_TARGET_DIR=/home/owl/target /home/owl/.cargo/bin/cargo build -q -p owlshift-cli --locked \
    2>&1 | tail -5 && EXTRA_PATH=/opt/otp/bin:/opt/elixir/bin \
    bash /src/docs/design/live-checks/owl-628.sh /home/owl/target/debug/owlshift "$@"' owl628 "$@"
