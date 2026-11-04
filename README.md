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
selection/copy, and mouse-wheel scrolling.

Navigation uses the table of contents MuPDF provides for PDF, EPUB, HTML,
FB2, and other structured documents. Documents without a table of contents,
including styleless plain-text and markup sources, use a page-number outline.
Moving in the outline previews the highlighted page; Enter keeps it, and
Escape restores the page that was visible before the outline opened.

Supported inputs depend on MuPDF: PDF, EPUB, MOBI, FB2, XPS/OXPS, CBZ, SVG,
HTML, Office formats such as DOCX/XLSX/PPTX, and common images. `txt`, `text`,
`md`/`markdown`/`mdown`/`mkd`, `rst`, `org`, `adoc`/`asciidoc`, `tex`, and
`log` files can also use the plain-text pretty layout described below.
`Reset view` (`0`) discards an explicit zoom and fits the page again.

MuReader creates one native window and needs no compositor socket. On Wayland
it requests focus through `xdg_activation_v1` when `XDG_ACTIVATION_TOKEN` is
present.

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

[keys]
quit = ["q", "Q", "Escape"]
next_page = "n"
prev_page = "p"
outline = "o"
search_page = "C-s"
search_book = "M-s"
zoom_in = "+"
zoom_out = "-"
font_increase = "C-="
font_decrease = "C--"
reset_view = "0"
rotate_right = "r"
rotate_left = "l"
copy = ["M-w", "C-w"]
```

`page_turn_ms` controls the page-slide duration. `window_fit_ms` debounces
re-layout after a window resize: the existing page is scaled to fit the new
window immediately, then re-rendered after the delay. Set it to `0` for the
fastest response (one frame).

`[reflowable]` CSS is applied to reflowable documents. The first `font-family`
declared there is also used by the outline, command menu, and other UI text;
CJK, emoji, and symbol fallback are shared. `pretty` enables the Unicode
line-breaking pass for TXT, Markdown, reStructuredText, Org, AsciiDoc, LaTeX,
and log files.

Keys are space-separated sequences; `C-`, `M-`, and `S-` are Control, Alt, and
Shift. Bindings replace the built-in action, and an empty array disables it.
Press `:` for the command menu. While it is open, the page is frozen and normal
shortcuts are disabled. For reflowable documents, `C-=` and `C--` change the base
text size; the document is reflowed and the current reading position is kept.
`0` re-fits the page after zooming, page turns, or window resizing.

## Changelog

### Unreleased

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
