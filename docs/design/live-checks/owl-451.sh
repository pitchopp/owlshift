#!/bin/sh
# OWL-451 live probe, quoted by the L7 bullet of docs/design/build-plan.md, "P7 · Background service". On macOS:
#   sh docs/design/live-checks/owl-451.sh
# Probe 1: whether SIGTERM to Claude Code's process group, sent before the group is killed, ends the process groups of
# its Bash calls, and how fast. Probe 2: whether a background job that a finished Bash call left behind outlives a run
# that ends by itself. Probe 3, no Claude Code run: whether a process can read another one's environment (`ps -E`,
# which reads it as sysctl KERN_PROCARGS2 does), for a program built here and for /bin/sleep.
# Two short `claude -p` runs on this machine's own Claude Code login, each at most 4 turns and 0.50 USD. It prints pids,
# process group and session ids, executable names, timings, and whether the probe's own variable was read (yes or no),
# never a command's arguments, its output or any environment variable. Scratch files under ${TMPDIR:-/tmp}, removed
# at the end with every process the probe saw.
set -u
PATH="$PATH:/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$HOME/.local/bin"
date -u +"date=%Y-%m-%dT%H:%M:%SZ"; echo "macos=$(sw_vers -productVersion) arch=$(uname -m)"
echo "claude=$(claude --version 2>/dev/null | head -1 || echo missing)"
d=$(mktemp -d "${TMPDIR:-/tmp}/owl451.XXXXXX")
seen=
cleanup() {
  for p in $seen; do alive "$p" && kill -s KILL "$p" 2>/dev/null; done
  rm -rf "$d"
}
trap cleanup EXIT

# getsid(2), which macOS's ps does not show, and a sleeper that is not an Apple binary.
cat > "$d/sid.rs" <<'EOF'
extern "C" {
    fn getsid(pid: i32) -> i32;
}
fn main() {
    for arg in std::env::args().skip(1) {
        let pid: i32 = arg.parse().unwrap_or(0);
        println!("{}", unsafe { getsid(pid) });
    }
}
EOF
printf 'fn main() { std::thread::sleep(std::time::Duration::from_secs(20)); }\n' > "$d/sleeper.rs"
for p in sid sleeper; do
  rustc -O -o "$d/$p" "$d/$p.rs" 2> "$d/rustc.err" || { echo "rustc failed: $(head -c 300 "$d/rustc.err")"; exit 1; }
done

# The Bash calls: one that leaves a background job behind and returns at once, one that runs a background job and a
# foreground sleep. Each writes the pids the probe follows.
printf 'sleep 301 >/dev/null 2>&1 &\necho $! > %s/orphan.pid\n' "$d" > "$d/orphan.sh"
printf 'sleep 302 >/dev/null 2>&1 &\necho $! > %s/bg.pid\necho $$ > %s/live.pid\n: > %s/ready\nsleep 100\n' \
  "$d" "$d" "$d" > "$d/live.sh"

