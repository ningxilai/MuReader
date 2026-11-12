// SPDX-License-Identifier: AGPL-3.0-only
//! Worker-side document engine.
//!
//! `RenderEngine` owns the MuPDF `Document`. It is created, used, and dropped
//! on the render worker thread; the UI thread never touches MuPDF. The only
//! things crossing the channel are commands and `RenderedPage` values, which
//! contain plain pixels plus an owned text layer for selection/copy.
//!
//! Reflowable documents get a fixed layout at 612×792 points. The document is
//! laid out once when it is opened, and its page count comes from that same
//! layout. A two-page spread is rendered as one composite bitmap, with text,
//! highlight, and link coordinates shifted into the composite space.
//!
//! Continuous scroll treats the story as one flow rather than a stack of
//! simulated pages: every reflowable page is rendered cropped to its content
//! box (text baselines plus embedded images) and blank pages are skipped.
//! Page chrome never shows, but the document's own spacing does: a second,
//! unpaginated layout measures the true distance between the blocks that
//! pagination split, so paragraph and figure margins survive at every seam.
//! Page numbers stay as the shared coordinate for spread mode, but scrolling
//! itself is page-free.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use fontdb::Weight;
use mupdf::{
    Colorspace, Context, Document, Matrix, Outline, Pixmap, Point, Quad, Rect, TextBlockContent,
    TextPage, TextPageFlags,
};

use crate::book_color;
use crate::fonts::FontCatalog;
use crate::outline_text::{TextEngine, TextTarget};
use crate::theme::{rgb8, OutlineStyle, DEFAULT_WINDOW};

/// No letterboxing around page content. The initial view fits the whole page
/// into the viewport, which is normally height-constrained but switches to
/// width for wide media pages. An explicit zoom switches to a fixed scale.
pub const MARGIN: u32 = 0;

pub const ZOOM_IN_FACTOR: f32 = 1.25;
pub const ZOOM_OUT_FACTOR: f32 = 0.8;
/// Lowest explicit zoom scale. It is intentionally much smaller than a
/// normal text scale because a very long image may need a tiny automatic fit
/// scale, which must still round-trip through zoom/reset.
pub const MIN_SCALE: f32 = 0.0001;
pub const MAX_SCALE: f32 = 8.0;

const MAX_DIMENSION: f32 = 16_384.0;
const MAX_PIXELS: f32 = 40_000_000.0;

const LAYOUT_WIDTH: f32 = 612.0;
const LAYOUT_HEIGHT: f32 = 792.0;
const LAYOUT_EM: f32 = 12.0;
/// Height used for the second, unpaginated layout that continuous scroll
/// measures real inter-block spacing from. MuPDF clamps a margin that crosses
/// a page boundary, so the paginated story loses paragraph and image spacing
/// exactly at its page seams. A page tall enough to never break keeps the
/// document's own spacing intact; text is extracted from it, never rendered.
const TALL_LAYOUT_HEIGHT: f32 = 10_000_000.0;
const FONT_SIZE_MIN: f32 = 8.0;
const FONT_SIZE_MAX: f32 = 24.0;
pub const FONT_SIZE_STEP: f32 = 1.1;
/// Keep the four pages of a spread; each turn replaces the page that leaves.
const PAGE_CACHE_CAPACITY: usize = 4;
/// Outside a spread only the current page and one neighbor are useful.
const PAGE_CACHE_SINGLE_CAPACITY: usize = 2;

/// The gutter is part of the spread geometry, not a decoration that vanishes
/// when space is tight. Its width follows the rendered page width linearly, so
/// it scales with every fit change and is always at least one pixel.
pub const SPREAD_GAP_RATIO: f32 = 0.004;

pub fn spread_gap(page_width: u32) -> u32 {
    ((page_width as f32 * SPREAD_GAP_RATIO).floor() as u32).max(1)
}

/// Pages kept in a scroll strip. The strip is a sliding window: the reader
/// keeps at least one page behind and two ahead, and re-centers before the
/// viewport reaches an edge, so continuous scrolling never stalls on a boundary.
const SCROLL_STRIP_PAGES: usize = 4;
/// Cap the strip so it stays within common GPU texture limits. The automatic
/// width fit shrinks slightly for extreme aspect ratios instead of failing.
const SCROLL_STRIP_MAX_HEIGHT: f32 = 8192.0;
/// A standalone illustration may extend past a synthesized reflow page, but
/// keep one plate within a sane bitmap height so a pathological aspect ratio
/// cannot allocate an enormous texture.
const IMAGE_PAGE_MAX_HEIGHT: f32 = 4096.0;
/// Do not blow a small image up beyond this multiple of the normal page fit.
/// The reflow engine may already have scaled it down, so some upscaling is
/// expected, but unbounded growth would only produce a blurry plate.
const IMAGE_PAGE_MAX_UPSCALE: f32 = 4.0;

/// Vertical breathing room between pages in the scroll strip.
pub fn scroll_gap(page_width: u32) -> u32 {
    (page_width / 100).clamp(8, 24)
}

/// Side margin reserved in continuous scroll. Unlike the paged fit, which can
/// let a page touch the window edge, a scrolling column always keeps a little
/// air on both sides, including in narrow windows and when the outline opens.
pub fn scroll_side_margin(viewport_width: u32) -> u32 {
    let proportional = (viewport_width as f32 * 0.04).round() as u32;
    proportional
        .clamp(12, 64)
        .min(viewport_width.saturating_div(4))
}

/// Longest side, in pixels, of a page rendered for background sampling. The
/// palette only needs the page margins, so a tiny preview is enough.
const BOOK_SAMPLE_SIZE: f32 = 96.0;

/// Left padding for outline titles. It also keeps the selection rail clear of
/// the first glyph.
const OUTLINE_TEXT_PADDING: usize = 12;
const OUTLINE_RAIL_WIDTH: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    None,
    /// Fit the whole page into the viewport, using whichever dimension is
    /// more constrained. This keeps normal pages height-fitted while making
    /// wide media pages fully visible.
    Page,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchScope {
    Page,
    Book,
}

#[derive(Debug)]
pub struct PageBitmap {
    pub width: u32,
    pub height: u32,
    /// `0x00RRGGBB`, top-to-bottom, row-major.
    pub pixels: Vec<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightRect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

/// Region of a rendered page bitmap to keep. Standalone illustrations are
/// cropped out of the synthesized reflow page they were laid out on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BitmapCrop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

/// Vertical content box of a reflowable page, measured from the laid-out
/// story. Continuous scroll renders only this region, so the synthetic
/// page margins never appear as gaps between content blocks.
#[derive(Debug, Clone, Copy)]
struct RollMetrics {
    /// Content top in page points: text lines, images, and vector art. This is
    /// the crop edge after the strip's shared page-margin baseline is applied.
    top: f32,
    /// Crop top derived from text lines alone, before images or the strip
    /// baseline. The document-wide minimum of this value is the simulated page
    /// margin that continuous scroll removes at every seam.
    text_top: Option<f32>,
    /// Content bottom in page points, exclusive.
    bottom: f32,
    /// Median distance between consecutive text lines, in page points.
    line_height: Option<f32>,
    /// Number of text lines on the page; image-only pages are scaled to the
    /// reading column like a plate.
    text_lines: usize,
    /// Bounds of the image union, set only for image-only pages.
    plate_bounds: Option<Rect>,
}

/// One block of content in the document's continuous flow. Text keeps its
/// characters so paginated pages can be aligned with the tall layout; images
/// and vector art are matched by kind because pagination may rescale an image
/// that sits at the bottom of a page.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FlowElement {
    Text(String),
    Image,
    Shape,
}

impl FlowElement {
    fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Text(a), Self::Text(b)) => a == b,
            (Self::Image, Self::Image) | (Self::Shape, Self::Shape) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
struct FlowItem {
    kind: FlowElement,
    rect: Rect,
    /// Index of the tall-layout page (an EPUB chapter) holding this block.
    chapter: usize,
}

/// The tall, unpaginated layout of the story, used only to measure the true
/// distance between blocks that pagination split across a page boundary.
struct TallFlow {
    elements: Vec<FlowItem>,
}

/// Where one paginated page's content sits in the tall flow.
#[derive(Debug, Clone, Copy)]
struct PageFlow {
    first_tall: usize,
    last_tall: usize,
    /// First and last content rectangles in page points, used to subtract the
    /// blank padding the roll already carries at the seam.
    first_rect: Rect,
    last_rect: Rect,
}

/// How one page will be rasterized into the current strip.
struct PagePlan {
    scale: f32,
    ctm: Matrix,
    crop: Option<BitmapCrop>,
    /// Full rendered page height before cropping, used to keep a scroll strip
    /// within the GPU texture limit.
    height: f32,
    /// A standalone illustration rendered at the reading-column width. Only
    /// these may be shrunk to keep a strip within the texture limit; text
    /// pages must keep the locked column scale.
    plate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextChar {
    pub ch: char,
    pub rect: HighlightRect,
    pub line: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextLine {
    /// Inclusive character index.
    pub start: usize,
    /// Exclusive character index.
    pub end: usize,
    pub rect: HighlightRect,
}

#[derive(Debug, Clone, Default)]
pub struct TextLayer {
    pub chars: Vec<TextChar>,
    pub lines: Vec<TextLine>,
}

impl TextLayer {
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Find the closest character to a page-bitmap coordinate.
    pub fn position_at(&self, x: f32, y: f32) -> Option<usize> {
        if self.chars.is_empty() {
            return None;
        }
        for (index, ch) in self.chars.iter().enumerate() {
            let r = ch.rect;
            if x >= r.x0 as f32 && x < r.x1 as f32 && y >= r.y0 as f32 && y < r.y1 as f32 {
                return Some(index);
            }
        }

        let line = self
            .lines
            .iter()
            .filter(|line| line.rect.x1 > line.rect.x0 && line.rect.y1 > line.rect.y0)
            .min_by(|a, b| {
                let da = rect_distance(a.rect, x, y);
                let db = rect_distance(b.rect, x, y);
                da.0.total_cmp(&db.0).then(da.1.total_cmp(&db.1))
            })?;
        let mut best = line.start;
        let mut best_dx = f32::INFINITY;
        for index in line.start..line.end.min(self.chars.len()) {
            let r = self.chars[index].rect;
            let dx = if x < r.x0 as f32 {
                r.x0 as f32 - x
            } else if x > r.x1 as f32 {
                x - r.x1 as f32
            } else {
                0.0
            };
            if dx < best_dx {
                best_dx = dx;
                best = index;
            }
        }
        (line.start < line.end).then_some(best)
    }

    /// Text between two inclusive character indices. Visual line wraps become
    /// a space for Latin text (or nothing for CJK), while paragraph spacing and
    /// page/column jumps keep the line break that was actually in the source.
    pub fn text_for_range(&self, start: usize, end: usize) -> String {
        if self.chars.is_empty() {
            return String::new();
        }
        let start = start.min(self.chars.len() - 1);
        let end = end.min(self.chars.len() - 1);
        let (start, end) = if start <= end {
            (start, end)
        } else {
            (end, start)
        };

        let mut out = String::new();
        let mut previous: Option<&TextChar> = None;
        for ch in &self.chars[start..=end] {
            if let Some(previous) = previous {
                if previous.line != ch.line {
                    out.push_str(self.line_separator(previous, ch));
                }
            }
            out.push(ch.ch);
            previous = Some(ch);
        }
        out
    }

    fn line_separator(&self, previous: &TextChar, next: &TextChar) -> &'static str {
        let (Some(previous_line), Some(next_line)) =
            (self.lines.get(previous.line), self.lines.get(next.line))
        else {
            return "\n";
        };
        let previous_height = previous_line.rect.y1.saturating_sub(previous_line.rect.y0) as f32;
        let advance = next_line.rect.y0 as f32 - previous_line.rect.y0 as f32;
        if advance <= 0.0 || advance > previous_height * PARAGRAPH_GAP_FACTOR {
            "\n"
        } else if soft_wrap_needs_space(previous.ch, next.ch) {
            " "
        } else {
            ""
        }
    }
}

/// A line advance this much larger than the previous line's height is
/// paragraph spacing, not a visual wrap. Ordinary line advances stay near one
/// line height.
const PARAGRAPH_GAP_FACTOR: f32 = 1.35;

/// Wrapped Latin words need the space that disappeared at the line end. CJK
/// text runs together without one, and existing whitespace already separates.
fn soft_wrap_needs_space(previous: char, next: char) -> bool {
    if previous.is_whitespace() || next.is_whitespace() {
        return false;
    }
    if is_wide_script(previous) || is_wide_script(next) {
        return false;
    }
    let previous_ends_word = previous.is_alphanumeric()
        || matches!(
            previous,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '"' | '\''
        );
    previous_ends_word && next.is_alphanumeric()
}

fn is_wide_script(ch: char) -> bool {
    matches!(
        ch as u32,
        0x2E80..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF | 0x20000..=0x2FA1F
    )
}

/// Vertical distance first, then horizontal. This keeps a click on a line's
/// baseline from jumping to a different column on a nearby line, while still
/// distinguishing two columns that share the same y coordinate.
fn rect_distance(rect: HighlightRect, x: f32, y: f32) -> (f32, f32) {
    let dx = if x < rect.x0 as f32 {
        rect.x0 as f32 - x
    } else if x > rect.x1 as f32 {
        x - rect.x1 as f32
    } else {
        0.0
    };
    let dy = if y < rect.y0 as f32 {
        rect.y0 as f32 - y
    } else if y > rect.y1 as f32 {
        y - rect.y1 as f32
    } else {
        0.0
    };
    (dy, dx)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextSelection {
    pub anchor: usize,
    pub focus: usize,
}

impl TextSelection {
    pub fn bounds(self) -> (usize, usize) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }
}

#[derive(Debug)]
pub struct RenderedPage {
    pub bitmap: PageBitmap,
    /// Current page number (one-based).
    pub page: usize,
    /// The rendered strip: one page, or up to four pages around the current
    /// page so either direction can slide without a pre-render gap.
    pub pages: Vec<usize>,
    /// Rendered width of each page in `pages`.
    pub page_widths: Vec<u32>,
    /// Visual gap between adjacent pages in the strip.
    pub gap: u32,
    pub page_count: Option<usize>,
    pub scale: f32,
    pub fit: Fit,
    pub rotation: u16,
    pub highlights: Vec<HighlightRect>,
    pub links: Vec<RenderedLink>,
    pub text: TextLayer,
    /// One canvas color sampled across the whole book. It stays constant for
    /// every page so the UI palette never flips mid-book.
    pub book_background: u32,
    /// True when the strip is a vertical continuous-scroll layout rather than a
    /// horizontal page or spread.
    pub scroll: bool,
    /// Geometry of each page inside the strip bitmap. Only meaningful for a
    /// scroll strip; paged layouts also fill them for completeness.
    pub page_offsets: Vec<u32>,
    pub page_heights: Vec<u32>,
    /// Text-size multiplier for reflowable documents. For fixed-layout
    /// documents this stays at 1.0; page zoom is used instead.
    pub font_scale: f32,
    pub reflowable: bool,
    /// Typical distance between consecutive text lines, in layout points.
    /// Continuous scroll uses it as the height of one line step, so the
    /// step follows the CSS the document asks for instead of a fixed guess.
    pub line_height: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// One-based page number for an internal destination.
    Page(usize),
    /// An external URI, opened by the desktop's default handler.
    Uri(String),
}

#[derive(Debug, Clone)]
pub struct RenderedLink {
    pub bounds: HighlightRect,
    /// Absent when the document supplies neither a page nor a URI.
    pub target: Option<LinkTarget>,
}

#[derive(Debug, Clone)]
pub struct OutlineEntry {
    pub title: String,
    pub page: Option<usize>,
    pub depth: usize,
}

struct PageRender {
    bitmap: PageBitmap,
    highlights: Vec<HighlightRect>,
    links: Vec<RenderedLink>,
    text: TextLayer,
}

pub struct RenderEngine {
    doc: Document,
    page: usize,
    page_count: Option<usize>,
    /// Pixels per PDF point. 1.0 == 72 dpi.
    scale: f32,
    fit: Fit,
    reflowable: bool,
    rotation: u16,
    spread: bool,
    /// Single-column continuous scroll. Mutually exclusive with `spread`.
    scroll: bool,
    /// Pages rendered before the scroll anchor. The reader chooses one for
    /// downward scrolling and two for upward scrolling, so the strip always
    /// has lookahead in the direction of travel.
    scroll_before: usize,
    /// Fit scale locked for the current scroll session. A continuous canvas
    /// must not re-zoom when the anchor moves to a page with slightly
    /// different bounds, so the first scroll render fixes it until the
    /// viewport, fit, rotation, or text size changes.
    scroll_scale: Option<f32>,
    viewport: (u32, u32),
    search_query: Option<String>,
    outline: Vec<OutlineEntry>,
    outline_text: Option<TextEngine>,
    fonts: Arc<FontCatalog>,
    source_path: std::path::PathBuf,
    /// The document was generated by the plain-text pretty-layout pass and
    /// must be regenerated when the text size changes.
    pretty_text: bool,
    /// Base text size in points for reflowable documents.
    font_size: f32,
    /// Window color behind a spread gutter. The UI supplies the CSS theme.
    background: u32,
    /// One canvas color interpolated from pages across the whole book. The UI
    /// derives a single palette from this instead of re-reading every page.
    book_background: u32,
    /// Vertical content box of a reflowable page, plus its line spacing.
    /// Used to pack continuous scroll without the simulated page margins.
    roll_metrics_cache: HashMap<usize, Option<RollMetrics>>,
    /// Smallest text crop top seen in the roll. This is the simulated page
    /// margin shared by every page; pages are cropped here so real paragraph
    /// and chapter margins survive while page chrome disappears.
    roll_top_baseline: Option<f32>,
    /// The unpaginated layout used to recover spacing at page seams. Built on
    /// the first roll render; an empty flow means the tall layout failed.
    tall_flow: Option<TallFlow>,
    /// Paginated page -> position in the tall flow. `None` marks a page whose
    /// content could not be aligned (a blank page, for example).
    page_flow: HashMap<usize, Option<PageFlow>>,
    /// Recently rendered pages for the spread strip. Consecutive page turns
    /// overlap three of the four pages, so reusing them avoids rasterizing
    /// the whole strip on every turn. Each entry records the scale it was
    /// rendered at because mixed page sizes can change the spread fit scale.
    page_cache: Vec<(usize, f32, PageRender)>,
    #[cfg(test)]
    page_renders: usize,
}

impl RenderEngine {
    pub fn open(
        path: &Path,
        fonts: Arc<FontCatalog>,
        css: Option<String>,
        pretty: bool,
    ) -> Result<Self, String> {
        // MuPDF's TXT handler also claims .log, but log output is machine
        // text, not a document. Reject it instead of showing it as a book.
        if path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("log"))
        {
            return Err(format!("unsupported document format: {}", path.display()));
        }

