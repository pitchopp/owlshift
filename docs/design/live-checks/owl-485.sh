#!/bin/sh
# OWL-485 live check, next to L6 of docs/design/build-plan.md, "P7 · Background service": what `ioreg -r -n
# IOPMrootDomain` reports as "System Capabilities" at a full wake, with the display asleep and in a dark wake. Two modes:
#   sh docs/design/live-checks/owl-485.sh awake   harmless: samples with the display on, then with only the display put to
#                                                 sleep (`caffeinate -i` keeps the system awake, no run is suspended)
#   sh docs/design/live-checks/owl-485.sh sleep   run by the maintainer in a terminal of the Mac, on power. It PUTS THE MAC
#                                                 TO SLEEP, which suspends every run Owlshift has in flight: pause the watch
#                                                 (`owlshift pause`) and let the runs in flight end before you start it.
# `sleep` samples every 2 s to a scratch file, counts down 20 s (Ctrl-C ends it before the sleep), runs `pmset sleepnow`
# and keeps sampling for at most 15 minutes. A dark wake needs a sleep of a few minutes with the lid closed or the system
# idle (a Power Nap or a maintenance wake produces one): wait for the DarkWake line of `pmset -g log`, not only for the
# wake, then press Ctrl-C to end the script early, or let it run out. Paste back everything it prints. The capability
# letters of the pmset lines (a full wake lists V, a dark wake does not) only say which samples were a dark wake; the
# reading is the "System Capabilities" value. Nothing is printed but the readings; scratch files under ${TMPDIR:-/tmp}
# are removed at the end.
set -u
mode=${1:-}
case $mode in awake | sleep) ;; *) echo "usage: sh owl-485.sh awake|sleep" >&2; exit 2 ;; esac
date -u +"date=%Y-%m-%dT%H:%M:%SZ"; echo "macos=$(sw_vers -productVersion) build=$(sw_vers -buildVersion) arch=$(uname -m) mode=$mode"
d=$(mktemp -d "${TMPDIR:-/tmp}/owl485.XXXXXX")
spid=; cpid=
trap '[ -n "$spid" ] && kill "$spid" 2>/dev/null; [ -n "$cpid" ] && { kill "$cpid" 2>/dev/null; wait "$cpid" 2>/dev/null; }; rm -rf "$d"' EXIT
t0=$(date +%s)
since=$(date -r "$t0" "+%Y-%m-%d %H:%M:%S")

# One reading: the epoch second and the "System Capabilities" line of the root power domain.
reading() {
  echo "$(date +%s) $(ioreg -r -n IOPMrootDomain | grep '"System Capabilities"' | tr -s ' ' | head -1)"
}
# The pmset lines of the sleeps, wakes and display on/off since the start, with their capability letters.
pmset_lines() {
  echo "sleeps, wakes and display changes since $since:"
  pmset -g log | awk -v s="$since" '$1" "$2 >= s && ($4 == "Sleep" || $4 == "Wake" || $4 == "DarkWake" || /Display is turned/) {print " ", substr($0, 1, 160)}'
}
# The samples with their time of day; a run of equal values is shown by its first and last sample, and a gap of
# more than 4 s (the time asleep) shows both samples around it.
print_samples() {
  pv=; pe=0; held=; n=0
  while read -r epoch rest; do
    if [ $n = 0 ] || [ "$rest" != "$pv" ] || [ $((epoch - pe)) -gt 4 ]; then
      [ -n "$held" ] && echo "$held"
      held=; echo "  $(date -r "$epoch" "+%H:%M:%S") $rest"
    else
      held="  $(date -r "$epoch" "+%H:%M:%S") $rest"
    fi
    pv=$rest; pe=$epoch; n=$((n + 1))
  done < "$1"
  [ -n "$held" ] && echo "$held"
}

if [ "$mode" = awake ]; then
  echo "display on: 10 samples, 2 s apart"
  : > "$d/on"; i=0
  while [ $i -lt 10 ]; do reading >> "$d/on"; sleep 2; i=$((i + 1)); done
  print_samples "$d/on"
  echo "display asleep (pmset displaysleepnow), system held awake by caffeinate -i, displaysleepnow repeated before each of 10 samples"
  caffeinate -i -t 45 & cpid=$!
  : > "$d/off"; i=0
  while [ $i -lt 10 ]; do pmset displaysleepnow; sleep 1; reading >> "$d/off"; sleep 1; i=$((i + 1)); done
  print_samples "$d/off"
  pmset_lines
  exit 0
fi

echo "WARNING: the Mac goes to sleep in 20 s and every run in flight is suspended. Ctrl-C now to cancel."
: > "$d/all"
( while :; do reading >> "$d/all"; sleep 2; done ) & spid=$!
i=20; while [ $i -gt 0 ]; do echo "  $i"; sleep 1; i=$((i - 1)); done
trap 'stop=1' INT
stop=0
echo "pmset sleepnow (sampling goes on for at most 15 minutes; Ctrl-C after the wake ends it)"
pmset sleepnow > /dev/null; echo "pmset sleepnow status=$?"
while [ $stop = 0 ] && [ $(( $(date +%s) - t0 )) -lt 900 ]; do sleep 2; done
kill "$spid" 2>/dev/null; spid=
echo "samples (time of day, then the value; a gap is the time asleep):"
print_samples "$d/all"
pmset_lines
