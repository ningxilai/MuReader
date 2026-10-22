# MuReader

- `mureader` — a small standalone MuPDF document reader built on MuPDF and winit, with an integrated, deliberately minimal Wayland clipboard module.

## Build

The project uses the current stable Rust toolchain.

```bash
cargo build --release
```

Or install MuReader:

```bash
cargo install --git https://github.com/ningxilai/emthin.git
```

The repository vendors `mupdf-sys` 0.8.0 under `vendor/mupdf-sys` with a small
reflow patch that keeps oversized images inside one reflowed page.

## Test

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

## Run

```bash
target/release/mureader
```

With no arguments, MuReader asks the XDG Desktop Portal for a document, then
detaches from the invoking shell. The window provides page navigation,
page/book search, outline, zoom, rotation, text selection/copy, and mouse-wheel
scrolling.

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
goto_page = "M-g g"
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
- Fixed spread/keyboard interaction regressions and smoothed two-page turns.

## License

MuReader links `mupdf-rs` and is licensed under AGPL-3.0.
