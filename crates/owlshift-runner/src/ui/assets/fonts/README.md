# Fonts of the local web UI

Carried in the binary (`include_bytes!` in `../../server.rs`) and served from
`/fonts/`, so the UI loads nothing from a third-party server (visual identity
section 5). Both typefaces are under the SIL Open Font License 1.1; the texts
are `OFL-BricolageGrotesque.txt` and `OFL-JetBrainsMono.txt`.

Produced once, offline, on 2026-11-06 (OWL-165) with fontTools 4.68.0 and
brotli; the build never needs Python.

| File | Source | Source SHA-256 | WOFF2 SHA-256 |
| --- | --- | --- | --- |
| `BricolageGrotesque-latin.woff2` | Bricolage Grotesque 1.001, `BricolageGrotesque[opsz,wdth,wght].ttf` from [google/fonts](https://github.com/google/fonts/tree/main/ofl/bricolagegrotesque) | `413e7357809ddd12fd80a96a8a396de0e401638d4acd3cb3e37532f0472ac682` (equals the hash recorded in visual-identity.md section 6) | `d224face25e27e8e11ac08c7a1249c71cd89a1ba22494e7e8082309840b83bec` |
| `JetBrainsMono-Regular-latin.woff2` | JetBrains Mono 2.305, `fonts/ttf/JetBrainsMono-Regular.ttf` from [JetBrains/JetBrainsMono](https://github.com/JetBrains/JetBrainsMono) `master` | `e6fd0d7e91550b3ed2b735d4312474362c4716edc4fc0577a0f61ed782d5aed1` (computed on 2026-11-06; no hash was recorded before) | `46df3b07ca075566f2b25ef6e39ceddb94e228e78722da107a28a70b29a3c495` |
| `JetBrainsMono-Medium-latin.woff2` | JetBrains Mono 2.305, `fonts/ttf/JetBrainsMono-Medium.ttf`, same repository path | `d16e6dc99672734698d629705f617c79f6eb6040f5113efe3a145204dc988109` (equals the hash recorded in visual-identity.md section 3) | `4190784985b2df4f2a64a53f299cc1ad9d618832db65e59fb0f1476020fa1287` |

The 2.304 release archive holds version 2.304 files, not 2.305: the 2.305
files come from the repository's `master`.

## Commands

Latin set: Basic Latin, Latin-1 Supplement, general punctuation, the euro
sign, and the check and cross marks (U+2713, U+2717) the state classes use.

```sh
U="U+0020-007E,U+00A0-00FF,U+2010-2027,U+2030-205E,U+20AC,U+2713,U+2717"

# Bricolage Grotesque: the width axis is pinned to 100 (the size cap needs it);
# opsz (12-96) and wght (200-800) stay.
fonttools varLib.instancer 'BricolageGrotesque[opsz,wdth,wght].ttf' wdth=100 -o bric-w100.ttf
pyftsubset bric-w100.ttf --unicodes="$U" --flavor=woff2 --layout-features='*' \
  --output-file=BricolageGrotesque-latin.woff2

# JetBrains Mono
pyftsubset JetBrainsMono-Regular.ttf --unicodes="$U" --flavor=woff2 --layout-features='*' \
  --output-file=JetBrainsMono-Regular-latin.woff2
pyftsubset JetBrainsMono-Medium.ttf --unicodes="$U" --flavor=woff2 --layout-features='*' \
  --output-file=JetBrainsMono-Medium-latin.woff2
```

Bricolage Grotesque has no U+2713 or U+2717: the state symbols fall back to
the next family of the stack.
