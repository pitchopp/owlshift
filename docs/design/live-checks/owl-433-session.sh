#!/bin/sh
# OWL-433 live checks the maintainer runs at the Mac, quoted by the L1, L3, L4 and L5 bullets of docs/design/build-plan.md,
# "P7 · Background service". Each step bootstraps throwaway LaunchAgents (labels dev.owlshift.probe433.*) from
# ~/Library/Logs/owlshift-live-checks/owl-433, never from ~/Library/LaunchAgents, so none starts at a later login.
# It reads no Owlshift entry: its keychain item is its own, under a service named owlshift-probe433-<random>, and only
# whether the value read back matches, and its length, are printed.
#
# Usage, in a terminal of the Mac (sh docs/design/live-checks/owl-433-session.sh <step>):
#   arm logout | arm shutdown   two agents with ExitTimeOut 90, one ignoring SIGTERM, one exiting 60 s after it; then
#                               log out (or shut down) and come back; run `report`. Arm again for the other one.
#   l3                          an agent reads a throwaway keychain item every 15 s; lock the screen 2 minutes, unlock,
#                               sleep the Mac 3 minutes, wake it, then run `l3-report`.
#   l4                          one notification from an agent; answer whether its banner showed.
#   l5                          optional: caffeinate -i -w on a 30-minute target from an agent; leave the Mac idle
#                               past its sleep setting, come back, then run `l5-report`.
#   report | l3-report | l5-report   print the results to paste back.
#   cleanup                     boots out every probe agent, deletes the keychain item, removes the folder.
# Paste back everything a `report` step prints. Nothing here needs sudo.
set -u
base="$HOME/Library/Logs/owlshift-live-checks/owl-433"
dom="gui/$(id -u)"
L=dev.owlshift.probe433
mkdir -p "$base" && chmod 700 "$base"

hdr() { date -u +"date=%Y-%m-%dT%H:%M:%SZ"; echo "macos=$(sw_vers -productVersion) arch=$(uname -m)"; }

# sleeps <file>: the system's sleeps and wakes since the file was written, date, time and kind only.
sleeps() {
  since=$(stat -f %Sm -t "%Y-%m-%d %H:%M:%S" "$1")
  echo "sleeps and wakes since $since:"
  pmset -g log | awk -v s="$since" '$1" "$2 >= s && ($4 == "Sleep" || $4 == "Wake" || $4 == "DarkWake") {print " ", $1, $2, $4}' | tail -8
}

# plist <path> <label> <extra keys> <program arguments...>
plist() {
  p=$1; l=$2; x=$3; shift 3
  {
    echo '<?xml version="1.0" encoding="UTF-8"?>'
    echo '<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">'
    echo "<plist version=\"1.0\"><dict><key>Label</key><string>$l</string><key>ProgramArguments</key><array>"
    for a in "$@"; do echo "<string>$a</string>"; done
    echo "</array><key>RunAtLoad</key><true/>$x</dict></plist>"
  } > "$p"
  launchctl bootstrap "$dom" "$p"; echo "bootstrap $l status=$?"
}

# The heartbeat: one line a second, flushed to disk, and each SIGTERM; with `coop`, exits 0 60 s after SIGTERM.
# shellcheck disable=SC2016 # Perl code, expanded by perl, not by the shell
HB='use IO::Handle; my ($f, $mode) = @ARGV; open(my $h, ">>", $f) or die; my $t;
  sub w { print $h "@_ ", time, "\n"; $h->flush; $h->sync }
  $SIG{TERM} = sub { $t //= time; w("got TERM") }; $SIG{HUP} = sub { w("got HUP") };
  w("start pid=$$");
  while (1) { w("hb"); if ($mode eq "coop" and defined $t and time - $t >= 60) { w("exits"); exit 0 } sleep 1 }'

case ${1:-} in
arm)
  phase=${2:?arm logout or arm shutdown}
  d="$base/$phase-$(date +%Y%m%d%H%M%S)"; mkdir -p "$d"; hdr > "$d/armed"
  for m in slow coop; do
    plist "$d/$m.plist" "$L.$phase.$m" \
      '<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict><key>ThrottleInterval</key><integer>30</integer><key>ExitTimeOut</key><integer>90</integer>' \
      /usr/bin/perl -e "$HB" "$d/$m.log" "$m"
  done
  echo "Armed for $phase. Now $phase (Apple menu), come back, open a terminal and run: sh $0 report" ;;
report)
  hdr
  for d in "$base"/logout-* "$base"/shutdown-*; do
    [ -d "$d" ] || continue
    echo "== $(basename "$d") ($(sed -n 1p "$d/armed"))"
    for m in slow coop; do
      awk -v m=$m '$1=="got" && $2=="TERM" && !t {t=$3} $1=="hb" {last=$2} $1=="exits" {ex=$2} $1=="start" {n++}
        END {if (!t) {print m ": no SIGTERM logged (inconclusive)"; exit}
             printf "%s: SIGTERM, last heartbeat %+ds after it%s, starts=%d\n", m, last - t, (ex ? sprintf(", exited 0 at %+ds", ex - t) : ""), n}' "$d/$m.log"
    done
    t=$(awk '$1=="got" && $2=="TERM" {print $3; exit}' "$d/slow.log" 2>/dev/null)
    [ -n "$t" ] && echo "SIGTERM at $(date -r "$t" "+%Y-%m-%d %H:%M:%S")"
  done
  # launchd's own log is root's; the system's record of the last shutdown and boot says when they happened.
  last -3 shutdown reboot 2>/dev/null | head -3 ;;
