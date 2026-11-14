# MuReader

- `mureader` — a small standalone MuPDF document reader built on MuPDF and winit, with an integrated, deliberately minimal Wayland clipboard module.

## Build

The project uses the current stable Rust toolchain.

```bash
cargo build --release
```

Or install MuReader:

```bash
cargo install --git https://github.com/ningxilai/MuReader.git
```

MuPDF is built from `ningxilai/mupdf-rs` at a pinned revision
(`501076c4015a98068db57c45591ee3a068e8818c`), which carries the reflow layout
fixes used by MuReader. Pinning the revision and committing `Cargo.lock` keeps
builds reproducible. This keeps the MuPDF C changes in the fork instead of
vendoring the whole C source tree in this repository. Cargo fetches the fork
and its MuPDF submodules during the first build.

## Test

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Run

```bash
target/release/mureader [FILE]
```

With no arguments, MuReader asks the XDG Desktop Portal for a document. Pass a
file path to open it directly. Either way the process detaches from the
invoking shell; set `MUREADER_FOREGROUND=1` to keep it attached. The window
provides page navigation, page/book search, outline, zoom, rotation, text
selection/copy, and mouse-wheel scrolling. MuPDF rasterizes pages on the
worker thread; the window uploads the finished bitmaps as textures and
composites pages, highlights, selection, outline, menus, and prompts with
wgpu.

Navigation uses the table of contents MuPDF provides for PDF, EPUB, HTML,
FB2, and other structured documents. Documents without a table of contents,
including styleless plain-text sources, use a page-number outline.
The outline panel takes a bounded share of the window (about 15%, clamped to
232–360 px) so it stays usable on narrow and wide windows alike.
Moving in the outline previews the highlighted page; Enter keeps it, and
Escape restores the page that was visible before the outline opened.

Supported inputs depend on MuPDF: PDF, EPUB, MOBI, FB2, XPS/OXPS, CBZ, SVG,
HTML, Office formats such as DOCX/XLSX/PPTX, and common images. `txt` and
`text` files use the plain-text pretty layout described below. Markdown,
reStructuredText, Org, AsciiDoc, and LaTeX are not supported because MuPDF has
no handler for them; log files are excluded as well because they are machine
output rather than readable documents. Showing any of these as raw text would
be misleading.
`Reset view` (`0`) discards an explicit zoom and fits the page again.

MuReader creates one native window and needs no compositor socket. On Wayland
it requests focus through `xdg_activation_v1` when `XDG_ACTIVATION_TOKEN` is
present.

## Reading progress

MuReader remembers the last page of every document it opens and returns to it
on the next launch. Progress lives in `$XDG_STATE_HOME/mureader/progress`
(usually `~/.local/state/mureader/progress`). `MUREADER_PROGRESS` overrides
that path, and a relative `XDG_STATE_HOME` is ignored as required by the XDG
Base Directory specification.

The file uses viminfo-style file marks, newest first. Paths are the last field,
so spaces need no quoting:

```text
# MuReader reading progress.
# Written automatically; lines starting with ' are file marks stored
# newest first: '<page> <path>.
'42 /home/user/Documents/book.pdf
'7 /home/user/Documents/notes.epub
```

Only settled pages are recorded: outline previews do not overwrite progress
until they are committed. The file is rewritten atomically with mode `0600`.
`progress.max_documents` controls how many documents are remembered (default
`100`); the oldest entry is dropped when the limit is reached, and `0` disables
reading progress.

## Configuration

The default config file is `~/.config/mureader/config.toml`; set
`MUREADER_CONFIG` to use another path. `MUREADER_FOREGROUND=1` keeps the
process attached to the terminal for debugging.

```toml
[animation]
page_turn_ms = 320
window_fit_ms = 120

[reflowable]
pretty = true

[progress]
max_documents = 100

[keys]
quit = ["q", "Q", "Escape"]
next_page = "n"
prev_page = "p"
back = ["M-Left", "M-h"]
forward = ["M-Right", "M-l"]
outline = "o"
search_page = "C-s"
search_book = "M-s"
zoom_in = "+"
zoom_out = "-"
font_increase = "C-="
font_decrease = "C--"
reset_view = "0"
toggle_spread = "s"
rotate_right = "r"
rotate_left = "l"
copy = ["M-w", "C-w"]
```

`page_turn_ms` controls the page-slide duration. `window_fit_ms` debounces
re-layout after a window resize: the existing page is scaled to fit the new
window immediately, then re-rendered after the delay. Set it to `0` for the
fastest response (one frame).