        let mut context = Context::get();
        context
            .set_user_css(css.as_deref().unwrap_or(""))
            .map_err(|e| format!("reflowable CSS: {e}"))?;

        let mut pretty_text = pretty && crate::pretty_text::is_plain_text(path);
        let mut doc = if pretty_text {
            match crate::pretty_text::html_for_path(path, &fonts) {
                Ok(html) => match Document::from_copied_bytes(html.as_bytes(), "text/html") {
                    Ok(document) => document,
                    Err(error) => {
                        eprintln!("MuReader: pretty text render failed: {error}; using MuPDF");
                        pretty_text = false;
                        Document::open(path).map_err(|e| format!("open {}: {e}", path.display()))?
                    }
                },
                Err(error) => {
                    eprintln!("MuReader: pretty text failed: {error}; using MuPDF");
                    pretty_text = false;
                    Document::open(path).map_err(|e| format!("open {}: {e}", path.display()))?
                }
            }
        } else {
            Document::open(path).map_err(|e| format!("open {}: {e}", path.display()))?
        };
        let reflowable = doc
            .is_reflowable()
            .map_err(|e| format!("document type: {e}"))?;
        if reflowable {
            doc.layout(LAYOUT_WIDTH, LAYOUT_HEIGHT, LAYOUT_EM)
                .map_err(|e| format!("layout: {e}"))?;
        }
        let count = doc.page_count().map_err(|e| format!("page count: {e}"))? as usize;
        if count == 0 {
            return Err("document has no pages".to_string());
        }
        let page_count = Some(count);
        let outline = build_outline(&doc, count);
        let mut engine = Self {
            doc,
            page: 0,
            page_count,
            scale: 1.0,
            fit: Fit::Page,
            reflowable,
            rotation: 0,
            spread: false,
            scroll: false,
            scroll_before: 1,
            scroll_scale: None,
            viewport: (1100, 900),
            search_query: None,
            outline,
            outline_text: None,
            fonts,
            source_path: path.to_path_buf(),
            pretty_text,
            font_size: LAYOUT_EM,
            background: DEFAULT_WINDOW,
            book_background: DEFAULT_WINDOW,
            roll_metrics_cache: HashMap::new(),
            roll_top_baseline: None,
            tall_flow: None,
            page_flow: HashMap::new(),
            page_cache: Vec::new(),
            #[cfg(test)]
            page_renders: 0,
        };
        engine.book_background = engine.sample_book_background();
        engine.skip_blank_forward();
        Ok(engine)
    }

    pub fn font_scale(&self) -> f32 {
        self.font_size / LAYOUT_EM
    }

    /// Change the base text size for a reflowable document. The layout is
    /// recomputed and the current reading position is restored from the first
    /// text line of the current page.
    pub fn adjust_font(&mut self, factor: f32) -> Result<bool, String> {
        if !self.reflowable || !factor.is_finite() || factor <= 0.0 {
            return Ok(false);
        }
        let target = (self.font_size * factor).clamp(FONT_SIZE_MIN, FONT_SIZE_MAX);
        if (target - self.font_size).abs() < 0.01 {
            return Ok(false);
        }
        self.page_cache.clear();
        self.roll_metrics_cache.clear();
        self.roll_top_baseline = None;
        self.tall_flow = None;
        self.page_flow.clear();
        self.scroll_scale = None;
        let old_page = self.page;
        let old_count = self.page_count.unwrap_or(1).max(1);
        let anchor = self.page_anchor();
        self.search_query = None;

        if self.pretty_text {
            let html =
                crate::pretty_text::html_for_path_at(&self.source_path, &self.fonts, target)?;
            let mut doc = Document::from_copied_bytes(html.as_bytes(), "text/html")
                .map_err(|e| format!("pretty text render: {e}"))?;
            doc.layout(LAYOUT_WIDTH, LAYOUT_HEIGHT, target)
                .map_err(|e| format!("layout: {e}"))?;
            self.doc = doc;
        } else {
            self.doc
                .layout(LAYOUT_WIDTH, LAYOUT_HEIGHT, target)
                .map_err(|e| format!("layout: {e}"))?;
        }

        self.font_size = target;
        let count = self
            .doc
            .page_count()
            .map_err(|e| format!("page count: {e}"))? as usize;
        if count == 0 {
            return Err("document has no pages".to_string());
        }
        self.page_count = Some(count);
        self.outline = build_outline(&self.doc, count);
        self.page = self
            .find_anchor(anchor.as_deref(), old_page, count)
            .unwrap_or_else(|| {
                let scaled = old_page * count / old_count.max(1);
                scaled.min(count.saturating_sub(1))
            });
        self.skip_blank_forward();
        Ok(true)
    }

    fn page_anchor(&self) -> Option<String> {
        let page = self.doc.load_page(self.page as i32).ok()?;
        let text_page = page.to_text_page(TextPageFlags::PRESERVE_WHITESPACE).ok()?;
        for block in text_page.structured().blocks {
            let TextBlockContent::Text { lines } = block.content else {
                continue;
            };
            for line in lines {
                let mut text = String::new();
                for ch in line.chars {
                    if text.is_empty() && ch.ch.is_whitespace() {
                        continue;
                    }
                    text.push(if ch.ch.is_whitespace() { ' ' } else { ch.ch });
                    if text.chars().count() >= 24 {
                        break;
                    }
                }
                let text = text.trim().to_string();
                if text.chars().count() >= 4 {
                    return Some(text);
                }
            }
        }
        None
    }

    fn find_anchor(&self, anchor: Option<&str>, old_page: usize, count: usize) -> Option<usize> {
        let anchor = anchor?;
        if anchor.chars().count() < 4 || count == 0 {
            return None;
        }
        let start = old_page.saturating_sub(8);
        let end = (old_page + 16).min(count);
        let mut tried = vec![false; count];
        let mut order = Vec::with_capacity(count);
        for page_no in start..end {
            if let Some(seen) = tried.get_mut(page_no) {
                *seen = true;
            }
            order.push(page_no);
        }
        for (page_no, seen) in tried.iter().enumerate() {
            if !seen {
                order.push(page_no);
            }
        }
        for page_no in order {
            let Ok(page) = self.doc.load_page(page_no as i32) else {
                continue;
            };
            if page
                .search(anchor, 8)
                .map(|quads| !quads.is_empty())
                .unwrap_or(false)
            {
                return Some(page_no);
            }
        }
        None
    }

    pub fn set_viewport(&mut self, width: u32, height: u32) {
        let next = (width.max(1), height.max(1));
        if next != self.viewport {
            self.viewport = next;
            self.scroll_scale = None;
            self.page_cache.clear();
        }
    }

    pub fn fit(&self) -> Fit {
        self.fit
    }

    /// Switch the reading model. A spread is paged; everything else is the
    /// single-column scroll layout.
    pub fn set_layout(&mut self, spread: bool) {
        let scroll = !spread;
        if self.spread != spread || self.scroll != scroll {
            self.spread = spread;
            self.scroll = scroll;
            self.scroll_scale = None;
            self.page_cache.clear();
        }
    }

    /// Move the scroll strip without skipping blank pages. Page jumps still use
    /// `goto_page`; this only slides the rendered window while scrolling.
    /// `before` chooses how many pages sit above the anchor, so the reader can
    /// keep lookahead in whichever direction it is moving.
    pub fn scroll_to(&mut self, page: usize, before: usize) -> bool {
        let target = match self.page_count {
            Some(count) => page.max(1).min(count) - 1,
            None => page.max(1) - 1,
        };
        let before = before.min(self.scroll_strip_pages().saturating_sub(1));
        if target == self.page && before == self.scroll_before {
            return false;
        }
        self.page = target;
        self.scroll_before = before;
        true
    }

    pub fn next_page(&mut self) -> bool {
        let next = self.page + 1;
        if let Some(count) = self.page_count {
            if next >= count {
                return false;
            }
        } else if self.doc.load_page(next as i32).is_err() {
            return false;
        }
        self.page = next;
        self.skip_blank_forward();
        true
    }

    pub fn prev_page(&mut self) -> bool {
        if self.page == 0 {
            return false;
        }
        self.page = self.page.saturating_sub(1);
        self.skip_blank_backward();
        true
    }

    pub fn first_page(&mut self) -> bool {
        if self.page == 0 {
            return false;
        }
        self.page = 0;
        true
    }

    pub fn last_page(&mut self) -> bool {
        let Some(count) = self.page_count else {
            return false;
        };
        let target = if self.spread {
            count.saturating_sub(2)
        } else {
            count - 1
        };
        if self.page == target {
            return false;
        }
        self.page = target;
        true
    }

    pub fn goto_page(&mut self, page: usize) -> bool {
        let page = page.max(1);
        let target = match self.page_count {
            Some(count) => page.min(count) - 1,
            None => page - 1,
        };
        if target == self.page {
            return false;
        }
        let backward = target < self.page;
        self.page = target;
        if backward {
            self.skip_blank_backward();
        } else {
            self.skip_blank_forward();
        }
        true
    }

    /// Reflow can leave a source page empty after an oversized image is
    /// scaled down. Skip such pages while navigating.
    fn skip_blank_forward(&mut self) {
        if !self.reflowable {
            return;
        }
        while self.page + 1 < self.page_count.unwrap_or(0) && self.page_is_blank(self.page) {
            self.page += 1;
        }
    }

    fn skip_blank_backward(&mut self) {
        if !self.reflowable {
            return;
        }
        while self.page > 0 && self.page_is_blank(self.page) {
            self.page -= 1;
        }
    }

