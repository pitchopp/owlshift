#!/bin/sh
# OWL-433 live check L6, quoted by the L6 bullet of docs/design/build-plan.md, "P7 · Background service": a step and
# the runner's deadlines across a sleep of the Mac. Run by the maintainer in a terminal of the Mac, on power:
#   sh docs/design/live-checks/owl-433-sleep.sh
# It builds a small Rust program (rustc, from rustup) whose 300 s deadline is measured with std::time::Instant, as
# the runner's are (process/tree.rs), and starts one short `claude -p` run (this machine's own Claude Code login, one
# Bash call `sleep 240`, at most 3 turns and 0.50 USD). Once the Bash call has started, it puts the Mac to sleep
# (`pmset sleepnow`): wait about 3 minutes, wake it, and wait for the script to end (at most 15 minutes). Paste back
# everything it prints. It prints the run's exit status and three fields of its JSON result (subtype, is_error,
# num_turns), never its text or any environment variable. Scratch files under ${TMPDIR:-/tmp}, removed at the end.
set -u
PATH="$PATH:/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin:$HOME/.local/bin"
date -u +"date=%Y-%m-%dT%H:%M:%SZ"; echo "macos=$(sw_vers -productVersion) arch=$(uname -m)"
echo "rustc=$(rustc --version 2>/dev/null || echo missing) claude=$(claude --version 2>/dev/null | head -1 || echo missing)"
d=$(mktemp -d "${TMPDIR:-/tmp}/owl433-sleep.XXXXXX")
cpid=; ppid_=
trap '[ -n "$cpid" ] && kill "$cpid" 2>/dev/null; [ -n "$ppid_" ] && kill "$ppid_" 2>/dev/null; rm -rf "$d"' EXIT
cat > "$d/probe.rs" <<'EOF'
use std::time::{Duration, Instant, SystemTime};
fn main() {
    let deadline = Duration::from_secs(300);
    let (i0, s0) = (Instant::now(), SystemTime::now());
    let mut gap = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let i = i0.elapsed().as_secs();
        let s = s0.elapsed().map(|d| d.as_secs()).unwrap_or(0);
        if s.saturating_sub(i) > gap {
            gap = s - i;
        }
        if i0.elapsed() >= deadline {
            println!("deadline of 300 s by Instant fired at Instant {i} s, wall clock {s} s; wall ahead of Instant by {gap} s");
            return;
        }
    }
}
EOF
rustc -O -o "$d/probe" "$d/probe.rs" 2> "$d/rustc.err" || { echo "rustc failed: $(head -c 300 "$d/rustc.err")"; exit 1; }
t0=$(date +%s)
"$d/probe" > "$d/probe.out" & ppid_=$!
(cd "$d" && claude -p "Use the Bash tool exactly once, to run this command: : > $d/ready; sleep 240 -- then reply with the word done." \
  --max-turns 3 --max-budget-usd 0.50 --allowedTools Bash --output-format json > "$d/claude.json" 2> "$d/claude.err"
  echo $? > "$d/claude.status"; date +%s > "$d/claude.end") & cpid=$!
i=0; while [ ! -e "$d/ready" ] && [ $i -lt 120 ]; do sleep 1; i=$((i+1)); done
[ -e "$d/ready" ] || { echo "the Bash call never started within 120 s"; exit 1; }
echo "the Bash call started after ${i}s. The Mac sleeps in 10 s: wake it after about 3 minutes, then wait here."
sleep 10; pmset sleepnow > /dev/null; echo "pmset sleepnow status=$?"
wait "$cpid"; cpid=
wait "$ppid_"; ppid_=
echo "probe: $(cat "$d/probe.out")"
echo "claude: exit status $(cat "$d/claude.status"), ended $(( $(cat "$d/claude.end") - t0 )) s after the start (wall)"
echo "claude result: $(grep -oE '"(subtype|is_error|num_turns)":("[a-z_]+"|true|false|[0-9]+)' "$d/claude.json" | tr '\n' ' ')"
echo "claude stderr bytes: $(wc -c < "$d/claude.err" | tr -d ' ')"
since=$(date -r "$t0" "+%Y-%m-%d %H:%M:%S")
echo "sleeps and wakes since $since:"
pmset -g log | awk -v s="$since" '$1" "$2 >= s && ($4 == "Sleep" || $4 == "Wake" || $4 == "DarkWake") {print " ", $1, $2, $4}' | tail -8