The reader starts in a two-page spread whenever the window fits a comfortable
pair. `toggle_spread` switches between that paged spread and a single-column
continuous scroll. The choice lasts until it is toggled again, while a narrow
window that cannot fit a comfortable pair uses continuous scroll automatically.
Page numbers are the shared coordinate: switching modes and following outline,
search, or link destinations lands at the start of that page, and while
scrolling the title and progress report the page at the top of the viewport.
Continuous scroll is a sliding page window rather than a sequence of page
jumps: the rendered window prefetches in the direction of travel, input that
runs past it is replayed when new pages arrive, the viewport never snaps back
to a worker focus page, and one scale stays locked for the session so pages of
different sizes cannot re-zoom mid-scroll. The column also keeps side margins
in every window geometry, including narrow windows and while the outline is
open. Reflowable pages are cropped at a shared page-margin baseline measured
across the roll, so the simulated page chrome never appears between paragraphs,
lines, or illustrations: the story reads as one continuous roll with no
page-break gaps. The document's real spacing survives at a seam because a
second, unpaginated layout measures the true distance between the blocks
pagination split, including paragraph margins and the space around
illustrations; a new chapter keeps its breathing room instead of butting
against the previous text.
Arrow keys move one line at a time, matching a mouse-wheel notch, and the step
follows the line spacing the document's CSS asks for, while `Space`/`PageDown`
and `PageUp` move most of a screen. Key steps and wheel notches glide smoothly
over a few frames, following the same idea as CSSOM View's
`scroll-behavior: smooth` and EPUB's scrolled-continuous flow, so holding an
arrow key carries the column forward instead of stepping it page by page.
Trackpad pixel deltas stay direct. The glide uses a Torricelli-like decay:
the remaining distance is the finite volume of a deformed Gabriel's horn, so
the movement starts at once and then thins into a long, gentle tail instead of
stopping abruptly. Inertia is spent only against the preloaded strip and never
travels more than one viewport height, so it cannot outrun the render buffer or
throw the eye past the text it was following. Paged spreads disable pixel
scrolling entirely: vertical wheel and trackpad motion is ignored there,
while a horizontal swipe turns exactly one page per gesture; holding Shift
turns a vertical wheel into that sideways gesture. Key and screen steps turn
whole pages.
Continuous scroll works for fixed-layout documents such as PDFs as well
as reflowable ones. In a reflowable document, a page that holds only an
illustration is drawn as the illustration itself: MuPDF still lays it out on
its simulated page, but the reader crops to the picture and renders it at the
text flow's scale, keeping the picture's actual bitmap size and aspect ratio.
A tall plate therefore flows past the viewport instead of sitting inside a
page-shaped frame with white margins, and a small bitmap is not blown up into
a blurry column-width plate. A page that carries an illustration together with
text keeps that text in the same flow. Paged spreads, quarter-turned pages, and
an explicit zoom keep the whole page.

### Sidecar stylesheets

Per-document styling is not configured in `config.toml`. MuReader hashes the
document bytes with SHA-256 and uses:

```text
$XDG_DATA_HOME/mureader/css/<name>-<sha256>.css
```

`MUREADER_CSS_DIR` overrides that directory. For EPUB, `<name>` is the title
embedded in the book's own metadata, not the file name. Other reflowable
formats use the file stem. The lookup applies to EPUB, HTML/XHTML, FB2, MOBI,
DOCX/XLSX/PPTX/HWPX, and TXT/text files.

On the first open of a reflowable document, MuReader creates the sidecar from
the legacy `[reflowable] css`/`css_file` values and `[perfect_palette]`
settings, sanitizes it, and writes it out. The file then belongs to you: it is
authoritative on every later open, and changes to `config.toml` no longer
affect that document. Edit the sidecar directly; delete it to regenerate it
from the current config on the next open.

By default the sidecar overlays the document's own CSS, so use `!important`
where the book should be overridden. To replace the document CSS instead, make
this the first line of the sidecar:

```css
/* mureader: replace-document-css */
```

A sidecar that contains only comments is a placeholder: no user CSS is applied
and the document's own CSS stays enabled. An unreadable, non-UTF-8, or
oversized (over 4 MiB) sidecar is reported and the document's own CSS is used;
the legacy config CSS is never a fallback once the sidecar model is active.
The plain-text pretty layout always keeps document CSS enabled because its
generated HTML carries the required page layout.

