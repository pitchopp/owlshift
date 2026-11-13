#!/bin/bash
# R3 on macOS (OWL-553; build plan, "P9 · Open-source readiness", R3): a binary replaced under a running daemon
# leaves the daemon's children on the build it started with, until `owlshift start`, run from the new build, drains
# the daemon, which the service manager then starts again on that build.
#
#   rename  B replaces A at its path by an atomic rename, as `owlshift self-update` does.
#   brew    the path is a link into a versioned folder, as Homebrew's; the link is repointed to B's folder and A's
#           folder removed, as `brew upgrade` and its cleanup do.
#
# A and B are two builds, B superseding A (OWL-462: same version, B built later from another or a dirty tree). The
# data folder goes under <data parent>, the scratch folder by default: another volume there (a mounted disk image)
# checks the plain copy the daemon makes when its file system cannot clone across.
#
# Nothing here calls `launchctl`, so a service of the person's own stays as it is: a stand-in supervisor writes the
# launcher's `launched` marker, runs `owlshift daemon` and runs it again on any exit but 0, as the LaunchAgent does
# (OWL-437); the manifest lists a systemd unit in the scratch folder, so a manager call, which no step here should
# make, reaches the stand-in `systemctl` first on the PATH, which records it and fails. Git reads no system or global
# configuration but a local proxy, which every HTTPS request is sent to, so nothing leaves the machine; the served
# project does not exist, so no keychain entry matches it. It prints results only.
#
# Usage: bash owl-553-mac.sh rename|brew <binary A> <binary B> <new scratch folder> [<data parent>]
set -u
mode=$1 a=$2 b=$3 root=$4 data_parent=${5:-$4}
case $mode in rename | brew) ;; *) echo "rename or brew, not $mode"; exit 2 ;; esac
[ -x "$a" ] && [ -x "$b" ] || { echo "A and B must be binaries"; exit 2; }
[ ! -e "$root" ] || { echo "$root exists: name a new folder"; exit 2; }
mkdir -p "$root" "$data_parent"
root=$(cd "$root" && pwd -P)
data_parent=$(cd "$data_parent" && pwd -P)
data=$(mktemp -d "$data_parent/owl553-data.XXXXXX")
service=$data/service
bin=$root/bin/owlshift
mkdir -p "$service" "$root/bin" "$root/config" "$root/units" "$root/stand-in"

say() { printf '%s %s\n' "$(date -u +%H:%M:%SZ)" "$*"; }
# A build in a few words: its commit, whether dirty, when built.
version() { "$1" --version | sed -n 's/^build: commit \([0-9a-f]\{12\}\)[0-9a-f]*\(.*\), from .*/\1\2/p'; }
record() { python3 -c 'import json, sys; r = json.load(open(sys.argv[1])); print(r["pid"] if sys.argv[2] == "pid" else r["build"])' "$service/daemon.json" "$1" 2> /dev/null; }
daemon_pid() { record pid; }
daemon_build() { record build; }
# The file a process runs, as the kernel holds it.
runs() { lsof -a -p "$1" -d txt -Fn 2>/dev/null | sed -n 's/^n//p' | head -1; }
wait_for() { # what, seconds, a condition evaluated at each try
  local what=$1 left=$2
  until eval "$3"; do
    left=$((left - 1))
    [ "$left" -ge 0 ] || { say "FAIL: no $what"; exit 1; }
    sleep 1
  done
}

case $mode in
  rename) cp "$a" "$bin" ;;
  brew)
    mkdir -p "$root/Cellar/owlshift/a/bin"
    cp "$a" "$root/Cellar/owlshift/a/bin/owlshift"
    ln -s ../Cellar/owlshift/a/bin/owlshift "$bin"
    ;;
esac
say "A: $(version "$a")"
say "B: $(version "$b")"
say "data folder: $data, on $(df "$data" | awk 'NR == 2 { print $1 }'); binary on $(df "$root/bin" | awk 'NR == 2 { print $1 }')"

