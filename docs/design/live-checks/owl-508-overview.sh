#!/bin/sh
# OWL-508 browser check, quoted by the "Browser checks" bullet of docs/design/build-plan.md, part 4 ("The overview"). On macOS:
#   sh docs/design/live-checks/owl-508-overview.sh
# Builds a bench data directory (a registered project, its ticket refs, a ready.json and an event log; no Linear, no
# GitHub, no daemon) and serves the web UI on it, on 127.0.0.1:18466 unless the first argument is another port. It prints
# a login link, which you open in Safari or Chrome, and the two commands that change the bench while the page is open.
# The link holds a one-time code that logs in: keep it out of chats and pull requests. Press Enter in this terminal for a
# fresh link, Ctrl-C to end. The scratch folder, under ${TMPDIR:-/tmp}, is removed on exit; a script that was killed
# leaves it, and its path is printed first.
set -u
PATH="$PATH:/opt/homebrew/opt/rustup/bin:$HOME/.cargo/bin"
root=$(cd "$(dirname "$0")/../../.." && pwd) || exit 1
port=${1:-18466}
date -u +"date=%Y-%m-%dT%H:%M:%SZ"; echo "macos=$(sw_vers -productVersion 2>/dev/null || uname -sr)"
d=$(mktemp -d "${TMPDIR:-/tmp}/owl508.XXXXXX") || exit 1
trap 'rm -rf "$d"' EXIT
trap 'exit 130' INT TERM
echo "scratch=$d"
cd "$root" || exit 1

echo "Building the bench (a few minutes the first time)..."
cargo build -q --locked -p owlshift-testkit --bin owlshift-ui-bench || { echo "the build failed"; exit 1; }
OWLSHIFT_BENCH_KEEP="$d/keep" cargo test -q --locked -p owlshift-testkit --test on_demand \
  keep_a_bench_for_the_browsers -- --ignored >"$d/seed.log" 2>&1 || { tail -n 30 "$d/seed.log"; echo "the seeding failed"; exit 1; }
data=$(cat "$d/keep/data-path") || exit 1
target=$(cargo metadata --locked --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
bin="$target/debug/owlshift-ui-bench"
# What the page reads must be there before it is served.
[ -f "$data/events.jsonl" ] || { echo "$data holds no event log"; exit 1; }
[ -n "$(find "$data/projects" -name ready.json 2>/dev/null)" ] || { echo "$data holds no ready.json"; exit 1; }

echo
echo "Change the bench while the page is open, from another terminal:"
echo "  '$bin' touch-ready '$data'    # DEMO-8 joins the ready column"
echo "  '$bin' append-event '$data'   # a park of DEMO-2 enters the log, dated now"
echo
"$bin" serve "$data" --port "$port"