The sidecar is parsed with a CSS syntax parser and sanitized before MuPDF sees
it. It keeps `@font-face`, `@page`, the properties MuPDF implements, and
`--mureader-*` custom properties; other at-rules, `var()`, `calc()`,
unsupported properties, unbounded numbers, and absolute or parent-escaping
URLs are dropped. Rule order is preserved. Font and image URLs must stay
relative to the document archive. The first `font-family` in the sidecar is
also used by the outline, command menu, and other UI text; CJK, emoji, and
symbol fallback are shared. `pretty` enables the Unicode line-breaking pass
for TXT and text files.

`[perfect_palette]` is an optional HPLuv eye-comfort palette ported from the
`perfect-palette` helper in the author's Emacs configuration. It is a
migration input: on first open it tunes the theme implied by the legacy CSS
and its rules are written into the generated sidecar. Every generated color is
available as `--mureader-palette-0` through `--mureader-palette-(colors-1)`.
After migration, edit the concrete colors in the sidecar instead of tuning the
palette.

The sidecar supplies one theme for every non-reading surface: the outline,
command panel, prompts, loading card, borders, and search/selection highlights.
The effective `background`/`background-color` and `color` on `body`, `html`, or
`:root` set the base surface and text, and panel shades, muted labels, chips,
and separators are derived from them. Optional `--mureader-*` custom
properties override individual parts:

```css
:root {
    --mureader-background: #1b1b1b;
    --mureader-surface: #242424;
    --mureader-text: #e8e2d8;
    --mureader-muted: #b8b0a4;
    --mureader-accent: #d7a45e;
    --mureader-border: #3d3f45;
    --mureader-search: #ffe61e;
    --mureader-selection: #5078d2;
    --mureader-selected-text: #ffffff;
}
```

If the sidecar contains no colors, the UI follows the document: it starts from
the HTML default canvas (a white page with dark text) and derives one palette
from the whole book. Pages spread evenly through the document are sampled at a
small preview size and their canvas colors are joined with a Lagrange
interpolation over page position. The color is read through a soft window
around the middle of the book, and the target sample count grows continuously
with the book length, blending adjacent counts by the fractional part, so
re-layouts and page-count changes do not make the palette jump. The result is
kept for the whole book; a cover, an embedded image, or a PDF page with
different colors therefore cannot flip the shell while reading. The
interpolation itself lives in a small module that knows nothing about themes or
the optional eye-comfort palette — consumers decide whether to derive the UI
theme from the color, feed it to the palette, or only tint the outline. A
sidecar that only sets `font-family` matches the book instead of forcing a
dark shell. A light background automatically derives dark text and a darker
selected-text color, keeping outline and menu selections readable without
adding a hue.

Keys are space-separated sequences; `C-`, `M-`, and `S-` are Control, Alt, and
Shift. Bindings replace the built-in action, and an empty array disables it.
Press `:` for the command panel. Each action is shown next to its effective
binding from `[keys]`, including combinations such as `C-s`, `M-s`, `C-=`, and
`M-w`. Press a complete binding to run it, or use the arrow keys and `Enter`.
Multi-key sequences wait for the next key and list their continuations, like
which-key; `Backspace` removes the last key, and `Esc` cancels the pending
sequence or closes the panel when no sequence is pending. Repeatable actions
(page turns, zoom, rotation, text size) keep the panel open and echo the last
action at the bottom; `q` or `Esc` closes it. Outline, search, and quit close
the panel because they change mode. For reflowable documents, `C-=` and `C--`
change the base text size; the document is reflowed and the current reading
position is kept. `0` re-fits the page after zooming, page turns, or window
resizing. Multi-key actions behave like hydra: after a continuation runs, the
shared prefix stays armed, so pressing another continuation works without
retyping it. `Esc` or `Backspace` drops the prefix.

Following an internal link or committing an outline entry records the page you
came from. `Back` and `Forward` move through those destinations, while ordinary
page turns and scrolling stay out of the history. External `http`, `https`, and
`mailto` links are opened with the desktop's default handler; other schemes are
reported in the title instead of being launched. `Back` is bound to `M-Left`
and `M-h`, `Forward` to `M-Right` and `M-l`.

## Changelog

### Unreleased