printf '#!/bin/sh\necho "systemctl $*" >> "%s/manager-calls"\nexit 1\n' "$root" > "$root/stand-in/systemctl"
chmod +x "$root/stand-in/systemctl"
free_port() { python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
# Every HTTPS request, git's included, goes through a local proxy that accepts and answers nothing for 20 s, then
# closes: a served watch's first clone lives that long, so the file it runs can be read, then fails, and the daemon
# starts the watch again on backoff.
hole=$(free_port)
python3 -c '
import socket, sys, threading, time
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1); s.bind(("127.0.0.1", int(sys.argv[1]))); s.listen()
def hold(c): time.sleep(20); c.close()
while True: threading.Thread(target=hold, args=(s.accept()[0],), daemon=True).start()
' "$hole" &
server=$!
printf '[http]\n\tproxy = http://127.0.0.1:%s\n' "$hole" > "$root/gitconfig"
export PATH="$root/stand-in:$PATH" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$root/gitconfig" GIT_TERMINAL_PROMPT=0
export HTTPS_PROXY="http://127.0.0.1:$hole" https_proxy="http://127.0.0.1:$hole" ALL_PROXY="http://127.0.0.1:$hole"
export OWLSHIFT_DATA_DIR="$data" OWLSHIFT_CONFIG_DIR="$root/config"
# `start` and `stop` name the scratch folders once more, so neither reaches a daemon of the person's own.
owl() { OWLSHIFT_DATA_DIR="$data" OWLSHIFT_CONFIG_DIR="$root/config" "$bin" "$@"; }
port=$(free_port)
printf '[notifications]\ndesktop = false\n\n[ui]\nport = %s\n' "$port" > "$root/config/config.toml"
touch "$root/units/owlshift.service"
printf '{"format": 1, "service": {"manager": "systemd", "unit": "%s", "launcher": "%s", "binary": "%s"}, "keychain": []}\n' \
  "$root/units/owlshift.service" "$root/units/owlshift-launcher.sh" "$bin" > "$service/manifest.json"
# The served project, a repository that does not exist, and the checkout `owlshift start` runs in.
checkout=$root/checkout
printf '{"format": 1, "projects": [{"repository": "owl553-r3/scratch", "checkout": "%s"}]}\n' "$checkout" \
  > "$service/projects.json"
mkdir -p "$checkout"
git -C "$checkout" init -q
git -C "$checkout" remote add origin https://github.com/owl553-r3/scratch.git
cat > "$checkout/owlshift.toml" <<'EOF'
requires = ">=0.0"
[tracker]
kind = "linear"
team = "R3"
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

cat > "$root/supervise.sh" <<'EOF'
#!/bin/bash
# The service manager's stand-in: the launcher's marker, then the daemon, again on any exit but 0.
while :; do
  /bin/sh -c 'echo $$ > "$1"; exec "$2" daemon' sh "$2/launched" "$1" >> "$2/daemon.log" 2>&1
  code=$?
  echo "supervisor: the daemon exited $code" >> "$2/daemon.log"
  [ "$code" -eq 0 ] && exit 0
  sleep 1
done
EOF
nohup bash "$root/supervise.sh" "$bin" "$service" < /dev/null > /dev/null 2>&1 &
supervisor=$!
# Every process the daemon starts, as it is spawned (argv[0] is the path it was spawned by), polled every 50 ms.
python3 - "$service" "$root/spawned" <<'EOF' &
import json, subprocess, sys, time
service, out = sys.argv[1], sys.argv[2]
seen = set()
while True:
    try:
        daemon = json.load(open(service + "/daemon.json"))["pid"]
    except Exception:
        daemon = None
    if daemon is not None:
        ps = subprocess.run(["ps", "-axo", "pid=,ppid=,command="], capture_output=True, text=True).stdout
        for line in ps.splitlines():
            pid, ppid, command = line.split(None, 2)
            if int(ppid) == daemon and pid not in seen:
                seen.add(pid)
                with open(out, "a") as f:
                    f.write("%s daemon %s spawned %s: %s\n" % (time.strftime("%H:%M:%SZ", time.gmtime()), daemon, pid, command))
    time.sleep(0.05)
