# Owlshift — visual identity

Status: draft, 2026-10-04 (OWL-162); the web UI's layout and the public site's added on 2026-11-16. Companion documents: [design & architecture](architecture.md), [runtime & operations](runtime-and-operations.md), [build plan](build-plan.md).

This document is the reference for how Owlshift looks and sounds: its personality, its voice, the logo, the colours, the typefaces, and how each surface applies them. The files it describes live in [`assets/brand/`](../../assets/brand/). Like every design document, it comes first: a surface that needs a colour, a typeface or a tone this document does not give starts with a change here, reviewed like code. Every choice below was made with the maintainer from mock-ups compared side by side, on 2026-10-04 unless it gives another date; section 7 keeps what was rejected and why.

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

The build plan fixes some messages word for word, such as doctor's summary ([build plan](build-plan.md#cli-surface-for-p0-and-p1), "How `doctor` reports"), and tests check them. That wording stands until the follow-up that gives the CLI this voice rewrites it, in the build plan first (OWL-163).

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
| Accent as a graphic (a chart's bar, a gauge, the current stage's ring) | `apricot-burnt` | `apricot` |
| Accent as text (a figure of spend) | `apricot-ink` | `apricot` |
| Danger tint (a failed run's row) | `#F9EEF0`: danger at 8 % over the surface | `#3C1D3E`: danger at 8 % over the surface |

The last three rows came with the web UI's layout (section 6, OWL-575, 2026-11-16). The accent as a graphic takes the focus ring's values; the danger tint is written as the colour it gives on the surface, so the contrast tests can read it.

### Contrast

WCAG 2.2 AA, computed on 2026-10-04 from the hex values above: text at least 4.5:1 on every background it is listed for, a graphic (an icon, a focus ring, an input's border, a part of the mark) at least 3:1. Hairline borders only separate areas that differ otherwise and are exempt. So is the "Needs input" badge's fill used as the border of a card that needs you (section 6): the border only marks the card, whose words and primary action say what it waits for. A state's colour may also fill a small shape whose symbol is in the surface colour, as a stage done on the ticket page's stepper: the pair is the one the table lists for that state on the surface.

| Text | Light: page · surface · raised | Dark: page · surface · raised |
| --- | --- | --- |
| Text | 15.66 · 16.48 · 14.89 | 16.66 · 14.89 · 12.39 |
| Secondary text | 8.91 · 9.38 · 8.47 | 9.57 · 8.55 · 7.12 |
| Muted text | 6.06 · 6.37 · 5.76 | 6.45 · 5.76 · 4.79 |
| Link | 5.93 · 6.24 · 5.64 | 10.62 · 9.49 · 7.90 |
| Success | 5.04 · 5.30 · 4.79 | 11.58 · 10.35 · 8.61 |
| Warning | 5.63 · 5.93 · 5.36 | 12.79 · 11.43 · 9.51 |
| Danger | 5.85 · 6.15 · 5.56 | 6.74 · 6.02 · 5.01 |
| Accent as text | 5.93 · 6.24 · 5.64 | 8.15 · 7.28 · 6.06 |

On the danger tint, computed on 2026-11-16 (OWL-575): text 14.53 light, 13.20 dark; secondary text 8.27 · 7.58; muted text 5.62 · 5.11; link 5.50 · 8.42; danger 5.42 · 5.34.

| Pair | Ratio |
| --- | --- |
| Primary action, light (`cream` on `night`) | 14.89 |
| Primary action, dark (`night` on `apricot`) | 7.28 |
| "Needs input" badge, light · dark | 7.35 · 8.52 |
| Focus ring, light (`apricot-burnt` on `paper` · white) | 3.73 · 3.93 |
| Focus ring, dark (`apricot` on `night-deep` · `night`) | 8.15 · 7.28 |
| Strong border (`plum-400`), light: page · surface · raised | 3.77 · 3.96 · 3.58 |
| Strong border (`plum-400`), dark: page · surface · raised | 4.65 · 4.16 · 3.46 |
| Accent as a graphic, light (`apricot-burnt`): page · surface · raised | 3.73 · 3.93 · 3.55 |
| Accent as a graphic, dark (`apricot`): page · surface · raised | 8.15 · 7.28 · 6.06 |
| Mark, dark: braces · eyes on the tile | 14.89 · 7.28 |
| Mark, light: braces · eyes on the tile | 14.89 · 3.55 |
| Dark tile on white · on `paper` | 16.48 · 15.66 |
| Lockup, light: wordmark · cursor on white | 16.48 · 3.93 |
| Lockup, dark: wordmark · cursor on GitHub's dark page (`#0D1117`) | 17.10 · 8.37 |

Two pairs fail and are therefore ruled out: `apricot` as text on a light background (2.15:1 on `paper`), hence `apricot-ink` for links; and the light tile on white (1.11:1), hence the dark tile there. The dark tile on `night-deep` (1.12:1) is allowed: the tile then melts into the page and the braces and eyes keep their own contrast.

### In the terminal

A terminal has its user's theme, light or dark, and its user's palette. Owlshift respects both.

- **States keep the 16 standard ANSI colours,** as doctor does already ([build plan](build-plan.md#cli-surface-for-p0-and-p1), "How `doctor` reports"): green `✓`, yellow `!`, red `✗`, the default foreground for `·`, bold for headings. The user's theme decides how they look, so they stay legible on a light terminal as on a dark one; the brand's exact hues would not.
- **The brand appears only in the `{o,o}` signature,** its eyes `o,o` in `apricot`, its braces in the default foreground: in 24-bit colour (`ESC[38;2;255;142;94m`) when `COLORTERM` is `truecolor` or `24bit`; otherwise in colour 209 of the 256-colour palette (`#ff875f`) when `TERM` names a 256-colour terminal; otherwise without colour. What these variables say on each terminal Owlshift supports is checked live by the follow-up that builds the signature (OWL-164), before code depends on it.
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
| README and GitHub | The README opens with the lockup in a `<picture>` that follows GitHub's light or dark theme (`prefers-color-scheme`), alternative text "Owlshift", then the tagline. The repository's social preview is `owlshift-social.png` (below), which the maintainer uploads in the repository's settings. Once the public site is live, its timeline follows the tagline as an image, in a `<picture>` for both themes too ([build plan](build-plan.md), "The public site", part 4) | This change (OWL-162); the timeline's image with the public site |
| CLI: the voice | Section 2 applied to every message of `doctor`, `init`, `do`, `continue`, `watch`, `logs` and `config show`, and to desktop notifications; the build plan's fixed wording and the tests that check it change with it | Follow-up, OWL-163 |
| CLI: the signature | `{o,o}` in the header of the CLI's reports, its eyes coloured as section 4 says | Follow-up, OWL-164 |
| Ticket comments | The voice applies to their human-readable text, with a light hand since a whole team reads them; their markers, numbering and structure stay as they are (section 2, rule 4) | With the CLI's voice, OWL-163 |
| Local web UI (P8) | The themes and both typefaces, carried in the binary; the mark as favicon; the layout below ("The local web UI's layout") | Themes, typefaces and favicon done in OWL-165: both typefaces from `crates/owlshift-runner/src/ui/assets/fonts/` (sources and hashes in its README), the mark at `/favicon.svg`, the contrast table checked by tests in both themes. The layout, decided on 2026-11-16, done in OWL-575: the frame, the figures, the ticket page's stepper and the charts, with the accent and danger-tint roles of section 4 checked by the same tests |
| Menu-bar app (P11) | The app icon from the dark mark on macOS's icon grid; the menu-bar icon from the one-colour mark as a template image (black with transparency, which macOS tints) | Follow-up, P11, OWL-166 |
| Public site | The themes and typefaces of the web UI, the lockups, the social preview's art direction; the layout below ("The public site's layout") | The layout, decided on 2026-11-16: built right after P9's exit gate ([build plan](build-plan.md), "The public site"): OWL-576 to OWL-579 |

**GitHub, checked live on 2026-10-04.** GitHub keeps a `<picture>` with a `<source media="(prefers-color-scheme: dark)">` in a README: `gh api markdown` (mode `gfm`) on the README's header returned it intact, wrapped in GitHub's own `<themed-picture>`. GitHub has no API to set a repository's social preview: the mutations of its GraphQL schema hold none for a social, Open Graph or preview image, and `repository { usesCustomOpenGraphImage }` read `false`. The repository was already public (`visibility: PUBLIC`), so the preview can be uploaded now rather than when the repository opens.

**The social preview.** 1280×640, as GitHub recommends, drawn in `owlshift-social.svg` and exported to `owlshift-social.png` by headless Chrome at a device scale of 1. On the `night-deep` page: the lockup on dark, centred, the mark 120 px tall with its top at 140 px; the tagline "Your backlog works the night shift." in `cream`, Bricolage Grotesque at 48 px (`wght` 800, `opsz` 48, `wdth` 100), tracked −0.01 em, shaped and kerned by HarfBuzz, centred on its outline, its baseline at 362 px; at the bottom, a dawn of three discs centred on the bottom edge, `night` (radius 200), `plum-800` (150) and `apricot` (100), bleeding off it. The lockup and the tagline keep 60 px from every edge. The tagline's outlines come from Bricolage Grotesque version 1.001 (`BricolageGrotesque[opsz,wdth,wght].ttf` from [google/fonts](https://github.com/google/fonts/tree/main/ofl/bricolagegrotesque), SHA-256 `413e7357809ddd12fd80a96a8a396de0e401638d4acd3cb3e37532f0472ac682`), shaped with uharfbuzz 0.56.2 and drawn with fontTools 4.66.1, on 2026-10-04 (OWL-162).

### The local web UI's layout

Chosen with the maintainer on 2026-11-16, from mock-ups of the Overview, a ticket page and Spend, each in both themes and filled with real data from Owlshift's own drain. The screens and what each shows stay those of [runtime & operations](runtime-and-operations.md), "Observability & the local web UI"; this subsection says how they look. The UI P8 delivered set every block as a titled box of sentences, the same on every screen, so nothing stood out but a badge: these rules replace that look, not what the screens show.

- **The frame.** A side bar on the left holds the mark and the wordmark; the screens (Overview, Graph, Spend, Orchestrator, Config), each with a stroke icon and the current one on the raised colour; how many tickets wait for you, as the "Needs input" badge on Overview; the served projects with their run slots; and, at the bottom, the daemon's state with its symbol and the theme switch (Dark, Light, Auto). Each screen opens with a top bar: its title, then the machine's run slots and each harness's usage as gauges, with the time the usage resets. Together they hold what runtime & operations calls the header. Below 768 px the side bar becomes a bar across the top that wraps.
- **Surfaces.** Cards on the surface colour, a hairline border and 14 px corners; chips and the selected item of a switch on the raised colour. No shadow, gradient or coloured side border. Spacing follows a 4 px grid: 12 px between cards of a row, 20 px inside a card, 24 to 28 px between sections. A card that needs you, such as one of For you, takes the "Needs input" badge's fill as its border.
- **Type.** Section 5's scale and nothing else: 32 for a screen's title, 20 for a section's or a card's, 16 for text, 13 and 12 for labels and metadata, 48 for a figure. Identifiers, durations, token counts and amounts are set in JetBrains Mono, so columns of them align.
- **A figure before a list.** A count is shown as a figure at 48 px, weight 800, its label above it and its first three identifiers below it, then "+N", which opens the rest. "Since your last visit" is five such tiles: delivered, merged, parked, asked, spent. A figure that is a state takes that state's colour and symbol (merged `✓` in success, parked `!` in warning); spend takes the accent, as text: `apricot` on dark, `apricot-ink` on light.
- **One primary action per card.** It is section 4's primary action; any other is drawn with the strong border and no fill, and an action not offered stays visible, disabled, with the reason as its title. Buttons are at least 44 px tall. A choice between views or periods is a segmented switch.
- **Empty means said once.** An area with nothing to show says so in one line, in the voice, with the `{o,o}` signature; it never draws a grid of zeros. A stage with no ticket in the columns view is a dashed outline; a ticket page leaves out a section that has nothing to show rather than drawing it with "Nothing yet".
- **The ticket page.** It opens with the identifier in JetBrains Mono, the ticket's title when local state keeps one, chips for its stage, its round and its delivery, and links to the pull request and to the tracker. Then its stages as a horizontal stepper: a stage done is a filled circle with `✓` in success, the current one a ring in the accent, one ahead an outline in the strong border; each says when it started and, in one line, what happened in it (review passes, questions, a failed run). Below, two columns: on the left the latest run, its summary and its log's link, then the runs, a failed one marked `✗` on a row tinted with danger at 8 % opacity; on the right the cost by role, the questions and their answers, and what watch noted, grouped by kind with a count.
- **Charts.** Spend per day is a bar chart, its bars in `plum-400` and the period's costliest day in the accent; a ticket's cost and a ticket's budget are horizontal bars in the accent on a hairline-coloured track. Each bar reads at least 3:1 against the surface in both themes (`plum-400`: 4.16 on dark, 3.96 on light; the accent: 7.28 and 3.93), and none needs a palette of categories: what a bar is, its label says, never its hue. The UI's own script draws them as SVG, sized by attributes, since the Content-Security-Policy refuses style attributes ([architecture](architecture.md), section 5, "The local web UI"); no chart library is added. Each bar carries its value as its title, and the table of exact figures stays below the chart. A bar's track is a hairline and exempt, like any hairline: the accent reaches only 2.99:1 on it on light, but the bar is read against the surface around it.
- **Both themes, every time.** Every colour comes from a role of section 4, light and dark, so a screen's layout is the same in both; a mock-up or a change to the UI shows both themes.

### The public site's layout

Chosen by the maintainer on 2026-11-16, from three mock-ups of the whole page compared side by side, each in both themes and at desktop and phone widths; section 7 keeps the two set aside. The [build plan](build-plan.md), "The public site", says what the page holds and how it is built and served; this subsection says how it looks. Its surfaces, its type scale and its one primary action per card are the web UI's, above: cards on the surface colour with a hairline border and 14 px corners, no shadow or gradient, section 5's sizes and nothing else, buttons at least 44 px tall. The mock-ups are not kept in the repository: these rules are the reference, and what they leave open is settled when the maintainer reads the captures of the pull request that builds the page.

Two uses of the accent recur below, with the pairs the web UI's layout already uses. *The accent as text* is `apricot` on dark and `apricot-ink` on light, as the UI's figures. *The accent as a shape* is `apricot` on dark and `apricot-burnt` on light, the focus ring's pair.

- **The night band.** The page opens on a band that is dark in both themes, in the social preview's art direction. It is the one area that does not follow the theme: the dark theme's roles apply on it, on the `night-deep` page, so its primary action is `apricot` with `night` text whatever the theme. Centred on it, in order: one line of facts at 13 px in the muted text colour; the tagline as the page's only `h1`, 48 px at weight 800, tracked −0.01 em, on at most two lines; a lead of two sentences at 20 px in the secondary text colour; the primary action and, beside it, a second one drawn with the strong border. At the band's bottom edge, the dawn of the social preview: three discs centred on the edge and cut by it, `night`, `plum-800` and `apricot`, their radii in the preview's proportion of 4, 3 and 2. The dawn is 640 px wide or 1.4 times the band's width, whichever is less, and 0.4 times as tall as it is wide, so the largest disc's radius is 256 px at desktop width and the band's sides cut it on a phone. It is decoration, hidden from assistive technology, and nothing is ever drawn over it: at every width the band keeps 40 px between the buttons and the top of the largest disc. Above the line of facts the band keeps 88 px, 56 below 900 px; the tagline is at most 760 px wide and the lead 640; 20 px separate the facts, the tagline and the lead, and 28 the lead and the buttons.
- **The bar above it.** On the current theme's page colour, 68 px tall, with a hairline under it: the lockup on the left, 30 px tall, the one for light or for dark backgrounds as the theme says; on the right the links to the page's sections and to the documentation, at 14 px and weight 600 in the secondary text colour, then the repository's link drawn with the strong border. Below 900 px the lockup and the repository's link remain.
- **One night as a timeline.** The first section after the band tells one run as a vertical timeline, at most 860 px wide. Each moment is a row of three columns, 88 px, 24 px and the rest, 16 px apart, with 36 px between two moments: its time in JetBrains Mono at 20 px in the accent as text, right-aligned, with a word for the part of the day (evening, night, breakfast) under it where that helps, in the muted colour; a ring in the accent as a shape on a 2 px line in the hairline colour, which joins the moments; then a heading at 20 px and one or two sentences in the secondary text colour. The moment a question is asked carries a card that redraws the question as the ticket shows it: its kind and round in JetBrains Mono, the question, its options one per row with the recommended one on the raised colour and tagged, and the line that says how to reply. The last moment, the pull request, takes success: its ring is filled with it, and a small card gives `✓`, the pull request's number in JetBrains Mono and its title. Below 900 px the time column narrows to 64 px and its figures go from 20 to 16 px; below 560 px an option's "recommended" tag sits under its label, not beside it.
- **The sections after it.** Each keeps 72 px above and below its content, 48 below 900 px, and opens with a title at 32 px and, where it has one, a lead at 20 px, at most 720 px wide; 40 px separate them from what follows. Cards follow the web UI's grid: 12 px between the cards of a row, 20 px inside a card. *What's different* is three cards in a row, each a number in JetBrains Mono at 13 px in the accent as text, a heading at 20 px and a paragraph. *It stays yours* sits on the surface colour between two hairlines, the page's one change of ground after the band: four facts in two columns, with no card, beside one card on the page colour that says what Owlshift is not. *Install* is two blocks of commands side by side, in JetBrains Mono at 13 px on the raised colour, whose long lines wrap and never scroll or clip; then three cards that are links, one per tracker, each ending with its link's words in the link colour. The foot holds the `{o,o}` signature with its eyes in the accent as text, the repository's links, the licence, the theme switch (Dark, Light, Auto, as the web UI's layout names them, above) and one line in the voice.
- **Widths.** The content is 1120 px wide at most, with 24 px at each side. Below 900 px every row of cards and every pair of columns becomes one column, the tagline goes from 48 to 32 px and a section's title from 32 to 24. The page is checked at 1280 and at 390 px.
- **Nothing moves.** The first version has no animation and no content that waits for a script: the page is whole without one.
- **The voice.** Section 2 applies. The page is several messages, not one: the tagline stands apart, as on the README and the social preview, and each part of the page, and each moment of the timeline, takes one owl touch at most. Security, confinement, credentials and the licence are stated soberly.
- **Both themes, every time,** as for the web UI: every colour outside the band comes from a role of section 4 or from the accent's two pairs above, and a mock-up or a change to the page shows both themes.

## 7. Decisions

Made with the maintainer on 2026-10-04 (OWL-162), from mock-ups compared side by side.

| Subject | Decision | Rejected |
| --- | --- | --- |
| The mark | The `{o,o}` terminal owl, in its base form | Among four directions: a geometric owl's head, which read as a cat; two eyes in a crescent moon, which vanish at 16 px; a perched mascot, too detailed for a favicon. Among the terminal owl's variants: V-shaped brows, parentheses for the head, a cursor for the beak, one eye open |
| Personality | Warm and playful, the watch owl | A calm, sober watcher, whose navy, gold and cream were judged neither original nor memorable; a raw, almost monochrome tool, judged cold |
| Palette | Dawn: aubergine and apricot | A mid violet, on which the mark lost its contrast; Phosphor, a night-vision green whose brand colour merges with `✓`; Tawny, a brown and orange close to Rust's own brand, whose orange reads as a warning; Electric midnight, an ink and chartreuse unusable on light backgrounds |
| Typefaces | Bricolage Grotesque with JetBrains Mono | Nunito, common and childish at large sizes; Recursive, one family for sans and mono but a heavier file; Figtree, safe and forgettable |
| Voice | The owl speaks in every text, with the four rules of section 2 | A measured wink limited to calm moments; a voice-free product with only a visual personality |
| Social preview | The lockup and the tagline over a dawn rising from the bottom edge (section 6) | Without the dawn, the lockup and the tagline alone; a larger dawn pushing the text up |
| Terminal colour | The 16 ANSI colours for states, the brand only in the signature | The brand's own hues for states, unreadable on a light terminal |
| Where it lives | This document and the source files in `assets/brand/`; a surface's tokens in code arrive with the surface | A `tokens.json` read by a build script from today, tooling written before any reader exists; a brand book published outside the repository, out of review |
| The web UI's layout (2026-11-16) | A side bar and a top bar, figures before lists, a stepper on the ticket page, bar charts for spend, both themes from section 4's roles (section 6, "The local web UI's layout") | The look P8 delivered, every block a titled box of sentences: after days of daily use the maintainer judged it archaic, with lists of identifiers where a count was wanted and grids of zeros when the drain was idle. For the chart's ordinary bars, a softer apricot: it fell under 3:1 on white |
| The public site's layout (2026-11-16) | "The night": the tagline over the social preview's dawn on a band dark in both themes, then one night of one ticket as a timeline (section 6, "The public site's layout") | Two other directions for the first screen and the way a run is shown, the rest of the page being the same in all three. "The ticket": the text on the left and a ticket's thread on the right (the question, the reply, the pull request), then the night as four cards; it puts the question loop on the first screen and leaves the dawn out. "The terminal": the install commands and a terminal's capture, then the seven stages as a stepper; it shows lines in the CLI's voice, which the CLI does not print before OWL-163 |
