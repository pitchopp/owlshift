# Owlshift — visual identity

Status: draft, 2026-10-04 (OWL-162). Companion documents: [design & architecture](architecture.md), [runtime & operations](runtime-and-operations.md), [build plan](build-plan.md).

This document is the reference for how Owlshift looks and sounds: its personality, its voice, the logo, the colours, the typefaces, and how each surface applies them. The files it describes live in [`assets/brand/`](../../assets/brand/). Like every design document, it comes first: a surface that needs a colour, a typeface or a tone this document does not give starts with a change here, reviewed like code. Every choice below was made with the maintainer on 2026-10-04, from mock-ups compared side by side; section 7 keeps what was rejected and why.

## 1. Personality

**The watch owl.** Owlshift is warm, playful and dependable. It works the night shift on the backlog, and the questions it could not settle are waiting at breakfast. Its humour is the friendliness of a colleague who stayed up so you would not have to, never a joke at the expense of what the reader needs to know.

What it is not: childish, loud, or a mascot that talks over the facts. A user who reads `owlshift doctor` every morning should find it pleasant on the hundredth reading, not tiring.

## 2. Voice

Every text of the product is written in English, as the rest of the repository is, and the owl speaks in all of them: the CLI, desktop notifications, the human-readable part of ticket comments, the web UI, the README and the site.

