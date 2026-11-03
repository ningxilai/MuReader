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

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use fontdb::Weight;
use mupdf::{
    Colorspace, Context, Document, Matrix, Outline, Pixmap, Point, Quad, Rect, TextBlockContent,
    TextPage, TextPageFlags,
};

use crate::fonts::FontCatalog;
use crate::outline_text::{TextEngine, TextTarget};

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
const FONT_SIZE_MIN: f32 = 8.0;
const FONT_SIZE_MAX: f32 = 24.0;
pub const FONT_SIZE_STEP: f32 = 1.1;
/// Keep the four pages of a spread; each turn replaces the page that leaves.
const PAGE_CACHE_CAPACITY: usize = 4;
/// Outside a spread only the current page and one neighbor are useful.
const PAGE_CACHE_SINGLE_CAPACITY: usize = 2;

/// A spread gap is visual, not structural. It scales with the rendered page
/// width and collapses when the viewport only fits the two page widths.
pub fn spread_gap(page_width: u32) -> u32 {
    ((page_width as f32 * 0.018).round() as u32).clamp(6, 24)
}

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

    /// Text between two inclusive character indices, with line breaks between
    /// MuPDF text lines.
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
        let mut previous_line = None;
        for ch in &self.chars[start..=end] {
            if previous_line.is_some_and(|line| line != ch.line) && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push(ch.ch);
            previous_line = Some(ch.line);
        }
        out
    }
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
    /// Text-size multiplier for reflowable documents. For fixed-layout
    /// documents this stays at 1.0; page zoom is used instead.
    pub font_scale: f32,
    pub reflowable: bool,
}