    fn page_is_blank(&self, page_no: usize) -> bool {
        let Ok(page) = self.doc.load_page(page_no as i32) else {
            return false;
        };
        let Ok(text_page) = page.to_text_page(TextPageFlags::PRESERVE_IMAGES) else {
            return false;
        };
        !text_page.blocks().any(|block| match block.r#type() {
            mupdf::text_page::TextBlockType::Image
            | mupdf::text_page::TextBlockType::Vector
            | mupdf::text_page::TextBlockType::Grid
            | mupdf::text_page::TextBlockType::Struct => true,
            mupdf::text_page::TextBlockType::Text => block.lines().any(|line| {
                line.chars()
                    .any(|ch| ch.char().is_some_and(|c| !c.is_whitespace()))
            }),
        })
    }

    /// Continuous scroll means the reflowable story flows as one column with
    /// no simulated page chrome. Content is laid out by MuPDF in paginated
    /// pages underneath (the spread mode and page numbers stay consistent
    /// with it), but the pages are rendered cropped to their content so page
    /// chrome never shows between blocks, lines, or illustrations. The
    /// document's real inter-block spacing is measured from an unpaginated
    /// layout and restored at each seam. An explicit zoom keeps the
    /// page-shaped view so the reader can inspect the page as laid out.
    fn roll(&self) -> bool {
        self.scroll && self.reflowable && self.fit == Fit::Page && self.rotation.is_multiple_of(180)
    }

    /// Measure the vertical content box and the line spacing of one reflowable
    /// page. Returns `None` for a fully blank page, which continuous scroll
    /// skips entirely instead of drawing a blank band.
    fn roll_metrics(&mut self, page_no: usize) -> Option<RollMetrics> {
        if let Some(cached) = self.roll_metrics_cache.get(&page_no) {
            return *cached;
        }
        let computed = self.extract_roll_metrics(page_no);
        self.roll_metrics_cache.insert(page_no, computed);
        computed
    }

    /// Learn the simulated page margin from the pages in the strip. The
    /// smallest text top is the blank band every page carries above its first
    /// line; anything larger is a paragraph or chapter margin the reader
    /// should see. The baseline only ever decreases, so a strip that happens
    /// to begin with a heading cannot widen the gap between pages.
    fn observe_roll_baseline(&mut self, pages: &[usize]) {
        let mut baseline = self.roll_top_baseline;
        for &page_no in pages {
            let Some(text_top) = self
                .roll_metrics(page_no)
                .and_then(|metrics| metrics.text_top)
            else {
                continue;
            };
            if !baseline.is_some_and(|current| text_top >= current - 0.01) {
                baseline = Some(text_top);
            }
        }
        if baseline != self.roll_top_baseline {
            self.roll_top_baseline = baseline;
            // Cached pages were cropped with the previous baseline.
            self.page_cache.clear();
        }
    }

    fn extract_roll_metrics(&self, page_no: usize) -> Option<RollMetrics> {
        let page = self.doc.load_page(page_no as i32).ok()?;
        let text_page = page
            .to_text_page(TextPageFlags::PRESERVE_WHITESPACE | TextPageFlags::PRESERVE_IMAGES)
            .ok()?;
        let mut lines: Vec<Rect> = Vec::new();
        let mut images: Vec<Rect> = Vec::new();
        let mut shapes: Vec<Rect> = Vec::new();
        for block in text_page.blocks() {
            match block.r#type() {
                mupdf::text_page::TextBlockType::Text => {
                    for line in block.lines() {
                        let bounds = line.bounds();
                        if bounds.is_valid() && !bounds.is_empty() {
                            lines.push(bounds);
                        }
                    }
                }
                mupdf::text_page::TextBlockType::Image => {
                    let bounds = block.bounds();
                    if bounds.is_valid() && !bounds.is_empty() {
                        images.push(bounds);
                    }
                }
                _ => {
                    // Vector, Grid, and Struct blocks carry drawing operations:
                    // inline SVG plates, ornaments, rules. A page holding only
                    // vector art is content, not a blank page.
                    let bounds = block.bounds();
                    if bounds.is_valid() && !bounds.is_empty() {
                        shapes.push(bounds);
                    }
                }
            }
        }
        if lines.is_empty() && images.is_empty() && shapes.is_empty() {
            return None;
        }
        // The laid-out story already obeys the document's CSS, including its
        // line spacing. Measure it from one line to the next rather than
        // guessing from the root font size.
        let mut ys: Vec<f32> = lines.iter().map(|rect| rect.y0).collect();
        ys.sort_by(f32::total_cmp);
        let mut deltas: Vec<f32> = ys
            .windows(2)
            .filter_map(|pair| {
                let delta = pair[1] - pair[0];
                (delta > 0.25).then_some(delta)
            })
            .collect();
        deltas.sort_by(f32::total_cmp);
        let line_height = (!deltas.is_empty()).then(|| deltas[deltas.len() / 2]);
        let base = line_height.unwrap_or(self.font_size * 1.2);
        let mid_min = lines
            .iter()
            .map(|rect| (rect.y0 + rect.y1) * 0.5)
            .fold(f32::INFINITY, f32::min);
        let mid_max = lines
            .iter()
            .map(|rect| (rect.y0 + rect.y1) * 0.5)
            .fold(f32::NEG_INFINITY, f32::max);
        let ink_top = lines
            .iter()
            .map(|rect| rect.y0)
            .fold(f32::INFINITY, f32::min);
        let ink_bottom = lines
            .iter()
            .map(|rect| rect.y1)
            .fold(f32::NEG_INFINITY, f32::max);
        let bounds = page.bounds().ok()?;
        // Leading above the first line: half a line from the line-box centre,
        // but never less than one point above the glyph ink. The smallest
        // such top across the strip is the simulated page margin, which the
        // roll removes at every seam. A larger top is a real paragraph or
        // chapter margin, so it survives.
        let text_top = if lines.is_empty() {
            None
        } else {
            Some((mid_min - base * 0.5).min(ink_top - 1.0))
        };
        // Without text, the crop is decided by the images and vector art
        // alone, so start above the page and let those bounds pull it down.
        let mut top = text_top.unwrap_or(f32::INFINITY);
        let mut bottom = mid_max + base * 0.5;
        bottom = bottom.max(ink_bottom + 1.0);
        for rect in images.iter().chain(&shapes) {
            top = top.min(rect.y0);
            bottom = bottom.max(rect.y1);
        }
        top = top.max(bounds.y0);
        bottom = bottom.min(bounds.y1);
        if bottom - top < 1.0 {
            return None;
        }
        let plate_bounds = if lines.is_empty() && (!images.is_empty() || !shapes.is_empty()) {
            images
                .iter()
                .chain(&shapes)
                .copied()
                .reduce(|union, rect| union.r#union(&rect))
        } else {
            None
        };
        Some(RollMetrics {
            top,
            text_top,
            bottom,
            line_height,
            text_lines: lines.len(),
            plate_bounds,
        })
    }

    /// Build the tall layout on first use. Failure is not fatal: the roll
    /// still renders, it just cannot recover margins that pagination dropped.
    fn ensure_tall_flow(&mut self) {
        if self.tall_flow.is_some() {
            return;
        }
        let flow = match self.build_tall_flow() {
            Ok(flow) => flow,
            Err(error) => {
                eprintln!("MuReader: continuous flow unavailable: {error}");
                TallFlow {
                    elements: Vec::new(),
                }
            }
        };
        self.tall_flow = Some(flow);
    }

    fn build_tall_flow(&self) -> Result<TallFlow, String> {
        let mut doc = if self.pretty_text {
            let html = crate::pretty_text::html_for_path_at(
                &self.source_path,
                &self.fonts,
                self.font_size,
            )?;
            Document::from_copied_bytes(html.as_bytes(), "text/html")
                .map_err(|e| format!("tall pretty text: {e}"))?
        } else {
            Document::open(self.source_path.as_path())
                .map_err(|e| format!("tall layout: open {}: {e}", self.source_path.display()))?
        };
        doc.layout(LAYOUT_WIDTH, TALL_LAYOUT_HEIGHT, self.font_size)
            .map_err(|e| format!("tall layout: {e}"))?;
        let count = doc.page_count().map_err(|e| format!("tall layout: {e}"))?;
        let mut elements = Vec::new();
        for chapter in 0..count {
            let page = doc
                .load_page(chapter)
                .map_err(|e| format!("tall layout page: {e}"))?;
            let text_page = page
                .to_text_page(TextPageFlags::PRESERVE_WHITESPACE | TextPageFlags::PRESERVE_IMAGES)
                .map_err(|e| format!("tall layout text: {e}"))?;
            for (kind, rect) in flow_elements(&text_page) {
                elements.push(FlowItem {
                    kind,
                    rect,
                    chapter: chapter as usize,
                });
            }
        }
        Ok(TallFlow { elements })
    }

    /// Place one paginated page in the tall flow. Pages are matched by their
    /// block sequence, not by a page number API: the tall layout has no page
    /// breaks to ask about. A page's text and images must be a contiguous run
    /// of the tall flow, which is guaranteed because both layouts use the same
    /// width and therefore the same line breaks.
    fn map_page(&mut self, page_no: usize) -> Option<PageFlow> {
        if let Some(cached) = self.page_flow.get(&page_no) {
            return *cached;
        }
        let flow = self.map_page_uncached(page_no);
        self.page_flow.insert(page_no, flow);
        flow
    }

    fn map_page_uncached(&mut self, page_no: usize) -> Option<PageFlow> {
        let page = self.doc.load_page(page_no as i32).ok()?;
        let text_page = page
            .to_text_page(TextPageFlags::PRESERVE_WHITESPACE | TextPageFlags::PRESERVE_IMAGES)
            .ok()?;
        let items = flow_elements(&text_page);
        if items.is_empty() {
            return None;
        }
        self.ensure_tall_flow();
        let tall = self.tall_flow.as_ref()?;
        let len = items.len();
        if len > tall.elements.len() {
            return None;
        }
        // Already-mapped neighbours bound the search, so a page is never
        // matched to content that belongs before or after it.
        let previous = self
            .page_flow
            .iter()
            .filter(|(mapped, flow)| **mapped < page_no && flow.is_some())
            .map(|(mapped, flow)| (*mapped, flow.unwrap().last_tall))
            .max_by_key(|(mapped, _)| *mapped);
        let next = self
            .page_flow
            .iter()
            .filter(|(mapped, flow)| **mapped > page_no && flow.is_some())
            .map(|(mapped, flow)| (*mapped, flow.unwrap().first_tall))
            .min_by_key(|(mapped, _)| *mapped);
        let start = previous.map_or(0, |(_, last)| last + 1);
        let end = next.map_or(tall.elements.len(), |(_, first)| first);
        if start >= end {
            return None;
        }
        // A short page, especially an image-only one, can match several runs
        // of the tall flow. Prefer the run nearest where this page should sit:
        // neighbours interpolate the tightest estimate, and a cold jump falls
        // back to the page's proportional position in the document.
        let expected = match (previous, next) {
            (Some((previous_page, previous_last)), Some((next_page, next_first))) => {
                let span = next_first.saturating_sub(previous_last + 1) as f64;
                let pages = next_page.saturating_sub(previous_page).max(1) as f64;
                previous_last as f64
                    + 1.0
                    + span * (page_no.saturating_sub(previous_page) as f64 / pages)
            }
            (Some((_, previous_last)), None) => previous_last as f64 + 1.0,
            (None, Some((_, next_first))) => next_first.saturating_sub(len) as f64,
            (None, None) => {
                let count = self.page_count.unwrap_or(page_no + 1).max(1) as f64;
                (page_no as f64 + 0.5) / count * tall.elements.len() as f64
            }
        };
        let mut best: Option<(usize, f64)> = None;
        for index in start..end {
            if index + len > tall.elements.len() || index + len > end {
                break;
            }
            if !tall.elements[index].kind.matches(&items[0].0) {
                continue;
            }
            let aligned = tall.elements[index..index + len]
                .iter()
                .zip(&items)
                .all(|(item, (kind, _))| item.kind.matches(kind));
            if !aligned {
                continue;
            }
            // A page never spans a chapter boundary in the paginated layout.
            if tall.elements[index].chapter != tall.elements[index + len - 1].chapter {
                continue;
            }
            let distance = (index as f64 - expected).abs();
            if best.is_none_or(|(_, best_distance)| distance < best_distance) {
                best = Some((index, distance));
            }
        }
        best.map(|(index, _)| PageFlow {
            first_tall: index,
            last_tall: index + len - 1,
            first_rect: items[0].1,
            last_rect: items[len - 1].1,
        })
    }

    /// Extra blank rows to insert between two consecutive roll pages so the
    /// seam carries the document's real spacing. The roll already includes the
    /// leading above and below the page's own content, so only the difference
    /// between the true block spacing and that padding is added.
    fn seam_gap(&mut self, previous: usize, next: usize, baseline: Option<f32>, scale: f32) -> u32 {
        let Some(previous_flow) = self.map_page(previous) else {
            return 0;
        };
        let Some(next_flow) = self.map_page(next) else {
            return 0;
        };
        if previous_flow.last_tall + 1 != next_flow.first_tall {
            return 0;
        }
        let (Some(previous_metrics), Some(next_metrics)) =
            (self.roll_metrics(previous), self.roll_metrics(next))
        else {
            return 0;
        };
        let Some(tall) = self.tall_flow.as_ref() else {
            return 0;
        };
        let last = &tall.elements[previous_flow.last_tall];
        let first = &tall.elements[next_flow.first_tall];
        // A chapter starts a fresh tall page, so its top margin is already
        // visible in the cropped page and the two layouts share no coordinate
        // system to measure across.
        if last.chapter != first.chapter {
            return 0;
        }
        let true_gap = first.rect.y0 - last.rect.y1;
        if !true_gap.is_finite() {
            return 0;
        }
        let bottom_pad = (previous_metrics.bottom - previous_flow.last_rect.y1).max(0.0);
        let crop_top = if next_metrics.text_lines == 0 {
            next_metrics.top
        } else {
            baseline.map_or(next_metrics.top, |baseline| next_metrics.top.min(baseline))
        };
        let top_pad = (next_flow.first_rect.y0 - crop_top).max(0.0);
        let extra = (true_gap - bottom_pad - top_pad).max(0.0);
        // A real paragraph or figure margin is a few lines at most. Bound a
        // misaligned seam so it can never insert a page-sized blank band.
        let line = previous_metrics
            .line_height
            .or(next_metrics.line_height)
            .unwrap_or(self.font_size * 1.2);
        let extra = extra.min(line * 8.0);
        (extra * scale).round().max(0.0) as u32
    }

    /// Set the window color used behind the spread gutter.
    pub fn set_background(&mut self, background: u32) {
        self.background = background;
    }

    /// Sample page canvases across the whole book and interpolate one color.
    /// Each page is rendered at a tiny preview size, so this stays cheap even
    /// for long books. The target sample count is continuous, so adjacent
    /// integer counts are mixed instead of switching abruptly.
    fn sample_book_background(&self) -> u32 {
        let Some(count) = self.page_count else {
            return DEFAULT_WINDOW;
        };
        let budget = book_color::sample_budget(count);
        if budget <= 0.0 {
            return DEFAULT_WINDOW;
        }
        let lower = budget.floor().max(1.0) as usize;
        let upper = (lower + 1).min(count.max(1));
        let fraction = (budget - lower as f64).clamp(0.0, 1.0);
        let mut cache = BTreeMap::new();
        let low = self.sampled_canvas(count, lower, &mut cache);
        if fraction <= f64::EPSILON || upper <= lower {
            low
        } else {
            let high = self.sampled_canvas(count, upper, &mut cache);
            book_color::mix(low, high, fraction)
        }
    }

    /// Average the canvas colors of one sampling scheme across the book.
    fn sampled_canvas(
        &self,
        page_count: usize,
        samples: usize,
        cache: &mut BTreeMap<usize, u32>,
    ) -> u32 {
        let mut points = Vec::with_capacity(samples);
        for (page_no, position) in book_color::sample_pages(page_count, samples) {
            let color = *cache
                .entry(page_no)
                .or_insert_with(|| self.preview_color(page_no).unwrap_or(0x00ff_ffff));
            points.push((position, color));
        }
        book_color::soft_color(&points)
    }

    /// Render one page at preview size and reduce it to its canvas color.
    fn preview_color(&self, page_no: usize) -> Option<u32> {
        let page = self.doc.load_page(page_no as i32).ok()?;
        let bounds = page.bounds().ok()?;
        let (width, height) = rotated_size(&bounds, self.rotation);
        let scale = BOOK_SAMPLE_SIZE / width.max(height).max(1.0);
        let ctm = self.ctm_for(&bounds, scale);
        let pixmap = page
            .to_pixmap(&ctm, &Colorspace::device_rgb(), false, true)
            .ok()?;
        Some(sample_page_background(&bitmap_from_pixmap(&pixmap)))
    }

    pub fn clear_search(&mut self) {
        self.search_query = None;
    }

    pub fn outline(&self) -> &[OutlineEntry] {
        &self.outline
    }

    /// Render the outline as a fixed-row bitmap for a side panel. Text is shaped
    /// by native HarfBuzz with `fontdb` fallback and rasterized with `ab_glyph`;
    /// no document or HTML engine is involved, so row metrics stay independent
    /// of the format.
    #[allow(clippy::too_many_arguments)]
    pub fn render_outline(
        &mut self,
        width: u32,
        row_height: u32,
        start: usize,
        count: usize,
        selected: Option<usize>,
        veil: Option<u32>,
        rail: Option<u32>,
        style: OutlineStyle,
    ) -> Result<Option<PageBitmap>, String> {
        if self.outline.is_empty() {
            return Ok(None);
        }
        let width = width.max(160);
        let row_height = row_height.max(16);
        let start = start.min(self.outline.len());
        let count = count
            .max(1)
            .min(self.outline.len().saturating_sub(start))
            .min((MAX_DIMENSION / row_height as f32) as usize);
        if count == 0 {
            return Ok(None);
        }
        let height = row_height * count as u32;
        let mut pixels = vec![style.panel; width as usize * height as usize];

        if self.outline_text.is_none() {
            self.outline_text = Some(TextEngine::new(self.fonts.clone())?);
        }
        let outline = &self.outline;
        let engine = self
            .outline_text
            .as_mut()
            .expect("outline text engine was initialized");

        for (index, entry) in outline[start..start + count].iter().enumerate() {
            let indent = entry.depth.saturating_mul(14).min(width as usize / 2);
            let highlighted = selected == Some(start + index);
            let row_top = (index * row_height as usize) as u32;
            // Paint the selection wash before the text so the glyphs keep
            // their own color and only become brighter.
            if highlighted {
                if let Some(veil) = veil {
                    fill_bitmap_rect(
                        &mut pixels,
                        width,
                        height,
                        0,
                        row_top,
                        width,
                        row_height,
                        veil,
                    );
                }
                if let Some(rail) = rail {
                    fill_bitmap_rect(
                        &mut pixels,
                        width,
                        height,
                        1,
                        row_top,
                        OUTLINE_RAIL_WIDTH,
                        row_height,
                        rail,
                    );
                }
            }
            let (r, g, b) = rgb8(if highlighted {
                // Selection is brightness, not a hue change.
                style.selected_text
            } else if entry.depth == 0 {
                style.text
            } else {
                style.muted
            });
            let title = truncate_outline_title(&entry.title, width, indent + OUTLINE_TEXT_PADDING);
            let row_top = row_top as i32;
            let mut target = TextTarget::new(&mut pixels, width, height, row_top, row_height);
            engine.draw_line(
                &title,
                14.0,
                if highlighted || entry.depth == 0 {
                    Weight::BOLD
                } else {
                    Weight::NORMAL
                },
                &mut target,
                (indent + OUTLINE_TEXT_PADDING) as i32,
                row_top,
                row_height,
                (r, g, b),
            );
        }

        Ok(Some(PageBitmap {
            width,
            height,
            pixels,
        }))
    }

    pub fn zoom_by(&mut self, factor: f32) {
        if factor.is_finite() && factor > 0.0 {
            self.fit = Fit::None;
            self.scale = (self.scale * factor).clamp(MIN_SCALE, MAX_SCALE);
            self.scroll_scale = None;
            self.page_cache.clear();
        }
    }

    #[cfg(test)]
    pub fn set_scale(&mut self, scale: f32) {
        if scale.is_finite() && scale > 0.0 {
            self.fit = Fit::None;
            self.scale = scale.clamp(MIN_SCALE, MAX_SCALE);
            self.scroll_scale = None;
            self.page_cache.clear();
        }
    }

    /// Restore the initial fit-to-page layout after an explicit zoom.
    pub fn fit_page(&mut self) {
        self.fit = Fit::Page;
        self.search_query = None;
        self.scroll_scale = None;
        self.page_cache.clear();
    }

    pub fn rotate(&mut self, delta: i32) {
        self.rotation = ((self.rotation as i32 + delta).rem_euclid(360)) as u16;
        self.scroll_scale = None;
        self.page_cache.clear();
    }

    /// Render the current page at the current viewport size.
    pub fn render(&mut self) -> Result<RenderedPage, String> {
        let query = self.search_query.clone();
        let quads = match query {
            Some(query) => self
                .doc
                .load_page(self.page as i32)
                .and_then(|page| page.search(&query, 64))
                .map(|quads| quads.to_vec())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        self.render_with_quads(&quads)
    }

    /// Search the current page or the whole book, starting at the current page
    /// and wrapping around. Returns a rendered page with highlights when a
    /// match is found.
    pub fn search(
        &mut self,
        needle: &str,
        scope: SearchScope,
        cancel: &AtomicBool,
    ) -> Result<Option<RenderedPage>, String> {
        if needle.is_empty() {
            return Ok(None);
        }
        let start = self.page;
        match scope {
            SearchScope::Page => {
                let quads = self.search_page(start, needle)?;
                if quads.is_empty() {
                    return Ok(None);
                }
                self.page = start;
                self.search_query = Some(needle.to_string());
                self.render_with_quads(&quads).map(Some)
            }
            SearchScope::Book => {
                if let Some(count) = self.page_count {
                    for offset in 0..count {
                        if cancel.load(Ordering::Relaxed) {
                            return Ok(None);
                        }
                        let page_no = (start + offset) % count;
                        let quads = self.search_page(page_no, needle)?;
                        if !quads.is_empty() {
                            self.page = page_no;
                            self.search_query = Some(needle.to_string());
                            return self.render_with_quads(&quads).map(Some);
                        }
                    }
                    return Ok(None);
                }

                // The page count is normally known. This fallback still
                // walks forward and wraps for handlers that cannot report it.
                let mut page_no = start;
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        return Ok(None);
                    }
                    match self.search_page(page_no, needle) {
                        Ok(quads) if !quads.is_empty() => {
                            self.page = page_no;
                            self.search_query = Some(needle.to_string());
                            return self.render_with_quads(&quads).map(Some);
                        }
                        Ok(_) => page_no += 1,
                        Err(e) if page_no == start => return Err(e),
                        Err(_) => break,
                    }
                }
                for page_no in 0..start {
                    if cancel.load(Ordering::Relaxed) {
                        return Ok(None);
                    }
                    let quads = self.search_page(page_no, needle)?;
                    if !quads.is_empty() {
                        self.page = page_no;
                        self.search_query = Some(needle.to_string());
                        return self.render_with_quads(&quads).map(Some);
                    }
                }
                Ok(None)
            }
        }
    }

    fn search_page(&self, page_no: usize, needle: &str) -> Result<Vec<Quad>, String> {
        let page = self
            .doc
            .load_page(page_no as i32)
            .map_err(|e| format!("load page {}: {e}", page_no + 1))?;
        page.search(needle, 64)
            .map(|quads| quads.to_vec())
            .map_err(|e| format!("search page {}: {e}", page_no + 1))
    }

    /// Reading column width used by continuous scroll, excluding the side
    /// margins that keep the page clear of the window edges.
    fn scroll_available_width(&self) -> f32 {
        self.viewport
            .0
            .saturating_sub(scroll_side_margin(self.viewport.0).saturating_mul(2))
            .max(1) as f32
    }

    fn scale_for(&mut self, bounds: &Rect) -> Result<f32, String> {
        let (rotated_w, rotated_h) = rotated_size(bounds, self.rotation);

        let available_w = if self.scroll {
            self.scroll_available_width()
        } else {
            self.viewport
                .0
                .saturating_sub(MARGIN.saturating_mul(2))
                .max(1) as f32
        };
        let available_h = self
            .viewport
            .1
            .saturating_sub(MARGIN.saturating_mul(2))
            .max(1) as f32;
        // A spread fits two pages and the gutter between them. Reserving a
        // proportional slice for the gap keeps the gutter visible at every
        // scale instead of letting it collapse once the pages fill the width.
        let available_w = if self.spread && self.fit != Fit::None {
            (available_w / (2.0 + SPREAD_GAP_RATIO)).max(1.0)
        } else {
            available_w
        };

        let mut scale = match self.fit {
            // Continuous scroll reads at page width, not page height, so the
            // content flows naturally under the viewport.
            Fit::Page if self.scroll => {
                // Mixed page sizes must not re-zoom the column when the window
                // slides. Reuse the first fit computed for this scroll session
                // and keep the per-page caps below as a safety net.
                if let Some(locked) = self.scroll_scale {
                    locked
                } else {
                    let fitted = (available_w / rotated_w).min(
                        SCROLL_STRIP_MAX_HEIGHT / self.scroll_strip_pages() as f32 / rotated_h,
                    );
                    self.scroll_scale = Some(fitted);
                    fitted
                }
            }
            Fit::Page => fit_page_scale(rotated_w, rotated_h, available_w, available_h),
            Fit::None => self.scale,
        };
        // Do not impose the zoom lower bound here. A very long image can
        // legitimately need a scale far below it to fit the whole page.
        scale = scale.min(MAX_SCALE);
        scale = scale
            .min(MAX_DIMENSION / rotated_w)
            .min(MAX_DIMENSION / rotated_h);
        scale = scale.min((MAX_PIXELS / (rotated_w * rotated_h)).sqrt());
        if self.scroll {
            // The strip is taller than one page. Keep it inside both the GPU
            // texture edge and a single page's pixel budget.
            let count = self.scroll_strip_pages() as f32;
            scale = scale
                .min(MAX_DIMENSION / (count * rotated_h))
                .min((MAX_PIXELS / (rotated_w * rotated_h * count)).sqrt());
        }
        if !scale.is_finite() || scale <= 0.0 {
            Err("invalid page scale".to_string())
        } else {
            Ok(scale)
        }
    }

    fn ctm_for(&self, bounds: &Rect, scale: f32) -> Matrix {
        let base = Matrix::new_scale(scale, scale) * Matrix::new_rotate(self.rotation as f32);
        let corners = [
            Point::new(bounds.x0, bounds.y0),
            Point::new(bounds.x1, bounds.y0),
            Point::new(bounds.x0, bounds.y1),
            Point::new(bounds.x1, bounds.y1),
        ];
        let mut min_x = f32::INFINITY;
        let mut min_y = f32::INFINITY;
        for corner in corners {
            let p = corner.transform(&base);
            min_x = min_x.min(p.x);
            min_y = min_y.min(p.y);
        }
        base * Matrix::new_translate(-min_x, -min_y)
    }

    /// Choose the render scale and transform for one page in the current strip.
    ///
    /// MuPDF's reflow engine places a standalone illustration on a synthetic
    /// 612x792 page and pads the rest with canvas. Rendering that page as-is
    /// shows the image inside a simulated page with white margins. In
    /// continuous scroll, crop to the illustration and render it at the
    /// reading column width instead, so tall plates keep their aspect ratio
    /// and flow through the viewport instead of being forced into a page.
    fn page_plan(
        &mut self,
        page_no: usize,
        base_scale: f32,
        max_image_height: f32,
    ) -> Result<PagePlan, String> {
        let bounds = self
            .doc
            .load_page(page_no as i32)
            .and_then(|page| page.bounds())
            .map_err(|e| format!("page bounds: {e}"))?;

        // In continuous scroll the reflowable story is rendered cropped to its
        // content: synthetic page margins never become visible gaps, and image
        // plates keep the reading-column width of the roll.
        if self.roll() {
            if let Some(metrics) = self.roll_metrics(page_no) {
                // Crop text pages at the shared page-margin baseline, but
                // never below content that starts higher (an illustration at
                // the page top). Real paragraph and chapter margins sit below
                // the baseline and therefore remain visible. A standalone
                // plate keeps its own bounds so its aspect ratio stays true.
                let top = if metrics.text_lines == 0 {
                    metrics.top
                } else {
                    self.roll_top_baseline
                        .map_or(metrics.top, |baseline| metrics.top.min(baseline))
                };
                let (rotated_w, rotated_h) = rotated_size(&bounds, self.rotation);
                let content_w = (bounds.x1 - bounds.x0).max(1.0);
                let content_h = (metrics.bottom - top).max(1.0);
                if rotated_w > 0.0 && rotated_h > 0.0 {
                    let scale = if metrics.text_lines == 0 && self.fit == Fit::Page {
                        // An image-only page: fill the reading column like a
                        // plate while keeping the locked text scale as a floor.
                        let plate_w = metrics
                            .plate_bounds
                            .map(|rect| rect.width().max(1.0))
                            .unwrap_or(content_w);
                        (self.scroll_available_width() / plate_w)
                            .min(base_scale * IMAGE_PAGE_MAX_UPSCALE)
                    } else {
                        base_scale
                    }
                    .min(MAX_SCALE)
                    .min(MAX_DIMENSION / content_w)
                    .min(MAX_DIMENSION / content_h)
                    .min((MAX_PIXELS / (content_w * content_h)).sqrt())
                    .min(max_image_height / content_h);
                    let (rect_x0, rect_x1) = metrics
                        .plate_bounds
                        .map(|rect| (rect.x0, rect.x1))
                        .unwrap_or((bounds.x0, bounds.x1));
                    if scale.is_finite() && scale > 0.0 {
                        let ctm = self.ctm_for(&bounds, scale);
                        let rect = Rect::new(rect_x0, top, rect_x1, metrics.bottom);
                        if let Some(crop) = crop_for_image(&rect, &ctm, rotated_w, rotated_h, scale)
                        {
                            return Ok(PagePlan {
                                scale,
                                ctm,
                                crop: Some(crop),
                                height: (content_h * scale).round().max(1.0),
                                plate: metrics.text_lines == 0,
                            });
                        }
                    }
                }
            }
        }

        // A quarter-turned plate keeps the original behavior: fit the reading
        // column, cropped to the picture, because rotating swaps the axes and
        // vertical trim no longer matches the scroll direction.
        if self.scroll && self.reflowable && self.fit == Fit::Page && !self.roll() {
            if let Some(metrics) = self.roll_metrics(page_no) {
                if metrics.text_lines == 0 {
                    let image = metrics.plate_bounds.unwrap_or_else(|| {
                        Rect::new(bounds.x0, metrics.top, bounds.x1, metrics.bottom)
                    });
                    let (rotated_w, rotated_h) = rotated_size(&bounds, self.rotation);
                    let (image_w, image_h) = rotated_size(&image, self.rotation);
                    if image_w > 0.0 && image_h > 0.0 && rotated_w > 0.0 && rotated_h > 0.0 {
                        let scale = (self.scroll_available_width() / image_w)
                            .min(base_scale * IMAGE_PAGE_MAX_UPSCALE)
                            .min(MAX_SCALE)
                            .min(MAX_DIMENSION / rotated_w)
                            .min(MAX_DIMENSION / rotated_h)
                            .min((MAX_PIXELS / (rotated_w * rotated_h)).sqrt())
                            .min(max_image_height / rotated_h);
                        if scale.is_finite() && scale > 0.0 {
                            let ctm = self.ctm_for(&bounds, scale);
                            if let Some(crop) =
                                crop_for_image(&image, &ctm, rotated_w, rotated_h, scale)
                            {
                                return Ok(PagePlan {
                                    scale,
                                    ctm,
                                    crop: Some(crop),
                                    height: (image_h * scale).round().max(1.0),
                                    plate: true,
                                });
                            }
                        }
                    }
                }
            }
        }

        Ok(PagePlan {
            scale: base_scale,
            ctm: self.ctm_for(&bounds, base_scale),
            crop: None,
            height: {
                let (_, page_h) = rotated_size(&bounds, self.rotation);
                (page_h * base_scale).round().max(1.0)
            },
            plate: false,
        })
    }

    fn render_one(
        &self,
        page_no: usize,
        ctm: &Matrix,
        crop: Option<BitmapCrop>,
        quads: &[Quad],
    ) -> Result<PageRender, String> {
        let page = self
            .doc
            .load_page(page_no as i32)
            .map_err(|e| format!("load page {}: {e}", page_no + 1))?;
        let pix = page
            .to_pixmap(ctm, &Colorspace::device_rgb(), false, true)
            .map_err(|e| format!("render page {}: {e}", page_no + 1))?;

        // The pixmap may start at a non-zero device coordinate; bitmap index 0
        // corresponds to `pix.x()`/`pix.y()`.
        let crop_bitmap_region = crop.map(|crop| {
            let x = crop.x as i64 - pix.x() as i64;
            let y = crop.y as i64 - pix.y() as i64;
            let x = x.clamp(0, pix.width().saturating_sub(1) as i64) as u32;
            let y = y.clamp(0, pix.height().saturating_sub(1) as i64) as u32;
            let width = crop.width.min(pix.width() - x).max(1);
            let height = crop.height.min(pix.height() - y).max(1);
            BitmapCrop {
                x,
                y,
                width,
                height,
            }
        });

        let full = bitmap_from_pixmap(&pix);
        let bitmap = match crop_bitmap_region {
            Some(region) => crop_bitmap(&full, region),
            None => full,
        };
        // Content coordinates follow the bitmap after a crop. Shifting the
        // transform is equivalent to translating every finished rectangle,
        // and keeps highlights, links, and text clipping in one place.
        let content_ctm = match crop {
            Some(crop) => ctm.clone() * Matrix::new_translate(-(crop.x as f32), -(crop.y as f32)),
            None => ctm.clone(),
        };
        let width = bitmap.width;
        let height = bitmap.height;
        let highlights = quads
            .iter()
            .filter_map(|quad| highlight_from_quad(quad, &content_ctm, width, height))
            .collect();
        let text = page
            .to_text_page(TextPageFlags::PRESERVE_WHITESPACE)
            .map(|text_page| text_layer_from_page(&text_page, &content_ctm, width, height))
            .unwrap_or_default();
        let links = page
            .links()
            .map(|links| {
                links
                    .filter_map(|link| {
                        let bounds = link_rect(&link.bounds, &content_ctm, width, height)?;
                        let target = link
                            .dest
                            .and_then(|dest| usize::try_from(dest.loc.page_number).ok())
                            .map(|page| LinkTarget::Page(page + 1))
                            .or_else(|| {
                                (!link.uri.is_empty()).then_some(LinkTarget::Uri(link.uri))
                            });
                        Some(RenderedLink { bounds, target })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(PageRender {
            bitmap,
            highlights,
            links,
            text,
        })
    }

    fn page_numbers_for(&self) -> Vec<usize> {
        if self.scroll {
            // The strip is a sliding window around the anchor. `scroll_before`
            // decides how much sits above it, so scrolling up and down both
            // keep lookahead. An explicit zoom renders fewer pages so the
            // vertical strip stays within texture limits.
            let count = self.scroll_strip_pages();
            let before = self.scroll_before.min(count.saturating_sub(1));
            let start = self.page.saturating_sub(before);
            (start..start + count).collect()
        } else if self.spread && self.fit != Fit::None {
            let start = self.page.saturating_sub(1);
            vec![start, start + 1, start + 2, start + 3]
        } else {
            vec![self.page]
        }
    }

    /// Number of contiguous pages in the scroll strip. Auto-fit can afford the
    /// full window; explicit zoom keeps three pages (one behind, one ahead).
    fn scroll_strip_pages(&self) -> usize {
        if self.fit == Fit::Page {
            SCROLL_STRIP_PAGES
        } else {
            3
        }
    }

    fn render_with_quads(&mut self, quads: &[Quad]) -> Result<RenderedPage, String> {
        let page_numbers = self.page_numbers_for();
        let cacheable = quads.is_empty() && self.search_query.is_none();
        // Blank reflow pages are skipped in paged layouts, but a scroll strip
        // used to keep them as contiguous viewport chunks. Continuous scroll
        // instead treats the chapter as one flow, so blank pages disappear
        // from the strip entirely and no blank band appears between blocks.
        if self.roll() {
            let filtered: Vec<usize> = page_numbers
                .iter()
                .copied()
                .filter(|&page_no| self.roll_metrics(page_no).is_some())
                .collect();
            if !filtered.is_empty() {
                return self.render_pages(&filtered, quads, cacheable);
            }
        } else if self.reflowable && !self.scroll {
            let filtered: Vec<usize> = page_numbers
                .iter()
                .copied()
                .filter(|&page_no| !self.page_is_blank(page_no))
                .collect();
            if !filtered.is_empty() {
                return self.render_pages(&filtered, quads, cacheable);
            }
        }
        self.render_pages(&page_numbers, quads, cacheable)
    }

    fn render_pages(
        &mut self,
        page_numbers: &[usize],
        quads: &[Quad],
        cacheable: bool,
    ) -> Result<RenderedPage, String> {
        let first_no = *page_numbers
            .first()
            .ok_or_else(|| "no page to render".to_string())?;

        // Collapse duplicate or out-of-range page numbers before rendering.
        let mut numbers = vec![first_no];
        let mut pages = vec![first_no + 1];
        for &page_no in page_numbers.iter().skip(1) {
            if self.page_count.is_some_and(|count| page_no >= count) {
                break;
            }
            if numbers.contains(&page_no) {
                continue;
            }
            numbers.push(page_no);
            pages.push(page_no + 1);
        }

        // Scale from the page being read, not the page that happens to start
        // the strip. In scroll mode the focus is often the second page.
        let scale_no = if self.scroll { self.page } else { first_no };
        let scale_bounds = self
            .doc
            .load_page(scale_no as i32)
            .and_then(|page| page.bounds())
            .map_err(|e| format!("page bounds: {e}"))?;
        let scale = self.scale_for(&scale_bounds)?;
        self.scale = scale;

        // The roll removes one shared band of simulated page margin. Measure
        // it from this strip before planning any page.
        if self.roll() {
            self.observe_roll_baseline(&numbers);
        }

        // Search highlights apply to the page being read, which may not be the
        // first page of a scroll strip. Normal reading reuses cached pages.
        let mut plans = Vec::with_capacity(numbers.len());
        for &page_no in &numbers {
            plans.push(self.page_plan(page_no, scale, IMAGE_PAGE_MAX_HEIGHT)?);
        }
        // The roll's seam bands are measured before the texture budget is
        // checked: they are real content spacing, not slack that may be
        // dropped. Paged strips keep one uniform gutter instead.
        let seam_gaps: Vec<u32> = if self.roll() {
            let baseline = self.roll_top_baseline;
            numbers
                .windows(2)
                .map(|pair| self.seam_gap(pair[0], pair[1], baseline, scale))
                .collect()
        } else {
            Vec::new()
        };
        // Illustrations are much taller than text pages. If a strip of them
        // would exceed the texture height limit, shrink only the image pages
        // so their total still fits; text pages keep the locked column scale.
        if self.scroll && numbers.len() > 1 {
            let gap = if self.roll() {
                0.0
            } else {
                scroll_gap(self.scroll_available_width().round() as u32) as f32
            };
            let seam_total = if self.roll() {
                seam_gaps.iter().sum::<u32>() as f32
            } else {
                gap * (numbers.len() - 1) as f32
            };
            let total: f32 = plans.iter().map(|plan| plan.height).sum::<f32>() + seam_total;
            if total > SCROLL_STRIP_MAX_HEIGHT {
                let image_total: f32 = plans
                    .iter()
                    .filter(|plan| plan.plate)
                    .map(|plan| plan.height)
                    .sum();
                if image_total > 0.0 {
                    let other_total = total - image_total;
                    let budget = (SCROLL_STRIP_MAX_HEIGHT - other_total).max(1.0);
                    let factor = (budget / image_total).min(1.0);
                    for (index, &page_no) in numbers.iter().enumerate() {
                        if plans[index].plate {
                            let cap = plans[index].height * factor;
                            plans[index] = self.page_plan(page_no, scale, cap)?;
                        }
                    }
                }
            }
        }

        let mut direct = Vec::new();
        let mut page_scales = Vec::with_capacity(numbers.len());
        for (index, &page_no) in numbers.iter().enumerate() {
            let plan = &plans[index];
            let page_scale = plan.scale;
            page_scales.push(page_scale);
            if cacheable
                && self.page_cache.iter().any(|(cached, cached_scale, _)| {
                    *cached == page_no && (*cached_scale - page_scale).abs() < 1.0e-4
                })
            {
                continue;
            }
            let page_quads = if page_no == self.page { quads } else { &[] };
            #[cfg(test)]
            {
                self.page_renders += 1;
            }
            let rendered = self.render_one(page_no, &plan.ctm, plan.crop, page_quads)?;
            if cacheable {
                self.page_cache.retain(|(cached, _, _)| *cached != page_no);
                self.make_room_for_page(&numbers);
                self.page_cache.push((page_no, page_scale, rendered));
            } else {
                direct.push(rendered);
            }
        }

        let rendered: Vec<&PageRender> = if cacheable {
            let mut refs = Vec::with_capacity(numbers.len());
            for (index, page_no) in numbers.iter().enumerate() {
                let page_scale = page_scales[index];
                let Some((_, _, page)) =
                    self.page_cache.iter().find(|(cached, cached_scale, _)| {
                        cached == page_no && (*cached_scale - page_scale).abs() < 1.0e-4
                    })
                else {
                    return Err("page cache missing a rendered page".to_string());
                };
                refs.push(page);
            }
            refs
        } else {
            direct.iter().collect()
        };

        let page_widths: Vec<u32> = rendered.iter().map(|page| page.bitmap.width).collect();
        // Blank reflow pages may have been filtered out, so the current page
        // is not necessarily at `self.page - first_no`. Find its actual slot.
        let focus_index = pages
            .iter()
            .position(|&page| page == self.page + 1)
            .unwrap_or(0);
        let (bitmap, page_offsets, page_heights, gap) = if self.scroll {
            // Continuous scroll packs the reflowable story tightly; paged
            // spreads and fixed-layout documents keep a small strip gap.
            let gap = if self.roll() {
                0
            } else {
                scroll_gap(page_widths.get(focus_index).copied().unwrap_or(1))
            };
            let seam_gaps: Vec<u32> = if self.roll() {
                seam_gaps
            } else {
                vec![gap; rendered.len().saturating_sub(1)]
            };
            let strip = compose_pages_vertical(&rendered, &seam_gaps, self.background);
            (strip.page, strip.offsets, strip.heights, gap)
        } else {
            let gap = if page_widths.len() > 1 {
                spread_gap(page_widths[focus_index])
            } else {
                0
            };
            (
                compose_pages(&rendered, gap, self.background),
                vec![0; page_widths.len()],
                rendered.iter().map(|page| page.bitmap.height).collect(),
                gap,
            )
        };

        // The roll measures the document's own line rhythm so the UI can
        // step it one line at a time. This is a property of the reflow layout,
        // so it also applies to an explicit zoom where pages stay page-shaped.
        let line_height = if self.scroll && self.reflowable {
            let mut heights: Vec<f32> = numbers
                .iter()
                .filter_map(|&page_no| {
                    self.roll_metrics(page_no)
                        .and_then(|metrics| metrics.line_height)
                })
                .collect();
            heights.sort_by(f32::total_cmp);
            (!heights.is_empty()).then(|| heights[heights.len() / 2])
        } else {
            None
        };

        Ok(RenderedPage {
            bitmap: bitmap.bitmap,
            page: self.page + 1,
            pages,
            page_widths,
            gap,
            page_count: self.page_count,
            scale,
            fit: self.fit,
            rotation: self.rotation,
            highlights: bitmap.highlights,
            links: bitmap.links,
            text: bitmap.text,
            book_background: self.book_background,
            scroll: self.scroll,
            page_offsets,
            page_heights,
            font_scale: self.font_scale(),
            reflowable: self.reflowable,
            line_height,
        })
    }

    fn cache_capacity(&self) -> usize {
        if self.scroll {
            // A scroll strip references every page in its window at once.
            self.scroll_strip_pages()
        } else if self.spread && self.fit != Fit::None {
            PAGE_CACHE_CAPACITY
        } else {
            PAGE_CACHE_SINGLE_CAPACITY
        }
    }

    /// Make room for a page without evicting any page the current strip needs.
    fn make_room_for_page(&mut self, keep: &[usize]) {
        let capacity = self.cache_capacity();
        if self.page_cache.len() < capacity {
            return;
        }
        let center = self.page;
        let mut victim = None;
        let mut farthest = 0;
        for (index, (page_no, _, _)) in self.page_cache.iter().enumerate() {
            let distance = page_no.abs_diff(center);
            if !keep.contains(page_no) && (victim.is_none() || distance >= farthest) {
                victim = Some(index);
                farthest = distance;
            }
        }
        if victim.is_none() {
            for (index, (page_no, _, _)) in self.page_cache.iter().enumerate() {
                let distance = page_no.abs_diff(center);
                if victim.is_none() || distance >= farthest {
                    victim = Some(index);
                    farthest = distance;
                }
            }
        }
        if let Some(index) = victim {
            self.page_cache.remove(index);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fill_bitmap_rect(
    pixels: &mut [u32],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    color: u32,
) {
    let x1 = x.saturating_add(w).min(width);
    let y1 = y.saturating_add(h).min(height);
    for row in y..y1 {
        let start = row as usize * width as usize + x.min(width) as usize;
        let end = row as usize * width as usize + x1 as usize;
        pixels[start..end].fill(color);
    }
}

fn compose_pages(pages: &[&PageRender], gap: u32, background: u32) -> PageRender {
    let width = pages
        .iter()
        .map(|page| page.bitmap.width)
        .sum::<u32>()
        .saturating_add(gap.saturating_mul(pages.len().saturating_sub(1) as u32));
    let height = pages
        .iter()
        .map(|page| page.bitmap.height)
        .max()
        .unwrap_or(0);
    let mut pixels = vec![background; width as usize * height as usize];

    let mut highlights = Vec::new();
    let mut links = Vec::new();
    let mut text = TextLayer::default();
    let mut x = 0;
    for page in pages {
        blit_bitmap(&mut pixels, width, &page.bitmap, x, 0);
        for rect in &page.highlights {
            highlights.push(HighlightRect {
                x0: rect.x0 + x,
                x1: rect.x1 + x,
                ..*rect
            });
        }
        for link in &page.links {
            let mut link = link.clone();
            link.bounds.x0 += x;
            link.bounds.x1 += x;
            links.push(link);
        }
        let char_offset = text.chars.len();
        let line_offset = text.lines.len();
        for ch in &page.text.chars {
            text.chars.push(TextChar {
                rect: HighlightRect {
                    x0: ch.rect.x0 + x,
                    x1: ch.rect.x1 + x,
                    ..ch.rect
                },
                line: ch.line + line_offset,
                ..*ch
            });
        }
        for line in &page.text.lines {
            text.lines.push(TextLine {
                start: line.start + char_offset,
                end: line.end + char_offset,
                rect: HighlightRect {
                    x0: line.rect.x0 + x,
                    x1: line.rect.x1 + x,
                    ..line.rect
                },
            });
        }
        x += page.bitmap.width + gap;
    }
    PageRender {
        bitmap: PageBitmap {
            width,
            height,
            pixels,
        },
        highlights,
        links,
        text,
    }
}
struct VerticalStrip {
    page: PageRender,
    offsets: Vec<u32>,
    heights: Vec<u32>,
}

impl PageRender {
    fn translated(&self, x: u32, y: u32, char_offset: usize, line_offset: usize) -> TranslatedPage {
        let highlight = |rect: &HighlightRect| HighlightRect {
            x0: rect.x0 + x,
            y0: rect.y0 + y,
            x1: rect.x1 + x,
            y1: rect.y1 + y,
        };
        let mut text = TextLayer::default();
        for ch in &self.text.chars {
            text.chars.push(TextChar {
                rect: highlight(&ch.rect),
                line: ch.line + line_offset,
                ..*ch
            });
        }
        for line in &self.text.lines {
            text.lines.push(TextLine {
                start: line.start + char_offset,
                end: line.end + char_offset,
                rect: highlight(&line.rect),
            });
        }
        let mut links = Vec::with_capacity(self.links.len());
        for link in &self.links {
            let mut link = link.clone();
            link.bounds = highlight(&link.bounds);
            links.push(link);
        }
        TranslatedPage {
            highlights: self.highlights.iter().map(highlight).collect(),
            links,
            text,
        }
    }
}

struct TranslatedPage {
    highlights: Vec<HighlightRect>,
    links: Vec<RenderedLink>,
    text: TextLayer,
}

/// Stack pages vertically with a gap. Every page is centered in the strip
/// width, and all geometry is translated so highlights, links, and text hit
/// testing keep working in strip coordinates.
///
/// `gaps` holds one blank band per seam (`pages.len() - 1` entries). The roll
/// uses a measured band at each seam so paragraph and figure margins survive
/// pagination; paged strips pass one uniform gutter.
fn compose_pages_vertical(pages: &[&PageRender], gaps: &[u32], background: u32) -> VerticalStrip {
    let width = pages
        .iter()
        .map(|page| page.bitmap.width)
        .max()
        .unwrap_or(0);
    let mut offsets = Vec::with_capacity(pages.len());
    let mut lefts = Vec::with_capacity(pages.len());
    let mut heights = Vec::with_capacity(pages.len());
    let mut y = 0u32;
    for (index, page) in pages.iter().enumerate() {
        offsets.push(y);
        lefts.push((width.saturating_sub(page.bitmap.width)) / 2);
        heights.push(page.bitmap.height);
        y = y
            .saturating_add(page.bitmap.height)
            .saturating_add(gaps.get(index).copied().unwrap_or(0));
    }
    let height = y.saturating_sub(
        gaps.get(pages.len().saturating_sub(1))
            .copied()
            .unwrap_or(0),
    );
    let mut pixels = vec![background; width as usize * height as usize];

    let mut highlights = Vec::new();
    let mut links = Vec::new();
    let mut text = TextLayer::default();
    for (index, page) in pages.iter().enumerate() {
        let x = lefts[index];
        let y = offsets[index];
        blit_bitmap(&mut pixels, width, &page.bitmap, x, y);
        let char_offset = text.chars.len();
        let line_offset = text.lines.len();
        let page = page.translated(x, y, char_offset, line_offset);
        highlights.extend(page.highlights);
        links.extend(page.links);
        text.chars.extend(page.text.chars);
        text.lines.extend(page.text.lines);
    }

    VerticalStrip {
        page: PageRender {
            bitmap: PageBitmap {
                width,
                height,
                pixels,
            },
            highlights,
            links,
            text,
        },
        offsets,
        heights,
    }
}

fn blit_bitmap(dst: &mut [u32], dst_width: u32, src: &PageBitmap, x: u32, y: u32) {
    if src.width == 0 || src.height == 0 {
        return;
    }
    let dx = x as usize;
    let dy = y as usize;
    let dst_width = dst_width as usize;
    let src_width = src.width as usize;
    for row in 0..src.height as usize {
        let src_start = row * src_width;
        let dst_start = (dy + row) * dst_width + dx;
        dst[dst_start..dst_start + src_width]
            .copy_from_slice(&src.pixels[src_start..src_start + src_width]);
    }
}

/// Flatten a structured text page into the document's reading order. This is
/// the common vocabulary used to align a paginated page with the tall flow.
fn flow_elements(page: &TextPage) -> Vec<(FlowElement, Rect)> {
    let mut items = Vec::new();
    for block in page.blocks() {
        match block.r#type() {
            mupdf::text_page::TextBlockType::Text => {
                for line in block.lines() {
                    let text: String = line.chars().filter_map(|ch| ch.char()).collect();
                    let text = text.trim();
                    if text.is_empty() {
                        continue;
                    }
                    let bounds = line.bounds();
                    if bounds.is_valid() && !bounds.is_empty() {
                        items.push((FlowElement::Text(text.to_string()), bounds));
                    }
                }
            }
            mupdf::text_page::TextBlockType::Image => {
                let bounds = block.bounds();
                if bounds.is_valid() && !bounds.is_empty() {
                    items.push((FlowElement::Image, bounds));
                }
            }
            _ => {
                let bounds = block.bounds();
                if bounds.is_valid() && !bounds.is_empty() {
                    items.push((FlowElement::Shape, bounds));
                }
            }
        }
    }
    items
}

fn text_layer_from_page(page: &TextPage, ctm: &Matrix, width: u32, height: u32) -> TextLayer {
    let mut layer = TextLayer::default();
    for block in page.structured().blocks {
        let TextBlockContent::Text { lines } = block.content else {
            continue;
        };
        for line in lines {
            let line_index = layer.lines.len();
            let start = layer.chars.len();
            let mut line_rect: Option<HighlightRect> = None;
            for ch in line.chars {
                let Some(rect) = text_rect_from_quad(&ch.quad, ctm, width, height) else {
                    continue;
                };
                layer.chars.push(TextChar {
                    ch: ch.ch,
                    rect,
                    line: line_index,
                });
                line_rect = Some(match line_rect {
                    Some(existing) => union_rect(existing, rect),
                    None => rect,
                });
            }
            if let Some(rect) = line_rect {
                layer.lines.push(TextLine {
                    start,
                    end: layer.chars.len(),
                    rect,
                });
            }
        }
    }
    layer
}

fn link_rect(bounds: &Rect, ctm: &Matrix, width: u32, height: u32) -> Option<HighlightRect> {
    let corners = [
        Point::new(bounds.x0, bounds.y0),
        Point::new(bounds.x1, bounds.y0),
        Point::new(bounds.x0, bounds.y1),
        Point::new(bounds.x1, bounds.y1),
    ];
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for corner in corners {
        let point = corner.transform(ctm);
        min_x = min_x.min(point.x);
        min_y = min_y.min(point.y);
        max_x = max_x.max(point.x);
        max_y = max_y.max(point.y);
    }
    let x0 = min_x.max(0.0).min(width as f32) as u32;
    let y0 = min_y.max(0.0).min(height as f32) as u32;
    let x1 = max_x.max(0.0).min(width as f32) as u32;
    let y1 = max_y.max(0.0).min(height as f32) as u32;
    (x0 < x1 && y0 < y1).then_some(HighlightRect { x0, y0, x1, y1 })
}

fn text_rect_from_quad(
    quad: &Quad,
    ctm: &Matrix,
    width: u32,
    height: u32,
) -> Option<HighlightRect> {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for point in [quad.ul, quad.ur, quad.ll, quad.lr] {
        let p = point.transform(ctm);
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    let x0 = min_x.floor().clamp(0.0, width as f32) as u32;
    let y0 = min_y.floor().clamp(0.0, height as f32) as u32;
    let x1 = max_x.ceil().clamp(0.0, width as f32) as u32;
    let y1 = max_y.ceil().clamp(0.0, height as f32) as u32;
    Some(HighlightRect { x0, y0, x1, y1 })
}

fn union_rect(a: HighlightRect, b: HighlightRect) -> HighlightRect {
    HighlightRect {
        x0: a.x0.min(b.x0),
        y0: a.y0.min(b.y0),
        x1: a.x1.max(b.x1),
        y1: a.y1.max(b.y1),
    }
}

fn highlight_from_quad(
    quad: &Quad,
    ctm: &Matrix,
    width: u32,
    height: u32,
) -> Option<HighlightRect> {
    let mut min_x = f32::INFINITY;
    let mut min_y = f32::INFINITY;
    let mut max_x = f32::NEG_INFINITY;
    let mut max_y = f32::NEG_INFINITY;
    for point in [quad.ul, quad.ur, quad.ll, quad.lr] {
        let p = point.transform(ctm);
        min_x = min_x.min(p.x);
        min_y = min_y.min(p.y);
        max_x = max_x.max(p.x);
        max_y = max_y.max(p.y);
    }
    let x0 = min_x.floor().max(0.0) as u32;
    let y0 = min_y.floor().max(0.0) as u32;
    let x1 = max_x.ceil().min(width as f32) as u32;
    let y1 = max_y.ceil().min(height as f32) as u32;
    (x0 < x1 && y0 < y1).then_some(HighlightRect { x0, y0, x1, y1 })
}

fn bitmap_from_pixmap(pix: &Pixmap) -> PageBitmap {
    let width = pix.width();
    let height = pix.height();
    let stride = pix.stride().unsigned_abs();
    let components = pix.n() as usize;
    let samples = pix.samples();
    let mut pixels = vec![0u32; width as usize * height as usize];

    for y in 0..height as usize {
        let row_start = y * stride;
        if row_start >= samples.len() {
            break;
        }
        let row = &samples[row_start..];
        for x in 0..width as usize {
            let i = x * components;
            if i + components > row.len() {
                break;
            }
            let (r, g, b) = match components {
                1 | 2 => {
                    let v = row[i] as u32;
                    (v, v, v)
                }
                _ => (row[i] as u32, row[i + 1] as u32, row[i + 2] as u32),
            };
            pixels[y * width as usize + x] = (r << 16) | (g << 8) | b;
        }
    }

    PageBitmap {
        width,
        height,
        pixels,
    }
}

/// Page dimensions after applying the reading rotation.
fn rotated_size(bounds: &Rect, rotation: u16) -> (f32, f32) {
    let width = bounds.width().max(1.0);
    let height = bounds.height().max(1.0);
    if rotation.is_multiple_of(180) {
        (width, height)
    } else {
        (height, width)
    }
}

/// Map the illustration's page-space bounds into the rendered page bitmap and
/// clamp them to its pixels. Returns `None` when rounding leaves no area.
fn crop_for_image(
    image: &Rect,
    ctm: &Matrix,
    page_w: f32,
    page_h: f32,
    scale: f32,
) -> Option<BitmapCrop> {
    let transformed = image.transform(ctm);
    let page_px_w = (page_w * scale).round().max(1.0) as i64;
    let page_px_h = (page_h * scale).round().max(1.0) as i64;
    let x0 = transformed.x0.floor().max(0.0).min(page_px_w as f32) as i64;
    let y0 = transformed.y0.floor().max(0.0).min(page_px_h as f32) as i64;
    let x1 = transformed.x1.ceil().max(0.0).min(page_px_w as f32) as i64;
    let y1 = transformed.y1.ceil().max(0.0).min(page_px_h as f32) as i64;
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(BitmapCrop {
        x: x0 as u32,
        y: y0 as u32,
        width: (x1 - x0) as u32,
        height: (y1 - y0) as u32,
    })
}

/// Keep only the crop region of a rendered page bitmap.
fn crop_bitmap(bitmap: &PageBitmap, crop: BitmapCrop) -> PageBitmap {
    if bitmap.width == 0 || bitmap.height == 0 {
        return PageBitmap {
            width: bitmap.width,
            height: bitmap.height,
            pixels: bitmap.pixels.clone(),
        };
    }
    let x = crop.x.min(bitmap.width - 1);
    let y = crop.y.min(bitmap.height - 1);
    let width = crop.width.min(bitmap.width - x).max(1);
    let height = crop.height.min(bitmap.height - y).max(1);
    let mut pixels = Vec::with_capacity(width as usize * height as usize);
    for row in 0..height {
        let start = ((y + row) * bitmap.width + x) as usize;
        pixels.extend_from_slice(&bitmap.pixels[start..start + width as usize]);
    }
    PageBitmap {
        width,
        height,
        pixels,
    }
}

/// Pick a representative canvas color from one rendered page. The top-left
/// pixel can belong to a cover or an embedded image, so sample the border
/// instead: a text page's margins are its background, and even a full-bleed
/// illustration gives a stable color.
fn sample_page_background(bitmap: &PageBitmap) -> u32 {
    let width = bitmap.width as usize;
    let height = bitmap.height as usize;
    if width == 0 || height == 0 || bitmap.pixels.len() < width * height {
        return bitmap.pixels.first().copied().unwrap_or(0x00ff_ffff);
    }
    let edge = (width.min(height) / 40).clamp(1, 8);
    let mut counts: BTreeMap<u32, usize> = BTreeMap::new();
    let mut sample = |x: usize, y: usize| {
        let color = bitmap.pixels[y * width + x];
        *counts.entry(color).or_insert(0) += 1;
    };
    for y in 0..height {
        if y < edge || y + edge >= height {
            for x in 0..width {
                sample(x, y);
            }
        } else {
            for x in 0..edge {
                sample(x, y);
            }
            for x in width.saturating_sub(edge)..width {
                sample(x, y);
            }
        }
    }
    counts
        .into_iter()
        .max_by_key(|&(color, count)| (count, std::cmp::Reverse(color)))
        .map(|(color, _)| color)
        .unwrap_or(0x00ff_ffff)
}

fn fit_page_scale(page_w: f32, page_h: f32, available_w: f32, available_h: f32) -> f32 {
    (available_w / page_w.max(1.0)).min(available_h / page_h.max(1.0))
}

fn flatten_outlines(outlines: &[Outline], depth: usize) -> Vec<OutlineEntry> {
    let mut entries = Vec::new();
    for outline in outlines {
        let page = outline
            .dest
            .as_ref()
            .map(|dest| dest.loc.page_number as usize + 1);
        entries.push(OutlineEntry {
            title: outline.title.clone(),
            page,
            depth,
        });
        entries.extend(flatten_outlines(&outline.down, depth + 1));
    }
    entries
}

/// Use the document's own table of contents when MuPDF provides one, then
/// fall back to a page-number outline. The fallback keeps outline navigation
/// useful for image-only, untagged, or styleless text documents.
fn build_outline(doc: &Document, page_count: usize) -> Vec<OutlineEntry> {
    let mut outline = doc
        .outlines()
        .map(|outlines| flatten_outlines(&outlines, 0))
        .unwrap_or_default();
    for entry in &mut outline {
        if entry.title.trim().is_empty() {
            entry.title = entry
                .page
                .map_or_else(|| "Untitled".to_string(), |page| format!("Page {page}"));
        }
    }
    if outline.is_empty() {
        outline = page_outline(page_count);
    }
    outline
}

fn page_outline(page_count: usize) -> Vec<OutlineEntry> {
    (1..=page_count)
        .map(|page| OutlineEntry {
            title: format!("Page {page}"),
            page: Some(page),
            depth: 0,
        })
        .collect()
}

fn outline_title_units(title: &str) -> usize {
    title
        .chars()
        .map(|ch| if ch.is_ascii() { 1 } else { 2 })
        .sum()
}

fn truncate_outline_title(title: &str, width: u32, indent: usize) -> String {
    let available = width.saturating_sub(indent as u32 + 20) as usize;
    let max_units = (available / 8).max(8);
    let title: String = title
        .chars()
        .map(|ch| if ch == '\n' || ch == '\r' { ' ' } else { ch })
        .collect();
    if outline_title_units(&title) <= max_units {
        return title;
    }
    let mut truncated = String::new();
    let mut units = 0;
    for ch in title.chars() {
        let ch_units = if ch.is_ascii() { 1 } else { 2 };
        if units + ch_units > max_units.saturating_sub(2) {
            break;
        }
        truncated.push(ch);
        units += ch_units;
    }
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static DOC_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    static TEST_DIR_COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct DocTestGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for DocTestGuard {
        fn drop(&mut self) {
            // MuPDF keeps the user CSS in process-global state. Clear it even
            // if a test panics so the next test starts from a clean slate.
            let _ = Context::get().set_user_css("");
        }
    }

    fn doc_test_guard() -> DocTestGuard {
        DocTestGuard {
            _lock: DOC_TEST_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }

    fn test_dir(name: &str) -> std::path::PathBuf {
        let serial = TEST_DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("mureader-{name}-{}-{serial}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn selection_layer() -> TextLayer {
        TextLayer {
            chars: vec![
                TextChar {
                    ch: 'H',
                    rect: HighlightRect {
                        x0: 10,
                        y0: 10,
                        x1: 20,
                        y1: 30,
                    },
                    line: 0,
                },
                TextChar {
                    ch: 'i',
                    rect: HighlightRect {
                        x0: 20,
                        y0: 10,
                        x1: 25,
                        y1: 30,
                    },
                    line: 0,
                },
                TextChar {
                    ch: 'x',
                    rect: HighlightRect {
                        x0: 10,
                        y0: 40,
                        x1: 20,
                        y1: 60,
                    },
                    line: 1,
                },
            ],
            lines: vec![
                TextLine {
                    start: 0,
                    end: 2,
                    rect: HighlightRect {
                        x0: 10,
                        y0: 10,
                        x1: 25,
                        y1: 30,
                    },
                },
                TextLine {
                    start: 2,
                    end: 3,
                    rect: HighlightRect {
                        x0: 10,
                        y0: 40,
                        x1: 20,
                        y1: 60,
                    },
                },
            ],
        }
    }

    fn wrapped_layer(first: char, second: char, advance: u32) -> TextLayer {
        TextLayer {
            chars: vec![
                TextChar {
                    ch: first,
                    rect: HighlightRect {
                        x0: 10,
                        y0: 10,
                        x1: 20,
                        y1: 30,
                    },
                    line: 0,
                },
                TextChar {
                    ch: second,
                    rect: HighlightRect {
                        x0: 10,
                        y0: 10 + advance,
                        x1: 20,
                        y1: 30 + advance,
                    },
                    line: 1,
                },
            ],
            lines: vec![
                TextLine {
                    start: 0,
                    end: 1,
                    rect: HighlightRect {
                        x0: 10,
                        y0: 10,
                        x1: 20,
                        y1: 30,
                    },
                },
                TextLine {
                    start: 1,
                    end: 2,
                    rect: HighlightRect {
                        x0: 10,
                        y0: 10 + advance,
                        x1: 20,
                        y1: 30 + advance,
                    },
                },
            ],
        }
    }

    #[test]
    fn copied_text_joins_visual_line_wraps() {
        // Latin wraps keep the space that the line break swallowed.
        assert_eq!(wrapped_layer('H', 'i', 20).text_for_range(0, 1), "H i");
        assert_eq!(wrapped_layer('中', '文', 20).text_for_range(0, 1), "中文");
        assert_eq!(wrapped_layer('.', 'A', 20).text_for_range(0, 1), ". A");
    }

    #[test]
    fn copied_text_keeps_paragraph_breaks() {
        assert_eq!(wrapped_layer('H', 'i', 34).text_for_range(0, 1), "H\ni");
    }

    #[test]
    fn hit_testing_prefers_character_then_nearest_line_character() {
        let layer = selection_layer();
        assert_eq!(layer.position_at(12.0, 15.0), Some(0));
        assert_eq!(layer.position_at(24.0, 15.0), Some(1));
        assert_eq!(layer.position_at(19.0, 45.0), Some(2));
        assert_eq!(layer.position_at(100.0, 45.0), Some(2));
        assert_eq!(layer.position_at(100.0, 10.0), Some(1));
    }

    #[test]
    fn hit_testing_distinguishes_columns_at_the_same_height() {
        let layer = TextLayer {
            chars: vec![
                TextChar {
                    ch: 'L',
                    rect: HighlightRect {
                        x0: 10,
                        y0: 10,
                        x1: 20,
                        y1: 30,
                    },
                    line: 0,
                },
                TextChar {
                    ch: 'R',
                    rect: HighlightRect {
                        x0: 110,
                        y0: 10,
                        x1: 120,
                        y1: 30,
                    },
                    line: 1,
                },
            ],
            lines: vec![
                TextLine {
                    start: 0,
                    end: 1,
                    rect: HighlightRect {
                        x0: 10,
                        y0: 10,
                        x1: 20,
                        y1: 30,
                    },
                },
                TextLine {
                    start: 1,
                    end: 2,
                    rect: HighlightRect {
                        x0: 110,
                        y0: 10,
                        x1: 120,
                        y1: 30,
                    },
                },
            ],
        };
        assert_eq!(layer.position_at(115.0, 20.0), Some(1));
        assert_eq!(layer.position_at(15.0, 20.0), Some(0));
        assert_eq!(layer.position_at(60.0, 20.0), Some(0));
    }

    #[test]
    fn reflowable_internal_links_resolve_pages() {
        let _guard = doc_test_guard();
        let dir = test_dir("link");
        let path = dir.join("sample.html");
        std::fs::write(
            &path,
            {
                let mut html = String::from(
                    r##"<!doctype html><html><body><p><a href="#target">Jump to target</a></p>"##,
                );
                for i in 0..80 {
                    html.push_str(&format!(
                        "<p>Paragraph {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>"
                    ));
                }
                html.push_str(r##"<p id="target">Target page.</p></body></html>"##);
                html
            },
        )
        .unwrap();
        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(800, 600);
        let page = engine.render().unwrap();
        assert!(!page.links.is_empty(), "no links extracted");
        assert!(
            page.links
                .iter()
                .any(|link| matches!(&link.target, Some(LinkTarget::Page(page)) if *page > 1)),
            "internal link did not resolve to a later page: {:?}",
            page.links
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn external_links_keep_their_uri() {
        let _guard = doc_test_guard();
        let dir = test_dir("external-link");
        let path = dir.join("sample.html");
        std::fs::write(
            &path,
            r##"<!doctype html><html><body><p><a href="https://example.com/read">Read online</a></p></body></html>"##,
        )
        .unwrap();
        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(800, 600);
        let page = engine.render().unwrap();
        assert!(
            page.links.iter().any(|link| matches!(
                &link.target,
                Some(LinkTarget::Uri(uri)) if uri == "https://example.com/read"
            )),
            "external link URI was lost: {:?}",
            page.links
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_text_outline_falls_back_to_pages() {
        let _guard = doc_test_guard();
        let dir = test_dir("outline");
        let path = dir.join("notes.txt");
        let mut text = String::from("# Chapter One\n\nIntro paragraph.\n\n## Section A\n\n");
        for i in 0..80 {
            text.push_str(&format!(
                "Paragraph {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit.\n\n"
            ));
        }
        text.push_str("# Chapter Two\n\n");
        for i in 0..80 {
            text.push_str(&format!(
                "Paragraph {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit.\n\n"
            ));
        }
        std::fs::write(&path, text).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog.clone(), None, true).unwrap();
        let outline = engine.outline();
        assert_eq!(outline.len(), engine.page_count.unwrap());
        assert_eq!(outline[0].title, "Page 1");
        assert!(outline.iter().all(|entry| entry.title.starts_with("Page ")));

        assert!(engine.adjust_font(1.5).unwrap());
        let reflowed = engine.outline();
        assert_eq!(reflowed.len(), engine.page_count.unwrap());
        assert!(reflowed
            .iter()
            .all(|entry| entry.title.starts_with("Page ")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn html_outline_comes_from_the_document() {
        let _guard = doc_test_guard();
        let dir = test_dir("html-outline");
        let path = dir.join("sections.html");
        let mut html =
            String::from("<!doctype html><html><body><h1>Chapter One</h1><h2>Section A</h2>");
        for i in 0..160 {
            html.push_str(&format!(
                "<p>Paragraph {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>"
            ));
        }
        html.push_str("<h1>Chapter Two</h1></body></html>");
        std::fs::write(&path, html).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        let titles: Vec<&str> = engine
            .outline()
            .iter()
            .map(|entry| entry.title.as_str())
            .collect();
        assert!(titles.contains(&"Chapter One"), "{titles:?}");
        assert!(titles.contains(&"Section A"), "{titles:?}");
        assert!(titles.contains(&"Chapter Two"), "{titles:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn outline_selection_brightens_text_and_keeps_a_clear_rail() {
        let _guard = doc_test_guard();
        let dir = test_dir("outline-selection");
        let path = dir.join("sections.html");
        let mut html = String::from("<!doctype html><html><body><h1>Chapter One</h1>");
        for i in 0..160 {
            html.push_str(&format!(
                "<p>Paragraph {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>"
            ));
        }
        html.push_str("<h1>Chapter Two</h1></body></html>");
        std::fs::write(&path, html).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        let veil = 0x0040_4448;
        let rail = 0x0012_3456;
        let style = OutlineStyle::default();
        let plain = engine
            .render_outline(320, 28, 0, 4, None, None, None, style)
            .unwrap()
            .unwrap();
        let selected = engine
            .render_outline(320, 28, 0, 4, Some(0), Some(veil), Some(rail), style)
            .unwrap()
            .unwrap();

        let width = plain.width as usize;
        let row_height = 28usize;
        fn row_block(bitmap: &PageBitmap, row: usize, width: usize, row_height: usize) -> &[u32] {
            let start = row * row_height * width;
            &bitmap.pixels[start..start + row_height * width]
        }
        let last = row_height * width - 1;

        // Selection bakes a faint row veil into the bitmap.
        assert_eq!(row_block(&plain, 0, width, row_height)[last], style.panel);
        let selected_row = row_block(&selected, 0, width, row_height);
        assert_eq!(selected_row[last], veil);
        // The rail stays in the left padding, clear of the title glyphs.
        assert_eq!(selected_row[1], rail);
        assert_eq!(selected_row[4], veil);
        assert!(
            !selected_row[4..OUTLINE_TEXT_PADDING + 1].contains(&rail),
            "rail must not reach the text padding"
        );

        // Selected text is pure white; unselected text is only near-white, so
        // the selection brightens without adding a warm hue.
        let bright = |row: &[u32], from: usize| {
            row[from..]
                .iter()
                .filter(|pixel| {
                    let r = (*pixel >> 16) & 0xff;
                    let g = (*pixel >> 8) & 0xff;
                    let b = *pixel & 0xff;
                    r == 0xff && g == 0xff && b == 0xff
                })
                .count()
        };
        assert_eq!(
            bright(
                row_block(&plain, 0, width, row_height),
                OUTLINE_TEXT_PADDING
            ),
            0
        );
        assert!(
            bright(selected_row, OUTLINE_TEXT_PADDING) > 0,
            "selected title should be drawn in white"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spread_gap_scales_linearly_with_page_width() {
        assert_eq!(spread_gap(0), 1, "the gutter is never absent");
        assert_eq!(spread_gap(250), 1);
        assert_eq!(spread_gap(1000), 4);
        assert_eq!(spread_gap(2000), 8);
        assert!(spread_gap(800) > spread_gap(400));
    }

    #[test]
    fn scroll_side_margin_always_leaves_air_and_room_for_the_page() {
        assert_eq!(scroll_side_margin(0), 0);
        for width in [120, 240, 400, 800, 1600, 3200] {
            let margin = scroll_side_margin(width);
            assert!(margin <= width / 4, "margins must not swallow the page");
        }
        assert!(scroll_side_margin(400) >= 12);
        assert!(scroll_side_margin(3200) <= 64);
        assert!(scroll_side_margin(1600) >= scroll_side_margin(400));
    }

    #[test]
    fn page_outline_covers_every_page() {
        let outline = page_outline(3);
        assert_eq!(outline.len(), 3);
        assert_eq!(outline[0].title, "Page 1");
        assert_eq!(outline[2].page, Some(3));
    }

    #[test]
    fn log_files_are_not_documents() {
        let _guard = doc_test_guard();
        let dir = test_dir("log");
        let path = dir.join("session.log");
        std::fs::write(&path, "2026-11-04 INFO started\n").unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let result = RenderEngine::open(&path, catalog, None, true);
        let Err(error) = result else {
            panic!("log files must not open as documents");
        };
        assert!(error.contains("unsupported document format"), "{error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pretty_text_paginates_plain_text() {
        let _guard = doc_test_guard();
        let dir = test_dir("pretty");
        let path = dir.join("sample.txt");
        let mut text = String::from(
            "A deliberately long paragraph exercises the Unicode line breaker: the quick brown fox jumps over the lazy dog, again and again and again, while 中文句子也应该在合适的位置换行，而不是被硬切。\n\n",
        );
        for i in 0..120 {
            text.push_str(&format!(
                "Paragraph {i}. The quick brown fox jumps over the lazy dog. 你好，世界。\n\n"
            ));
        }
        std::fs::write(&path, text).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, true).unwrap();
        engine.set_viewport(1100, 900);
        let first = engine.render().unwrap();
        let total = first.page_count.unwrap_or(0);
        assert!(total > 1);
        assert!(!first.text.is_empty());
        assert_eq!(first.pages, vec![1]);
        assert!(first.text.chars.iter().any(|ch| ch.ch == '你'));

        let search = engine
            .search("quick", SearchScope::Page, &AtomicBool::new(false))
            .unwrap();
        assert!(search.is_some());

        assert!(engine.goto_page(2));
        let second = engine.render().unwrap();
        assert_eq!(second.pages, vec![2]);
        assert!(!second.text.is_empty());

        assert!(engine.goto_page(total));
        let last = engine.render().unwrap();
        assert!(!last.text.is_empty(), "pretty text ended with a blank page");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spread_renders_two_pages_side_by_side() {
        let _guard = doc_test_guard();
        let dir = test_dir("spread");
        let path = dir.join("sample.html");
        let mut html = String::from("<!doctype html><html><body>");
        for i in 0..160 {
            html.push_str(&format!(
                "<p>Paragraph {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>"
            ));
        }
        html.push_str("</body></html>");
        std::fs::write(&path, html).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(2400, 900);
        let single = engine.render().unwrap();
        assert_eq!(single.pages, vec![1]);
        let single_width = single.bitmap.width;
        let single_height = single.bitmap.height;

        engine.set_layout(true);
        engine.set_viewport(single_width * 2, 900);
        let tight = engine.render().unwrap();
        assert_eq!(tight.pages, vec![1, 2, 3, 4]);
        assert!(tight.page_widths[0] <= single_width);
        assert_eq!(
            tight.gap,
            spread_gap(tight.page_widths[0]),
            "the gutter is part of the fit geometry"
        );
        assert!(tight.gap > 0, "the gutter never collapses");
        assert!(
            tight.page_widths[0] * 2 + tight.gap <= single_width * 2,
            "the pair and its gutter must fit the viewport"
        );

        engine.set_viewport(2400, 900);
        let spread = engine.render().unwrap();
        assert_eq!(spread.pages, vec![1, 2, 3, 4]);
        assert_eq!(spread.gap, spread_gap(spread.page_widths[0]));
        assert!(spread.gap > 0);
        assert!(spread.bitmap.width > single_width);
        assert_eq!(spread.bitmap.height, single_height);
        assert!(!spread.text.is_empty());
        assert!(
            spread.text.chars.iter().any(|ch| ch.rect.x0 > single_width),
            "second-page text was not shifted into composite coordinates"
        );

        // The gutter follows the UI theme's window color instead of a
        // document-owned constant.
        let gap_pixel = spread.page_widths[0] as usize;
        assert_eq!(spread.bitmap.pixels[gap_pixel], DEFAULT_WINDOW);
        engine.set_background(0x0012_3456);
        let themed = engine.render().unwrap();
        assert_eq!(themed.bitmap.pixels[gap_pixel], 0x0012_3456);
        engine.set_background(DEFAULT_WINDOW);

        // A narrower viewport still keeps both pages: each half is fitted so
        // the pair shrinks instead of one page being clipped.
        let narrow_width = single_width + 200;
        engine.set_viewport(narrow_width, 900);
        let narrow = engine.render().unwrap();
        assert_eq!(narrow.pages, vec![1, 2, 3, 4]);
        assert!(narrow.page_widths[0] < single_width);
        assert!(narrow.page_widths[0].saturating_mul(2) <= narrow_width);
        assert_eq!(narrow.gap, spread_gap(narrow.page_widths[0]));
        assert!(narrow.gap > 0);

        assert!(engine.next_page());
        let next = engine.render().unwrap();
        assert_eq!(next.page, 2);
        assert_eq!(next.pages, vec![1, 2, 3, 4]);
        assert!(engine.prev_page());
        let prev = engine.render().unwrap();
        assert_eq!(prev.page, 1);
        assert_eq!(prev.pages, vec![1, 2, 3, 4]);

        engine.set_scale(2.0);
        let zoomed = engine.render().unwrap();
        assert_eq!(
            zoomed.pages,
            vec![1],
            "an explicit zoom must not render a four-page strip"
        );
        engine.fit_page();
        let fitted = engine.render().unwrap();
        assert_eq!(fitted.fit, Fit::Page);
        assert_eq!(fitted.pages, vec![1, 2, 3, 4]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roll_measures_the_css_line_height() {
        let _guard = doc_test_guard();
        let dir = test_dir("roll-line-height");

        let render = |name: &str, line_height: u32| -> f32 {
            let path = dir.join(format!("{name}.html"));
            let mut html = String::from("<!doctype html><html><head><style>");
            html.push_str(&format!(
                "body {{ font-size: 16px; line-height: {line_height}px; }} p {{ margin: 0; }}"
            ));
            html.push_str("</style></head><body>");
            for i in 0..120 {
                html.push_str(&format!(
                    "<p>Line height sample {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>"
                ));
            }
            html.push_str("</body></html>");
            std::fs::write(&path, html).unwrap();

            let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
            mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
            let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
            engine.set_viewport(900, 1200);
            engine.set_layout(false);
            let strip = engine.render().unwrap();
            strip
                .line_height
                .expect("continuous scroll reports the line rhythm")
        };

        let tight = render("tight", 18);
        let loose = render("loose", 36);
        assert!(
            (loose / tight - 2.0).abs() < 0.15,
            "the line step must follow the CSS line-height, got {tight} vs {loose}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roll_pages_meet_without_blank_bands() {
        let _guard = doc_test_guard();
        let dir = test_dir("roll-seams");
        let path = dir.join("sample.html");
        let mut html = String::from("<!doctype html><html><body>");
        for i in 0..160 {
            html.push_str(&format!(
                "<p>Paragraph {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>"
            ));
        }
        html.push_str("</body></html>");
        std::fs::write(&path, html).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(900, 1200);
        engine.set_layout(false);
        let strip = engine.render().unwrap();
        let line = strip.line_height.expect("line rhythm") * strip.scale;
        let width = strip.bitmap.width as usize;
        let pixels = &strip.bitmap.pixels;
        let blank_run = |mut y: usize, step: isize| -> usize {
            let mut run = 0;
            loop {
                let row = &pixels[y * width..(y + 1) * width];
                if row.iter().any(|&pixel| pixel != row[0]) {
                    break;
                }
                run += 1;
                if (step < 0 && y == 0) || (step > 0 && y + 1 >= strip.bitmap.height as usize) {
                    break;
                }
                y = (y as isize + step) as usize;
            }
            run
        };
        for &offset in &strip.page_offsets[1..] {
            let seam = offset as usize;
            let blank = blank_run(seam, -1) + blank_run(seam, 1);
            assert!(
                blank as f32 <= line * 2.5 + 2.0,
                "page seam at {seam}px leaves a {blank}px blank band (one line is {line:.1}px)"
            );
        }

        // An explicit zoom keeps the page-shaped view, but the CSS line rhythm
        // is still reported so arrow-key steps stay proportional to the text.
        engine.set_scale(2.0);
        let zoomed = engine.render().unwrap();
        assert!(zoomed.gap > 0, "explicit zoom keeps the page gap");
        let ratio = zoomed.page_heights[0] as f32 / zoomed.page_widths[0] as f32;
        assert!(
            (ratio - 792.0 / 612.0).abs() < 0.05,
            "explicit zoom stays page-shaped, got {ratio}"
        );
        assert!(
            zoomed.line_height.is_some(),
            "the CSS line rhythm survives an explicit zoom"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roll_restores_image_margins_at_page_seams() {
        let _guard = doc_test_guard();
        let dir = test_dir("roll-image-margins");
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="300" height="600"><rect width="300" height="600" fill="#336699"/></svg>"##;
        std::fs::write(dir.join("plate.svg"), svg).unwrap();
        let path = dir.join("plates.html");
        let mut html = String::from(
            "<!doctype html><html><head><style>\
             html, body { margin: 0; }\
             img { display: block; margin: 30px 0; }\
             </style></head><body>",
        );
        for _ in 0..8 {
            html.push_str(r#"<img src="plate.svg">"#);
        }
        html.push_str("</body></html>");
        std::fs::write(&path, html).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(900, 1200);
        engine.set_layout(false);
        let strip = engine.render().unwrap();
        let scale = strip.scale;
        let margin = 30.0 * scale;

        let mut checked = 0;
        for index in 0..strip.page_offsets.len().saturating_sub(1) {
            let gap = strip.page_offsets[index + 1]
                - strip.page_offsets[index]
                - strip.page_heights[index];
            checked += 1;
            assert!(
                gap as f32 >= margin * 0.5,
                "image margin lost at seam {index}: gap {gap}px, margin {margin:.1}px"
            );
            assert!(
                gap as f32 <= margin * 2.0 + 2.0,
                "image seam {index} gained an unexpected band: {gap}px, margin {margin:.1}px"
            );
        }
        assert!(checked > 0, "the plate strip has at least one seam");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roll_restores_margins_between_text_and_images() {
        let _guard = doc_test_guard();
        let dir = test_dir("roll-mixed-margins");
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="300" height="240"><rect width="300" height="240" fill="#336699"/></svg>"##;
        std::fs::write(dir.join("plate.svg"), svg).unwrap();
        let path = dir.join("mixed.html");
        let mut html = String::from(
            "<!doctype html><html><head><style>\
             html, body { margin: 0; }\
             body { font-size: 16px; line-height: 24px; }\
             p { margin: 0 0 16px 0; }\
             img { display: block; margin: 20px 0; }\
             </style></head><body>",
        );
        for i in 0..30 {
            html.push_str(&format!(
                "<p>Mixed paragraph {i}. Lorem ipsum dolor sit amet, consectetur \
                 adipiscing elit, sed do eiusmod tempor incididunt.</p>"
            ));
            if i % 3 == 0 {
                html.push_str(r#"<img src="plate.svg">"#);
            }
        }
        html.push_str("</body></html>");
        std::fs::write(&path, html).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(900, 1200);
        engine.set_layout(false);
        let strip = engine.render().unwrap();
        let scale = strip.scale;
        let baseline = engine.roll_top_baseline;
        let count = engine.page_count.unwrap();
        let image_margin = 20.0 * scale;

        let mut mixed = 0;
        for page_no in 1..count {
            let (Some(previous), Some(next)) =
                (engine.map_page(page_no - 1), engine.map_page(page_no))
            else {
                continue;
            };
            let Some(tall) = engine.tall_flow.as_ref() else {
                break;
            };
            let last = &tall.elements[previous.last_tall].kind;
            let first = &tall.elements[next.first_tall].kind;
            let is_mixed = matches!(
                (last, first),
                (FlowElement::Text(_), FlowElement::Image)
                    | (FlowElement::Image, FlowElement::Text(_))
            );
            if !is_mixed {
                continue;
            }
            mixed += 1;
            let gap = engine.seam_gap(page_no - 1, page_no, baseline, scale);
            assert!(
                gap as f32 >= image_margin * 0.5,
                "text/image seam {page_no} lost its margin: {gap}px, \
                 image margin {image_margin:.1}px"
            );
            assert!(
                gap as f32 <= image_margin * 2.0 + 4.0,
                "text/image seam {page_no} gained an unexpected band: \
                 {gap}px, image margin {image_margin:.1}px"
            );
        }
        assert!(mixed > 0, "no page seam fell between text and an image");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roll_restores_paragraph_margins_at_page_seams() {
        let _guard = doc_test_guard();
        let dir = test_dir("roll-paragraph-margins");
        let path = dir.join("paragraphs.html");
        let mut html = String::from(
            "<!doctype html><html><head><style>\
             html, body { margin: 0; }\
             body { font-size: 16px; line-height: 24px; }\
             p { margin: 0 0 24px 0; text-indent: 2em; }\
             </style></head><body>",
        );
        for i in 0..120 {
            html.push_str(&format!(
                "<p>Paragraph {i}. Lorem ipsum dolor sit amet, consectetur adipiscing \
                 elit, sed do eiusmod tempor incididunt ut labore.</p>"
            ));
        }
        html.push_str("</body></html>");
        std::fs::write(&path, html).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(900, 1200);
        engine.set_layout(false);
        let strip = engine.render().unwrap();
        let scale = strip.scale;
        let line = strip.line_height.expect("line rhythm") * scale;
        let margin = 24.0 * scale;

        // Ink-to-ink spacing between two lines of the same paragraph.
        let lines = &strip.text.lines;
        let mut line_gaps: Vec<f32> = lines
            .windows(2)
            .map(|pair| pair[1].rect.y0 as f32 - pair[0].rect.y1 as f32)
            .filter(|gap| *gap > 0.0 && *gap < line * 0.9)
            .collect();
        line_gaps.sort_by(f32::total_cmp);
        let line_gap = line_gaps[line_gaps.len() / 2];
        let left = lines
            .iter()
            .map(|line| line.rect.x0 as f32)
            .fold(f32::INFINITY, f32::min);
        // A paragraph's first line is indented by 2em; continuation lines are
        // not, so the indent says which kind of break a page seam landed on.
        let indented = |line: &TextLine| line.rect.x0 as f32 >= left + 16.0 * scale;

        let mut checked_paragraphs = 0;
        for &offset in &strip.page_offsets[1..] {
            let seam = offset as f32;
            let Some(above) = lines.iter().rev().find(|line| line.rect.y1 as f32 <= seam) else {
                continue;
            };
            let Some(below) = lines.iter().find(|line| line.rect.y0 as f32 >= seam) else {
                continue;
            };
            let gap = below.rect.y0 as f32 - above.rect.y1 as f32;
            if indented(below) {
                checked_paragraphs += 1;
                assert!(
                    gap >= line_gap + margin * 0.5,
                    "paragraph margin lost at seam {seam}: gap {gap:.1}px, \
                     line gap {line_gap:.1}px, margin {margin:.1}px"
                );
            } else {
                assert!(
                    gap <= line_gap + margin * 0.5,
                    "mid-paragraph seam {seam} gained a paragraph gap: \
                     {gap:.1}px vs line gap {line_gap:.1}px"
                );
            }
        }
        assert!(
            checked_paragraphs > 0,
            "no seam landed on a paragraph boundary"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roll_keeps_a_chapter_margin_above_the_heading() {
        let _guard = doc_test_guard();
        let dir = test_dir("roll-chapter-margin");
        let path = dir.join("chapter.html");
        let mut html = String::from(
            "<!doctype html><html><head><style>\
             @page { margin: 10px; }\
             html, body { margin: 0; }\
             p { margin: 0; font-size: 12px; line-height: 16px; }\
             h1 { margin: 72px 0 0 0; font-size: 24px; line-height: 36px; }\
             </style></head><body><h1>Chapter heading</h1>",
        );
        for i in 0..160 {
            html.push_str(&format!(
                "<p>Paragraph {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>"
            ));
        }
        html.push_str("</body></html>");
        std::fs::write(&path, html).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(900, 1200);
        engine.set_layout(false);
        let strip = engine.render().unwrap();
        let line = strip.line_height.expect("line rhythm") * strip.scale;
        let width = strip.bitmap.width as usize;
        let pixels = &strip.bitmap.pixels;
        let blank_run = |mut y: usize, step: isize| -> usize {
            let mut run = 0;
            loop {
                let row = &pixels[y * width..(y + 1) * width];
                if row.iter().any(|&pixel| pixel != row[0]) {
                    break;
                }
                run += 1;
                if (step < 0 && y == 0) || (step > 0 && y + 1 >= strip.bitmap.height as usize) {
                    break;
                }
                y = (y as isize + step) as usize;
            }
            run
        };

        // The chapter margin is a real block margin. A new chapter starts its
        // flow (an EPUB chapter is a separate document), so the roll must keep
        // the space above the heading instead of cropping to the glyph ink.
        let text: String = strip.text.chars.iter().map(|ch| ch.ch).collect();
        let heading_byte = text.find("Chapter heading").expect("heading text");
        let heading = text[..heading_byte].chars().count();
        let heading_y = strip.text.chars[heading].rect.y0 as usize;
        let above = blank_run(heading_y, -1);
        assert!(
            above as f32 >= line * 3.0,
            "chapter margin collapsed: only {above}px of {line:.1}px-line space \
             above the heading"
        );

        // Text seams stay about one line: the shared page margin is removed
        // everywhere else.
        for &offset in &strip.page_offsets[1..] {
            let seam = offset as usize;
            let blank = blank_run(seam, -1) + blank_run(seam, 1);
            assert!(
                blank as f32 <= line * 1.5 + 2.0,
                "page seam at {seam}px leaves a {blank}px blank band (one line is {line:.1}px)"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vector_only_pages_are_not_blank_in_the_roll() {
        let _guard = doc_test_guard();
        let dir = test_dir("vector-roll");
        let path = dir.join("vector.html");
        std::fs::write(
            &path,
            r##"<!doctype html><html><body><svg width="300" height="600" viewBox="0 0 300 600" xmlns="http://www.w3.org/2000/svg"><rect width="300" height="600" fill="#336699"/></svg></body></html>"##,
        )
        .unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(900, 1200);
        engine.set_layout(false);
        let strip = engine.render().unwrap();
        assert!(
            strip.pages.contains(&1),
            "the vector-only page must appear in the roll: {:?}",
            strip.pages
        );
        assert!(
            strip.page_heights.iter().all(|&height| height > 0),
            "vector-only pages report a content height"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scroll_renders_a_vertical_strip() {
        let _guard = doc_test_guard();
        let dir = test_dir("scroll");
        let path = dir.join("sample.html");
        let mut html = String::from("<!doctype html><html><body>");
        for i in 0..160 {
            html.push_str(&format!(
                "<p>Paragraph {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>"
            ));
        }
        html.push_str("</body></html>");
        std::fs::write(&path, html).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(900, 1200);
        engine.set_layout(false);

        let strip = engine.render().unwrap();
        assert!(strip.scroll);
        assert_eq!(strip.pages, vec![1, 2, 3, 4]);
        let book_background = strip.book_background;
        let first_height = strip.page_heights[0];
        // The roll packs content blocks with no simulated page chrome and
        // restores the document's real spacing at each seam, so a page may
        // start a little below the previous page's bitmap. A fully blank
        // reflow page never shows up.
        assert_eq!(strip.gap, 0);
        assert_eq!(strip.page_offsets[0], 0);
        assert!(
            strip.page_offsets[1] >= first_height,
            "the second page cannot overlap the first"
        );
        assert!(
            strip.page_heights.iter().all(|&height| height > 0),
            "every page in the roll reports a content height"
        );
        assert!(
            strip.line_height.is_some_and(|height| height > 0.0),
            "continuous scroll reports the CSS line rhythm"
        );
        assert_eq!(
            strip.bitmap.width,
            strip.page_widths.iter().copied().max().unwrap(),
            "the strip is as wide as its widest page"
        );
        assert!(strip.bitmap.height > first_height, "pages stack vertically");
        let margin = scroll_side_margin(900);
        assert!(margin > 0, "scroll reserves side margins");
        assert!(
            strip.page_widths[0] <= 900 - margin * 2 + 1,
            "scroll fits the page width inside the viewport margins"
        );
        assert!(
            strip.text.chars.iter().any(|ch| ch.rect.y0 >= first_height),
            "later pages are shifted into vertical strip coordinates"
        );

        // An explicit zoom keeps a three-page window so scrolling can still
        // move in both directions without exceeding the texture budget.
        engine.set_scale(2.0);
        let zoomed = engine.render().unwrap();
        assert!(zoomed.scroll);
        assert_eq!(zoomed.pages, vec![1, 2, 3]);
        assert_eq!(zoomed.book_background, book_background);

        engine.fit_page();
        // Scrolling up and down renders different sides of the anchor so the
        // strip always keeps lookahead in the direction of travel.
        if engine.page_count.unwrap_or(0) >= 8 {
            assert!(engine.scroll_to(5, 2));
            let upward = engine.render().unwrap();
            assert_eq!(upward.page, 5);
            assert_eq!(upward.pages, vec![3, 4, 5, 6]);
            assert!(engine.scroll_to(5, 1));
            let downward = engine.render().unwrap();
            assert_eq!(downward.page, 5);
            assert_eq!(downward.pages, vec![4, 5, 6, 7]);
        }

        engine.goto_page(4);
        let recentered = engine.render().unwrap();
        assert_eq!(recentered.page, 4);
        assert_eq!(recentered.pages, vec![3, 4, 5, 6]);
        assert_eq!(recentered.page_offsets[0], 0);
        assert_eq!(recentered.book_background, book_background);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn standalone_illustrations_flow_at_reading_width() {
        let _guard = doc_test_guard();
        let dir = test_dir("image-layout");
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="300" height="600"><rect width="300" height="600" fill="#336699"/><circle cx="150" cy="300" r="120" fill="#ffcc00"/></svg>"##;
        std::fs::write(dir.join("tall.svg"), svg).unwrap();
        let path = dir.join("image.html");
        std::fs::write(
            &path,
            r#"<!doctype html><html><body><img src="tall.svg"></body></html>"#,
        )
        .unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(900, 1200);
        engine.set_layout(false);

        let strip = engine.render().unwrap();
        let width = strip.page_widths[0];
        let height = strip.page_heights[0];
        let column = engine.scroll_available_width().round() as u32;
        assert!(
            width.abs_diff(column) <= 1,
            "illustration width {width} should fill the {column}px reading column"
        );
        // A 300x600 illustration is 1:2, so the rendered plate must keep that
        // aspect ratio instead of being forced into a 612x792 page.
        let ratio = height as f32 / width as f32;
        assert!(
            (ratio - 2.0).abs() < 0.02,
            "illustration aspect ratio {ratio} should stay close to 2.0"
        );
        // The page was cropped to the illustration, so the corners carry the
        // image instead of the simulated page's white margins.
        let corner = strip.bitmap.pixels[0];
        let center = strip.bitmap.pixels
            [(height as usize / 2) * strip.bitmap.width as usize + width as usize / 2];
        assert_ne!(corner, 0x00ff_ffff, "cropped plate must not start white");
        assert_eq!(
            center, 0x00ff_cc00,
            "the illustration disc should be visible"
        );

        // Rotation swaps the plate's axes and still fits the reading column.
        engine.rotate(90);
        let rotated = engine.render().unwrap();
        let (rotated_w, rotated_h) = (rotated.page_widths[0], rotated.page_heights[0]);
        assert!(
            rotated_w > rotated_h,
            "a quarter-turned portrait plate should become landscape"
        );
        assert!(
            rotated_w.abs_diff(column) <= 1,
            "a rotated plate should still fill the reading column"
        );
        engine.rotate(-90);

        // Paged mode keeps whole synthetic pages: the image stays inside the
        // page box rather than being cropped out of it.
        engine.set_layout(true);
        let paged = engine.render().unwrap();
        let paged_ratio = paged.page_heights[0] as f32 / paged.page_widths[0] as f32;
        assert!(
            (paged_ratio - 792.0 / 612.0).abs() < 0.05,
            "paged mode should render a whole 612x792 page, got {paged_ratio}"
        );

        // A page with a caption flows with the roll as well: it is cropped to
        // its content instead of keeping the synthetic page chrome, and the
        // caption text is part of the flowing story.
        engine.set_layout(false);
        let mixed = dir.join("mixed.html");
        std::fs::write(
            &mixed,
            r#"<!doctype html><html><body><img src="tall.svg"><p>A caption runs beneath the illustration and makes this a text page.</p></body></html>"#,
        )
        .unwrap();
        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut mixed_engine = RenderEngine::open(&mixed, catalog, None, false).unwrap();
        mixed_engine.set_viewport(900, 1200);
        mixed_engine.set_layout(false);
        let mixed_strip = mixed_engine.render().unwrap();
        let mixed_ratio = mixed_strip.page_heights[0] as f32 / mixed_strip.page_widths[0] as f32;
        let column = mixed_engine.scroll_available_width().round() as u32;
        assert!(
            mixed_strip.page_widths[0].abs_diff(column) <= 1,
            "a captioned plate keeps the reading column width"
        );
        assert!(
            (mixed_ratio - 792.0 / 612.0).abs() > 0.05,
            "a captioned plate flows with the roll, not page-shaped"
        );
        assert!(
            mixed_strip
                .text
                .chars
                .iter()
                .map(|ch| ch.ch)
                .collect::<String>()
                .contains("caption"),
            "the caption text is part of the flowing story"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scroll_scale_is_locked_until_the_geometry_changes() {
        let _guard = doc_test_guard();
        let dir = test_dir("scroll-scale");
        let path = dir.join("sample.html");
        let mut html = String::from("<!doctype html><html><body>");
        for i in 0..80 {
            html.push_str(&format!(
                "<p>Paragraph {i}. Lorem ipsum dolor sit amet.</p>"
            ));
        }
        html.push_str("</body></html>");
        std::fs::write(&path, html).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(900, 1200);
        engine.set_layout(false);

        // A mixed-size page in the same window must reuse the first fit, so the
        // column does not re-zoom when the scroll window slides.
        let portrait = Rect::new(0.0, 0.0, 612.0, 792.0);
        let landscape = Rect::new(0.0, 0.0, 1224.0, 792.0);
        let locked = engine.scale_for(&portrait).unwrap();
        assert!((engine.scale_for(&landscape).unwrap() - locked).abs() < 1.0e-6);

        // A resize invalidates the lock and refits the column.
        engine.set_viewport(1800, 1200);
        let refit = engine.scale_for(&portrait).unwrap();
        assert!(refit > locked * 1.5, "a wider window refits the column");

        // Switching layout also starts a fresh scroll session.
        engine.set_layout(true);
        assert!(engine.scroll_scale.is_none());
        engine.set_layout(false);
        assert!(engine.scroll_scale.is_none());

        // Rotation invalidates the lock as well.
        let _ = engine.scale_for(&portrait).unwrap();
        engine.rotate(90);
        assert!(engine.scroll_scale.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn spread_reuses_overlapping_pages() {
        let _guard = doc_test_guard();
        let dir = test_dir("spread-cache");
        let path = dir.join("sample.html");
        let mut html = String::from("<!doctype html><html><body>");
        for i in 0..160 {
            html.push_str(&format!(
                "<p>Paragraph {i}. Lorem ipsum dolor sit amet, consectetur adipiscing elit.</p>"
            ));
        }
        html.push_str("</body></html>");
        std::fs::write(&path, html).unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let mut engine = RenderEngine::open(&path, catalog, None, false).unwrap();
        engine.set_viewport(2400, 900);
        engine.set_layout(true);

        let first = engine.render().unwrap();
        assert_eq!(first.pages, vec![1, 2, 3, 4]);
        assert_eq!(engine.page_renders, 4);

        assert!(engine.goto_page(3));
        let second = engine.render().unwrap();
        assert_eq!(second.pages, vec![2, 3, 4, 5]);
        assert_eq!(
            engine.page_renders, 5,
            "a one-page shift should only rasterize the entering page"
        );

        assert!(engine.goto_page(4));
        let third = engine.render().unwrap();
        assert_eq!(third.pages, vec![3, 4, 5, 6]);
        assert_eq!(engine.page_renders, 6);

        assert!(engine.goto_page(3));
        let back = engine.render().unwrap();
        assert_eq!(back.pages, vec![2, 3, 4, 5]);
        assert_eq!(
            engine.page_renders, 7,
            "going back should reuse the pages still in the strip"
        );

        assert!(engine.goto_page(2));
        let back_again = engine.render().unwrap();
        assert_eq!(back_again.pages, vec![1, 2, 3, 4]);
        assert_eq!(engine.page_renders, 8);

        engine.set_viewport(2400, 901);
        let resized = engine.render().unwrap();
        assert_eq!(resized.pages, vec![1, 2, 3, 4]);
        assert_eq!(
            engine.page_renders, 12,
            "a viewport change invalidates every cached page"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reflowable_css_applies_background_and_foreground() {
        let _guard = doc_test_guard();
        let dir = test_dir("css");
        let path = dir.join("sample.html");
        std::fs::write(
            &path,
            "<!doctype html><html><body><h1>CSS test</h1><p>Foreground and background.</p></body></html>",
        )
        .unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));
        let css = "html, body { background-color: #112233 !important; color: #ffcc00 !important; }";
        let mut engine = RenderEngine::open(&path, catalog, Some(css.to_string()), false).unwrap();
        engine.set_viewport(800, 600);
        let page = engine.render().unwrap();
        Context::get().set_user_css("").unwrap();
        let light = page
            .bitmap
            .pixels
            .iter()
            .filter(|&&pixel| pixel == 0x00112233)
            .count();
        let yellow = page
            .bitmap
            .pixels
            .iter()
            .filter(|&&pixel| pixel == 0x00ffcc00)
            .count();
        assert!(light > 1000, "CSS background not applied");
        assert!(yellow > 0, "CSS foreground not applied");
        assert_eq!(page.bitmap.pixels[0], 0x00112233);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reflowable_font_size_reflows_and_keeps_a_page() {
        let _guard = doc_test_guard();
        let dir = test_dir("font-size");
        let path = dir.join("sample.html");
        let body = (0..120)
            .map(|i| format!("<p>Paragraph {i} has enough words to wrap across the page.</p>"))
            .collect::<String>();
        std::fs::write(
            &path,
            format!("<!doctype html><html><body>{body}</body></html>"),
        )
        .unwrap();

        let fonts = Arc::new(FontCatalog::new(None).unwrap());
        let mut engine = RenderEngine::open(&path, fonts, None, false).unwrap();
        let before_pages = engine.page_count.unwrap();
        let before_font = engine.font_scale();
        assert!(engine.adjust_font(1.5).unwrap());
        let after_pages = engine.page_count.unwrap();
        assert!(after_pages > before_pages);
        assert!((engine.font_scale() - before_font * 1.5).abs() < 0.01);
        assert!(engine.page < after_pages);
    }

    #[test]
    fn pretty_text_font_size_reflows() {
        let _guard = doc_test_guard();
        let dir = test_dir("pretty-font-size");
        let path = dir.join("sample.txt");
        let text = (0..160)
            .map(|i| format!("Paragraph {i} has enough words to wrap across the page."))
            .collect::<Vec<_>>()
            .join("\n\n");
        std::fs::write(&path, text).unwrap();

        let fonts = Arc::new(FontCatalog::new(None).unwrap());
        let mut engine = RenderEngine::open(&path, fonts, None, true).unwrap();
        let before = engine.page_count.unwrap();
        assert!(engine.adjust_font(1.5).unwrap());
        assert!(engine.page_count.unwrap() > before);
        assert!((engine.font_scale() - 1.5).abs() < 0.01);
    }

    #[test]
    fn reflowable_txt_html_and_fb2_smoke() {
        let _guard = doc_test_guard();
        let dir = test_dir("reflowable");

        std::fs::write(
            dir.join("sample.txt"),
            "Reflowable text\n\n中文 mixed with English and 😀.\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("sample.html"),
            "<!doctype html><html><body><h1>HTML 标题</h1><p>Reflowable 😀 paragraph.</p></body></html>",
        )
        .unwrap();
        std::fs::write(
            dir.join("sample.fb2"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook xmlns="http://www.gribuser.ru/xml/fictionbook/2.0">
<description><title-info><book-title>FB2 Test</book-title></title-info></description>
<body><section><p>FB2 中文 and English.</p></section></body>
</FictionBook>"#,
        )
        .unwrap();

        let catalog = Arc::new(crate::fonts::FontCatalog::new(None).unwrap());
        mupdf::set_font_loader(crate::fonts::EpubFontLoader::new(catalog.clone()));

        for name in ["sample.txt", "sample.html", "sample.fb2"] {
            let path = dir.join(name);
            let mut engine = RenderEngine::open(&path, catalog.clone(), None, false).unwrap();
            engine.set_viewport(800, 600);
            let page = engine.render().unwrap();
            assert!(page.page_count.is_some(), "{name} has no page count");
            assert!(!page.bitmap.pixels.is_empty(), "{name} rendered empty");

            let (scale, width, height) = (page.scale, page.bitmap.width, page.bitmap.height);
            engine.zoom_by(ZOOM_IN_FACTOR);
            engine.render().unwrap();
            engine.set_scale(scale);
            let restored = engine.render().unwrap();
            assert_eq!(
                (restored.bitmap.width, restored.bitmap.height),
                (width, height),
                "{name} did not restore bitmap after absolute scale"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_pages_can_use_a_very_small_fit_scale() {
        let scale = fit_page_scale(1000.0, 100_000.0, 800.0, 600.0);
        assert!(scale < 0.02);
        assert!((scale - 0.006).abs() < 1.0e-6);
    }

    #[test]
    fn copied_text_has_line_breaks_and_clamps_ranges() {
        let layer = selection_layer();
        assert_eq!(layer.text_for_range(0, 2), "Hi\nx");
        assert_eq!(layer.text_for_range(2, 0), "Hi\nx");
        assert_eq!(layer.text_for_range(99, 99), "x");
    }

    #[test]
    fn page_background_ignores_an_embedded_image_in_the_first_pixel() {
        let mut pixels = vec![0x00ff_ffff; 100 * 100];
        // A cover image occupies the top-left corner and would win a naive
        // first-pixel sample.
        for y in 0..10 {
            for x in 0..10 {
                pixels[y * 100 + x] = 0x00cc_0000;
            }
        }
        // A few dark text strokes in the middle must not beat the margins.
        for y in 40..60 {
            for x in 40..60 {
                pixels[y * 100 + x] = 0x0000_0000;
            }
        }
        let bitmap = PageBitmap {
            width: 100,
            height: 100,
            pixels,
        };
        assert_eq!(sample_page_background(&bitmap), 0x00ff_ffff);
    }

    #[test]
    fn page_background_falls_back_for_an_empty_bitmap() {
        let empty = PageBitmap {
            width: 0,
            height: 0,
            pixels: Vec::new(),
        };
        assert_eq!(sample_page_background(&empty), 0x00ff_ffff);
    }
}
