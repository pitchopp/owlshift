#!/bin/sh
# OWL-597 on Linux (build plan, "P9 tasks & exit gate", "OWL-597, the home folder the sandbox refused"): what does a
# read or a write to a closed home folder answer under bwrap, with the home laid out as
# `owlshift_platform::sandbox::bwrap_args` lays it out for a gate command: `/` read-only, a private `/tmp`, the home a
# private tmpfs, an allowed cache bound back read-only (`--ro-bind-try`), the worktree and the run's temporary folder
# bound read and written. And what do npm and pip print for the same refusals?
#
# Ubuntu 24.04, bubblewrap, npm and pip from the distribution, a non-root user `owl` (uid 1001, home /home/owl)
# whose home holds `~/.cache/closed/file`, `~/.cache/allowed/file` and a `~/.npm` cache, and the two options bwrap
# needs inside Docker (`--security-opt seccomp=unconfined --security-opt systempaths=unconfined`, not --privileged).
#
# Usage, Docker running: sh docs/design/live-checks/owl-597-linux.sh
# No network is used inside the sandbox runs but npm's and pip's own attempts, which are expected to fail or be
# refused first. It prints results only.
set -eu
image=owlshift-owl597-rig
docker build -q -t "$image" - <<'EOF' >/dev/null
FROM ubuntu:24.04
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends \
      bubblewrap ca-certificates npm python3-pip >/dev/null \
    && rm -rf /var/lib/apt/lists/*
RUN useradd -m -u 1001 owl
USER owl
RUN mkdir -p /home/owl/.cache/closed /home/owl/.cache/allowed /home/owl/.npm/_cacache /home/owl/wt /home/owl/run-tmp \
    && echo secret-free > /home/owl/.cache/closed/file && echo cached > /home/owl/.cache/allowed/file
EOF
docker run --rm -i --security-opt seccomp=unconfined --security-opt systempaths=unconfined "$image" sh -s <<'EOF'
set -u
H=/home/owl
echo "$(. /etc/os-release && echo "$PRETTY_NAME"), $(bwrap --version), uid $(id -u), npm $(npm --version), $(pip3 --version | cut -d' ' -f1-2)"
confined() {
  bwrap --die-with-parent --unshare-pid --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
    --tmpfs "$H" --ro-bind-try "$H/.cache/allowed" "$H/.cache/allowed" \
    --bind-try "$H/run-tmp" "$H/run-tmp" --bind-try "$H/wt" "$H/wt" --chdir "$H/wt" \
    --setenv HOME "$H" --setenv TMPDIR "$H/run-tmp" -- sh -c "$1" 2>&1 | sed 's/^/  | /'
}
check() { echo "\$ $1"; confined "$1; echo \"exit \$?\""; }
check 'cat ~/.cache/closed/file'
check 'ls ~/.cache/closed'
check 'touch ~/.cache/closed/new'
check 'mkdir -p ~/.cache/demo && touch ~/.cache/demo/x && ls ~/.cache/demo'
check 'touch ~/top-level'
check 'cat ~/.cache/allowed/file'
check 'touch ~/.cache/allowed/new'
check 'mkdir ~/.cache/allowed/sub'
check 'ls ~/.ssh'
echo "after the runs, on the host side: $(ls -A "$H/.cache" | tr '\n' ' ')"
echo "--- npm and pip, their caches in the closed home, no network"
cd "$H/wt" && printf '{"name":"p","version":"1.0.0","dependencies":{"is-number":"7.0.0"}}' > package.json
check 'npm install --prefer-offline --no-audit --no-fund --fetch-retries=0 --fetch-timeout=2000 --registry http://127.0.0.1:9/ 2>&1 | tail -6'
check 'npm install --offline --no-audit --no-fund --cache ~/.cache/allowed/npm 2>&1 | tail -6'
check 'pip3 download --no-deps --retries 0 --timeout 2 --index-url http://127.0.0.1:9/simple --cache-dir ~/.cache/allowed/pip -d "$TMPDIR/d" six 2>&1 | tail -6'
EOF