- Per-document styling now lives in an auto-generated sidecar at
  `$XDG_DATA_HOME/mureader/css/<name>-<sha256>.css`, keyed by the document's
  SHA-256. EPUB uses its embedded title for the readable part of the name;
  other formats use the file stem. On first open it is seeded from the legacy
  `[reflowable]` CSS and
  `[perfect_palette]`, sanitized with `cssparser` to the at-rules, properties,
  and URL forms MuPDF actually supports, and then owned by the user. Generated
  sidecars overlay the document CSS by default; a first-line
  `/* mureader: replace-document-css */` marker replaces it. `config.toml` no
  longer carries styling or palette options. UI theme and font extraction share
  the same parser instead of hand-written CSS scanning.
- Reading-model ownership now lives in the renderer. The reader sends only the
  user's `LayoutIntent` (`Auto` or `Scroll`); the worker resolves it against
  the current window and reports the effective `Layout` with every rendered
  strip. Input, geometry, and the status line follow that reported layout, so a
  resize cannot leave the UI driving a model that no longer matches the bitmap.
- A paged spread keeps its slide animation when both visible pages are
  illustrations. Blank reflow pages used to shorten the rendered strip to the
  two visible pages, leaving no lookahead for the turn; the spread now scans
  past blank pages until it holds four non-blank pages. A window too narrow
  for a comfortable pair now reliably falls back to continuous scroll instead
  of keeping a cramped single paged column, and that decision no longer
  depends on how many pages the strip happens to contain.
- Paged spreads no longer turn pages from vertical wheel or trackpad motion.
  A paged spread has nothing to scroll, so vertical input is ignored and a
  horizontal swipe turns exactly one page per gesture; key and screen steps
  still turn whole pages. The previous scroll-distance quantizer could make
  two short flicks flip one page, which read as scrolling rather than paging.
- Continuous scroll now glides between positions at pixel granularity instead
  of teleporting on each key press. Wheel notches and `Down`/`Up`, `Space`,
  `PageUp`/`PageDown`, and `C-n`/`C-p` feed one incremental decay curve, in
  the spirit of CSSOM View's `scroll-behavior: smooth` and EPUB's
  `rendition:flow: scrolled-continuous`. The curve is a deformed Torricelli
  trumpet: remaining distance falls as `1 / (1 + t/tau)^2`, giving the old
  ease-out's immediate first pixels a longer, thinner tail so the eye can
  follow moving text instead of being caught by a hard stop. Repeated input
  extends the same glide, high-resolution trackpad deltas remain direct, and a
  prefetched window can re-center underneath the glide without a visual jump.
  Inertia is computed from the existing preload buffer and capped to the
  rendered strip and one viewport height, so it cannot spend motion on pages
  that have not arrived yet. Paged spreads disable pixel scrolling entirely:
  vertical wheel and trackpad motion is ignored there, while a horizontal
  swipe turns one page per gesture and key/screen steps still turn whole
  pages.
- Continuous scroll now renders reflowable pages as one unbroken roll instead
  of a stack of page-shaped blocks. The roll learns the simulated page margin
  shared by the document's pages and crops every page at that baseline, so
  fully blank pages disappear and adjacent pages carry no page chrome or
  strip gap. The document's real spacing is restored at every seam: a second,
  unpaginated layout measures the true distance between the blocks that
  pagination split, so paragraph margins and the space around illustrations
  survive text-to-text, text-to-image, and image-to-text seams instead of the
  two pages being stitched together. A chapter heading keeps its space instead
  of butting against the previous chapter. Page numbers remain the shared
  coordinate with spread mode, outline jumps, and links. Pixel-level
  regression tests check that no blank band appears at a normal seam, that a
  chapter margin is preserved, and that paragraph and image margins return at
  both text and mixed text/image seams.
- Arrow keys now move the continuous scroll by one line instead of nearly a
  full screen. `Down`/`Up` and the mouse wheel share a single-line step, and
  the step is measured from the line spacing the document's CSS produces
  rather than a fixed pixel guess, so it follows the current text size and
  zoom. `Space`, `PageUp`/`PageDown`, and `C-n`/`C-p` keep the larger screen
  step, so holding an arrow key glides through the column instead of jumping
  page-sized distances.
- Standalone illustrations in reflowable documents no longer sit on MuPDF's
  simulated 612x792 reflow page. In continuous scroll the reader crops such a
  page to its images and renders them at the text flow's scale with their
  actual bitmap size and aspect ratio, so tall plates flow through the viewport
  instead of being framed by page margins and small bitmaps stay sharp instead
  of being upscaled to the column. Captioned pages now flow with the same roll;
  paged spreads, quarter-turned pages, and explicit zoom still render whole
  pages.
