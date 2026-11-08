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
(`ccbeff0373aba5d83eddfc9c44190c1552c4fb22`), which carries the reflow layout
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
css = ""
css_file = ""
pretty = true

[perfect_palette]
enable = false
colors = 9
step = 4
hue_start = 0.58
saturation = 45
lightness = 65
hpluv = true

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
pair. `toggle_spread` switches between that spread and a centered single page;
the choice lasts until it is toggled again, while a narrow window still falls
back to one page automatically.

`[reflowable]` CSS is applied to reflowable documents. The first `font-family`
declared there is also used by the outline, command menu, and other UI text;
CJK, emoji, and symbol fallback are shared. `pretty` enables the Unicode
line-breaking pass for TXT and text files.

`[perfect_palette]` is an optional eye-comfort palette ported from the HPLuv
`perfect-palette` helper in the author's Emacs configuration. Instead of
imposing an unrelated theme, it reads the window, text, and accent hue from
your `[reflowable]` CSS (or from the HTML default when no colors are named),
then regenerates the page paper and ink plus the UI accent, search, and
selection colors around that hue family. `hue_start` is only the fallback for
a fully neutral theme. The palette walks `colors` hues with a
`step / colors`-turn spacing at fixed `saturation` and `lightness`; every
generated color is available as `--mureader-palette-0` through
`--mureader-palette-(colors-1)`. The tuned stylesheet is appended after your
CSS, so it transforms the resulting palette, and when `enable = false` the
original CSS and colors are left exactly untouched. Light and dark themes keep
their character; `lightness` only controls the generated palette colors.

The same CSS supplies one theme for every non-reading surface: the outline,
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

If the CSS contains no colors, the UI follows the document: it starts from the
HTML default canvas (a white page with dark text) and is re-derived from the
rendered page background as pages arrive. A CSS file that only sets
`font-family` therefore matches the book instead of forcing a dark shell. A
light background automatically derives dark text and a darker selected-text
color, keeping outline and menu selections readable without adding a hue.

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

- The outline, command panel, prompts, loading card, and search/selection
  highlights now share one CSS-derived theme. `body`/`html`/`:root` background
  and color supply the base, with `--mureader-*` custom properties for
  per-surface overrides; when the CSS names no colors the theme follows the
  rendered page, starting from the HTML default white canvas.
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
  keeps the rail clear of the glyphs, and the tint follows the page background
  for CSS-styled documents. Previews stay debounced and are skipped for
  oversized zoomed pages.
- Two-page reading keeps a proportional central gutter that never collapses,
  and falls back to a single page once each spread page would be too narrow.
  Press `s` (configurable as `toggle_spread`) to switch to a centered single
  page and back. The outline overlays the page at full-window layout scale, so
  opening or closing it no longer triggers a page re-render or layout hitch.
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