#[derive(Debug, Clone)]
pub struct RenderedLink {
    pub bounds: HighlightRect,
    /// One-based target page for an internal link.
    pub target_page: Option<usize>,
    pub uri: String,
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
        let outline = doc
            .outlines()
            .map(|outlines| flatten_outlines(&outlines, 0))
            .unwrap_or_default();
        let mut engine = Self {
            doc,
            page: 0,
            page_count,
            scale: 1.0,
            fit: Fit::Page,
            reflowable,
            rotation: 0,
            spread: false,
            viewport: (1100, 900),
            search_query: None,
            outline,
            outline_text: None,
            fonts,
            source_path: path.to_path_buf(),
            pretty_text,
            font_size: LAYOUT_EM,
            page_cache: Vec::new(),
            #[cfg(test)]
            page_renders: 0,
        };
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
        self.outline = self
            .doc
            .outlines()
            .map(|outlines| flatten_outlines(&outlines, 0))
            .unwrap_or_default();
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
            self.page_cache.clear();
        }
    }

    pub fn fit(&self) -> Fit {
        self.fit
    }

    pub fn set_spread(&mut self, spread: bool) {
        self.spread = spread;
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
            mupdf::text_page::TextBlockType::Image => true,
            mupdf::text_page::TextBlockType::Text => block.lines().any(|line| {
                line.chars()
                    .any(|ch| ch.char().is_some_and(|c| !c.is_whitespace()))
            }),
            _ => false,
        })
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
    pub fn render_outline(
        &mut self,
        width: u32,
        row_height: u32,
        start: usize,
        count: usize,
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
        let mut pixels = vec![0x00202020u32; width as usize * height as usize];

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
            let (r, g, b) = if entry.depth == 0 {
                (0xf2, 0xf2, 0xf2)
            } else {
                (0xcf, 0xcf, 0xcf)
            };
            let title = truncate_outline_title(&entry.title, width, indent);
            let row_top = (index * row_height as usize) as i32;
            let mut target = TextTarget::new(&mut pixels, width, height, row_top, row_height);
            engine.draw_line(
                &title,
                14.0,
                if entry.depth == 0 {
                    Weight::BOLD
                } else {
                    Weight::NORMAL
                },
                &mut target,
                indent as i32,
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
            self.page_cache.clear();
        }
    }

    #[cfg(test)]
    pub fn set_scale(&mut self, scale: f32) {
        if scale.is_finite() && scale > 0.0 {
            self.fit = Fit::None;
            self.scale = scale.clamp(MIN_SCALE, MAX_SCALE);
            self.page_cache.clear();
        }
    }

    /// Restore the initial fit-to-page layout after an explicit zoom.
    pub fn fit_page(&mut self) {
        self.fit = Fit::Page;
        self.search_query = None;
        self.page_cache.clear();
    }

    pub fn rotate(&mut self, delta: i32) {
        self.rotation = ((self.rotation as i32 + delta).rem_euclid(360)) as u16;
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

    fn scale_for(&self, bounds: &Rect) -> Result<f32, String> {
        let page_w = bounds.width().max(1.0);
        let page_h = bounds.height().max(1.0);
        let (rotated_w, rotated_h) = if self.rotation.is_multiple_of(180) {
            (page_w, page_h)
        } else {
            (page_h, page_w)
        };

        let available_w = self
            .viewport
            .0
            .saturating_sub(MARGIN.saturating_mul(2))
            .max(1) as f32;
        let available_h = self
            .viewport
            .1
            .saturating_sub(MARGIN.saturating_mul(2))
            .max(1) as f32;

        let mut scale = match self.fit {
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

    fn render_one(
        &self,
        page_no: usize,
        ctm: &Matrix,
        quads: &[Quad],
    ) -> Result<PageRender, String> {
        let page = self
            .doc
            .load_page(page_no as i32)
            .map_err(|e| format!("load page {}: {e}", page_no + 1))?;
        let pix = page
            .to_pixmap(ctm, &Colorspace::device_rgb(), false, true)
            .map_err(|e| format!("render page {}: {e}", page_no + 1))?;

        let bitmap = bitmap_from_pixmap(&pix);
        let width = bitmap.width;
        let height = bitmap.height;
        let highlights = quads
            .iter()
            .filter_map(|quad| highlight_from_quad(quad, ctm, width, height))
            .collect();
        let text = page
            .to_text_page(TextPageFlags::PRESERVE_WHITESPACE)
            .map(|text_page| text_layer_from_page(&text_page, ctm, width, height))
            .unwrap_or_default();
        let links = page
            .links()
            .map(|links| {
                links
                    .filter_map(|link| {
                        let bounds = link_rect(&link.bounds, ctm, width, height)?;
                        Some(RenderedLink {
                            bounds,
                            target_page: link.dest.map(|dest| dest.loc.page_number as usize + 1),
                            uri: link.uri,
                        })
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

    fn render_with_quads(&mut self, quads: &[Quad]) -> Result<RenderedPage, String> {
        // A spread is a fit-to-page layout. Once the user zooms explicitly,
        // rendering four pages at that scale can cost hundreds of megabytes;
        // fall back to a single page even if the UI request has not arrived.
        let page_numbers = if self.spread && self.fit != Fit::None {
            let start = self.page.saturating_sub(1);
            vec![start, start + 1, start + 2, start + 3]
        } else {
            vec![self.page]
        };
        let cacheable = quads.is_empty() && self.search_query.is_none();
        if self.reflowable {
            let mut filtered = Vec::with_capacity(page_numbers.len());
            for &page_no in &page_numbers {
                if !self.page_is_blank(page_no) {
                    filtered.push(page_no);
                }
            }
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

        let first_bounds = self
            .doc
            .load_page(first_no as i32)
            .and_then(|page| page.bounds())
            .map_err(|e| format!("page bounds: {e}"))?;
        let scale = self.scale_for(&first_bounds)?;
        self.scale = scale;
        let first_ctm = self.ctm_for(&first_bounds, scale);

        // Search highlights only apply to the first page, so searches take
        // the direct path. Normal reading reuses the overlapping spread pages.
        let mut direct = Vec::new();
        for (index, &page_no) in numbers.iter().enumerate() {
            if cacheable
                && self.page_cache.iter().any(|(cached, cached_scale, _)| {
                    *cached == page_no && (*cached_scale - scale).abs() < 1.0e-4
                })
            {
                continue;
            }
            let ctm = if index == 0 {
                first_ctm.clone()
            } else {
                let bounds = self
                    .doc
                    .load_page(page_no as i32)
                    .and_then(|page| page.bounds())
                    .map_err(|e| format!("page bounds: {e}"))?;
                self.ctm_for(&bounds, scale)
            };
            let page_quads = if index == 0 { quads } else { &[] };
            #[cfg(test)]
            {
                self.page_renders += 1;
            }
            let rendered = self.render_one(page_no, &ctm, page_quads)?;
            if cacheable {
                self.page_cache.retain(|(cached, _, _)| *cached != page_no);
                self.make_room_for_page(&numbers);
                self.page_cache.push((page_no, scale, rendered));
            } else {
                direct.push(rendered);
            }
        }

        let rendered: Vec<&PageRender> = if cacheable {
            let mut refs = Vec::with_capacity(numbers.len());
            for page_no in &numbers {
                let Some((_, _, page)) =
                    self.page_cache.iter().find(|(cached, cached_scale, _)| {
                        cached == page_no && (*cached_scale - scale).abs() < 1.0e-4
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
        let gap = if page_widths.len() > 1 {
            let visible = page_widths[focus_index]
                .saturating_add(page_widths.get(focus_index + 1).copied().unwrap_or(0));
            spread_gap(page_widths[focus_index]).min(self.viewport.0.saturating_sub(visible))
        } else {
            0
        };
        let bitmap = compose_pages(&rendered, gap);

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
            font_scale: self.font_scale(),
            reflowable: self.reflowable,
        })
    }

    fn cache_capacity(&self) -> usize {
        if self.spread && self.fit != Fit::None {
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

fn compose_pages(pages: &[&PageRender], gap: u32) -> PageRender {
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
    let mut pixels = vec![0x00181818u32; width as usize * height as usize];

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
                .any(|link| link.target_page.is_some_and(|page| page > 1)),
            "internal link did not resolve to a later page: {:?}",
            page.links
        );
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

        engine.set_spread(true);
        engine.set_viewport(single_width * 2, 900);
        let tight = engine.render().unwrap();
        assert_eq!(tight.pages, vec![1, 2, 3, 4]);
        assert_eq!(tight.page_widths[0], single_width);
        assert_eq!(
            tight.gap, 0,
            "the visual gap must collapse when only two page widths fit"
        );

        engine.set_viewport(2400, 900);
        let spread = engine.render().unwrap();
        assert_eq!(spread.pages, vec![1, 2, 3, 4]);
        assert!(spread.gap < 24);
        assert!(spread.gap > 0);
        assert!(spread.bitmap.width > single_width);
        assert_eq!(spread.bitmap.height, single_height);
        assert!(!spread.text.is_empty());
        assert!(
            spread.text.chars.iter().any(|ch| ch.rect.x0 > single_width),
            "second-page text was not shifted into composite coordinates"
        );

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
        engine.set_spread(true);

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
}
