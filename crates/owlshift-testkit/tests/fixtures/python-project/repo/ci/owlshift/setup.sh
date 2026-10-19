#!/bin/sh
# The fixture's stack.setup (OWL-330): installs both groups into a kept
# .venv from the wheels the operator allows read-only, uv's own cache in the
# run's temporary folder and the system's Python.
set -eu
export UV_CACHE_DIR="$TMPDIR/uv-cache" UV_PYTHON_DOWNLOADS=never UV_NO_MANAGED_PYTHON=1
[ -d .venv ] || uv venv .venv
uv pip install --python .venv/bin/python --offline --no-index \
    --find-links "$HOME/.cache/owlshift-wheels" --group lint --group test
count=$(cat .setup-count 2>/dev/null || echo 0)
echo $((count + 1)) > .setup-count
