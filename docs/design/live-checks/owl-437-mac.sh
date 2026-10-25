#!/bin/bash
# OWL-437's manual check on the maintainer's Mac (build plan, "P7 · Background service", part 6): the user
# LaunchAgent dev.owlshift.daemon and its launcher, installed in ~/Library/LaunchAgents by the binary under test,
# started, restarted after a crash, stopped, removed by `stop --at-login`, removed by its own launcher once the
# binary is gone, and removed by `uninstall --purge`.
#
# It runs on a scratch data directory and personal folder (OWLSHIFT_DATA_DIR, OWLSHIFT_CONFIG_DIR), and serves a
# scratch checkout of a repository that does not exist, removed once registered: its watch refuses before any
# credential, or its first clone fails, so no ticket and no keychain entry is read or written. It refuses to run
# when a dev.owlshift.daemon is installed or loaded already. It prints results only.
#
# Usage, from a checkout of the branch: cargo build -p owlshift-cli && bash docs/design/live-checks/owl-437-mac.sh
set -u
built=${1:-target/debug/owlshift}
label=dev.owlshift.daemon
target="gui/$(id -u)/$label"
agents="$HOME/Library/LaunchAgents"
plist="$agents/$label.plist"
launcher="$agents/owlshift-launcher.sh"
[ -x "$built" ] || { echo "no binary at $built: cargo build -p owlshift-cli first"; exit 1; }
if [ -e "$plist" ] || [ -e "$launcher" ] || launchctl print "$target" >/dev/null 2>&1; then
  echo "a $label is installed or loaded already: nothing done"; exit 1
fi
scratch=$(mktemp -d /tmp/owl437.XXXXXX)
bin="$scratch/bin/owlshift"
mkdir -p "$scratch/bin" "$scratch/config"
cp "$built" "$bin"
export OWLSHIFT_DATA_DIR="$scratch/data" OWLSHIFT_CONFIG_DIR="$scratch/config"
printf '[notifications]\ndesktop = false\n' > "$OWLSHIFT_CONFIG_DIR/config.toml"
record="$OWLSHIFT_DATA_DIR/service/daemon.json"
manifest="$OWLSHIFT_DATA_DIR/service/manifest.json"

cleanup() {
  launchctl bootout "$target" >/dev/null 2>&1
  if [ -f "$manifest" ] && grep -q '"service"' "$manifest"; then rm -f "$plist" "$launcher"; fi
  rm -rf "$scratch"
}
trap cleanup EXIT

checkout() {
  rm -rf "$scratch/co"; mkdir -p "$scratch/co"
  git -C "$scratch/co" init -q
  git -C "$scratch/co" remote add origin https://github.com/owlshift-live-check/owl-437-none.git
  cat > "$scratch/co/owlshift.toml" <<'EOF'
requires = ">=0.0"
[tracker]
kind = "linear"
team = "NONE"
admit = { label = "agent" }
admitters = ["u1"]
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }
[stack]
gate = ["true"]
[pipeline]
default = "trivial"
plan_approval = "never"
[models]
[policy]
always_human = []
EOF
}
in_checkout() { checkout; (cd "$scratch/co" && "$bin" "$@"); local code=$?; rm -rf "$scratch/co"; return $code; }
job_pid() { launchctl print "$target" 2>/dev/null | awk -F' = ' '/^\tpid = /{print $2; exit}'; }
job_state() { launchctl print "$target" >/dev/null 2>&1 || { echo "not loaded"; return; }; launchctl print "$target" | awk -F' = ' '/^\tstate = /{print $2; exit}'; }
record_pid() { sed -n 's/.*"pid": *\([0-9]*\).*/\1/p' "$record" 2>/dev/null; }
wait_for() { local i; for i in $(seq 1 "$1"); do eval "$2" && return 0; sleep 1; done; return 1; }
files() { for f in "$plist" "$launcher"; do [ -e "$f" ] && echo "  present: $f" || echo "  gone: $f"; done; }
listed() { grep -q '"service"' "$manifest" 2>/dev/null && echo "  manifest lists the service" || echo "  manifest lists no service"; }

echo "== 1. start --at-login"
in_checkout start --at-login; echo "  exit $?"
files
plutil -lint "$plist" | sed 's/^/  /'
echo "  plist mode $(stat -f %Lp "$plist")"
wait_for 10 '[ -n "$(record_pid)" ]'
echo "  job $(job_state), job pid $(job_pid), daemon.json pid $(record_pid)"
launchctl print "$target" | grep -E 'exit timeout|run at load|throttle|successful exit' | sed 's/^\t*/  /'

echo "== 2. doctor"
"$bin" doctor 2>/dev/null | grep -A4 '^  . service' | sed 's/^/  /'

echo "== 3. kill -9 of the daemon: launchd starts it again ~30 s later"
first=$(job_pid); kill -9 "$first"
wait_for 45 '[ -n "$(job_pid)" ] && [ "$(job_pid)" != "$first" ]'
echo "  pid $first -> $(job_pid), job $(job_state)"

echo "== 4. stop: the daemon ends 0 and launchd leaves it stopped"
"$bin" stop | sed 's/^/  /'
wait_for 70 '[ ! -f "$record" ]'; echo "  daemon.json gone: $([ -f "$record" ] && echo no || echo yes)"
sleep 35; echo "  35 s later: job $(job_state), pid '$(job_pid)'"

echo "== 5. start: through launchd, not as a second daemon"
in_checkout start | sed 's/^/  /'
echo "  job $(job_state), job pid $(job_pid), daemon.json pid $(record_pid)"

echo "== 6. stop --at-login: drained, unloaded, removed"
"$bin" stop --at-login | sed 's/^/  /'
files; listed; echo "  job $(job_state)"

echo "== 7. the launcher with its binary gone"
in_checkout start --at-login >/dev/null; wait_for 10 '[ -n "$(record_pid)" ]'
"$bin" stop >/dev/null; wait_for 70 '[ ! -f "$record" ]'
mv "$bin" "$bin.away"
launchctl kickstart "$target"; echo "  kickstart exit $?"
sleep 5; files; listed
sleep 35; echo "  35 s later: job $(job_state), pid '$(job_pid)'"
launchctl bootout "$target" 2>/dev/null; echo "  bootout exit $? (3: not loaded)"
mv "$bin.away" "$bin"

echo "== 8. uninstall --purge: stopped checked, removed"
in_checkout start --at-login >/dev/null; wait_for 10 '[ -n "$(record_pid)" ]'
echo "  daemon pid $(record_pid)"
"$bin" uninstall --purge | sed 's/^/  /'
files; echo "  job $(job_state); data directory $([ -e "$OWLSHIFT_DATA_DIR" ] && echo present || echo gone)"

echo "== 9. launchd's log on owlshift-launcher.sh, the last 20 minutes (none expected)"
log show --last 20m --style compact --predicate 'process == "launchd" AND eventMessage CONTAINS "owlshift-launcher"' 2>/dev/null \
  | grep -v '^Timestamp' | tail -5 | sed 's/^/  /'
echo "done"
