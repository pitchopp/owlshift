#!/bin/bash
# OWL-647 on macOS, beside owl-647.sh: why `swift test` of a swift-testing target fails, outside Owlshift. A scratch
# package (one target, one swift-testing test, no dependency), each run from an empty `.build`:
#   1. unconfined, RUNS times each, plain `swift test --disable-sandbox` and the same with swift-testing's plugin
#      folder (`-Xswiftc -plugin-path`, the path `swift -print-target-info` gives): the plugin left out;
#   2. under `sandbox-exec` with small profiles, the plugin folder given, `TMPDIR` and the clang module cache in DIR:
#      everything allowed, then the services that answer the user's temporary folder closed (as Owlshift's profile
#      does since OWL-637) with writes refused in `/private/tmp`, `/private/var/tmp` and `/private/var/folders`, one,
#      two or three of them;
#   3. the link of the test bundle a failing run printed, replayed under the profile closing `/private/tmp` and
#      `/private/var/tmp`, with the environment Swift Build gives it (`PATH` alone), then with `TMPDIR` added; and
#      `swiftc --version` the same way.
#
# Usage: [RUNS=N] bash owl-647-bisect.sh DIR   (DIR: an empty folder outside /tmp, /var/tmp and /var/folders, which
#   the profiles close; RUNS defaults to 5). It writes only under DIR, removed at the end unless KEEP=1, and prints
#   results only.
set -u
dir=$(mkdir -p "$1" && cd "$1" && pwd -P)
runs=${RUNS:-5}
say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
say "macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)) $(uname -m), $(xcode-select -p);" \
  "$(swift --version 2>/dev/null | grep -o 'Swift version [^ ]*')"

pkg=$dir/pkg
mkdir -p "$pkg/Sources/Probe" "$pkg/Tests/ProbeTests" "$dir/tmp" "$dir/modules"
printf '// swift-tools-version:6.0\nimport PackageDescription\n\nlet package = Package(\n    name: "Probe",\n    targets: [\n        .target(name: "Probe"),\n        .testTarget(name: "ProbeTests", dependencies: ["Probe"]),\n    ]\n)\n' \
  >"$pkg/Package.swift"
printf 'public func word() -> String {\n    return "Probe"\n}\n' >"$pkg/Sources/Probe/Probe.swift"
printf 'import Testing\n@testable import Probe\n\n@Test func probeWord() {\n    #expect(word() == "Probe")\n}\n' \
  >"$pkg/Tests/ProbeTests/ProbeTests.swift"
swiftc=$(xcrun --find swiftc)
plugins="$(swift -print-target-info | sed -n 's/.*"runtimeResourcePath": "\(.*\)".*/\1/p')/host/plugins/testing"

# verdict LOG: how a run ended.
verdict() {
  if grep -q "plugin for module 'TestingMacros' not found" "$1"; then echo "TestingMacros plugin not found"
  elif grep -q 'permissionDenied' "$1"; then echo "permissionDenied at the link"
  elif grep -q 'Test run with 1 test.*passed' "$1"; then echo "test passed"
  else echo "other: $(grep -m1 'error:' "$1" | cut -c1-120)"; fi
}

say "--- 1. unconfined"
for label in plain plugin-path; do
  for i in $(seq "$runs"); do
    rm -rf "$pkg/.build"
    if [ $label = plain ]; then set -- swift test --disable-sandbox
    else set -- swift test --disable-sandbox -Xswiftc -plugin-path -Xswiftc "$plugins"; fi
    (cd "$pkg" && "$@") >"$dir/run.log" 2>&1
    say "$label, run $i: exit $?, $(verdict "$dir/run.log")"
  done
done

say "--- 2. sandbox-exec, the plugin folder given"
dirhelper='(deny mach-lookup (global-name "com.apple.bsd.dirhelper") (global-name "com.apple.system.opendirectoryd.membership"))'
tmp='(subpath "/private/tmp")' vartmp='(subpath "/private/var/tmp")' folders='(subpath "/private/var/folders")'
# profile LABEL PROFILE
profile() {
  rm -rf "$pkg/.build" "$dir/tmp" && mkdir -p "$dir/tmp"
  (cd "$pkg" && /usr/bin/sandbox-exec -p "$2" -- /usr/bin/env TMPDIR="$dir/tmp/" TMP="$dir/tmp" TEMP="$dir/tmp" \
    CLANG_MODULE_CACHE_PATH="$dir/modules" swift test --disable-sandbox -Xswiftc -plugin-path -Xswiftc "$plugins") \
    >"$dir/run.log" 2>&1
  say "$1: exit $?, $(verdict "$dir/run.log")"
  [ -s "$dir/link.log" ] || ! grep -q 'permissionDenied' "$dir/run.log" || cp "$dir/run.log" "$dir/link.log"
}
profile "everything allowed" '(version 1)(allow default)'
profile "services closed" "(version 1)(allow default)$dirhelper"
for pair in "tmp:$tmp" "var-tmp:$vartmp" "var-folders:$folders" "tmp var-tmp:$tmp $vartmp" \
  "tmp var-folders:$tmp $folders" "var-tmp var-folders:$vartmp $folders" "all three:$tmp $vartmp $folders"; do
  profile "services closed, writes refused in ${pair%%:*}" "(version 1)(allow default)(deny file-write* ${pair#*:})$dirhelper"
done

say "--- 3. the link replayed"
closed="(version 1)(allow default)(deny file-write* $tmp $vartmp)$dirhelper"
link=$( [ -s "$dir/link.log" ] && sed 's/\x1b\[[0-9;]*m//g' "$dir/link.log" | grep -A1 'error: Ld ' | tail -1 | sed 's/^ *//')
if [ -z "$link" ]; then
  say "no failing link to replay"
else
  for with in "without TMPDIR:" "with TMPDIR:TMPDIR=$dir/tmp/"; do
    (cd "$pkg" && /usr/bin/sandbox-exec -p "$closed" -- /usr/bin/env -i PATH=/usr/bin:/bin ${with#*:} sh -c "$link") \
      >"$dir/run.log" 2>&1
    say "the link, ${with%%:*}: exit $?, $(grep -m1 -o 'permissionDenied' "$dir/run.log" || echo 'no permissionDenied')"
  done
fi
/usr/bin/sandbox-exec -p "$closed" -- /usr/bin/env -i PATH=/usr/bin:/bin "$swiftc" --version >/dev/null 2>&1
say "swiftc --version, without TMPDIR: exit $?"
/usr/bin/sandbox-exec -p "$closed" -- /usr/bin/env -i PATH=/usr/bin:/bin TMPDIR="$dir/tmp/" "$swiftc" --version >/dev/null 2>&1
say "swiftc --version, with TMPDIR: exit $?"

if [ -n "${KEEP:-}" ]; then say "kept $dir"; exit 0; fi
rm -rf "$dir"
say "removed $dir"
