#!/bin/bash
# OWL-647 (runtime and operations, "More stacks"), on macOS directly and on Linux through owl-647-linux.sh: Swift's
# lines, checked with the real `owlshift gate`, confined, on a fixture repository (the package of owl-628.sh: `Files`
# 4.2.0 from GitHub, one swift-testing test). The harness is owl-628.sh's.
#
# `init` writes a note for Swift, so OWLSHIFT_BINARY is a local build whose `stack::swift` writes the note's lines
# instead, as OWL-638's check did: the setup `swift package --disable-sandbox resolve`, the gate `swift build
# --disable-sandbox` and the note's `swift test` line, and `run_caches.CLANG_MODULE_CACHE_PATH = "clang-modules"`.
# Its `owlshift.toml` holds the test line as a TOML literal string, which this script reads back.
#
# The runs, in order, each a `owlshift gate` of the same checkout, so the run caches fill once:
#   1. as `init` wrote it, on empty run caches: the cold gate's time;
#   2. as written again, RUNS times;
#   3. once per `run_caches` line, that line alone removed;
#   4. the test line replaced by a plain `swift test --disable-sandbox`, RUNS times;
#   5. unconfined, in a fresh clone each time, `swift test --disable-sandbox` and the test line as written, RUNS
#      times each, with the module cache in a scratch folder.
#
# Usage: [RUNS=N] [LOGS=DIR] [EXTRA_PATH=DIRS] bash owl-647.sh OWLSHIFT_BINARY   (network needed)
#   RUNS: how many times the runs 2, 4 and 5 are repeated (default 3).
#   LOGS: a folder that keeps each gate's whole log.
#   EXTRA_PATH: folders put first on `PATH`, never under `/tmp`, which the sandbox closes.
# It writes only under a temporary folder, removed at the end, unless KEEP=1. It prints results only, the home shown
# as `~`.
set -u
bin=$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")
home=$(cd "$HOME" && pwd -P)
work=$(mktemp -d "${TMPDIR:-/tmp}/owl647.XXXXXX"); work=$(cd "$work" && pwd -P)
export OWLSHIFT_CONFIG_DIR=$work/config OWLSHIFT_DATA_DIR=$work/data OWLSHIFT_CACHE_DIR=$work/cache
export GIT_CONFIG_GLOBAL=$work/gitconfig
runs=${RUNS:-3}
say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
clean() { sed "s|$home|~|g; s|$work|<work>|g; s/^/  | /"; }
if [ "$(uname -s)" = Darwin ]; then
  export PATH=${EXTRA_PATH:+$EXTRA_PATH:}/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin
  system="macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m), $(xcode-select -p)"
else
  export PATH=${EXTRA_PATH:+$EXTRA_PATH:}/usr/local/bin:/usr/bin:/bin
  system="$(. /etc/os-release && echo "$PRETTY_NAME") $(uname -m), $(bwrap --version), uid $(id -u)"
fi

mkdir -p "$work/config" "$work/ssh/owl647"
printf '#!/bin/sh\n# The git command an ssh host would run, run here.\ncd %s && exec sh -c "$2"\n' "'$work/ssh'" >"$work/local-ssh"
chmod +x "$work/local-ssh"
for kv in "core.sshCommand $work/local-ssh" "ssh.variant simple" "user.name Probe" "user.email probe@example.com" \
  "init.defaultBranch main" "commit.gpgsign false"; do
  git config --global "${kv%% *}" "${kv#* }"
done
say "$system; $("$bin" --version | head -1); $(swift --version 2>/dev/null | grep -o 'Swift version [^ ]*')"

# The fixture of owl-628.sh, its dependency resolved once unconfined, into a throwaway cache, to check the network.
dir=$work/src/swift
mkdir -p "$dir/Sources/Probe" "$dir/Tests/ProbeTests"
printf '// swift-tools-version:6.0\nimport PackageDescription\n\nlet package = Package(\n    name: "Probe",\n    dependencies: [\n        .package(url: "https://github.com/JohnSundell/Files", exact: "4.2.0"),\n    ],\n    targets: [\n        .target(name: "Probe", dependencies: ["Files"]),\n        .testTarget(name: "ProbeTests", dependencies: ["Probe"]),\n    ]\n)\n' \
  >"$dir/Package.swift"
printf 'import Files\n\npublic func word() -> String {\n    _ = Folder.self\n    return "Probe"\n}\n' \
  >"$dir/Sources/Probe/Probe.swift"
printf 'import Testing\n@testable import Probe\n\n@Test func probeWord() {\n    #expect(word() == "Probe")\n}\n' \
  >"$dir/Tests/ProbeTests/ProbeTests.swift"
