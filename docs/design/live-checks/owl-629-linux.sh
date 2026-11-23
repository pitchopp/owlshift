#!/bin/sh
# OWL-629 on Linux: owl-629.sh's runs, in Docker, with a Linux build of this checkout's `owlshift` whose sandbox is
# bwrap. Ubuntu 24.04 with bubblewrap and git; node 24.22.3 from nodejs.org in /opt/node, with pnpm 10.33.2 installed
# there by npm as the machine's pnpm; the fixtures' tool chains installed by npm into /opt/owl629-tools, one prefix
# each, as owl-629.sh's `TOOLS` names them; a non-root user `owl` (uid 1001) whose rustup and tool chain, from
# rustup's own installer, build `owlshift` from this checkout, mounted read-only, into a volume of its own. The two
# options bwrap needs inside Docker: `--security-opt seccomp=unconfined --security-opt systempaths=unconfined`. As
# owl-628-linux.sh, whose rig this is with the tool chains of OWL-629 in place of OWL-628's.
#
# Usage, Docker running: [STACK_EDIT=SED] sh docs/design/live-checks/owl-629-linux.sh [FIXTURE...]   (network needed)
# It prints results only. The volumes `owl629-cargo` and `owl629-target` keep the build between runs: remove them
# with `docker volume rm owl629-cargo owl629-target`.
set -eu
repo=$(cd "$(dirname "$0")/../../.." && pwd -P)
image=owlshift-owl629-rig
docker build -q -t "$image" - <<'EOF' >/dev/null
FROM ubuntu:24.04
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
      bubblewrap ca-certificates curl git xz-utils build-essential >/dev/null \
    && rm -rf /var/lib/apt/lists/*
RUN arch=$(dpkg --print-architecture | sed 's/amd64/x64/') \
    && curl -fsSL "https://nodejs.org/dist/v24.22.3/node-v24.22.3-linux-$arch.tar.xz" | tar -xJ -C /opt \
    && mv /opt/node-v24.22.3-linux-$arch /opt/node && export PATH=/opt/node/bin:$PATH \
    && npm install -g --silent pnpm@10.33.2 \
    && for spec in pnpm9:pnpm@9.15.9 pnpm11:pnpm@11.28.13 pnpm12:pnpm@12.21.0 yarn4:@yarnpkg/cli-dist@4.19.0 \
         yarn3:@yarnpkg/cli-dist@3.8.7 yarn2:@yarnpkg/cli-dist@2.4.2 corepack:corepack@0.36.0; do \
         npm install -g --silent --prefix "/opt/owl629-tools/${spec%%:*}" "${spec#*:}" || exit 1; done \
    && rm -rf /root/.npm
RUN useradd -m -u 1001 owl
USER owl
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain 1.98.1 >/dev/null 2>&1 \
    && mkdir -p /home/owl/.cargo/registry /home/owl/target
EOF
docker run --rm -i --security-opt seccomp=unconfined --security-opt systempaths=unconfined -e STACK_EDIT \
  -v "$repo:/src:ro" -v owl629-cargo:/home/owl/.cargo/registry -v owl629-target:/home/owl/target "$image" \
  bash -c 'cd /src && CARGO_TARGET_DIR=/home/owl/target /home/owl/.cargo/bin/cargo build -q -p owlshift-cli --locked \
    2>&1 | tail -5 && TOOLS=/opt/owl629-tools \
    bash /src/docs/design/live-checks/owl-629.sh /home/owl/target/debug/owlshift "$@"' owl629 "$@"
