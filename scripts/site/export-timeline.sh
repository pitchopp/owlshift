#!/usr/bin/env bash
# Exports the public site's timeline as the README's two images (OWL-579):
# assets/readme/timeline-light.png and timeline-dark.png, drawn by headless
# Chrome from the site as scripts/site/build.sh writes it, and records beside
# them, in assets/readme/timeline.sha256, the SHA-256 of the timeline's
# markup. The `site_page` test compares that hash with site/index.html, so a
# timeline changed without a new export fails CI. Only this script writes the
# hash: never edit it by hand, run the script again.
#
# What the hash covers: the lines of site/index.html from the first one
# holding `<ol class="timeline">` to the first one holding `</ol>`, whole
# lines, each ended by a newline. A change to site/site.css that alters how
# the timeline looks is not caught: re-run this script then too.
#
# Nothing in site/ or in build.sh changes. The page is built into a scratch
# folder and the timeline is put in a page of its own there, forced to one
# theme, without the Content-Security-Policy and the theme script (neither
# is needed to draw it), and served on 127.0.0.1 so that Chrome loads the
# fonts, which it refuses from file://.
#
# Needs Google Chrome (the CHROME variable names another) and python3.

set -eu

PROGRAM=${0##*/}
root=$(cd "$(dirname "$0")/../.." && pwd)
out="$root/assets/readme"
chrome=${CHROME:-"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
width=1000

if [ "$#" -ne 0 ]; then
    echo "Usage: $PROGRAM" >&2
    echo "Writes the timeline's two images and its hash into assets/readme/." >&2
    exit 2
fi
if ! command -v "$chrome" >/dev/null 2>&1; then
    echo "$PROGRAM: Chrome not found at $chrome; set CHROME" >&2
    exit 1
fi

scratch=$(mktemp -d)
server=
cleanup() {
    if [ -n "$server" ]; then
        kill "$server" 2>/dev/null || true
        wait "$server" 2>/dev/null || true
    fi
    rm -rf "$scratch"
}
trap cleanup EXIT

"$root/scripts/site/build.sh" "$scratch/site" >/dev/null
site="$scratch/site"

# The timeline's markup, and its hash.
sed -n '/<ol class="timeline">/,/<\/ol>/p' "$root/site/index.html" >"$scratch/timeline.html"
if [ "$(grep -c '<ol class="timeline">' "$scratch/timeline.html")" -ne 1 ]; then
    echo "$PROGRAM: expected one timeline in site/index.html" >&2
    exit 1
fi
mkdir -p "$out"
hash=$(shasum -a 256 <"$scratch/timeline.html" | cut -d' ' -f1)

# One page per theme, in the built site's folder: the stylesheet, the fonts
# and the brand files are there. The measuring script writes the document's
# height into the title, which --dump-dom prints.
page() {
    cat <<EOF
<!doctype html>
<html lang="en" data-theme="$1">
<head>
<meta charset="utf-8">
<link rel="stylesheet" href="site.css">
</head>
<body>
<div class="wrap"><section id="how" class="section">
EOF
    cat "$scratch/timeline.html"
    cat <<EOF
</section></div>
<script>
document.fonts.ready.then(function () {
  document.title = "height=" + Math.ceil(document.body.getBoundingClientRect().height);
});
</script>
</body>
</html>
EOF
}
page light >"$site/timeline-light.html"
page dark >"$site/timeline-dark.html"

port=$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$site" >/dev/null 2>&1 &
server=$!
sleep 1

for theme in light dark; do
    url="http://127.0.0.1:$port/timeline-$theme.html"
    height=$("$chrome" --headless=new --disable-gpu --hide-scrollbars \
        --window-size="$width,2000" --virtual-time-budget=5000 \
        --dump-dom "$url" 2>/dev/null | sed -n 's/.*<title>height=\([0-9]*\)<\/title>.*/\1/p')
    if [ -z "$height" ]; then
        echo "$PROGRAM: could not measure the $theme page" >&2
        exit 1
    fi
    "$chrome" --headless=new --disable-gpu --hide-scrollbars \
        --force-device-scale-factor=2 --window-size="$width,$height" \
        --virtual-time-budget=5000 --screenshot="$out/timeline-$theme.png" \
        "$url" >/dev/null 2>&1
    echo "$PROGRAM: $theme, ${width}x${height} at 2x"
done

printf '%s\n' "$hash" >"$out/timeline.sha256"
echo "$PROGRAM: wrote $out (timeline $hash)"