printf '.build/\n.swiftpm/\n' >"$dir/.gitignore"
(cd "$dir" && swift package --cache-path "$work/seed/cache" --config-path "$work/seed/config" \
  --security-path "$work/seed/security" resolve >/dev/null 2>&1 && rm -rf .build) || say "swift could not resolve"
git init -q --bare "$work/ssh/owl647/swift.git"
(cd "$dir" && git init -q && git remote add origin "git@github.com:owl647/swift.git" && git add -A \
  && git commit -q -m fixture && git push -q origin main) >/dev/null 2>&1 || say "the fixture could not be pushed"
(cd "$dir" && "$bin" init --tracker markdown --skip-secrets </dev/null 2>&1 | grep -E '^(Detected|  Note|Skipped)') | clean
printf 'tickets/\n' >>"$dir/.git/info/exclude"
say "the [stack] written:"
sed -n '/^\[stack\]/,/^# The files agents receive/p' "$dir/owlshift.toml" \
  | grep -E '^(gate|setup|caches|run_caches|\]|    |# read from|# note:)' | clean
cp "$dir/owlshift.toml" "$work/owlshift.toml.written"
# The test line, a TOML literal string since it holds double quotes.
test_line=$(sed -n "s/^    '''\(swift test .*\)''',\$/\1/p" "$work/owlshift.toml.written")
[ -n "$test_line" ] || say "no test line in the literal form this script reads"

# gate LABEL: commits and pushes the checkout, then runs `owlshift gate`, printing its verdict and its end.
gate() {
  local label=$1 out rc start
  (cd "$dir" && git add -A && git commit -q -m "$label" && git push -q origin main) >/dev/null 2>&1
  start=$(date +%s)
  out=$(cd "$dir" && "$bin" gate </dev/null 2>&1); rc=$?
  say "$label: owlshift gate exit $rc in $(($(date +%s) - start)) s"
  # LOGS: a folder that keeps each gate's whole log, named by its label.
  [ -n "${LOGS:-}" ] && mkdir -p "$LOGS" \
    && cp "$work/data/projects/github.com/owl647/swift/gate/run/gate.log" "$LOGS/$(printf '%s' "$label" | tr -c 'A-Za-z0-9\n' -).log"
  out=$(printf '%s\n' "$out" | grep -v '^Its whole output goes to\|^Gate log: ')
  if printf '%s\n' "$out" | grep -q '^Gate red'; then
    printf '%s\n' "$out" | grep -B"${TAIL:-6}" '^Gate red' | clean
  else
    printf '%s\n' "$out" | tail -"${TAIL:-4}" | clean
  fi
}

gate "as written, empty run caches"
say "the run caches: $(cd "$work/cache/github.com/owl647/swift" 2>/dev/null && du -sk -- * 2>/dev/null \
  | awk '{ printf "%s %s, ", $2, ($1 < 1024 ? $1 " kB" : sprintf("%.0f MB", $1 / 1024)) }')"
for i in $(seq "$runs"); do gate "as written, run $i"; done
for variable in $(sed -n 's/^run_caches\.\([A-Za-z_0-9]*\) = .*/\1/p' "$work/owlshift.toml.written"); do
  awk -v v="$variable" 'index($0, "run_caches." v " = ") != 1' "$work/owlshift.toml.written" >"$dir/owlshift.toml"
  gate "without run_caches.$variable"
done
# The test line as OWL-638 checked it, without the plugin path.
awk 'index($0, "    '"'''"'swift test ") == 1 { print "    \"swift test --disable-sandbox\","; next } { print }' \
  "$work/owlshift.toml.written" >"$dir/owlshift.toml"
for i in $(seq "$runs"); do gate "plain swift test --disable-sandbox, run $i"; done
cp "$work/owlshift.toml.written" "$dir/owlshift.toml"

# Unconfined: a fresh clone each time, so `.build` starts empty as in a gate's worktree.
for line in "swift test --disable-sandbox" "$test_line"; do
  for i in $(seq "$runs"); do
    rm -rf "$work/bare" && git clone -q "$work/ssh/owl647/swift.git" "$work/bare" 2>/dev/null
    start=$(date +%s)
    out=$(cd "$work/bare" && CLANG_MODULE_CACHE_PATH=$work/bare-modules sh -c "$line" </dev/null 2>&1); rc=$?
    say "unconfined, $(printf '%s' "$line" | cut -c1-40)..., run $i: exit $rc in $(($(date +%s) - start)) s"
    printf '%s\n' "$out" | grep -m1 "plugin for module 'TestingMacros' not found" | cut -c1-160 | clean
  done
done

if [ -n "${KEEP:-}" ]; then say "kept the scratch folder $work"; exit 0; fi
chmod -R u+w "$work" 2>/dev/null
rm -rf "$work"
say "removed the scratch folder"
