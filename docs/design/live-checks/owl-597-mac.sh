#!/bin/bash
# OWL-597 on macOS (build plan, "P9 tasks & exit gate", "OWL-597, the home folder the sandbox refused"): can a
# refusal by a Seatbelt profile shaped like the executor's be read afterwards, without root, from the unified log, and
# how long after? And what does the confined command itself print?
#
# The profile is `owlshift_platform::sandbox::seatbelt_profile`'s shape for a gate command: everything allowed, no
# write but the devices, the home closed to reads and writes, the run's temporary folder opened. It runs twice, as is
# and with `(debug deny)`, the one profile option that asks for denials to be logged. Each time the command lists a
# folder of the home and makes one, then exits. The script then waits 60 s and reads `log show --info --debug` as
# the user running it (no sudo) for any `deny` line naming either command's process or the probe's name, and counts
# the kernel's `Sandbox:` deny lines of other processes in the same window, which shows the log is readable.
#
# Usage: bash owl-597-mac.sh
# It writes nothing in the home: the sandbox refuses the write, which is what is checked. It prints results only,
# with the home shown as `~` and other processes' lines counted, not shown.
set -u
home=$(cd "$HOME" && pwd -P)
temp=$(mktemp -d "${TMPDIR:-/tmp}/owl597.XXXXXX")
probe=".owl597-probe-$$"
say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
start=$(date '+%Y-%m-%d %H:%M:%S')
say "macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)), uid $(id -u)"
pids=""
for variant in plain debug; do
  extra=""
  [ "$variant" = debug ] && extra="(debug deny)"
  profile="(version 1)
$extra
(allow default)
(deny file-write*)
(allow file-write* (subpath \"/dev\"))
(deny file-read-data file-read-xattr file-write* (subpath (param \"HOME0\")))
(allow file-read-data file-read-xattr file-write* (subpath (param \"T1\")))"
  say "$variant profile: ls ~/.cache; mkdir -p ~/.cache/$probe-$variant/sub"
  out=$(/usr/bin/sandbox-exec -D "HOME0=$home" -D "T1=$temp" -p "$profile" /bin/sh -c "
    echo pid \$\$
    ls \"$home/.cache\" > /dev/null
    mkdir -p \"$home/.cache/$probe-$variant/sub\"
    echo exit \$?" 2>&1)
  printf '%s\n' "$out" | sed "s|$home|~|g; s/^/  | /"
  pids="$pids $(printf '%s\n' "$out" | sed -n 's/^pid //p')"
  [ ! -e "$home/.cache/$probe-$variant" ] && say "nothing was written in the home"
done
rmdir "$temp"
say "waiting 60 s, then reading the unified log as $(id -un), no sudo"
sleep 60
window=$(/usr/bin/log show --style compact --info --debug --start "$start" 2>&1)
mine=$(printf '%s\n' "$window" | grep -v 'log run noninteractively' | grep -i 'deny\|violation' \
  | grep -e "$probe" $(for pid in $pids; do printf -- '-e (%s) ' "$pid"; done))
others=$(printf '%s\n' "$window" | grep -c 'Sandbox: .* deny(')
if [ -n "$mine" ]; then
  say "deny lines for the probe's processes:"
  printf '%s\n' "$mine" | sed "s|$home|~|g; s/^/  | /"
else
  say "no deny line for the probe's processes ($pids ) or names after 60 s"
fi
say "kernel 'Sandbox: ... deny(' lines of other processes in the same window: $others"
say "the probe's processes in the log:"
printf '%s\n' "$window" | grep -e "$probe" $(for pid in $pids; do printf -- '-e \[%s\] ' "$pid"; done) \
  | grep -v 'log run noninteractively' | sed "s|$home|~|g; s/^/  | /"
