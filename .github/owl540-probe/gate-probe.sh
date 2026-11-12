#!/bin/sh
# OWL-540 probe, never merged: a confined run started from the owlshift
# binary $1. `owlshift gate --at` the root commit of this public repository,
# which has no owlshift.toml, so the local one's gate runs, confined as a
# Build's: it writes in the run's temporary folder, and must fail to read a
# file of the home and to write in the real home. A write in the home fails
# under Seatbelt; under bwrap the home is a private, empty tmpfs, so the
# write lands there and is gone with the run: the real home is checked
# afterwards.
set -eu
bin=$1
root_commit=204c9c97f7e59c2fae9216d570458335c8e8eaa4
work=$(mktemp -d)
printf 'owl540 secret\n' > "$HOME/.owl540-secret"
rm -f "$HOME/owl540-escape"
git clone -q https://github.com/pitchopp/owlshift "$work/probe"
cat > "$work/probe/owlshift.toml" <<'EOF'
requires = ">=0.0"
[tracker]
kind = "markdown"
admit = "delegation"
states = { ready = "a", working = "b", needs_input = "c", review = "d" }
[stack]
gate = [
    '''sh -c 'echo inside > "$TMPDIR/owl540" && cat "$TMPDIR/owl540" && echo "HOME=$HOME"' ''',
    '''sh -c 'if cat "$HOME/.owl540-secret" 2>/dev/null; then echo "LEAK: home file read"; exit 1; fi; echo "home file not readable"' ''',
    '''sh -c 'if (echo x > "$HOME/owl540-escape") 2>/dev/null; then echo "home written inside the run only: $(ls -A "$HOME")"; else echo "home not writable"; fi' ''',
]
[pipeline]
default = "trivial"
plan_approval = "never"
[models]
[policy]
always_human = []
EOF
status=0
(cd "$work/probe" && OWLSHIFT_DATA_DIR="$work/data" OWLSHIFT_CONFIG_DIR="$work/config" "$bin" gate --at "$root_commit") || status=$?
if [ -e "$HOME/owl540-escape" ]; then
    echo "PROBE: LEAK, $HOME/owl540-escape exists"
    exit 1
fi
if [ "$status" -ne 0 ]; then
    echo "PROBE: owlshift gate exited $status"
    exit 1
fi
echo "PROBE: confined run passed (home file unreadable, real home unchanged)"