l3)
  d="$base/l3"; mkdir -p "$d"; hdr > "$d/armed"
  svc="owlshift-probe433-$(od -An -N4 -tx4 /dev/urandom | tr -d ' ')"; echo "$svc" > "$d/service"
  v=$(od -An -N24 -tx1 /dev/urandom | tr -d ' \n')
  printf 'add-generic-password -U -s "%s" -a "probe" -w "%s"\n' "$svc" "$v" | /usr/bin/security -i > /dev/null
  echo "store status=$?"
  printf '%s' "$v" | shasum -a 256 | cut -c1-64 > "$d/expected"; unset v
  echo "login keychain settings: $(/usr/bin/security show-keychain-info "$HOME/Library/Keychains/login.keychain-db" 2>&1 | sed 's/^Keychain ".*" *//')" >> "$d/armed"
  cat > "$d/reader.sh" <<EOF
#!/bin/sh
# Reads the item with Owlshift's own command, a 10 s bound, and logs only status and whether the value matches.
while :; do
  ( /usr/bin/security find-generic-password -s "$svc" -a probe -w 2>/dev/null | tr -d '\n' | shasum -a 256 | cut -c1-64 > "$d/got"; ) &
  r=\$!; i=0; while kill -0 \$r 2>/dev/null && [ \$i -lt 10 ]; do sleep 1; i=\$((i+1)); done
  if kill -0 \$r 2>/dev/null; then kill \$r; echo "\$(date +%H:%M:%S) no answer within 10 s" >> "$d/log"
  elif cmp -s "$d/got" "$d/expected"; then echo "\$(date +%H:%M:%S) read ok" >> "$d/log"
  else echo "\$(date +%H:%M:%S) read failed" >> "$d/log"; fi
  rm -f "$d/got"; sleep 15
done
EOF
  plist "$d/reader.plist" "$L.l3" "" /bin/sh "$d/reader.sh"
  echo "Now: lock the screen (Control-Command-Q) for 2 minutes and unlock it; then Apple menu > Sleep for 3 minutes and"
  echo "wake the Mac; note the times. Then run: sh $0 l3-report" ;;
l3-report)
  d="$base/l3"; hdr; cat "$d/armed"
  sleeps "$d/armed"
  uniq -c -f1 "$d/log" | awk '{print}' | tail -40
  launchctl bootout "$dom/$L.l3" 2>/dev/null
  /usr/bin/security delete-generic-password -s "$(cat "$d/service")" -a probe > /dev/null 2>&1; echo "item delete status=$?" ;;
l4)
  d="$base/l4"; mkdir -p "$d"; hdr
  cat > "$d/notify.sh" <<EOF
#!/bin/sh
/usr/bin/osascript -e 'on run argv' -e 'display notification (item 2 of argv) with title (item 1 of argv)' -e 'end run' \
  Owlshift "Live check L4 (OWL-433), sent by a LaunchAgent" 2> "$d/err"
echo "osascript status=\$?" > "$d/out"
EOF
  plist "$d/notify.plist" "$L.l4" "" /bin/sh "$d/notify.sh"
  sleep 3; cat "$d/out"; echo "stderr: $(head -c 200 "$d/err")"
  launchctl bootout "$dom/$L.l4" 2>/dev/null
  printf 'Did a banner titled Owlshift show (or land in Notification Center)? [y/n] '; read -r a; echo "banner seen: $a"
  echo "Focus or notification settings for Script Editor/osascript: $(defaults read com.apple.ncprefs 2>/dev/null | grep -c -i scripteditor) entries" ;;
l5)
  d="$base/l5"; mkdir -p "$d"; hdr > "$d/armed"; pmset -g | grep -E "^ *(sleep|displaysleep) " >> "$d/armed"
  cat > "$d/hold.sh" <<EOF
#!/bin/sh
sleep 1800 & t=\$!; caffeinate -i -w \$t & c=\$!
echo "target=\$t caffeinate=\$c" > "$d/pids"; wait \$t
EOF
  plist "$d/hold.plist" "$L.l5" "" /bin/sh "$d/hold.sh"
  sleep 2; cat "$d/pids"; cat "$d/armed"
  echo "Now leave the Mac untouched, on power, for longer than the 'sleep' minutes above (it must not be 0), at most 30"
  echo "minutes; then come back and run: sh $0 l5-report" ;;
l5-report)
  d="$base/l5"; hdr; cat "$d/armed"
  sleeps "$d/armed"
  launchctl bootout "$dom/$L.l5" 2>/dev/null; echo "booted out" ;;
cleanup)
  for l in $(launchctl list | awk -v L="$L." 'index($3, L) == 1 {print $3}'); do launchctl bootout "$dom/$l"; echo "bootout $l status=$?"; done
  [ -f "$base/l3/service" ] && /usr/bin/security delete-generic-password -s "$(cat "$base/l3/service")" -a probe > /dev/null 2>&1
  rm -rf "$base"; rmdir "$(dirname "$base")" 2>/dev/null
  echo "probe agents left: $(launchctl list | grep -c "$L.") (a job booted out may take up to 60 s to end)" ;;
*) sed -n '2,22p' "$0" ;;
esac