- Reflow images are atomic at page breaks: an image that does not fit the
  space left on a page moves to the next page at its measured size instead of
  being shrunk into the remainder. Two adjacent pictures split by pagination
  keep one scale, so a spread no longer shows one large and one small plate,
  and continuous scroll does not resize a picture as it slides into view. An
  image taller than a page is capped to the page once and placed on a fresh
  page.
- Continuous scroll now slides a prefetched page window instead of jumping
  between fixed strips. Scroll input that runs past the rendered window is kept
  and replayed when more pages arrive, the direction of travel decides how much
  lookahead is kept, a stale strip can no longer re-base the viewport onto its
  focus page, the column locks one scale for the whole scroll session so mixed
  page sizes cannot re-zoom mid-scroll, and it always reserves side margins,
  even in narrow windows and with the outline open.
- The outline, command panel, prompts, loading card, and search/selection
  highlights now share one CSS-derived theme. `body`/`html`/`:root` background
  and color supply the base, with `--mureader-*` custom properties for
  per-surface overrides; when the CSS names no colors the theme is derived once
  from pages sampled across the whole book and Lagrange-interpolated in page
  order over a soft window near the middle, with the sample budget easing
  continuously with the book length, starting from the HTML default white
  canvas, so embedded images and PDFs keep a single palette for the whole book.
  The sampler is isolated from the optional eye-comfort palette and only
  supplies a color that the palette may consume when enabled.
- Added an optional HPLuv `perfect-palette` eye-comfort theme. It derives the
  page paper, ink, accent, search, and selection colors from the existing
  window, text, and accent hue family, and the UI follows the tuned theme.
  Every parameter is configurable, the feature is off by default, and when
  disabled the original CSS and colors are left untouched.
- Unified internal and external link handling: internal links and committed
  outline entries share one navigation path with Back/Forward history, and
  external links open in the desktop handler.
- The command panel now behaves like hydra for multi-key actions, keeping the
  shared prefix armed so repeated continuations need no retyping.
- Loading a large document uses a larger, higher-contrast card and starts
  opening the file before GPU initialization completes.
- Reworked the `:` command menu into a shortcut-driven action panel that keeps
  repeatable actions open and echoes the last action.
- The `:` panel now reads the effective `[keys]` bindings, displays
  combinations such as `C-s` and `M-w`, and handles multi-key sequences like
  which-key: prefixes list their continuations, `Backspace` backs out, and
  `Esc` cancels the pending sequence.
- The window now appears after its first frame and shows a loading panel, so
  startup no longer flashes an uninitialized black surface.
- The `:` panel is a compact shortcut palette: key chips, an accent rail on
  the selected action, a scroll indicator, and a two-column grid while a
  multi-key prefix is pending.
- Outline selection brightens the selected title to white and bakes a faint
  page-tinted veil plus a left rail into the panel bitmap; the text padding
  keeps the rail clear of the glyphs, and the tint follows the book's sampled
  background. Previews stay debounced and are skipped for oversized zoomed
  pages.
- Two-page reading keeps a proportional central gutter that never collapses,
  and falls back to single-column continuous scroll once each spread page would
  be too narrow. Press `s` (configurable as `toggle_spread`) to switch between
  the spread and continuous scroll. Page keys become page jumps in scroll mode,
  while `Down`/`Up` move one line, `Space`, `PageUp`/`PageDown`, `C-n`/`C-p`
  move most of a screen, and the wheel scrolls continuously. The outline
  overlays the page at full-window layout scale, so opening or closing it no
  longer triggers a page re-render or layout hitch.
- Renamed the preview application to **MuReader** (`mureader`).
- Merged the Wayland data-control clipboard client into MuReader's clipboard
  module and reduced it to the copy path.
- Removed the `j`/`k` vertical-scroll convenience bindings to match the
  horizontal-scroll key cleanup; scrolling remains on arrows, page keys, and
  `C-n`/`C-p`.
- Added the Vim-style `:` command menu, CSS-driven UI/document fonts, and a
  configurable page-turn duration.
- Added reflowable text-size controls (`C-=`/`C--`) that reflow the document and
  preserve the current reading position.
- Moved MuPDF C patches to the `ningxilai/mupdf-rs` fork and removed the
  vendored `mupdf-sys` tree from this repository.
- Fixed spread/keyboard interaction regressions and smoothed two-page turns.

## License

MuReader links `mupdf-rs` and is licensed under AGPL-3.0.
