#!/bin/sh
# OWL-505 live check, next to L6 of docs/design/build-plan.md, "P7 · Background service": what `ioreg -r -n
# IOPMrootDomain` reports as "System Capabilities" on an awake Mac whose display has been asleep for minutes, the
# system held awake by `caffeinate -i`. owl-485.sh could only sample seconds of display sleep, because the display came
# back on at once; the dark-wake hold reads the graphics bit, and a 9 here would mean an awake Mac holds the watch.
#   sh docs/design/live-checks/owl-505.sh run    harmless: no sleep, no run is suspended. Reads the display's state,
#                                                runs `caffeinate -i`, issues `pmset displaysleepnow` ONCE, then reads
#                                                the value every 10 s for 5 minutes and prints a verdict. Leave the
#                                                Mac alone: a key press or a mouse move wakes the display and breaks
#                                                the window; the script never puts the display to sleep again, a new
#                                                window is a new run. Run it in a terminal of the Mac itself.
#   . owl-505.sh functions                       only defines the functions (tests call `verdict`, `start_state`).
# Paste back everything it prints. Scratch files under ${TMPDIR:-/tmp} are removed at the end.
mode=${1:-}
case $mode in run | functions) ;; *) echo "usage: sh owl-505.sh run|functions" >&2; exit 2 ;; esac

# The state of the display at $1 ("YYYY-MM-DD HH:MM:SS"): reads pmset log text on stdin, prints on, off or unknown
# from the last "Display is turned on/off" line before $1, lines at or after $1 ignored.
start_state() {
  awk -v s="$1" '$1" "$2 < s && /Display is turned/ {st = /turned off/ ? "off" : "on"} END {print st ? st : "unknown"}'
}

# verdict START END STARTSTATE ASSERT_START_FILE ASSERT_END_FILE, pmset log text on stdin. START and END bound the
# window ("YYYY-MM-DD HH:MM:SS"), STARTSTATE is on, off or unknown (display at START), the files hold
# `pmset -g assertions` taken at the start and the end. Lines outside [START, END] are ignored. Prints three
# separate findings, then "valid: yes" only when all three hold.
verdict() {
  v_log=$(awk -v s="$1" -v e="$2" '$1" "$2 >= s && $1" "$2 <= e')
  v_off=$(printf '%s\n' "$v_log" | grep -c 'Display is turned off')
  v_on=$(printf '%s\n' "$v_log" | grep -c 'Display is turned on')
  v_ok=1
  if [ "$v_on" -gt 0 ]; then
    echo "display: turned on inside the window, the window is broken"; v_ok=0
  elif [ "$v_off" -gt 0 ]; then
    echo "display: turned off inside the window, never on again, asleep for the whole window"
  elif [ "$3" = off ]; then
    echo "display: already off at the start, no line inside the window, asleep for the whole window"
  else
    echo "display: not shown asleep (state at the start: $3, no 'Display is turned off' line), the attempt does not count"; v_ok=0
  fi
  v_sl=$(printf '%s\n' "$v_log" | awk '$4 == "Sleep" || $4 == "DarkWake" || $4 == "Maintenance"' | wc -l | tr -d ' ')
  if [ "$v_sl" -gt 0 ]; then echo "sleep: $v_sl Sleep, DarkWake or Maintenance line(s) fell in the window, not valid"; v_ok=0
  else echo "sleep: no Sleep, DarkWake or Maintenance line in the window"; fi
  v_as=0; v_ae=0
  grep -Eq 'caffeinate.*PreventUserIdleSystemSleep|PreventUserIdleSystemSleep.*caffeinate' "$4" 2>/dev/null && v_as=1
  grep -Eq 'caffeinate.*PreventUserIdleSystemSleep|PreventUserIdleSystemSleep.*caffeinate' "$5" 2>/dev/null && v_ae=1
  if [ $v_as = 1 ] && [ $v_ae = 1 ]; then echo "assertion: a caffeinate PreventUserIdleSystemSleep assertion held at the start and at the end"
  else echo "assertion: no caffeinate PreventUserIdleSystemSleep assertion at the start (${v_as}) and the end (${v_ae}), not valid"; v_ok=0; fi
  if [ $v_ok = 1 ]; then echo "valid: yes"; else echo "valid: no"; fi
}

[ "$mode" = functions ] && { return 0 2>/dev/null || exit 0; }

set -u
date -u +"date=%Y-%m-%dT%H:%M:%SZ"; echo "macos=$(sw_vers -productVersion) build=$(sw_vers -buildVersion) arch=$(uname -m)"
d=$(mktemp -d "${TMPDIR:-/tmp}/owl505.XXXXXX")
cpid=
trap '[ -n "$cpid" ] && { kill "$cpid" 2>/dev/null; wait "$cpid" 2>/dev/null; }; rm -rf "$d"' EXIT
stamp() { date -r "$1" "+%Y-%m-%d %H:%M:%S"; }
reading() { echo "$(date +%s) $(ioreg -r -n IOPMrootDomain | grep '"System Capabilities"' | tr -s ' ' | head -1)"; }

t0=$(date +%s); since=$(stamp "$t0")
pmset -g log > "$d/log0"
state=$(start_state "$since" < "$d/log0")
echo "display state at the start ($since): $state"
caffeinate -i -t 420 & cpid=$!
sleep 1
pmset -g assertions > "$d/as0"
pmset displaysleepnow; echo "pmset displaysleepnow status=$?"
echo "sampling every 10 s for 5 minutes; leave the Mac alone"
: > "$d/samples"; i=0
while [ $i -lt 31 ]; do reading >> "$d/samples"; [ $i -lt 30 ] && sleep 10; i=$((i + 1)); done
t1=$(date +%s); until=$(stamp "$t1")
pmset -g assertions > "$d/as1"
echo "assertions at the end:"; grep -E 'caffeinate|PreventUserIdleSystemSleep' "$d/as1" | cut -c1-160
echo "samples: $(wc -l < "$d/samples" | tr -d ' ') from $(date -r "$t0" +%H:%M:%S) to $(date -r "$t1" +%H:%M:%S)"
echo "values: $(awk '{print $NF}' "$d/samples" | sort | uniq -c | tr -s ' ' | tr '\n' ';')"
pmset -g log > "$d/log1"
echo "display, Sleep, Wake, DarkWake and Maintenance lines from $since to $until:"
awk -v s="$since" -v e="$until" '$1" "$2 >= s && $1" "$2 <= e && ($4 == "Sleep" || $4 == "Wake" || $4 == "DarkWake" || $4 == "Maintenance" || /Display is turned/) {print " ", substr($0, 1, 160)}' "$d/log1"
echo "samples that are not 15:"; awk '$NF != 15 {print " ", $0}' "$d/samples"
echo "verdict:"
verdict "$since" "$until" "$state" "$d/as0" "$d/as1" < "$d/log1" | sed 's/^/  /'