EOF
watcher=$!
cleanup() {
  kill "$watcher" 2> /dev/null
  pid=$(daemon_pid)
  if [ -n "$pid" ] && kill -0 "$pid" 2> /dev/null; then owl stop > /dev/null 2>&1; sleep 25; fi
  kill "$supervisor" "$server" 2> /dev/null
}
trap cleanup EXIT

started=$(date +%s)
wait_for "daemon" 30 'test -s "$service/daemon.json"'
first=$(daemon_pid)
ui_of() { pgrep -P "$1" -f 'ui --daemon' | head -1; }
watch_of() { pgrep -P "$1" -f 'watch --drain' | head -1; }
wait_for "UI child" 30 'test -n "$(ui_of "$first")"'
wait_for "watch" 90 'test -n "$(watch_of "$first")"'
ui=$(ui_of "$first") watch=$(watch_of "$first")
say "daemon $first ($(daemon_build)) runs $(runs "$first")"
say "its UI $ui, up $(($(date +%s) - started)) s after the daemon's start, runs $(runs "$ui")"
say "its watch $watch runs $(runs "$watch")"
say "pinned: $(cd "$service/pinned" && find . -type f) ($(version "$service/pinned/$first/owlshift"))"
cmp -s "$service/pinned/$first/owlshift" "$a" && say "the copy is A" || say "FAIL: the copy is not A"

say "replacing the binary ($mode)"
swapped=$(date -u +%H:%M:%SZ)
case $mode in
  rename)
    cp "$b" "$root/bin/.owlshift.tmp"
    mv -f "$root/bin/.owlshift.tmp" "$bin"
    ;;
  brew)
    mkdir -p "$root/Cellar/owlshift/b/bin"
    cp "$b" "$root/Cellar/owlshift/b/bin/owlshift"
    ln -sfn ../Cellar/owlshift/b/bin/owlshift "$bin"
    rm -rf "$root/Cellar/owlshift/a"
    ;;
esac
say "$bin is now: $(version "$bin")"
old_ui=$ui old_watch=$(watch_of "$first")
kill -9 "$old_ui" $old_watch
say "killed the UI $old_ui and the watch $old_watch"
wait_for "UI started again" 60 'u=$(ui_of "$first"); test -n "$u" && test "$u" != "$old_ui"'
wait_for "watch started again" 120 'w=$(watch_of "$first"); test -n "$w" && test "$w" != "$old_watch"'
ui=$(ui_of "$first") watch=$(watch_of "$first")
say "the daemon started its UI again: $ui runs $(runs "$ui") ($(version "$(runs "$ui")"))"
say "the daemon started its watch again: $watch runs $(runs "$watch") ($(version "$(runs "$watch")"))"
awk -v t="$swapped" '$1 > t' "$root/spawned" | sed 's/^/  /'
cmp -s "$service/pinned/$first/owlshift" "$a" && say "the copy is still A" || say "FAIL: the copy changed"

say "owlshift start, from B:"
(cd "$checkout" && owl start) 2>&1 | sed 's/^/  /'
wait_for "new daemon" 120 'p=$(daemon_pid); test -n "$p" && test "$p" != "$first"'
second=$(daemon_pid)
grep -E "drain|exits with|supervisor" "$service/daemon.log" | sed 's/^/  log: /'
wait_for "new UI" 30 'test -n "$(ui_of "$second")"'
wait_for "new watch" 90 'test -n "$(watch_of "$second")"'
ui=$(ui_of "$second") watch=$(watch_of "$second")
say "daemon $second ($(daemon_build)) runs $(runs "$second")"
say "its UI $ui runs $(runs "$ui"); its watch $watch runs $(runs "$watch")"
say "pinned: $(cd "$service/pinned" && find . -type f)"
cmp -s "$service/pinned/$second/owlshift" "$b" && say "the copy is B" || say "FAIL: the copy is not B"

say "owlshift stop:"
owl stop 2>&1 | sed 's/^/  /'
wait_for "the supervisor's end" 60 '! kill -0 "$supervisor" 2> /dev/null'
say "pinned left: [$(ls -A "$service/pinned")]; manager calls: [$(cat "$root/manager-calls" 2> /dev/null)]"
