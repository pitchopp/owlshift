#!/bin/sh
# OWL-563, the Linux leg of the live check "Bash on the read-only roles" (docs/design/build-plan.md, the Claude Code
# harness adapter's "Permissions"): runs the two ignored tests of crates/owlshift-testkit/tests/executor_live.rs that
# OWL-563 changed or added, under bwrap in a Linux container, as OWL-344 and OWL-387 ran theirs:
#   a_read_only_bash_run_reads_and_never_writes        a Design-shaped run with Bash and its web tools
#   a_bash_session_resumes_under_the_read_only_profile  its session kept, then resumed
# Ubuntu 24.04, bubblewrap, git, curl, Rust and Claude Code (the version of this Mac's, 2.1.318 by default), a
# non-root user `owl` (uid 1001) whose home is /home/owl, and the two options bwrap needs inside Docker
# (`--security-opt seccomp=unconfined --security-opt systempaths=unconfined`, not --privileged).
#
# Usage, on macOS, from a checkout of the branch, Docker running, the token for agent runs stored by `owlshift init`:
#   sh docs/design/live-checks/owl-563-linux.sh
# The token is read from the macOS keychain (service `owlshift`, account `claude-agent`) and piped on standard
# input into a tmpfs under the closed home (`OWLSHIFT_LIVE_TOKEN_FILE`), which the run must not see; it is never
# printed, never in an argument or an environment variable, and gone with the container. `/outside` is a tmpfs
# the user can write outside every folder the sandbox makes private (`OWLSHIFT_LIVE_OUTSIDE_DIR`).
# The checkout is mounted read-only; the build goes to the Docker volumes owl563-target and owl563-cargo (remove them
# with `docker volume rm owl563-target owl563-cargo`). It prints the tests' lines and results only. The two runs
# spend a little of the subscription.
set -eu
here=$(cd "$(dirname "$0")/../../.." && pwd)
toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$here/rust-toolchain.toml")
claude_version=${CLAUDE_VERSION:-2.1.318}
image=owlshift-owl563-rig
docker build -q -t "$image" --build-arg "TOOLCHAIN=$toolchain" --build-arg "CLAUDE_VERSION=$claude_version" - <<'EOF' >/dev/null
FROM ubuntu:24.04
ARG TOOLCHAIN
ARG CLAUDE_VERSION
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
      bubblewrap ca-certificates curl gcc git libc6-dev pkg-config >/dev/null \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -m -u 1001 owl && mkdir -p /target /cargo/registry && chown -R owl /target /cargo
USER owl
ENV CARGO_HOME=/cargo RUSTUP_HOME=/home/owl/.rustup PATH=/home/owl/.local/bin:/cargo/bin:/usr/local/bin:/usr/bin:/bin
RUN curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain "$TOOLCHAIN" --no-modify-path
RUN curl -fsSL https://claude.ai/install.sh | bash -s "$CLAUDE_VERSION" >/dev/null
EOF
security find-generic-password -s owlshift -a claude-agent -w | docker run --rm -i \
  --security-opt seccomp=unconfined --security-opt systempaths=unconfined \
  --tmpfs /home/owl/.owl563:rw,uid=1001,gid=1001,mode=0700 \
  --tmpfs /outside:rw,uid=1001,gid=1001,mode=0755 \
  -v "$here:/src:ro" -v owl563-target:/target -v owl563-cargo:/cargo/registry \
  -e CARGO_TARGET_DIR=/target -e OWLSHIFT_LIVE_CLAUDE=1 \
  -e OWLSHIFT_LIVE_TOKEN_FILE=/home/owl/.owl563/token -e OWLSHIFT_LIVE_OUTSIDE_DIR=/outside \
  -w /src "$image" sh -c '
    set -eu
    umask 077
    cat > "$OWLSHIFT_LIVE_TOKEN_FILE"
    test -s "$OWLSHIFT_LIVE_TOKEN_FILE" || { echo "no token on standard input"; exit 1; }
    echo "kernel=$(uname -r) arch=$(uname -m) $(. /etc/os-release; echo "$PRETTY_NAME") $(bwrap --version) claude=$(claude --version) user=$(id -un) uid=$(id -u)"
    failed=0
    for test in a_read_only_bash_run_reads_and_never_writes a_bash_session_resumes_under_the_read_only_profile; do
      echo "== $test"
      log=/tmp/owl563-$test.log
      if cargo test -q -p owlshift-testkit --test executor_live --locked -- --ignored --nocapture --exact "$test" \
        >"$log" 2>&1; then status=0; else status=$?; fi
      grep -v "^\s*Compiling" "$log" || true
      # A test filtered out reports "0 passed" with status 0: that is no pass either.
      if [ "$status" -eq 0 ] && grep -q "test result: ok. 1 passed" "$log"; then
        echo "PASS $test"
      else
        echo "FAIL $test (cargo status $status)"
        failed=1
      fi
    done
    exit "$failed"
  '
