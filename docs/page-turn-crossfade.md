<!-- SPDX-License-Identifier: AGPL-3.0-only -->

# Page-turn transition: alpha cross-fade + small Gaussian

Status: implemented and enabled by default. The slide remains as the fallback
when the cross-fade cannot be drawn safely.

## 1. The old artifact

`SpreadAnimation` used to animate `spread_offset` from one spread offset to the
next. `draw` placed the whole strip at `window_left - spread_offset * scale` and
clipped it to the content window, so any glyph that crossed the clip boundary
was cut in half for the duration of the animation.

That is not a rasterization bug: it is what a clipped slide is. Foliate's
paginator does the same thing (300 ms `easeOutQuad` scroll of the column
container). The cross-fade removes the motion entirely: the text never moves,
so there is no edge clipping.

## 2. What mature implementations do

| Implementation | Transition | Old/new layers | Blur |
| --- | --- | --- | --- |
| MuPDF `fz_generate_transition` (`fitz/transition.c`) | Fade, wipe, blinds | Same-size pixmaps, per-pixel blend | No |
| Foliate paginator (`paginator.js`) | 300 ms `easeOutQuad` scroll | One scrolling column | No |
| Readest | Push / slide / curl | Slide uses View Transitions: outgoing snapshot over stationary incoming page; curl uses a WebGL mesh | No |
| KOReader core paging | Instant | n/a | No |
| Apple Books / Kindle / Moon+ | Curl or slide (closed source) | Separate page surfaces | No |

The closest precedent for the chosen effect is MuPDF's fade. Its whole
implementation is a linear per-channel cross-fade:

```c
*t++ = ((op << 8) + ((np - op) * time) + 0x80) >> 8;
```

`time` runs from 0 to 256, so at `t = 0` the output is the old pixmap and at
`t = 1` it is the new one. No translation, no clipping, no blur.

Readest's notes are also useful: its slide/curl styles need the old and new
pages as separate layers. MuReader is in a better position here: the spread
strip already contains both the old and the new spread, so the cross-fade
samples both windows from one texture and the worker does not render anything
extra.

## 3. GPU pipeline

The renderer is a single-pass textured-quad compositor with
`ALPHA_BLENDING`. The cross-fade needs no render targets and no second pass.

`Command::Texture` now carries:

```rust
Texture {
    id: u64,
    dst: Rect,          // screen rectangle for the whole source window
    src: Option<Rect>,  // bitmap-pixel window inside the strip; None = whole
    clip: Rect,
    color: [f32; 4],    // alpha carries the fade
    blur: f32,          // Gaussian radius in bitmap pixels; 0 disables
}
```

`append_texture` intersects each texture tile with `src`, maps the
intersection into `dst`, and computes UVs relative to the tile. A whole-texture
draw is `src: None` and keeps the old behaviour exactly.

The vertex layout gains `params: vec4<f32>` (`@location(3)` in, `@location(2)`
out). `params.xy` is the blur radius in tile UV units; `params.zw` is unused.

## 4. Cross-fade

### 4.1 Math

For opaque page bitmaps the exact cross-fade is:

- draw the old window with `alpha = 1`;
- draw the new window with `alpha = t`.

The blend equation is `t * new + (1 - t) * old`, exactly MuPDF's fade. Do
**not** draw both with `alpha = 1 - t` and `alpha = t`: the clear colour leaks
through and the result is `t*new + (1-t)^2*old + t(1-t)*background`.

At `t = 0` the frame is the old spread; at `t = 1` it is the new spread. The
text never moves.

### 4.2 Animation state

`SpreadAnimation` keeps `from`/`to` (bitmap offsets), `to_width` (the
destination spread width), and `progress` (the eased 0..1 value).
`tick_animation` updates `progress` and only moves `spread_offset` when the
slide fallback is in use. During a cross-fade `spread_offset` stays at `from`,
so hit-testing and `page_origin` continue to refer to the old spread.

