# beavy brand

beavy's identity has two parts. The icon is a pixel beaver with an Ember tail. The wordmark is a precise monoline "beavy". Around them are a near-black ground, off-white lines, a single orange accent, and a lot of empty space. The wordmark keeps the identity credible next to serious infrastructure tools. The beaver is the cute part, and it looks the same in a README, a favicon, and a terminal.

> [!NOTE]
> The pixel beaver is a first drawing, adopted as it was first made for the terminal. It will be polished, and possibly redrawn, later. Until then, use the files in this repository rather than redrawing it yourself.

All artwork is hand-built SVG in [`assets/brand/`](../assets/brand/). Every file comes in an `on-dark` version (Birch, for dark backgrounds) and an `on-light` version (Ink, for light backgrounds). Backgrounds are transparent.

| File | Use |
| --- | --- |
| `beavy-icon-on-{dark,light}.svg` | The icon: favicons, avatars, app icons, anywhere square and small |
| `beavy-wordmark-on-{dark,light}.svg` | The name alone, when the icon already appears nearby |
| `beavy-lockup-on-{dark,light}.svg` | Icon and wordmark together: README banner, docs header, slides |

<p>
  <img src="../assets/brand/beavy-icon-on-light.svg" width="64" alt="beavy icon">
  &nbsp;
  <img src="../assets/brand/beavy-lockup-on-light.svg" height="56" alt="beavy lockup">
</p>

## The icon

The icon is the terminal beaver drawn as square pixels. It is 18 by 9 pixels, faces right, and has an ear, an eye, a buck tooth, and a flat tail. In the terminal the tail is drawn with shade characters (`▒`). In the SVG the tail is solid Ember, because a checker pattern turns to mud at small sizes. Each pixel is an axis-aligned rectangle on a whole-unit grid.

The square icon puts the beaver on a 20 by 20 pixel canvas with one pixel of padding at each side, and centers it vertically. It uses `shape-rendering="crispEdges"`, so browsers snap the pixels instead of blurring them. At 16 px the eye and tooth drop out, and the silhouette and Ember tail carry the icon. From 32 px up every feature shows.

## Palette

| Name | Hex | Role |
| --- | --- | --- |
| Ink | `#12110F` | Background on dark surfaces; the beaver and the wordmark on light surfaces |
| Birch | `#F1ECE2` | The beaver and the wordmark on dark surfaces; background on light surfaces |
| Ember | `#E8590C` | The only accent. The beaver's tail, and at most one highlight per view |
| Ash | `#8A857D` | Secondary text and rules on Ink only. Never in the icon or the wordmark |

Contrast, measured with the WCAG 2 formula:

| Pair | Ratio | Allowed for |
| --- | --- | --- |
| Birch on Ink | 16.0:1 | Any text, any shape |
| Ember on Ink | 5.3:1 | Text and graphics |
| Ember on Birch | 3.0:1 | Graphics only (the tail, icons), never body text |
| Ash on Ink | 5.1:1 | Secondary text |

Ember is the only warm color. If a layout seems to need a second accent, use Ash or more space instead.

## Type

The wordmark is not set in a font. It is drawn letter by letter from circles and straight strokes, 4 units wide with round caps and joins. Never retype "beavy" in a typeface in place of the wordmark file.