1. **Facts first, the wink around them.** A message keeps all of its content: the subject, the detail, and for a problem the why and the fix. The voice dresses headings, summaries, transitions, empty states and successes; it never replaces or hides a fact.
2. **The owl speaks in the first person, briefly.** "I" is Owlshift; "you" is the operator or the decider. One owl touch per message at most, drawn from a short lexicon: *night shift*, *hoo*, *takeoff*, *nap*, *breakfast*, *keep watch*, *perch*. No emoji: the `{o,o}` signature stands in for one, and terminals render it everywhere.
3. **Serious topics stay sober.** On security, agent isolation, credentials and secrets, data loss, money and legal wording, the owl still speaks in the first person, without a joke. These are the always-human categories of [architecture, section 8](architecture.md#8-policy-security--trust), plus agent isolation and credentials.
4. **What a machine reads never changes.** The voice applies to text a person reads, never to text a program parses: `--json` output, event names and fields, exit codes, the hidden `<!-- owlshift:{…} -->` marker that opens a comment the runner reads back (`QUESTIONS`, `DECISION`, `RESUME`, `PARKED`), the numbering of questions, and the `go` a decider ends a reply with.

| Moment | Text |
| --- | --- |
| A report's header | `{o,o} owlshift 0.1.0 · macOS 15.6` |
| Not ready | `✗ Not ready: 1 problem to fix before takeoff` |
| Ready | `✓ Ready. Hand me a ticket: owlshift do TICKET` |
| Questions posted | `Hoo. I left 2 questions on OWL-42. They'll be waiting at breakfast.` |
| The quiet window | `Your answer is still warm: I'll wait 7 more minutes in case you're typing.` |
| An empty log | `Nothing in the log yet. The night is young.` |
| A sober failure | `✗ Agent isolation: bwrap is missing. I run no agent without it.` |

The build plan fixes some messages word for word, such as doctor's summary ([build plan](build-plan.md#cli-surface-for-p0-and-p1), "How `doctor` reports"), and tests check them. That wording stands until the follow-up that gives the CLI this voice rewrites it, in the build plan first (section 6).

## 3. Logo

### The mark

`{o,o}`: the ASCII owl, braces for the head, two `o` for the eyes, a comma for the beak. It says "a tool you run in a terminal" and "an owl" at once, and survives at 16 px, where it reads as a warm `{oo}` on a dark tile.

It is drawn on a 100-unit square: a tile with corners of radius 22; the braces and the eyes stroked 6 units wide with round caps and joins; the eyes centred at (38.5, 47) and (61.5, 47) with a radius of 7.5; a filled comma below and between them. The braces are always the light or dark contrast colour, the eyes and the comma always the accent. `owlshift-mark.svg` is the reference for the paths of the braces and the comma; every other file draws the mark with the same elements.

| File | Tile | Braces | Eyes and comma | Use |
| --- | --- | --- | --- | --- |
| `owlshift-mark.svg` | `night` | `cream` | `apricot` | The default: avatar, favicon, app icon, any light or mid background |
| `owlshift-mark-light.svg` | `cream` | `night` | `apricot-burnt` | On a dark or coloured background where the dark tile would sink; never on white, where the tile vanishes (1.11:1) |
| `owlshift-mark-mono.svg` | none | `currentColor` | `currentColor` | One colour: print, a macOS menu-bar template image, a very light or very dark background |

### The wordmark and the lockups

The wordmark is `owlshift_`, always lowercase, set in JetBrains Mono Medium and outlined, so no file needs the font. The underscore is a cursor and the wordmark's only colour: `apricot` on a dark background, `apricot-burnt` on a light one.

A lockup sets the mark before the wordmark, on the same line: the mark as tall as the wordmark's outline, from the top of its ascenders to the bottom of the underscore, and a quarter of the mark's width between the tile and the first letter. Both lockups use the default mark: on a dark page its tile melts in and the braces and eyes keep their own contrast (section 4, "Contrast").

The outlines were drawn once from JetBrains Mono Medium, version 2.305 (`JetBrainsMono-Medium.ttf` from [JetBrains/JetBrainsMono](https://github.com/JetBrains/JetBrainsMono), SHA-256 `d16e6dc99672734698d629705f617c79f6eb6040f5113efe3a145204dc988109`), with fontTools 4.66.1, on 2026-10-04 (OWL-162); nothing needs the font afterwards.

| File | Wordmark | Cursor | Use |
| --- | --- | --- | --- |
| `owlshift-lockup.svg` | `night` | `apricot-burnt` | Light backgrounds: the README in GitHub's light theme, documents |
| `owlshift-lockup-on-dark.svg` | `cream` | `apricot` | Dark backgrounds: the README in GitHub's dark theme, the site's dark theme |

### Rules

- **Smallest size:** 16 px for the tiled mark, 96 px wide for a lockup.
- **Clear space:** on every side, at least the outer diameter of one eye, 21 % of the mark's width.
- **The untiled mark** goes only where each of its parts keeps a 3:1 contrast with the background.
- **Never:** stretch, rotate, recolour outside the palette, add a shadow, a gradient or an outline, rearrange the glyphs, capitalise the wordmark or set it in another typeface.

Typed as text, `{o,o}` is the owl's signature: in the header of the CLI's reports, and sparingly in the README and on the site. It never appears in text a program parses (section 2, rule 4).

## 4. Colour

The palette is called **Dawn**: an aubergine night, an apricot sunrise, the questions read over breakfast. Aubergine is almost absent among developer tools, which makes it the palette's signature; apricot is the light the owl keeps on.

### Brand

| Token | Hex | Role |
| --- | --- | --- |
| `night` | `#2B1638` | The brand's dark: the tile, text on light, the dark theme's surfaces |
| `night-deep` | `#1C0E25` | The dark theme's page |
| `apricot` | `#FF8E5E` | The accent: the eyes, the primary action and focus on dark |
| `apricot-burnt` | `#D9572A` | The accent as a graphic on light: the light tile's eyes, the cursor, the focus ring |
| `apricot-ink` | `#A83E17` | The accent as text on light: links |
| `apricot-light` | `#FFB38F` | The accent as text on dark: links |
| `sun` | `#FFD166` | Warnings on dark, and illustrations; never next to a warning as decoration |
| `cream` | `#FFF1E6` | Text on dark, the light tile, raised surfaces on light |
| `paper` | `#FFF8F2` | The light theme's page |

### Neutrals

Greys tinted with plum, so that even the neutrals stay in the palette.

| Token | Hex | Role |
| --- | --- | --- |
| `plum-800` | `#3D2150` | Raised surfaces on dark |
| `plum-700` | `#4A2D5C` | Hairline borders on dark |
| `plum-600` | `#5A3A68` | Secondary text on light |
| `plum-500` | `#725480` | Muted text on light |
| `plum-400` | `#8E7899` | Strong borders, such as an input's, on both themes |
| `plum-300` | `#A692AE` | Muted text on dark |
| `plum-200` | `#C9B3D6` | Secondary text on dark |
| `plum-100` | `#EBDDE4` | Hairline borders on light |

### States

Each state has a value for dark backgrounds and one for light, and keeps its symbol everywhere, so colour never carries a meaning alone. Danger leans pink, to stay apart from the brand's apricot.

| State | Symbol | On dark | On light |
| --- | --- | --- | --- |
| Success | `✓` | `#7FE0B0` | `#1E7A52` |
| Warning | `!` | `#FFD166` (`sun`) | `#8A5A00` |
| Danger | `✗` | `#FF6B81` | `#B8283F` |
| Information | `·` | `#C9B3D6` (`plum-200`) | `#5A3A68` (`plum-600`) |

### Themes

The web UI, the menu-bar app and the site follow the system's light or dark setting, with a switch.

| Role | Light | Dark |
| --- | --- | --- |
| Page | `paper` | `night-deep` |
| Surface (cards, panels) | `#FFFFFF` | `night` |
| Raised (menus, selected rows) | `cream` | `plum-800` |
| Text | `night` | `cream` |
| Secondary text | `plum-600` | `plum-200` |
| Muted text | `plum-500` | `plum-300` |
| Hairline border | `plum-100` | `plum-700` |
| Strong border (inputs) | `plum-400` | `plum-400` |
| Primary action | `night` fill, `cream` text | `apricot` fill, `night` text |
| Link | `apricot-ink` | `apricot-light` |
| Focus ring | `apricot-burnt` | `apricot` |
| "Needs input" badge | `#FFDCCB` fill, `#7A2E10` text | `#5A2A1E` fill, `#FFD2BD` text |

### Contrast

WCAG 2.2 AA, computed on 2026-10-04 from the hex values above: text at least 4.5:1 on every background it is listed for, a graphic (an icon, a focus ring, an input's border, a part of the mark) at least 3:1. Hairline borders only separate areas that differ otherwise and are exempt.

| Text | Light: page · surface · raised | Dark: page · surface · raised |
| --- | --- | --- |
| Text | 15.66 · 16.48 · 14.89 | 16.66 · 14.89 · 12.39 |
| Secondary text | 8.91 · 9.38 · 8.47 | 9.57 · 8.55 · 7.12 |
| Muted text | 6.06 · 6.37 · 5.76 | 6.45 · 5.76 · 4.79 |
| Link | 5.93 · 6.24 · 5.64 | 10.62 · 9.49 · 7.90 |
| Success | 5.04 · 5.30 · 4.79 | 11.58 · 10.35 · 8.61 |
| Warning | 5.63 · 5.93 · 5.36 | 12.79 · 11.43 · 9.51 |
| Danger | 5.85 · 6.15 · 5.56 | 6.74 · 6.02 · 5.01 |

| Pair | Ratio |
| --- | --- |
| Primary action, light (`cream` on `night`) | 14.89 |
| Primary action, dark (`night` on `apricot`) | 7.28 |
| "Needs input" badge, light · dark | 7.35 · 8.52 |
| Focus ring, light (`apricot-burnt` on `paper` · white) | 3.73 · 3.93 |
| Focus ring, dark (`apricot` on `night-deep` · `night`) | 8.15 · 7.28 |
| Strong border (`plum-400`), light: page · surface · raised | 3.77 · 3.96 · 3.58 |
| Strong border (`plum-400`), dark: page · surface · raised | 4.65 · 4.16 · 3.46 |
| Mark, dark: braces · eyes on the tile | 14.89 · 7.28 |
| Mark, light: braces · eyes on the tile | 14.89 · 3.55 |
| Dark tile on white · on `paper` | 16.48 · 15.66 |
| Lockup, light: wordmark · cursor on white | 16.48 · 3.93 |
| Lockup, dark: wordmark · cursor on GitHub's dark page (`#0D1117`) | 17.10 · 8.37 |

Two pairs fail and are therefore ruled out: `apricot` as text on a light background (2.15:1 on `paper`), hence `apricot-ink` for links; and the light tile on white (1.11:1), hence the dark tile there. The dark tile on `night-deep` (1.12:1) is allowed: the tile then melts into the page and the braces and eyes keep their own contrast.

### In the terminal

A terminal has its user's theme, light or dark, and its user's palette. Owlshift respects both.

- **States keep the 16 standard ANSI colours,** as doctor does already ([build plan](build-plan.md#cli-surface-for-p0-and-p1), "How `doctor` reports"): green `✓`, yellow `!`, red `✗`, the default foreground for `·`, bold for headings. The user's theme decides how they look, so they stay legible on a light terminal as on a dark one; the brand's exact hues would not.
- **The brand appears only in the `{o,o}` signature,** its eyes `o,o` in `apricot`, its braces in the default foreground: in 24-bit colour (`ESC[38;2;255;142;94m`) when `COLORTERM` is `truecolor` or `24bit`; otherwise in colour 209 of the 256-colour palette (`#ff875f`) when `TERM` names a 256-colour terminal; otherwise without colour. What these variables say on each terminal Owlshift supports is checked live by the follow-up that builds the signature (section 6), before code depends on it.
- **Every rule that turns colour off turns the signature's off too:** standard output not a terminal, `NO_COLOR` set, `TERM=dumb`, native Windows.
- On a white terminal the eyes reach 2.26:1 (2.36:1 in colour 209). That is acceptable because the signature is decoration: it carries no meaning, and the same characters read without their colour.

## 5. Typography

| Typeface | Licence | Use | Weights |
| --- | --- | --- | --- |
| [Bricolage Grotesque](https://github.com/google/fonts/tree/main/ofl/bricolagegrotesque) | SIL Open Font License 1.1 | Headings and text: the web UI, the site, documents, images | 400 text, 600 labels and buttons, 800 headings |
| [JetBrains Mono](https://github.com/JetBrains/JetBrainsMono) | SIL Open Font License 1.1 | Code, commands, ticket identifiers (`OWL-42`), the wordmark, terminal captures | 400, 500 |

- **Bricolage Grotesque** has an optical-size axis: left on automatic, its shapes turn calmer at text sizes and more expressive at display sizes. Headings take a line height of 1.15 and, from 32 px, a letter spacing of −0.01 em; text takes 1.5.
- **Scale,** in pixels: 12, 13, 14, 16, 20, 24, 32, 48.
- **Fallbacks:** `"Bricolage Grotesque", system-ui, -apple-system, "Segoe UI", sans-serif` and `"JetBrains Mono", ui-monospace, "SF Mono", Menlo, Consolas, monospace`.
- **Where the files come from:** the local web UI and the menu-bar app carry both typefaces in the binary, as WOFF2 cut down to Latin, and never load them from a third-party server: a tool served on `127.0.0.1` makes no outside call, for privacy as for offline use. The site serves its own copies too.
- **Where the brand's typefaces do not reach:** a terminal keeps its user's font, so the brand reaches it through colour, symbols and voice (sections 2 and 4); GitHub renders the README in its own fonts, so the brand reaches it through images whose text is outlined (the lockup, the social preview).

## 6. Applications

| Surface | What applies | Status |
| --- | --- | --- |
| README and GitHub | The README opens with the lockup in a `<picture>` that follows GitHub's light or dark theme (`prefers-color-scheme`), alternative text "Owlshift", then the tagline. The social preview is `owlshift-social.png`, 1280×640 as GitHub recommends, from `owlshift-social.svg`; GitHub has no API for it, so the maintainer uploads it in the repository's settings when the repository opens (P9) | This change (OWL-162) |
| CLI: the voice | Section 2 applied to every message of `doctor`, `init`, `do`, `continue`, `watch`, `logs` and `config show`, and to desktop notifications; the build plan's fixed wording and the tests that check it change with it | Follow-up |
| CLI: the signature | `{o,o}` in the header of the CLI's reports, its eyes coloured as section 4 says | Follow-up |
| Ticket comments | The voice applies to their human-readable text, with a light hand since a whole team reads them; their markers, numbering and structure stay as they are (section 2, rule 4) | With the CLI's voice |
| Local web UI (P8) | The themes and both typefaces, carried in the binary; the mark as favicon | Follow-up, P8 |
| Menu-bar app (P11) | The app icon from the dark mark on macOS's icon grid; the menu-bar icon from the one-colour mark as a template image (black with transparency, which macOS tints) | Follow-up, P11 |
| Public site | The themes and typefaces of the web UI, the lockups, the social preview's art direction | When a site is planned; no issue yet |

## 7. Decisions

Made with the maintainer on 2026-10-04 (OWL-162), from mock-ups compared side by side.

| Subject | Decision | Rejected |
| --- | --- | --- |
| The mark | The `{o,o}` terminal owl, in its base form | Among four directions: a geometric owl's head, which read as a cat; two eyes in a crescent moon, which vanish at 16 px; a perched mascot, too detailed for a favicon. Among the terminal owl's variants: V-shaped brows, parentheses for the head, a cursor for the beak, one eye open |
| Personality | Warm and playful, the watch owl | A calm, sober watcher, whose navy, gold and cream were judged neither original nor memorable; a raw, almost monochrome tool, judged cold |
| Palette | Dawn: aubergine and apricot | A mid violet, on which the mark lost its contrast; Phosphor, a night-vision green whose brand colour merges with `✓`; Tawny, a brown and orange close to Rust's own brand, whose orange reads as a warning; Electric midnight, an ink and chartreuse unusable on light backgrounds |
| Typefaces | Bricolage Grotesque with JetBrains Mono | Nunito, common and childish at large sizes; Recursive, one family for sans and mono but a heavier file; Figtree, safe and forgettable |
| Voice | The owl speaks in every text, with the four rules of section 2 | A measured wink limited to calm moments; a voice-free product with only a visual personality |
| Terminal colour | The 16 ANSI colours for states, the brand only in the signature | The brand's own hues for states, unreadable on a light terminal |
| Where it lives | This document and the source files in `assets/brand/`; a surface's tokens in code arrive with the surface | A `tokens.json` read by a build script from today, tooling written before any reader exists; a brand book published outside the repository, out of review |