At the end, `finish_animation` sets `spread_offset = to` and sends
`NextPage`/`PrevPage`. The next frame still shows the same window from the old
strip, and the worker's replacement strip puts the same window at its new
offset, so there is no flash.

### 4.3 Draw path

When `crossfade()` returns parameters, `draw` builds two `texture_region`
commands into the same destination rectangle:

- old: `src.x = from`, `src.w = visible_width()`, `alpha = 1`;
- new: `src.x = to`, `src.w = to_width`, `alpha = progress`.

Both use `clip = window rect`, so the old and new windows are clipped exactly
where the page window ends. Text overlays (search highlights and selection)
are skipped during the fade: they are anchored to the old spread and would sit
on top of the incoming page. Input already calls `finish_animation` before
handling a new command.

## 5. The small Gaussian

### 5.1 Kernel

The shader uses a 3×3 Gaussian with σ = 1 texel. The normalized weights are:

```text
0.07511  0.12384  0.07511
0.12384  0.20418  0.12384
0.07511  0.12384  0.07511
```

They sum to 1. `params.xy` scales the sample offsets, so the weights stay fixed
and the radius follows the envelope. At radius zero the kernel is skipped and
the shader takes the single centre sample.

`textureSampleLevel(..., 0.0)` is used instead of `textureSample` so the branch
does not depend on derivatives.

### 5.2 Envelope

Both layers are blurred with the same symmetric envelope:

```text
b(t) = 4 * t * (1 - t) * B
```

with `B = CROSSFADE_BLUR_PX = 1.5` device pixels. The radius is zero at both
ends and peaks halfway through, so the old page starts sharp, defocuses into
the double exposure, and the new page lands sharp. Blurring both layers is the
chosen "defocus dissolve"; blurring only the outgoing page would keep incoming
text readable earlier but makes the two layers feel mismatched.

The radius is converted to bitmap pixels with `1.5 / scale` (where `scale` is
`display_scale`), so the visual radius stays 1.5 device pixels when the outline
shrinks the page.

### 5.3 Quality/performance trade-off

A 3×3 kernel is nine taps per layer. At mid-transition both layers are drawn,
so the fragment shader takes 18 samples per pixel for the duration of the
turn. The two quads share one bind group and one scissor, so they merge into a
single draw call. The alternative is a 5-tap cross (centre 0.375, four axes
0.15625), which is cheaper and slightly less isotropic; for a 1-3 px radius the
difference is hard to see. The 3×3 version is used because it is the smallest
kernel that still looks isotropic and matches the "Smol Gaussian" design.

The blur averages stored sRGB bytes, not linear light. That is the same colour
arithmetic the rest of the compositor uses, and at this radius the difference
is not visible. A physically correct blur would need a linear-light pass or a
larger kernel, neither of which is worth it for a 320 ms effect.

## 6. Fallbacks

The cross-fade is only used when all of these hold:

1. **Paged layout.** Continuous scroll keeps its own path.
2. **Single texture tile.** Large strips are split into one texture per tile;
   a Gaussian tap at a tile boundary would clamp and show a seam. The check is
   `GpuRenderer::can_crossfade(page.width, page.height)`.
3. **Matching spread widths.** `to_width == visible_width()`. The last spread
   can contain one page, and fixed-layout documents can have pages of different
   sizes; drawing two different widths into one destination would scale one
   layer incorrectly.
4. **No resize preview.** A resize preview draws the old strip at a fitted
   scale; the slide keeps moving until the worker lands the new bitmap.
5. **Both source windows fit inside the strip.** `from + width` and
   `to + width` are within `page.width`.

Otherwise the existing slide runs unchanged. `page_turn_ms = 0` still means no
animation at all.

## 7. About using a Bezier curve instead of `textureSample`

There are three different things a Bezier could mean here, and only one of
them replaces the blur kernel:

1. **Timing / easing.** A cubic Bezier is the right tool for the alpha and
   blur envelope. The existing cubic ease-in-out already does this on the CPU.