now_ms() { perl -MTime::HiRes=time -e 'printf "%d\n", time() * 1000'; }
alive() { s=$(ps -o stat= -p "$1" 2>/dev/null) && case $s in Z*|"") false ;; *) true ;; esac; }
show() { # role pid
  info=$(ps -o ppid= -o pgid= -o stat= -p "$2" 2>/dev/null) || { echo "  $1 pid=$2 gone"; return; }
  set -- "$1" "$2" $info
  echo "  $1 pid=$2 ppid=$3 pgid=$4 sid=$("$d/sid" "$2") stat=$5 exe=$(basename "$(ps -o comm= -p "$2" 2>/dev/null)")"
}
descendants() { # every pid descending from $1 by parent pid
  ps -A -o pid= -o ppid= | awk -v root="$1" '{ parent[$1] = $2 } END {
    set[root] = 1; more = 1
    while (more) { more = 0; for (p in parent) if (!(p in set) && (parent[p] in set)) { set[p] = 1; more = 1 } }
    for (p in set) if (p != root) print p }'
}
# Starts `claude -p "$1"` as the leader of a process group of its own, as a run's tree starts it; sets c and sub.
start() {
  rm -f "$d/claude.pid" "$d/claude.status" "$d/ready"
  (cd "$d" && perl -e 'setpgrp(0, 0); open(my $f, ">", "claude.pid") or die; print $f $$; close $f; exec @ARGV or die' \
     claude -p "$1" --max-turns 4 --max-budget-usd 0.50 --allowedTools Bash --output-format json \
     > claude.json 2> claude.err; echo $? > claude.status) & sub=$!
  i=0; while [ ! -s "$d/claude.pid" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done
  c=$(cat "$d/claude.pid"); seen="$seen $c"
}
result() { echo "  claude exit status $(cat "$d/claude.status" 2>/dev/null || echo none), result: $(grep -oE '"(subtype|is_error|num_turns)":("[a-z_]+"|true|false|[0-9]+)' "$d/claude.json" | tr '\n' ' ')"; }

echo "probe 1: SIGTERM to Claude Code's group during its second Bash call"
start "Use the Bash tool twice, one call after the other, neither in the background: first run: sh $d/orphan.sh -- then run: sh $d/live.sh -- then reply with the word done."
i=0; while [ ! -e "$d/ready" ] && [ $i -lt 1200 ]; do sleep 0.1; i=$((i+1)); done
[ -e "$d/ready" ] || { echo "  the second Bash call never started within 120 s"; kill -s KILL -- "-$c"; exit 1; }
sleep 1
echo "  claude: pid=$c sid=$("$d/sid" "$c") (the probe's own sid=$("$d/sid" $$))"
follow=$(descendants "$c")
for p in $follow; do show descendant "$p"; done
orphan=$(cat "$d/orphan.pid"); bg=$(cat "$d/bg.pid"); live=$(cat "$d/live.pid")
show "first call's background job" "$orphan"
show "second call's background job" "$bg"
show "second call's script" "$live"
follow=$(printf '%s\n' "$c" $follow "$orphan" "$bg" "$live" | sort -un)
seen="$seen $follow"
t0=$(now_ms); kill -s TERM -- "-$c"
gone=
while [ $(( $(now_ms) - t0 )) -lt 15000 ]; do
  for p in $follow; do
    case " $gone " in *" $p "*) continue ;; esac
    alive "$p" || { gone="$gone $p"; eval "at_$p=$(( $(now_ms) - t0 ))"; }
  done
  [ "$(echo $gone | wc -w)" -eq "$(echo $follow | wc -w)" ] && break
  sleep 0.1
done
for p in $follow; do
  eval "t=\${at_$p:-}"
  role=descendant
  [ "$p" = "$c" ] && role=claude; [ "$p" = "$orphan" ] && role="first call's background job"
  [ "$p" = "$bg" ] && role="second call's background job"; [ "$p" = "$live" ] && role="second call's script"
  if [ -n "$t" ]; then echo "  $role pid=$p gone ${t} ms after SIGTERM"; else echo "  $role pid=$p still running 15 s after SIGTERM"; fi
done
kill -s KILL -- "-$c" 2>/dev/null
wait "$sub" 2>/dev/null; result

echo "probe 2: a run that ends by itself after a Bash call that left a background job"
rm -f "$d/orphan.pid"
start "Use the Bash tool once, not in the background, to run: sh $d/orphan.sh -- then reply with the word done."
i=0; while kill -0 "$sub" 2>/dev/null && [ $i -lt 1200 ]; do sleep 0.1; i=$((i+1)); done
kill -0 "$sub" 2>/dev/null && { echo "  the run did not end within 120 s"; kill -s KILL -- "-$c"; }
wait "$sub" 2>/dev/null; result
sleep 2
if [ -s "$d/orphan.pid" ]; then
  orphan=$(cat "$d/orphan.pid"); seen="$seen $orphan"
  if alive "$orphan"; then echo "  the call's background job still runs 2 s after the run ended:"; show job "$orphan"
  else echo "  the call's background job pid=$orphan was gone 2 s after the run ended"; fi
else
  echo "  the Bash call never ran"
fi

echo "probe 3: whether another process of the same user reads a process's environment"
mark=$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')
for program in "$d/sleeper" /bin/sleep; do
  OWL451_MARK=$mark "$program" 20 > /dev/null 2>&1 & p=$!; seen="$seen $p"
  sleep 0.5
  n=$(ps -E -o command= -p "$p" 2>/dev/null | grep -c "OWL451_MARK=$mark")
  [ "$n" -gt 0 ] && read=yes || read=no
  case $program in /bin/*) name=$program ;; *) name="a program built here" ;; esac
  echo "  $name, started with the variable: read=$read"
  kill -s KILL "$p" 2>/dev/null; wait "$p" 2>/dev/null
done