Everything around the logo is set in **Geist** for text and **Geist Mono** for code, commands, and terminal output. Both are by Vercel and released under the [SIL Open Font License 1.1](https://github.com/vercel/geist-font), so they are free to use, embed, and ship, including commercially. Geist is precise without being cold, and Geist Mono matches the Vercel CLI guidelines that beavy's CLI follows. When Geist is unavailable, fall back to the system UI stack (`system-ui, sans-serif`) and `ui-monospace, monospace`.

The name is always lowercase: beavy, also at the start of a sentence and in titles.

## Unit, spacing, and size

**One unit.** In the lockup, one beaver pixel and one wordmark stroke are both 4 units, so the pixel art and the line art share a weight. Scale artwork uniformly. Never change `stroke-width` or pixel sizes, and never resample the beaver at a fractional size of its own grid in pixel-art contexts (sprites, terminals, game-style UI).

**Clear space.** Keep at least the beaver's height empty on every side of the lockup, and two pixels of the icon's canvas around the square icon. Nothing else (text, edges, other logos) enters that space.

**Lockup geometry is fixed.** The beaver stands on the wordmark's baseline. Its top sits within one stroke width of the wordmark's ascender, and the gap between them is part of the drawing. Use the lockup file rather than placing the icon and wordmark by hand.

**Minimum sizes.**

| Asset | Smallest size |
| --- | --- |
| Icon | 16 px square |
| Lockup | 24 px tall |
| Wordmark alone | 14 px tall |

Below 16 px, use the wordmark in text instead of the icon.

## Do and don't

Do:

- Put the logo on Ink or Birch, or on a plain surface close to one of them, using the matching `on-dark` or `on-light` file.
- Keep Ember on the tail only, and keep the rest of the beaver and the wordmark in the line color.
- Leave generous space. When in doubt, make the logo smaller and the margins larger.
- Use the lockup for first impressions and the icon alone once the name is already on screen.

Don't:

- Smooth, outline, round, or anti-alias the beaver's pixels, or redraw it in another style.
- Flip or rotate the beaver. It faces right, toward the content.
- Recolor the body Ember, or the tail anything but Ember.
- Stretch it, add shadows or gradients, or put it in a container shape it doesn't come with.
- Place it on photos, patterns, or mid-tone backgrounds where neither version reaches the contrast above.
- Capitalize the name or set it in another typeface as a logo.

## The terminal beaver

In the terminal the icon appears as text: the same beaver, 5 lines by 18 columns, for human moments on a TTY such as a finished build or a first successful run. It is decoration and carries no information, so removing it never loses anything.

Unicode, built from block elements (U+2580 to U+259F). The shaded cells are the tail's texture:

```text
       ▄▄▄▄ ██▄▄▄
     ▄███████▀███▄
 ▄▄▄▄████████████▀
▒▒▒▒▒▒█████████▀▄
 ▀▀▀▀ ▀▀  ▀▀  ▀▀
```

ASCII fallback, printable ASCII only, same footprint:

```text
        _   __
     .-' '-'  '-.
 .--/        o  '.
(###|          __>
 '--' 'nn---nn'U
```

With color enabled, the body uses the terminal's default foreground and the tail cells (`▒` or `#`) may use Ember (truecolor `232;89;12`, or ANSI 256 color 166 when truecolor is unavailable). With color disabled, print the same glyphs in the default foreground.

A finished build might look like this on stderr, with the receipt carrying all the information and the beaver beside it:

```text
       ▄▄▄▄ ██▄▄▄
     ▄███████▀███▄    ✓ Built 42 packages in 3.1s
 ▄▄▄▄████████████▀      38 from cache, 4 rebuilt
▒▒▒▒▒▒█████████▀▄
 ▀▀▀▀ ▀▀  ▀▀  ▀▀
```

### When the beaver may appear

beavy's CLI follows the [Vercel CLI Design Guidelines](https://vercel.com/design/cli): stdout is the answer, everything else goes to stderr, and machine formats are exact contracts. The beaver is "everything else", so these rules apply to every command:

1. The beaver is written to stderr only. It never appears on stdout, in any command, in any mode.
2. It appears only when stderr is a terminal. When stderr is redirected or piped, it is never printed.
3. It never appears when `--json` or any other machine format is selected, when quiet output is requested, or when the `CI` environment variable is set.
4. It never appears in errors, prompts, help, or progress output. Those must stay short and actionable.
5. It appears at most once per command, after the outcome is known and only for a successful human moment. It never replaces the receipt line or delays the exit.
6. It never animates and never asks for input.
7. Use the ASCII form when `TERM` is `dumb` or the locale isn't UTF-8 (no `UTF-8` in `LC_ALL`, `LC_CTYPE`, or `LANG`). Drop color, but keep the glyphs, when `NO_COLOR` is set or color is turned off.
8. Skip it when the terminal is narrower than the beaver plus the message beside it. Never wrap or truncate the drawing.
9. Users can turn it off for good with a setting. That setting changes nothing else about the output.