2. **Spatial mask or warp.** A Bezier can shape *where* the blur is applied or
   warp the sampling coordinates into a soft page-curl-like bend. It cannot
   create a blur by itself: one sample at a Bezier-offset coordinate is still
   one sample, so it only translates or distorts. Multiple samples along the
   curve produce a directional smear, not an isotropic Gaussian.
3. **Kernel approximation.** A Bezier can approximate the bell shape of a
   Gaussian, but the convolution still has to be evaluated. The fixed binomial
   weights are already the discrete Gaussian and are cheaper than fitting and
   evaluating a Bezier per pixel. Hardware bilinear filtering can fold a 9-tap
   1D kernel into 5 taps (RasterGrid's classic result), which is the standard
   optimization, not a Bezier fit.

So: Bezier for easing, yes; Bezier as a substitute for texture sampling, no.
If nine taps ever become too expensive, the real alternatives are a 5-tap
cross, a single mip/LOD sample (box filter, needs mipmaps), or a precomputed
blurred texture (extra memory and upload). None of them needs a Bezier.

## 8. Alternatives considered

### A. Cross-fade + small Gaussian (chosen)

- Removes translation and clipping completely.
- Minimal pipeline changes; no offscreen targets.
- Reads as a dissolve, not a page turn. A whole spread changes at once, and
  the page shared by consecutive sliding spreads is visible in both windows
  during the fade; the blur softens that double exposure.

### B. Offscreen two-layer composite

Render the old and new spreads into two offscreen textures, blur them with a
separable Gaussian (two passes), then composite with alpha. This is the
"correct" version if page edges must not blur or if old/new spreads have
different sizes. It needs two render targets, two or three passes, and more
VRAM; it is a larger change than the current renderer warrants for a 320 ms
effect.

### C. Edge-feathered slide + small blur

Keep the translation, but fade the strip out/in over a 24-48 px band at the
leading and trailing clip edges, optionally with a small blur in that band.
The text still moves, but it dissolves at the window edge instead of being cut.
This preserves the directional page-turn feel and is cheaper than a full
cross-fade.

### D. Page curl / cover

The Apple Books / Kindle / Readest approach. Needs a mesh, a back face, a
shadow, and separate page layers. Much larger than this change, and it does not
use the existing quad pipeline.

## 9. Risks and edge cases

- **Double exposure.** Two different page pairs blended at 50% can look muddy,
  especially with dense text. The shared page of consecutive sliding spreads is
  the main case; the `4t(1-t)` blur masks it. Visual testing may still suggest a
  slightly longer duration or a smaller radius.
- **Page edge halo.** A Gaussian on the whole page blurs the page boundary into
  the gutter. The envelope keeps this at zero at the start and end; the middle
  still shows a soft edge, which is why `B` stays at 1.5 px.
- **Performance.** 18 samples per pixel at mid-transition. On integrated GPUs
  at 4K this is worth measuring; the 5-tap cross or a lower `B` is the fallback.
- **Width mismatch.** Fixed-layout documents with mixed page sizes keep the
  slide. This is deliberate: scaling one layer would misalign the two windows.
- **Motion sensitivity.** `page_turn_ms = 0` disables the animation entirely.

## 10. References

- MuPDF `fitz/transition.c`, `fz_generate_transition` (fade/wipe/blinds).
- Foliate `paginator.js`, `#scrollTo` and `easeOutQuad` (300 ms slide).
- Readest `docs/page-curl.md` and `page-turn-styles-viewtransitions-555`
  notes (slide/curl layering, View Transitions, WebGL mesh).
- RasterGrid, "Efficient Gaussian blur with linear sampling" (separable
  Gaussian, binomial weights, 9 taps -> 5 bilinear taps).
- WGSL `textureSampleLevel` vs `textureSample` (explicit LOD avoids
  derivative requirements in non-uniform control flow).
