// SPDX-License-Identifier: AGPL-3.0-only
//! A deliberately small document viewer.
//!
//! This is not a compositor and does not host an editor. It opens one winit
//! window and asks a render worker for pages. There is no Wayland server,
//! XWayland, or IME/workspace machinery. The XDG portal supplies the document;
//! a small data-control client connection is used only when the user copies a
//! text selection.
//!
//! The UI thread never touches MuPDF. The worker owns the `Document`; only
//! finished page bitmaps and plain text-layer data cross the thread boundary.

// Keep the document, renderer, input, and progress paths free of unsafe code.
// The platform modules below opt out individually because they wrap FFI.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

mod activation;
mod clipboard;
mod config;
mod document;
mod fonts;
mod gpu;
mod keymap;
mod outline_text;
mod palette;
mod pretty_text;
mod progress;
mod startup;
mod theme;
mod worker;

use std::collections::VecDeque;
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use activation::Activation;
use clipboard::{ClipboardHandle, ClipboardNotice};
use config::Config;
use document::{
    Fit, HighlightRect, LinkTarget, OutlineEntry, PageBitmap, RenderedLink, RenderedPage,
    SearchScope, TextLayer, TextSelection, FONT_SIZE_STEP, MARGIN, MAX_SCALE, MIN_SCALE,
    SPREAD_GAP_RATIO, ZOOM_IN_FACTOR, ZOOM_OUT_FACTOR,
};
use fontdb::Weight;
use fonts::{EpubFontLoader, FontCatalog};
use gpu::{rgb, DrawList, GpuRenderer, Rect};
use keymap::{Action, Keymap, MenuKeymap, MenuResolution, Resolution};
use outline_text::{TextEngine, TextTarget};
use progress::ProgressStore;
use theme::{rgb8, Theme};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{Window, WindowId};

use worker::{Command, Event as WorkerEvent, WorkerHandle};

const TITLE: &str = "MuReader";
/// The outline takes a bounded share of the window instead of a fixed column.
const OUTLINE_MIN_WIDTH: u32 = 232;
const OUTLINE_MAX_WIDTH: u32 = 360;
const OUTLINE_MIN_CONTENT: u32 = 240;
const OUTLINE_ROW_HEIGHT: u32 = 28;
/// Below this fraction of a height-fitted single page, a spread page is too
/// narrow to read and the reader falls back to one page.
const MIN_SPREAD_PAGE_FRACTION: f32 = 0.7;
/// Rows requested at once when the outline needs a new bitmap window. A tall
/// window also gets enough margin to cover the viewport in a single request.
const OUTLINE_WINDOW_MIN_ROWS: usize = 256;
const MENU_HEADER_HEIGHT: u32 = 40;
const MENU_ROW_HEIGHT: u32 = 32;
const MENU_FOOTER_HEIGHT: u32 = 36;
const MENU_MAX_ROWS: usize = 10;
const MENU_PADDING: u32 = 16;
/// Room for a modifier combination such as `C-=` or `M-w`.
const MENU_KEY_COLUMN: u32 = 72;
/// Wait for the outline selection to settle before previewing it. Without this
/// debounce, holding an arrow key queues one full page render per row.
const OUTLINE_PREVIEW_DELAY: Duration = Duration::from_millis(90);
/// At an explicit zoom a page bitmap can be large. Skip live previews once it
/// exceeds this many pixels so outline browsing cannot stall the UI on an
/// expensive render/upload.
const OUTLINE_PREVIEW_MAX_PIXELS: u64 = 4_000_000;

const PAGE_TEXTURE: u64 = 1;
const OUTLINE_TEXTURE: u64 = 2;
const MENU_TEXTURE: u64 = 3;
const PROMPT_TEXTURE: u64 = 4;
const LOADING_TEXTURE: u64 = 5;

pub enum UserEvent {
    Worker(WorkerEvent),
    Clipboard(ClipboardNotice),
}

enum Prompt {
    None,
    Search(String, SearchScope),
}

#[derive(Clone, Copy)]
struct CommandEntry {
    label: &'static str,
    action: Action,
}

const COMMANDS: &[CommandEntry] = &[
    CommandEntry {
        label: "Next page",
        action: Action::NextPage,
    },
    CommandEntry {
        label: "Previous page",
        action: Action::PrevPage,
    },
    CommandEntry {
        label: "First page",
        action: Action::FirstPage,
    },
    CommandEntry {
        label: "Last page",
        action: Action::LastPage,
    },
    CommandEntry {
        label: "Back",
        action: Action::Back,
    },
    CommandEntry {
        label: "Forward",
        action: Action::Forward,
    },
    CommandEntry {
        label: "Show outline",
        action: Action::Outline,
    },
    CommandEntry {
        label: "Search this page",
        action: Action::SearchPage,
    },
    CommandEntry {
        label: "Search the whole book",
        action: Action::SearchBook,
    },
    CommandEntry {
        label: "Zoom in",
        action: Action::ZoomIn,
    },
    CommandEntry {
        label: "Zoom out",
        action: Action::ZoomOut,
    },
    CommandEntry {
        label: "Increase text size",
        action: Action::FontIncrease,
    },
    CommandEntry {
        label: "Decrease text size",
        action: Action::FontDecrease,
    },
    CommandEntry {
        label: "Reset view",
        action: Action::ResetView,
    },
    CommandEntry {
        label: "Toggle single page",
        action: Action::ToggleSpread,
    },
    CommandEntry {
        label: "Rotate right",
        action: Action::RotateRight,
    },
    CommandEntry {
        label: "Rotate left",
        action: Action::RotateLeft,
    },
    CommandEntry {
        label: "Copy selection",
        action: Action::Copy,
    },
    CommandEntry {
        label: "Quit",
        action: Action::Quit,
    },
];

/// Keys the panel owns. Bindings containing any of these are hidden from the
/// panel, so a config cannot create a shortcut that the panel would intercept.
const MENU_RESERVED: &[&str] = &[
    "Escape",
    "Enter",
    "Up",
    "Down",
    "PageUp",
    "PageDown",
    "Home",
    "End",
    "Backspace",
    "C-g",
    "q",
];

fn menu_actions() -> Vec<Action> {
    COMMANDS.iter().map(|entry| entry.action).collect()
}

fn command_label(action: Action) -> &'static str {
    COMMANDS
        .iter()
        .find(|entry| entry.action == action)
        .map_or("", |entry| entry.label)
}

fn action_closes_menu(action: Action) -> bool {
    matches!(
        action,
        Action::Outline | Action::SearchPage | Action::SearchBook | Action::Quit
    )
}

/// Modifier keys are delivered as their own press events and are not bindable.
/// They must be ignored rather than treated as a rejected chord.
fn is_modifier_key<S>(key: Key<S>) -> bool {
    matches!(
        key,
        Key::Named(
            NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::CapsLock
                | NamedKey::Control
                | NamedKey::Fn
                | NamedKey::FnLock
                | NamedKey::Hyper
                | NamedKey::Meta
                | NamedKey::NumLock
                | NamedKey::ScrollLock
                | NamedKey::Shift
                | NamedKey::Super
                | NamedKey::Symbol
                | NamedKey::SymbolLock
        )
    )
}

/// Display width in roughly 8px units: ASCII occupies one, CJK and other wide
/// characters occupy two. Used only for truncating small UI labels.
fn display_units(ch: char) -> usize {
    if ch.is_ascii() {
        1
    } else {
        2
    }
}

fn truncate_display(text: &str, max_units: usize) -> String {
    if text.chars().map(display_units).sum::<usize>() <= max_units {
        return text.to_string();
    }
    let ellipsis = display_units('…');
    if max_units < ellipsis {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let units = display_units(ch);
        if used + units + ellipsis > max_units {
            break;
        }
        out.push(ch);
        used += units;
    }
    out.push('…');
    out
}

#[allow(clippy::too_many_arguments)]
fn fill_ui_rect(
    pixels: &mut [u32],
    width: u32,
    height: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    color: u32,
) {
    let x0 = x.max(0).min(width as i32) as u32;
    let y0 = y.max(0).min(height as i32) as u32;
    let x1 = x.saturating_add(w).clamp(0, width as i32) as u32;
    let y1 = y.saturating_add(h).clamp(0, height as i32) as u32;
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    for row in y0..y1 {
        let start = row as usize * width as usize + x0 as usize;
        pixels[start..start + (x1 - x0) as usize].fill(color);
    }
}

/// The pending shortcut view is a which-key grid: candidates are short, so a
/// second column makes the panel wider rather than taller.
fn menu_pending_columns(viewport_w: u32, count: usize) -> usize {
    if count > 8 && viewport_w >= 720 {
        2
    } else {
        1
    }
}

fn menu_row_count(count: usize, columns: usize) -> usize {
    count.div_ceil(columns.max(1)).max(1)
}

/// How many command rows fit in the panel without hiding it under the window
/// edge. The panel still scrolls when the window is short.
fn menu_visible_rows(viewport_h: u32, count: usize) -> usize {
    let reserved = MENU_HEADER_HEIGHT + MENU_FOOTER_HEIGHT + 8;
    let available = viewport_h.saturating_sub(reserved);
    ((available / MENU_ROW_HEIGHT) as usize).clamp(1, count.max(1))
}

struct CommandMenu {
    selected: usize,
    scroll: usize,
    rows: usize,
    columns: usize,
    /// Last action, echoed in the panel footer and the window title.
    echo: Option<String>,
    dirty: bool,
    bitmap: Option<PageBitmap>,
    revision: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct OutlineRequest {
    width: u32,
    start: usize,
    selected: usize,
    background: u32,
}

struct PanelBitmap {
    text: String,
    width: u32,
    height: u32,
    revision: u64,
    bitmap: PageBitmap,
}

struct PendingOutlinePreview {
    page: usize,
    at: Instant,
}

struct PendingZoom {
    anchor: (f32, f32),
    screen: (f32, f32),
    expected_scale: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnDirection {
    Next,
    Prev,
}

/// Destination history for links and outline jumps. Ordinary page turns do
/// not record entries, so Back returns to the page a jump started from rather
/// than stepping through every page that was read.
#[derive(Default)]
struct NavigationHistory {
    back: Vec<usize>,
    forward: Vec<usize>,
}

impl NavigationHistory {
    const LIMIT: usize = 64;

    fn record(&mut self, page: usize) {
        if self.back.last() == Some(&page) {
            return;
        }
        self.back.push(page);
        if self.back.len() > Self::LIMIT {
            self.back.remove(0);
        }
        self.forward.clear();
    }

    fn go_back(&mut self, current: usize) -> Option<usize> {
        let page = self.back.pop()?;
        self.forward.push(current);
        Some(page)
    }

    fn go_forward(&mut self, current: usize) -> Option<usize> {
        let page = self.forward.pop()?;
        self.back.push(current);
        Some(page)
    }
}

struct SpreadAnimation {
    started: Instant,
    from: f32,
    to: f32,
    duration: Duration,
    command: TurnDirection,
}

struct App {
    path: PathBuf,
    proxy: EventLoopProxy<UserEvent>,
    worker: Option<WorkerHandle>,
    // Drop order matters: the GPU surface before the window.
    gpu: Option<GpuRenderer>,
    window: Option<Arc<Window>>,
    bitmap: Option<PageBitmap>,
    page_revision: u64,
    text: TextLayer,
    selection: Option<TextSelection>,
    selecting: bool,
    selection_dragged: bool,
    panning: bool,
    cursor: Option<PhysicalPosition<f64>>,
    clipboard: Option<ClipboardHandle>,
    activation: Activation,
    highlights: Vec<HighlightRect>,
    links: Vec<RenderedLink>,
    /// Rendered page background, used by the outline selection rail and row
    /// veil. White until the first page arrives; reflowable documents follow
    /// their effective CSS background.
    page_background: u32,
    outline: Vec<OutlineEntry>,
    outline_open: bool,
    outline_selected: usize,
    outline_bitmap: Option<PageBitmap>,
    outline_bitmap_start: usize,
    /// Width and page background baked into the current outline bitmap.
    outline_bitmap_width: u32,
    outline_bitmap_selected: Option<usize>,
    outline_bitmap_background: u32,
    outline_revision: u64,
    /// The exact outline bitmap request currently in flight.
    outline_requested: Option<OutlineRequest>,
    outline_scroll: f32,
    /// Pending, debounced preview of the outline selection.
    outline_preview: Option<PendingOutlinePreview>,
    /// Page shown before the outline started previewing other entries.
    outline_preview_origin: Option<usize>,
    /// Outline entry that contains the origin page. Selecting it again returns
    /// to that exact page instead of the entry's own first page.
    outline_origin_entry: Option<usize>,
    page: usize,
    pages: Vec<usize>,
    page_widths: Vec<u32>,
    spread_gap: u32,
    spread_offset: f32,
    window_index: usize,
    page_count: Option<usize>,
    scale: f32,
    page_fit: Fit,
    font_scale: f32,
    reflowable: bool,
    rotation: u16,
    scroll_x: f32,
    scroll_y: f32,
    modifiers: ModifiersState,
    prompt: Prompt,
    command_menu: Option<CommandMenu>,
    prompt_bitmap: Option<PanelBitmap>,
    loading_bitmap: Option<PanelBitmap>,
    menu_text: Option<TextEngine>,
    status: Option<String>,
    theme: Theme,
    /// True when the CSS names no colors, so the UI keeps following the
    /// rendered page background instead of pinning a palette.
    follow_document_theme: bool,
    fonts: Arc<FontCatalog>,
    keymap: Keymap,
    menu_keys: MenuKeymap,
    menu_entries: Vec<CommandEntry>,
    pending_zooms: VecDeque<PendingZoom>,
    last_requested_scale: f32,
    css: Option<String>,
    pretty_text: bool,
    page_turn_duration: Duration,
    window_fit_duration: Duration,
    pending_viewport: Option<Instant>,
    resize_preview: bool,
    last_viewport_sent: Option<(u32, u32)>,
    spread_active: bool,
    /// Set when a spread had to be abandoned because it no longer fit. This
    /// prevents the worker's replacement single-page bitmap from immediately
    /// enabling the spread again and creating a render loop.
    spread_blocked: bool,
    /// User preference for the two-page spread. It starts enabled so the
    /// default reading mode is a spread, and a manual toggle turns the
    /// automatic enabling off until the user asks for it again.
    spread_enabled: bool,
    pending_turn: Option<TurnDirection>,
    animation: Option<SpreadAnimation>,
    progress: ProgressStore,
    /// Pages visited by following a link or an outline entry. Normal page
    /// turns do not create history entries; Back/Forward move between
    /// destinations, like a browser's history.
    history: NavigationHistory,
    /// Last page written to the progress file.
    recorded_page: Option<usize>,
    /// Page restored from the progress file, used for the initial status.
    resume_page: Option<usize>,
    /// The window stays hidden until a frame has actually been presented, so
    /// the compositor never shows an uninitialized black surface.
    window_shown: bool,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        path: PathBuf,
        proxy: EventLoopProxy<UserEvent>,
        fonts: Arc<FontCatalog>,
        keymap: Keymap,
        css: Option<String>,
        theme: Theme,
        pretty_text: bool,
        page_turn_duration: Duration,
        window_fit_duration: Duration,
        progress_limit: usize,
    ) -> Self {
        let progress = ProgressStore::load(progress_limit);
        let resume_page = progress.page_for(&path).filter(|&page| page > 1);
        let menu_keys = keymap.menu_keymap(&menu_actions(), MENU_RESERVED);
        let menu_entries: Vec<CommandEntry> = COMMANDS
            .iter()
            .copied()
            .filter(|entry| menu_keys.has_binding(entry.action))
            .collect();
        let follow_document_theme = !Theme::has_css_colors(css.as_deref());
        Self {
            path,
            proxy,
            worker: None,
            gpu: None,
            window: None,
            bitmap: None,
            page_revision: 0,
            text: TextLayer::default(),
            selection: None,
            selecting: false,
            selection_dragged: false,
            panning: false,
            cursor: None,
            clipboard: None,
            activation: Activation::from_env(),
            highlights: Vec::new(),
            links: Vec::new(),
            page_background: 0x00ff_ffff,
            outline: Vec::new(),
            outline_open: false,
            outline_selected: 0,
            outline_bitmap: None,
            outline_bitmap_start: 0,
            outline_bitmap_width: 0,
            outline_bitmap_selected: None,
            outline_bitmap_background: 0x00ff_ffff,
            outline_revision: 0,
            outline_requested: None,
            outline_scroll: 0.0,
            outline_preview: None,
            outline_preview_origin: None,
            outline_origin_entry: None,
            page: 1,
            pages: Vec::new(),
            page_widths: Vec::new(),
            spread_gap: 0,
            spread_offset: 0.0,
            window_index: 0,
            page_count: None,
            scale: 1.0,
            page_fit: Fit::Page,
            font_scale: 1.0,
            reflowable: false,
            rotation: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            modifiers: ModifiersState::empty(),
            prompt: Prompt::None,
            command_menu: None,
            prompt_bitmap: None,
            loading_bitmap: None,
            menu_text: None,
            status: None,
            theme,
            follow_document_theme,
            fonts,
            keymap,
            menu_keys,
            menu_entries,
            pending_zooms: VecDeque::new(),
            last_requested_scale: 1.0,
            css,
            pretty_text,
            page_turn_duration,
            window_fit_duration,
            pending_viewport: None,
            resize_preview: false,
            last_viewport_sent: None,
            spread_active: false,
            spread_blocked: false,
            spread_enabled: true,
            pending_turn: None,
            animation: None,
            progress,
            history: NavigationHistory::default(),
            recorded_page: None,
            resume_page,
            window_shown: false,
        }
    }

    fn viewport(&self) -> (u32, u32) {
        self.window
            .as_ref()
            .map(|w| {
                let size = w.inner_size();
                (size.width, size.height)
            })
            .unwrap_or((0, 0))
    }

    fn send(&self, command: Command) {
        if let Some(worker) = self.worker.as_ref() {
            worker.send(command);
        }
    }

    /// Theme colors are baked into the cached panels and into the worker-side
    /// spread gutter, so drop the caches and repaint with the new palette.
    fn invalidate_theme(&mut self) {
        self.prompt_bitmap = None;
        self.loading_bitmap = None;
        self.outline_bitmap = None;
        self.outline_requested = None;
        self.outline_bitmap_selected = None;
        if let Some(menu) = self.command_menu.as_mut() {
            menu.dirty = true;
        }
        self.send(Command::SetBackground(self.theme.window));
    }

    fn request_redraw(&self) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn min_scroll_y(&self) -> f32 {
        let (_, viewport_h) = self.viewport();
        let scale = self.display_scale();
        let page_h = self.bitmap.as_ref().map(|b| b.height as f32).unwrap_or(0.0) * scale;
        ((page_h + MARGIN as f32 * 2.0 - viewport_h as f32) / 2.0 / scale).min(0.0)
    }

    fn max_scroll_y(&self) -> f32 {
        let (_, viewport_h) = self.viewport();
        let scale = self.display_scale();
        let page_h = self.bitmap.as_ref().map(|b| b.height as f32).unwrap_or(0.0) * scale;
        (page_h + MARGIN as f32 * 2.0 - viewport_h as f32).max(0.0) / scale
    }

    fn page_fits_height(&self) -> bool {
        let (_, viewport_h) = self.viewport();
        let scale = self.display_scale();
        self.bitmap.as_ref().is_some_and(|page| {
            (page.height as f32 * scale).round() as u32 <= viewport_h.saturating_add(1)
        })
    }

    fn content_left(&self) -> i32 {
        if self.outline_open {
            (self.outline_width() as i32).min(self.viewport().0 as i32)
        } else {
            0
        }
    }

    /// The outline widens with the window, but never enough to crowd out the
    /// page. This makes opening it on a small window much gentler than the old
    /// fixed 320px column.
    fn outline_width(&self) -> u32 {
        let (viewport_w, _) = self.viewport();
        let preferred = (viewport_w as f32 * 0.15).round() as u32;
        let max_by_content = viewport_w.saturating_sub(OUTLINE_MIN_CONTENT).max(160);
        preferred
            .clamp(OUTLINE_MIN_WIDTH, OUTLINE_MAX_WIDTH)
            .min(max_by_content)
    }

    /// Scale applied to the already-rendered page when the outline covers part
    /// of the window. Rendering stays at the full-window size, so opening and
    /// closing the outline only changes this scale instead of waiting for a
    /// worker re-render.
    fn display_scale(&self) -> f32 {
        if !self.outline_open || self.page_fit != Fit::Page {
            return 1.0;
        }
        let natural = self.visible_width() as f32;
        if natural <= 1.0 {
            return 1.0;
        }
        let (viewport_w, _) = self.viewport();
        let content_w = (viewport_w as i32 - self.content_left()).max(1) as f32;
        (content_w / natural).clamp(0.1, 1.0)
    }

    fn visible_page_count(&self) -> usize {
        self.page_widths
            .len()
            .saturating_sub(self.window_index)
            .min(2)
    }

    fn spread_pages_width(&self) -> u32 {
        let Some(&first) = self.page_widths.get(self.window_index) else {
            return 0;
        };
        match self.page_widths.get(self.window_index + 1) {
            Some(&second) => first.saturating_add(second),
            None => first,
        }
    }

    fn visible_width(&self) -> u32 {
        let pages = self.spread_pages_width();
        if self.visible_page_count() >= 2 {
            pages.saturating_add(self.spread_gap)
        } else {
            pages
        }
    }

    fn offset_for_index(&self, index: usize) -> f32 {
        let mut offset = 0;
        for slot in 0..index.min(self.page_widths.len()) {
            offset += self.page_widths[slot] + self.spread_gap;
        }
        offset as f32
    }

    fn normal_offset(&self) -> f32 {
        self.offset_for_index(self.window_index)
    }

    fn max_scroll_x(&self) -> f32 {
        let (viewport_w, _) = self.viewport();
        let content_w = (viewport_w as i32 - self.content_left()).max(1) as f32;
        let scale = self.display_scale();
        let page_w = self.visible_width() as f32 * scale;
        ((page_w - content_w) / 2.0 / scale).max(0.0)
    }

    /// Whether the current document and window can support a comfortable
    /// two-page spread. A spread may shrink both pages to fit, so this is not
    /// "two full-size pages fit"; instead it rejects a spread whose pages
    /// would be too narrow to read.
    fn spread_can_fit(&self) -> bool {
        if self.page_count.unwrap_or(0) < 2 || self.page_fit != Fit::Page {
            return false;
        }
        let Some(single) = self.single_page_width() else {
            return false;
        };
        let prospective = self.viewport().0 as f32 / (2.0 + SPREAD_GAP_RATIO);
        prospective >= single * MIN_SPREAD_PAGE_FRACTION
    }

    fn maybe_enable_spread(&mut self) {
        if self.spread_active || self.spread_blocked || self.pages.len() >= 2 {
            return;
        }
        if !self.spread_enabled {
            return;
        }
        // Do not start a spread while the outline is open: the overlay makes
        // the visible content narrower, and a pair that looked comfortable in
        // the full window can become cramped. An already active pair is kept
        // and display-scaled instead, so opening the outline never collapses it.
        if self.outline_open {
            return;
        }
        if !self.spread_can_fit() {
            return;
        }

        self.spread_active = true;
        self.send(Command::SetSpread(true));
        self.update_title();
    }

    /// Switch between the default two-page spread and a centered single page.
    /// The choice is a user preference, so single page stays single even after
    /// the next page arrives. Turning the spread back on clears an automatic
    /// narrow-window block and asks the worker to re-render a pair.
    fn toggle_spread(&mut self) {
        self.finish_animation();
        self.spread_enabled = !self.spread_enabled;
        self.spread_blocked = false;
        if !self.spread_enabled {
            self.spread_active = false;
            self.send(Command::SetSpread(false));
            self.status = Some("single page".to_string());
        } else if self.page_fit != Fit::Page {
            // A spread is a fit-to-page layout. Return to fit first; the next
            // page event can then start the spread through the normal path.
            self.spread_active = false;
            self.reset_view();
            self.status = Some("two-page spread".to_string());
        } else if self.spread_can_fit() {
            self.spread_active = true;
            self.send(Command::SetSpread(true));
            self.status = Some("two-page spread".to_string());
        } else {
            self.spread_active = false;
            self.status = Some("single page".to_string());
        }
        self.update_title();
        self.request_redraw();
    }

    fn update_spread_layout(&mut self) {
        if self.spread_active {
            // An explicit zoom must not keep rendering a four-page strip at
            // an unbounded scale. Drop back to one page and let the fit-page
            // path enable the spread again later.
            if self.page_fit != Fit::Page {
                self.spread_active = false;
                self.send(Command::SetSpread(false));
                return;
            }
            if self.pages.len() < 2 {
                return;
            }
            if self.bitmap.is_none() {
                return;
            }
            // Tiling another window can squeeze the pair below a comfortable
            // reading width. Compare each spread page with the width a
            // height-fitted single page would have, and return to one page
            // instead of shrinking indefinitely. Opening the outline no longer
            // triggers this path because the worker viewport is unchanged.
            let page_width = self
                .page_widths
                .get(self.window_index)
                .copied()
                .unwrap_or(0) as f32;
            let single_width = self.single_page_width();
            let too_narrow =
                single_width.is_some_and(|single| page_width < single * MIN_SPREAD_PAGE_FRACTION);
            if too_narrow {
                self.spread_active = false;
                self.spread_blocked = true;
                self.send(Command::SetSpread(false));
            }
        } else {
            self.maybe_enable_spread();
        }
    }

    /// Width of a height-fitted single page at the current window size. The
    /// current page's aspect ratio is preserved, including rotation.
    fn single_page_width(&self) -> Option<f32> {
        let page = self.bitmap.as_ref()?;
        let page_width = *self.page_widths.get(self.window_index)? as f32;
        let page_height = page.height as f32;
        if page_width <= 0.0 || page_height <= 0.0 {
            return None;
        }
        let (viewport_w, viewport_h) = self.viewport();
        let aspect = page_width / page_height;
        Some((viewport_h as f32 * aspect).min(viewport_w as f32))
    }

    fn send_viewport(&mut self) {
        self.pending_viewport = None;
        let (width, height) = self.viewport();
        if width == 0 || height == 0 {
            return;
        }
        // The worker always lays out for the full window. The outline is a
        // display overlay, so opening it never triggers a page re-render and
        // its width change cannot cause a delayed layout hitch.
        let target = (width, height);
        let unchanged = self.last_viewport_sent == Some(target);
        self.last_viewport_sent = Some(target);
        self.send(Command::Viewport(width, height));
        // At an explicit zoom the worker keeps the current bitmap, so no
        // further Page event will clear the preview. Finish it here.
        if resize_preview_done(self.page_fit, unchanged) {
            self.resize_preview = false;
        }
    }

    fn schedule_viewport(&mut self) {
        let delay = if self.window_fit_duration.is_zero() {
            Duration::from_millis(16)
        } else {
            self.window_fit_duration
        };
        self.pending_viewport = Some(Instant::now() + delay);
        self.resize_preview = true;
    }

    fn clamp_scroll(&mut self) {
        self.scroll_y = self
            .scroll_y
            .clamp(self.min_scroll_y(), self.max_scroll_y());
        self.scroll_x = self
            .scroll_x
            .clamp(-self.max_scroll_x(), self.max_scroll_x());
    }

    fn scroll_vertical(&mut self, delta: f32) {
        self.scroll_y += delta / self.display_scale();
        self.clamp_scroll();
        self.request_redraw();
    }

    fn scroll_horizontal(&mut self, delta: f32) {
        self.scroll_x += delta / self.display_scale();
        self.clamp_scroll();
        self.request_redraw();
    }

    fn scroll_down_screen(&mut self) {
        let (_, viewport_h) = self.viewport();
        self.scroll_vertical(viewport_h as f32 * 0.9);
    }

    fn scroll_up_screen(&mut self) {
        let (_, viewport_h) = self.viewport();
        self.scroll_vertical(-(viewport_h as f32 * 0.9));
    }

    fn scroll_down_or_next(&mut self) {
        if !self.page_fits_height() && self.scroll_y + 0.5 < self.max_scroll_y() {
            self.scroll_down_screen();
        } else {
            self.send(Command::NextPage);
        }
    }

    fn scroll_up_or_prev(&mut self) {
        if !self.page_fits_height() && self.scroll_y > self.min_scroll_y() + 0.5 {
            self.scroll_up_screen();
        } else {
            self.send(Command::PrevPage);
        }
    }

    fn scroll_right_most(&mut self) {
        self.scroll_x = self.max_scroll_x();
        self.request_redraw();
    }

    fn scroll_left_most(&mut self) {
        self.scroll_x = -self.max_scroll_x();
        self.request_redraw();
    }

    fn outline_height(&self) -> f32 {
        self.outline.len() as f32 * OUTLINE_ROW_HEIGHT as f32
    }

    fn outline_max_scroll(&self) -> f32 {
        let (_, viewport_h) = self.viewport();
        (self.outline_height() - viewport_h as f32).max(0.0)
    }

    fn request_outline_window(&mut self, start: usize, count: usize) {
        if !self.outline_open || self.outline.is_empty() {
            return;
        }
        let start = start.min(self.outline.len() - 1);
        let count = count.clamp(1, self.outline.len() - start);
        let selected = self.outline_selected.min(self.outline.len() - 1);
        let request = OutlineRequest {
            width: self.outline_width(),
            start,
            selected,
            background: self.page_background,
        };
        if self.outline_requested == Some(request) {
            return;
        }
        self.outline_requested = Some(request);
        self.send(Command::RenderOutline {
            width: request.width,
            row_height: OUTLINE_ROW_HEIGHT,
            start,
            count,
            selected: Some(selected),
            // Match the command panel: a selected row uses the raised surface
            // and a slim accent rail instead of a wash of the page background.
            veil: Some(self.theme.panel_selected),
            rail: Some(self.theme.accent),
            style: self.theme.outline,
        });
    }

    fn ensure_outline_window(&mut self) {
        if self.outline.is_empty() {
            return;
        }
        let (_, viewport_h) = self.viewport();
        let current_rows = self
            .outline_bitmap
            .as_ref()
            .map(|bitmap| (bitmap.height as usize / OUTLINE_ROW_HEIGHT as usize).max(1))
            .unwrap_or(0);
        let window = outline_window_request(
            self.outline.len(),
            self.outline_scroll,
            viewport_h,
            OUTLINE_ROW_HEIGHT,
            self.outline_bitmap_start,
            current_rows,
        );
        let stale = (self.outline_bitmap.is_some()
            && self.outline_bitmap_width != self.outline_width())
            || self.outline_bitmap_background != self.page_background;
        let (start, count) = match window {
            Some(window) => window,
            // The visible window is already cached, but the selection, panel
            // width, or page background changed: re-render the same rows.
            None if stale || self.outline_bitmap_selected != Some(self.outline_selected) => {
                let start = self.outline_bitmap_start.min(self.outline.len() - 1);
                (start, current_rows.max(1))
            }
            None => return,
        };
        self.request_outline_window(start, count);
    }

    fn ensure_outline_visible(&mut self) {
        let (_, viewport_h) = self.viewport();
        let row_height = OUTLINE_ROW_HEIGHT as f32;
        let top = self.outline_selected as f32 * row_height;
        let bottom = top + row_height;
        if top < self.outline_scroll {
            self.outline_scroll = top;
        } else if bottom > self.outline_scroll + viewport_h as f32 {
            self.outline_scroll = bottom - viewport_h as f32;
        }
        self.outline_scroll = self.outline_scroll.clamp(0.0, self.outline_max_scroll());
        self.ensure_outline_window();
    }

    fn close_outline(&mut self) {
        self.finish_animation();
        self.outline_preview = None;
        // A preview may still be in flight, so restore the original page even
        // when `self.page` has not caught up yet.
        if let Some(origin) = self.outline_preview_origin.take() {
            self.send(Command::GotoPage(origin));
        }
        self.outline_open = false;
        self.outline_requested = None;
        self.outline_bitmap_selected = None;
        self.outline_origin_entry = None;
        self.spread_blocked = false;
        self.pending_zooms.clear();
        self.last_requested_scale = self.scale;
        self.send_viewport();
        self.update_title();
        self.request_redraw();
    }

    fn select_outline(&mut self, index: usize) {
        if self.outline.is_empty() {
            return;
        }
        self.outline_selected = index.min(self.outline.len() - 1);
        self.ensure_outline_visible();
        self.preview_outline_selection();
        self.update_title();
        self.request_redraw();
    }

    fn move_outline(&mut self, delta: isize) {
        if self.outline.is_empty() {
            return;
        }
        let last = self.outline.len() - 1;
        let next = (self.outline_selected as isize + delta).clamp(0, last as isize) as usize;
        if next != self.outline_selected {
            self.select_outline(next);
        }
    }

    /// Preview the selected outline target after the selection settles.
    /// Escape restores the page that was visible before the outline opened;
    /// Enter keeps the previewed page.
    fn preview_outline_selection(&mut self) {
        self.outline_preview = None;
        if self.outline_preview_origin.is_none() {
            return;
        }
        let Some(page) = outline_target_page(
            &self.outline,
            self.outline_selected,
            self.outline_origin_entry,
            self.outline_preview_origin,
        ) else {
            return;
        };
        if page == self.page || !outline_preview_allowed(self.page_fit, self.bitmap.as_ref()) {
            return;
        }
        self.outline_preview = Some(PendingOutlinePreview {
            page,
            at: Instant::now() + OUTLINE_PREVIEW_DELAY,
        });
    }

    fn activate_outline(&mut self) {
        self.finish_animation();
        self.outline_preview = None;
        let page = outline_target_page(
            &self.outline,
            self.outline_selected,
            self.outline_origin_entry,
            self.outline_preview_origin,
        );
        if let Some(page) = page {
            if let Some(origin) = self.outline_preview_origin.take() {
                if origin != page {
                    self.push_history(origin);
                }
            }
            self.send(Command::GotoPage(page));
            self.close_outline();
        } else {
            self.status = Some("outline entry has no page".to_string());
            self.update_title();
            self.request_redraw();
        }
    }

    fn handle_outline_key(&mut self, event: &winit::event::KeyEvent) -> bool {
        match event.logical_key.as_ref() {
            Key::Named(NamedKey::Escape) => {
                self.close_outline();
                true
            }
            Key::Named(NamedKey::Enter) => {
                self.activate_outline();
                true
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.move_outline(1);
                true
            }
            Key::Named(NamedKey::ArrowUp) => {
                self.move_outline(-1);
                true
            }
            Key::Named(NamedKey::PageDown) => {
                self.move_outline(10);
                true
            }
            Key::Named(NamedKey::PageUp) => {
                self.move_outline(-10);
                true
            }
            Key::Named(NamedKey::Home) => {
                self.select_outline(0);
                true
            }
            Key::Named(NamedKey::End) => {
                self.select_outline(self.outline.len().saturating_sub(1));
                true
            }
            Key::Character(ch) => match ch {
                "o" => {
                    self.close_outline();
                    true
                }
                "q" | "Q" => {
                    self.close_outline();
                    true
                }
                "n" => {
                    self.move_outline(1);
                    true
                }
                "p" => {
                    self.move_outline(-1);
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn next_page(&mut self) {
        if self.animation.is_some() {
            self.finish_animation();
            return;
        }
        if self.pending_turn.is_some() {
            return;
        }
        if self.can_slide(TurnDirection::Next) {
            self.start_slide(TurnDirection::Next);
        } else {
            self.send(Command::NextPage);
        }
    }

    fn prev_page(&mut self) {
        if self.animation.is_some() {
            self.finish_animation();
            return;
        }
        if self.pending_turn.is_some() {
            return;
        }
        if self.can_slide(TurnDirection::Prev) {
            self.start_slide(TurnDirection::Prev);
        } else {
            self.send(Command::PrevPage);
        }
    }

    fn first_page(&mut self) {
        self.finish_animation();
        self.pending_turn = None;
        self.send(Command::FirstPage);
    }

    fn last_page(&mut self) {
        self.finish_animation();
        self.pending_turn = None;
        self.send(Command::LastPage);
    }

    fn goto_page(&mut self, page: usize) {
        self.finish_animation();
        self.pending_turn = None;
        self.send(Command::GotoPage(page));
    }

    /// Record a destination jump. Page turns and scrolling do not call this;
    /// only link following and outline activation create history entries.
    fn push_history(&mut self, page: usize) {
        self.history.record(page);
    }

    fn go_back(&mut self) {
        let Some(page) = self.history.go_back(self.page) else {
            self.status = Some("no previous location".to_string());
            self.update_title();
            self.request_redraw();
            return;
        };
        self.status = Some(format!("back → page {page}"));
        self.goto_page(page);
        self.update_title();
        self.request_redraw();
    }

    fn go_forward(&mut self) {
        let Some(page) = self.history.go_forward(self.page) else {
            self.status = Some("no forward location".to_string());
            self.update_title();
            self.request_redraw();
            return;
        };
        self.status = Some(format!("forward → page {page}"));
        self.goto_page(page);
        self.update_title();
        self.request_redraw();
    }

    /// The single path for every link: internal links jump to a page and are
    /// recorded in history, external links are handed to the desktop.
    fn follow_link(&mut self, target: Option<LinkTarget>) {
        let Some(target) = target else {
            self.status = Some("link has no destination".to_string());
            self.update_title();
            self.request_redraw();
            return;
        };
        match target {
            LinkTarget::Page(page) => {
                let page = self.page_count.map_or(page, |count| page.min(count));
                if page != self.page {
                    self.push_history(self.page);
                    self.goto_page(page);
                }
                self.status = Some(format!("link → page {page}"));
            }
            LinkTarget::Uri(uri) => match open_external(&uri) {
                Ok(()) => self.status = Some(format!("opened {uri}")),
                Err(error) => self.status = Some(error),
            },
        }
        self.update_title();
        self.request_redraw();
    }

    fn zoom_in(&mut self) {
        self.zoom(ZOOM_IN_FACTOR, Command::ZoomIn);
    }

    fn zoom_out(&mut self) {
        self.zoom(ZOOM_OUT_FACTOR, Command::ZoomOut);
    }

    fn font_increase(&mut self) {
        self.change_font_size(FONT_SIZE_STEP);
    }

    fn font_decrease(&mut self) {
        self.change_font_size(1.0 / FONT_SIZE_STEP);
    }

    fn change_font_size(&mut self, factor: f32) {
        if !self.reflowable {
            self.status = Some("text size is only available for reflowable documents".to_string());
            self.update_title();
            return;
        }
        self.finish_animation();
        self.pending_turn = None;
        self.status = None;
        self.send(Command::FontScale(factor));
    }

    fn reset_view(&mut self) {
        self.finish_animation();
        self.pending_zooms.clear();
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        // A reset means "fit the page again", so the spread may be eligible
        // after the worker reports the new fit-page scale.
        self.spread_blocked = false;
        self.send(Command::FitPage);
    }

    fn zoom(&mut self, factor: f32, command: Command) {
        self.finish_animation();
        let expected_scale = (self.last_requested_scale * factor).clamp(MIN_SCALE, MAX_SCALE);
        self.last_requested_scale = expected_scale;
        let screen = self.zoom_screen_point();
        if let Some(anchor) = self.page_point_at_screen(screen) {
            self.pending_zooms.push_back(PendingZoom {
                anchor,
                screen,
                expected_scale,
            });
        }
        self.send(command);
    }

    /// Zoom around the pointer when it is over the page, otherwise around the
    /// intersection of the viewport diagonals.
    fn zoom_screen_point(&self) -> (f32, f32) {
        if let Some(cursor) = self.cursor {
            if self.page_coords(cursor).is_some() {
                return (cursor.x as f32, cursor.y as f32);
            }
        }

        let (viewport_w, viewport_h) = self.viewport();
        let content_left = self.content_left();
        let content_w = (viewport_w as i32 - content_left).max(1) as f32;
        (
            content_left as f32 + content_w / 2.0,
            viewport_h as f32 / 2.0,
        )
    }

    /// The normalized page-space point currently under `screen`.
    fn page_point_at_screen(&self, screen: (f32, f32)) -> Option<(f32, f32)> {
        let page = self.bitmap.as_ref()?;
        let width = self.visible_width();
        if width == 0 || page.height == 0 {
            return None;
        }
        let scale = self.display_scale();
        let display_w = width as f32 * scale;
        let display_h = page.height as f32 * scale;
        // The screen coordinate must be relative to the visible window, not
        // the whole rendered strip. `page_origin()` points at the strip start
        // and therefore still includes `spread_offset`.
        let window_left = self.page_left_offset(width) as f32 - self.scroll_x * scale;
        let window_top = self.page_vertical_offset(page.height) as f32 - self.scroll_y * scale;
        Some((
            (screen.0 - window_left) / display_w,
            (screen.1 - window_top) / display_h,
        ))
    }

    /// Move the new bitmap so that a normalized page point sits under the
    /// screen point where the zoom was requested.
    fn place_page_point(&mut self, point: (f32, f32), screen: (f32, f32)) {
        let Some(page) = self.bitmap.as_ref() else {
            return;
        };
        let scale = self.display_scale();
        let width = self.visible_width();
        let display_w = width as f32 * scale;
        let display_h = page.height as f32 * scale;
        let left = self.page_left_offset(width) as f32;
        let top = self.page_vertical_offset(page.height) as f32;
        self.scroll_x = (left + point.0 * display_w - screen.0) / scale;
        self.scroll_y = (top + point.1 * display_h - screen.1) / scale;
        self.clamp_scroll();
    }

    fn rotate(&mut self, right: bool) {
        self.finish_animation();
        self.spread_blocked = false;
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.pending_zooms.clear();
        self.last_requested_scale = self.scale;
        self.send(if right {
            Command::RotateRight
        } else {
            Command::RotateLeft
        });
    }

    fn update_title(&self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string());

        if let Some(menu) = self.command_menu.as_ref() {
            let context = self
                .menu_keys
                .pending_label()
                .map(|prefix| format!(" — {prefix} …"))
                .or_else(|| menu.echo.as_ref().map(|echo| format!(" — {echo}")))
                .unwrap_or_default();
            window.set_title(&format!("{name} — Commands{context}"));
            return;
        }

        match &self.prompt {
            Prompt::Search(input, scope) => {
                let scope = match scope {
                    SearchScope::Page => "Search page",
                    SearchScope::Book => "Search book",
                };
                window.set_title(&format!("{name} — {scope}: {input}"));
                return;
            }
            Prompt::None => {}
        }

        if self.outline_open && !self.outline.is_empty() {
            let index = self.outline_selected.min(self.outline.len() - 1);
            let entry = &self.outline[index];
            let mut title: String = entry.title.chars().take(80).collect();
            if entry.title.chars().count() > 80 {
                title.push('…');
            }
            let page = outline_target_page(
                &self.outline,
                index,
                self.outline_origin_entry,
                self.outline_preview_origin,
            )
            .map(|page| format!(" — p. {page}"))
            .unwrap_or_default();
            window.set_title(&format!(
                "{name} — Outline {}/{}{page} — {title}",
                index + 1,
                self.outline.len()
            ));
            return;
        }

        if self.bitmap.is_none() {
            window.set_title(&format!("{name} — loading…"));
            return;
        }

        let start = self.window_index.min(self.pages.len());
        let visible = &self.pages[start..start + self.visible_page_count()];
        let page = match (visible.first(), visible.last(), self.page_count) {
            (Some(first), Some(last), Some(total)) if first != last => {
                format!("{first}–{last}/{total}")
            }
            (Some(first), _, Some(total)) => format!("{first}/{total}"),
            (Some(first), Some(last), None) if first != last => format!("{first}–{last}"),
            (Some(first), _, None) => first.to_string(),
            _ => self.page.to_string(),
        };
        let rotation = if self.rotation == 0 {
            String::new()
        } else {
            format!(" — {}°", self.rotation)
        };
        let text = if self.reflowable && (self.font_scale - 1.0).abs() > 1.0e-4 {
            format!(" — text {:.0}%", self.font_scale * 100.0)
        } else {
            String::new()
        };
        let status = self
            .status
            .as_ref()
            .map(|s| format!(" — {s}"))
            .unwrap_or_default();
        window.set_title(&format!(
            "{name} — {page} — {:.0}%{text}{rotation}{status}",
            self.scale * 100.0
        ));
    }

    fn start_search_prompt(&mut self, scope: SearchScope) {
        self.prompt = Prompt::Search(String::new(), scope);
        self.update_title();
        self.request_redraw();
    }

    /// The visible, single-line prompt shown above the page.
    fn prompt_line(&self) -> Option<(String, String)> {
        match &self.prompt {
            Prompt::Search(query, SearchScope::Page) => {
                Some(("Search page: ".to_string(), query.clone()))
            }
            Prompt::Search(query, SearchScope::Book) => {
                Some(("Search book: ".to_string(), query.clone()))
            }
            Prompt::None => None,
        }
    }

    fn ensure_prompt_bitmap(&mut self, viewport: (u32, u32)) {
        let Some((label, value)) = self.prompt_line() else {
            self.prompt_bitmap = None;
            return;
        };
        let panel_w = viewport
            .0
            .saturating_sub(80)
            .clamp(260, 560)
            .min(viewport.0.max(1));
        let panel_h = 44;
        let mut line = format!("{label}{value}");
        if value.is_empty() {
            line.push('_');
        } else {
            line.push('▏');
        }
        let max_chars = (panel_w / 8).max(16) as usize;
        if line.chars().count() > max_chars {
            line = line
                .chars()
                .skip(line.chars().count() - max_chars)
                .collect();
        }
        if let Some(cached) = self.prompt_bitmap.as_ref() {
            if cached.text == line && cached.width == panel_w && cached.height == panel_h {
                return;
            }
        }

        let theme = self.theme;
        let mut pixels = vec![theme.prompt; panel_w as usize * panel_h as usize];
        let border = theme.prompt_border;
        for x in 0..panel_w {
            pixels[x as usize] = border;
            pixels[(panel_h - 1) as usize * panel_w as usize + x as usize] = border;
        }
        if self.menu_text.is_none() {
            self.menu_text = TextEngine::new(self.fonts.clone()).ok();
        }
        if let Some(text) = self.menu_text.as_mut() {
            let mut target = TextTarget::new(&mut pixels, panel_w, panel_h, 0, panel_h);
            text.draw_line(
                &line,
                15.0,
                Weight::NORMAL,
                &mut target,
                16,
                0,
                panel_h,
                rgb8(theme.text),
            );
        }
        let revision = self
            .prompt_bitmap
            .as_ref()
            .map_or(1, |prompt| prompt.revision.wrapping_add(1));
        self.prompt_bitmap = Some(PanelBitmap {
            text: line,
            width: panel_w,
            height: panel_h,
            revision,
            bitmap: PageBitmap {
                width: panel_w,
                height: panel_h,
                pixels,
            },
        });
    }

    /// A centered panel shown until the first page bitmap arrives. It turns
    /// the unavoidable startup wait into an intentional loading state instead
    /// of an empty black window.
    fn ensure_loading_bitmap(&mut self, viewport: (u32, u32)) {
        if self.bitmap.is_some() || self.outline_open {
            self.loading_bitmap = None;
            return;
        }
        let name = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string());
        let panel_w = viewport
            .0
            .saturating_sub(96)
            .clamp(280, 560)
            .min(viewport.0.max(1));
        let panel_h = 116.min(viewport.1);
        if panel_w < 220 || panel_h < 72 {
            self.loading_bitmap = None;
            return;
        }
        // Roughly 8px per unit at 16px; CJK glyphs take two units.
        let max_units = (panel_w.saturating_sub(48) / 8).max(8) as usize;
        let line = truncate_display(&name, max_units);
        if let Some(cached) = self.loading_bitmap.as_ref() {
            if cached.text == line && cached.width == panel_w && cached.height == panel_h {
                return;
            }
        }

        let theme = self.theme;
        let background = theme.loading;
        let border = theme.loading_border;
        let mut pixels = vec![background; panel_w as usize * panel_h as usize];
        for x in 0..panel_w {
            pixels[x as usize] = border;
            pixels[(panel_h - 1) as usize * panel_w as usize + x as usize] = border;
        }
        for y in 0..panel_h {
            pixels[y as usize * panel_w as usize] = border;
            pixels[y as usize * panel_w as usize + panel_w as usize - 1] = border;
        }
        // A short accent rail makes the card read as progress rather than a
        // dead black surface.
        for y in 0..3 {
            let row = y as usize * panel_w as usize;
            pixels[row..row + panel_w as usize].fill(theme.accent);
        }

        if self.menu_text.is_none() {
            self.menu_text = TextEngine::new(self.fonts.clone()).ok();
        }
        if let Some(text) = self.menu_text.as_mut() {
            let left = 24;
            let mut target = TextTarget::new(&mut pixels, panel_w, panel_h, 0, panel_h);
            text.draw_line(
                &line,
                16.0,
                Weight::BOLD,
                &mut target,
                left,
                28,
                28,
                rgb8(theme.text_title),
            );
            let mut target = TextTarget::new(&mut pixels, panel_w, panel_h, 0, panel_h);
            text.draw_line(
                "Preparing the first page…",
                13.0,
                Weight::NORMAL,
                &mut target,
                left,
                66,
                24,
                rgb8(theme.text_muted),
            );
        }
        let revision = self
            .loading_bitmap
            .as_ref()
            .map_or(1, |panel| panel.revision.wrapping_add(1));
        self.loading_bitmap = Some(PanelBitmap {
            text: line,
            width: panel_w,
            height: panel_h,
            revision,
            bitmap: PageBitmap {
                width: panel_w,
                height: panel_h,
                pixels,
            },
        });
    }

    fn open_command_menu(&mut self) {
        if self.menu_entries.is_empty() {
            self.status = Some("no commands available".to_string());
            self.update_title();
            return;
        }
        self.finish_animation();
        self.prompt = Prompt::None;
        self.menu_keys.reset();
        self.command_menu = Some(CommandMenu {
            selected: 0,
            scroll: 0,
            rows: MENU_MAX_ROWS.min(self.menu_entries.len()),
            columns: 1,
            echo: None,
            dirty: true,
            bitmap: None,
            revision: 0,
        });
        self.keymap.reset();
        self.update_title();
        self.request_redraw();
    }

    fn close_command_menu(&mut self) {
        self.command_menu = None;
        self.menu_keys.reset();
        // The outline is the mode underneath the command panel. Re-assert its
        // selection frame so closing which-key always returns focus there.
        if self.outline_open {
            self.outline_requested = None;
            self.outline_bitmap_selected = None;
            self.ensure_outline_visible();
        }
        self.update_title();
        self.request_redraw();
    }

    fn move_menu_selection(&mut self, delta: isize) {
        let Some(menu) = self.command_menu.as_ref() else {
            return;
        };
        let last = self.menu_entries.len().saturating_sub(1) as isize;
        let selected = (menu.selected as isize + delta).clamp(0, last) as usize;
        self.select_menu_index(selected);
    }

    fn select_menu_index(&mut self, index: usize) {
        let Some(menu) = self.command_menu.as_mut() else {
            return;
        };
        menu.selected = index.min(self.menu_entries.len().saturating_sub(1));
        scroll_menu_to_selection(menu, self.menu_entries.len());
        menu.dirty = true;
        self.request_redraw();
    }

    fn scroll_menu_candidates(&mut self, delta: isize) {
        let candidates = self.menu_keys.candidates().len();
        let Some(menu) = self.command_menu.as_mut() else {
            return;
        };
        let total_rows = menu_row_count(candidates, menu.columns.max(1));
        let max_scroll = total_rows.saturating_sub(menu.rows.max(1)) as isize;
        menu.scroll = (menu.scroll as isize + delta).clamp(0, max_scroll) as usize;
        menu.dirty = true;
        self.request_redraw();
    }

    fn execute_menu_command(&mut self, event_loop: &ActiveEventLoop) {
        let Some(menu) = self.command_menu.as_ref() else {
            return;
        };
        let Some(entry) = self.menu_entries.get(menu.selected) else {
            return;
        };
        let action = entry.action;
        self.run_menu_action(event_loop, action, false);
    }

    fn run_menu_action(&mut self, event_loop: &ActiveEventLoop, action: Action, hydra: bool) {
        if !hydra {
            self.menu_keys.reset();
        }
        let count = self.menu_entries.len();
        if !hydra {
            if let Some(index) = self
                .menu_entries
                .iter()
                .position(|entry| entry.action == action)
            {
                if let Some(menu) = self.command_menu.as_mut() {
                    menu.selected = index;
                    scroll_menu_to_selection(menu, count);
                }
            }
        }
        if action_closes_menu(action) {
            self.close_command_menu();
            self.run_action(event_loop, action);
            return;
        }
        self.run_action(event_loop, action);
        if let Some(menu) = self.command_menu.as_mut() {
            menu.echo = Some(command_label(action).to_string());
            menu.dirty = true;
        }
        self.update_title();
        self.request_redraw();
    }

    fn cancel_menu_pending(&mut self) {
        self.menu_keys.reset();
        let count = self.menu_entries.len();
        if let Some(menu) = self.command_menu.as_mut() {
            scroll_menu_to_selection(menu, count);
            menu.dirty = true;
        }
        self.update_title();
        self.request_redraw();
    }

    fn after_menu_pending_change(&mut self) {
        let pending = self.menu_keys.has_pending();
        let count = self.menu_entries.len();
        if let Some(menu) = self.command_menu.as_mut() {
            if pending {
                menu.scroll = 0;
            } else {
                scroll_menu_to_selection(menu, count);
            }
            menu.dirty = true;
        }
        self.update_title();
        self.request_redraw();
    }

    fn handle_command_menu_key(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: &winit::event::KeyEvent,
    ) {
        let pending = self.menu_keys.has_pending();
        let key = event.logical_key.as_ref();
        let plain_q = !self.modifiers.control_key()
            && !self.modifiers.alt_key()
            && matches!(key, Key::Character(ch) if ch == "q");
        let cancel = matches!(key, Key::Named(NamedKey::Escape))
            || (self.modifiers.control_key()
                && !self.modifiers.alt_key()
                && matches!(key, Key::Character(ch) if ch == "g"));

        // `q` and Escape are panel controls. While a sequence is pending,
        // Escape only drops the prefix; q still closes the whole panel.
        if plain_q || (cancel && !pending) {
            self.close_command_menu();
            return;
        }
        if cancel {
            self.cancel_menu_pending();
            return;
        }

        let rows = self
            .command_menu
            .as_ref()
            .map_or(MENU_MAX_ROWS, |menu| menu.rows.max(1)) as isize;
        match key {
            Key::Named(NamedKey::Backspace) => {
                if pending {
                    self.menu_keys.pop_pending();
                    self.after_menu_pending_change();
                } else {
                    self.close_command_menu();
                }
            }
            Key::Named(NamedKey::Enter) => {
                if !pending {
                    self.execute_menu_command(event_loop);
                }
            }
            Key::Named(NamedKey::ArrowDown) => {
                if pending {
                    self.scroll_menu_candidates(1);
                } else {
                    self.move_menu_selection(1);
                }
            }
            Key::Named(NamedKey::ArrowUp) => {
                if pending {
                    self.scroll_menu_candidates(-1);
                } else {
                    self.move_menu_selection(-1);
                }
            }
            Key::Named(NamedKey::PageDown) => {
                if pending {
                    self.scroll_menu_candidates(rows);
                } else {
                    self.move_menu_selection(rows);
                }
            }
            Key::Named(NamedKey::PageUp) => {
                if pending {
                    self.scroll_menu_candidates(-rows);
                } else {
                    self.move_menu_selection(-rows);
                }
            }
            Key::Named(NamedKey::Home) => {
                if pending {
                    self.scroll_menu_candidates(-isize::MAX / 2);
                } else {
                    self.select_menu_index(0);
                }
            }
            Key::Named(NamedKey::End) => {
                if pending {
                    self.scroll_menu_candidates(isize::MAX / 2);
                } else {
                    self.select_menu_index(self.menu_entries.len().saturating_sub(1));
                }
            }
            _ => match self.menu_keys.press(event, self.modifiers) {
                MenuResolution::Action(action) => {
                    // A resolved multi-key binding leaves its prefix armed, so
                    // the action echoes while the next continuation is ready.
                    self.run_menu_action(event_loop, action, pending);
                }
                MenuResolution::Pending => {
                    if let Some(menu) = self.command_menu.as_mut() {
                        menu.scroll = 0;
                        menu.echo = None;
                        menu.dirty = true;
                    }
                    self.update_title();
                    self.request_redraw();
                }
                MenuResolution::Ignored(attempted) => {
                    let message = if attempted.is_empty() {
                        "no command for that key".to_string()
                    } else {
                        format!("no command for {attempted}")
                    };
                    let count = self.menu_entries.len();
                    if let Some(menu) = self.command_menu.as_mut() {
                        menu.echo = Some(message);
                        scroll_menu_to_selection(menu, count);
                        menu.dirty = true;
                    }
                    self.update_title();
                    self.request_redraw();
                }
            },
        }
    }

    fn ensure_command_menu_bitmap(&mut self, viewport: (u32, u32)) {
        let Some(menu) = self.command_menu.as_ref() else {
            return;
        };
        if !menu.dirty {
            return;
        }
        let theme = self.theme;

        let pending = self.menu_keys.pending_label();
        let view: Vec<(String, &'static str)> = if pending.is_some() {
            self.menu_keys
                .candidates()
                .into_iter()
                .map(|candidate| (candidate.key, command_label(candidate.action)))
                .collect()
        } else {
            self.menu_entries
                .iter()
                .map(|entry| {
                    (
                        self.menu_keys
                            .primary_label(entry.action)
                            .unwrap_or_else(|| "—".to_string()),
                        entry.label,
                    )
                })
                .collect()
        };

        let count = view.len();
        let columns = if pending.is_some() {
            menu_pending_columns(viewport.0, count)
        } else {
            1
        };
        let total_rows = menu_row_count(count, columns);
        let rows = menu_visible_rows(viewport.1, total_rows);
        let selected = if pending.is_some() {
            0
        } else {
            menu.selected.min(self.menu_entries.len().saturating_sub(1))
        };
        let echo = menu.echo.clone();
        let max_scroll = total_rows.saturating_sub(rows);
        let scroll = if pending.is_some() {
            menu.scroll.min(max_scroll)
        } else {
            let mut scroll = menu.scroll.min(max_scroll);
            if selected < scroll {
                scroll = selected;
            } else if selected >= scroll + rows {
                scroll = selected + 1 - rows;
            }
            scroll.min(max_scroll)
        };
        // A pending grid needs room for two key chips plus their labels.
        let max_width = if columns == 2 { 760 } else { 560 };
        let width = viewport
            .0
            .saturating_sub(72)
            .clamp(340, max_width)
            .min(viewport.0.max(1));
        let height = MENU_HEADER_HEIGHT + rows as u32 * MENU_ROW_HEIGHT + MENU_FOOTER_HEIGHT;
        let mut pixels = vec![theme.panel; width as usize * height as usize];

        fill_ui_rect(
            &mut pixels,
            width,
            height,
            0,
            0,
            width as i32,
            MENU_HEADER_HEIGHT as i32,
            theme.panel_raised,
        );
        let footer_top = MENU_HEADER_HEIGHT + rows as u32 * MENU_ROW_HEIGHT;
        fill_ui_rect(
            &mut pixels,
            width,
            height,
            0,
            footer_top as i32,
            width as i32,
            MENU_FOOTER_HEIGHT as i32,
            theme.panel_sunken,
        );
        fill_ui_rect(
            &mut pixels,
            width,
            height,
            0,
            MENU_HEADER_HEIGHT as i32 - 1,
            width as i32,
            1,
            theme.separator,
        );
        fill_ui_rect(
            &mut pixels,
            width,
            height,
            0,
            footer_top as i32,
            width as i32,
            1,
            theme.separator,
        );

        // Keep the selected row visibly raised, with a slim accent rail that
        // stops short of the panel edge so it reads as a marker, not a border.
        if pending.is_none() {
            for row in 0..rows {
                if scroll + row != selected {
                    continue;
                }
                let top = MENU_HEADER_HEIGHT as i32 + row as i32 * MENU_ROW_HEIGHT as i32;
                fill_ui_rect(
                    &mut pixels,
                    width,
                    height,
                    8,
                    top + 2,
                    width as i32 - 16,
                    MENU_ROW_HEIGHT as i32 - 4,
                    theme.panel_selected,
                );
                fill_ui_rect(
                    &mut pixels,
                    width,
                    height,
                    8,
                    top + 2,
                    3,
                    MENU_ROW_HEIGHT as i32 - 4,
                    theme.accent,
                );
            }
        }

        if self.menu_text.is_none() {
            self.menu_text = TextEngine::new(self.fonts.clone()).ok();
        }
        if let Some(text) = self.menu_text.as_mut() {
            fill_ui_rect(
                &mut pixels,
                width,
                height,
                MENU_PADDING as i32,
                ((MENU_HEADER_HEIGHT - 18) / 2) as i32,
                3,
                18,
                theme.accent,
            );
            let mut target = TextTarget::new(&mut pixels, width, height, 0, MENU_HEADER_HEIGHT);
            text.draw_line(
                "Commands",
                15.0,
                Weight::BOLD,
                &mut target,
                MENU_PADDING as i32 + 12,
                0,
                MENU_HEADER_HEIGHT,
                rgb8(theme.text_title),
            );

            // The right side of the header shows the pending prefix or the
            // selected index, depending on the mode.
            let hint = match pending.as_deref() {
                Some(prefix) => format!("{prefix}…"),
                None => format!("{} / {}", selected + 1, count.max(1)),
            };
            let hint_units = hint.chars().map(display_units).sum::<usize>().max(1) as i32;
            let hint_x =
                (width as i32 - MENU_PADDING as i32 - hint_units * 7).max(MENU_PADDING as i32);
            let mut target = TextTarget::new(&mut pixels, width, height, 0, MENU_HEADER_HEIGHT);
            text.draw_line(
                &hint,
                12.5,
                Weight::NORMAL,
                &mut target,
                hint_x,
                0,
                MENU_HEADER_HEIGHT,
                rgb8(theme.text_faint),
            );

            let cell_gap = 12i32;
            let cell_w = (width as i32 - 2 * MENU_PADDING as i32 - (columns as i32 - 1) * cell_gap)
                / columns as i32;
            for row in 0..rows {
                let top = MENU_HEADER_HEIGHT as i32 + row as i32 * MENU_ROW_HEIGHT as i32;
                let selected_row = pending.is_none() && scroll + row == selected;
                if !selected_row {
                    fill_ui_rect(
                        &mut pixels,
                        width,
                        height,
                        MENU_PADDING as i32,
                        top + MENU_ROW_HEIGHT as i32 - 1,
                        width as i32 - 2 * MENU_PADDING as i32,
                        1,
                        theme.separator,
                    );
                }
                for col in 0..columns {
                    // Fill the grid column by column so each column reads as a
                    // contiguous slice of the action list, like which-key.
                    let index = col * total_rows + scroll + row;
                    let Some((key_label, label)) = view.get(index) else {
                        continue;
                    };
                    let cell_x = MENU_PADDING as i32 + col as i32 * (cell_w + cell_gap);
                    let chip_x = cell_x + 8;
                    let chip_w = MENU_KEY_COLUMN as i32;
                    let chip_h = 20;
                    let chip_y = top + (MENU_ROW_HEIGHT as i32 - chip_h) / 2;
                    let chip_fill = if selected_row {
                        theme.chip_selected
                    } else {
                        theme.chip
                    };
                    let chip_border = if selected_row {
                        theme.chip_border_selected
                    } else {
                        theme.chip_border
                    };
                    fill_ui_rect(
                        &mut pixels,
                        width,
                        height,
                        chip_x,
                        chip_y,
                        chip_w,
                        chip_h,
                        chip_fill,
                    );
                    fill_ui_rect(
                        &mut pixels,
                        width,
                        height,
                        chip_x,
                        chip_y,
                        chip_w,
                        1,
                        chip_border,
                    );
                    fill_ui_rect(
                        &mut pixels,
                        width,
                        height,
                        chip_x,
                        chip_y + chip_h - 1,
                        chip_w,
                        1,
                        chip_border,
                    );
                    fill_ui_rect(
                        &mut pixels,
                        width,
                        height,
                        chip_x,
                        chip_y,
                        1,
                        chip_h,
                        chip_border,
                    );
                    fill_ui_rect(
                        &mut pixels,
                        width,
                        height,
                        chip_x + chip_w - 1,
                        chip_y,
                        1,
                        chip_h,
                        chip_border,
                    );

                    let key_color = rgb8(if selected_row {
                        theme.key_text_selected
                    } else {
                        theme.key_text
                    });
                    let mut target =
                        TextTarget::new(&mut pixels, width, height, top, MENU_ROW_HEIGHT);
                    text.draw_line(
                        key_label,
                        12.5,
                        Weight::BOLD,
                        &mut target,
                        chip_x + 9,
                        top,
                        MENU_ROW_HEIGHT,
                        key_color,
                    );

                    let label_x = chip_x + chip_w + 14;
                    let label_right = cell_x + cell_w - 4;
                    let max_units = ((label_right - label_x).max(24) / 7) as usize;
                    let label = truncate_display(label, max_units.max(4));
                    let (color, weight) = if selected_row {
                        (rgb8(theme.text_selected), Weight::BOLD)
                    } else {
                        (rgb8(theme.text_secondary), Weight::NORMAL)
                    };
                    let mut target =
                        TextTarget::new(&mut pixels, width, height, top, MENU_ROW_HEIGHT);
                    text.draw_line(
                        &label,
                        14.0,
                        weight,
                        &mut target,
                        label_x,
                        top,
                        MENU_ROW_HEIGHT,
                        color,
                    );
                }
            }

            if total_rows > rows {
                let track_x = width as i32 - 7;
                let track_y = MENU_HEADER_HEIGHT as i32 + 6;
                let track_h = (rows as i32 * MENU_ROW_HEIGHT as i32 - 12).max(1);
                fill_ui_rect(
                    &mut pixels,
                    width,
                    height,
                    track_x,
                    track_y,
                    3,
                    track_h,
                    theme.scrollbar_track,
                );
                let thumb_h = ((rows as f32 / total_rows as f32) * track_h as f32)
                    .max(20.0)
                    .min(track_h as f32) as i32;
                let frac = scroll as f32 / total_rows.saturating_sub(rows).max(1) as f32;
                let thumb_y = track_y + ((track_h - thumb_h) as f32 * frac) as i32;
                fill_ui_rect(
                    &mut pixels,
                    width,
                    height,
                    track_x,
                    thumb_y,
                    3,
                    thumb_h,
                    theme.scrollbar_thumb,
                );
            }

            let footer = if pending.is_some() {
                match echo.as_ref() {
                    Some(echo) => format!("→ {echo} · Backspace · Esc cancel"),
                    None => {
                        let hidden = total_rows.saturating_sub(rows);
                        if hidden > 0 {
                            format!(
                                "Backspace · Esc cancel · {} matches · {hidden} more rows",
                                count
                            )
                        } else {
                            format!("Backspace · Esc cancel · {} matches", count)
                        }
                    }
                }
            } else {
                echo.as_ref().map_or_else(
                    || "Type a shortcut · ↑↓ move · Enter run".to_string(),
                    |echo| format!("→ {echo}"),
                )
            };
            let footer_color = rgb8(if pending.is_none() && echo.is_some() {
                theme.footer_echo
            } else {
                theme.footer_text
            });
            let mut target = TextTarget::new(
                &mut pixels,
                width,
                height,
                footer_top as i32,
                MENU_FOOTER_HEIGHT,
            );
            text.draw_line(
                &footer,
                12.5,
                Weight::NORMAL,
                &mut target,
                MENU_PADDING as i32,
                footer_top as i32,
                MENU_FOOTER_HEIGHT,
                footer_color,
            );
        }

        // A single-pixel frame keeps the panel readable over a white page
        // without the heavy double border of the old design.
        let border = theme.panel_border;
        for x in 0..width {
            pixels[x as usize] = border;
            pixels[(height - 1) as usize * width as usize + x as usize] = border;
        }
        for y in 0..height {
            pixels[y as usize * width as usize] = border;
            pixels[y as usize * width as usize + width as usize - 1] = border;
        }

        let Some(menu) = self.command_menu.as_mut() else {
            return;
        };
        menu.rows = rows;
        menu.scroll = scroll;
        menu.columns = columns;
        menu.revision = menu.revision.wrapping_add(1);
        menu.bitmap = Some(PageBitmap {
            width,
            height,
            pixels,
        });
        menu.dirty = false;
    }

    fn command_menu_position(&self, viewport: (u32, u32)) -> Option<(i32, i32)> {
        let panel = self.command_menu.as_ref()?.bitmap.as_ref()?;
        Some((
            ((viewport.0 as i32 - panel.width as i32) / 2).max(0),
            (viewport.1 as i32 - panel.height as i32 - 48).max(0),
        ))
    }

    fn open_outline(&mut self) {
        self.finish_animation();
        if self.outline.is_empty() {
            self.status = Some("this document has no outline".to_string());
            self.update_title();
            return;
        }
        self.outline_preview = None;
        self.outline_requested = None;
        self.outline_bitmap_selected = None;
        // Remember where the reader was so Escape can undo live previews.
        if self.outline_preview_origin.is_none() {
            self.outline_preview_origin = Some(self.page);
        }
        // Select the chapter that contains the current page and remember it,
        // so focusing another chapter and coming back returns to this page.
        let origin_page = self.outline_preview_origin.unwrap_or(self.page);
        self.outline_origin_entry = outline_entry_for_page(&self.outline, origin_page);
        self.outline_selected = outline_selection_for_page(&self.outline, origin_page);
        self.outline_open = true;
        self.pending_zooms.clear();
        self.last_requested_scale = self.scale;
        // Opening the panel narrows only the displayed content; the worker
        // keeps the full-window viewport, so an active spread stays active and
        // no page re-render is queued.
        self.send_viewport();
        self.ensure_outline_visible();
        self.update_title();
        self.request_redraw();
    }

    fn cancel_prompt(&mut self) {
        self.prompt = Prompt::None;
        self.update_title();
        self.request_redraw();
    }

    fn commit_prompt(&mut self) {
        match std::mem::replace(&mut self.prompt, Prompt::None) {
            Prompt::Search(query, scope) => {
                if !query.is_empty() {
                    self.finish_animation();
                    self.status = Some(match scope {
                        SearchScope::Page => "searching this page…".to_string(),
                        SearchScope::Book => "searching the whole book…".to_string(),
                    });
                    self.send(Command::Search { query, scope });
                }
            }
            Prompt::None => {}
        }
        self.update_title();
        self.request_redraw();
    }

    /// Append committed IME text to the active prompt. The command menu is
    /// shortcut-driven, so committed text is ignored there.
    fn commit_text(&mut self, text: &str) {
        let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
        if text.is_empty() {
            return;
        }
        if self.command_menu.is_some() {
            return;
        }
        match &mut self.prompt {
            Prompt::Search(input, _) => input.push_str(&text),
            Prompt::None => return,
        }
        self.update_title();
        self.request_redraw();
    }

    fn handle_prompt_key(&mut self, event: &winit::event::KeyEvent) {
        if self.modifiers.control_key()
            && matches!(event.logical_key.as_ref(), Key::Character(ch) if ch == "g")
        {
            self.cancel_prompt();
            return;
        }
        if self.modifiers.control_key() || self.modifiers.alt_key() {
            return;
        }

        match event.logical_key.as_ref() {
            Key::Named(NamedKey::Escape) => self.cancel_prompt(),
            Key::Named(NamedKey::Enter) => self.commit_prompt(),
            Key::Named(NamedKey::Space) => {
                if let Prompt::Search(input, _) = &mut self.prompt {
                    input.push(' ');
                    self.update_title();
                    self.request_redraw();
                }
            }
            Key::Named(NamedKey::Backspace) => match &mut self.prompt {
                Prompt::Search(input, _) => {
                    input.pop();
                    self.update_title();
                    self.request_redraw();
                }
                Prompt::None => {}
            },
            Key::Character(ch) => match &mut self.prompt {
                Prompt::Search(input, _) => {
                    for c in ch.chars() {
                        if !c.is_control() {
                            input.push(c);
                        }
                    }
                    self.update_title();
                    self.request_redraw();
                }
                Prompt::None => {}
            },
            _ => {}
        }
    }

    fn handle_key(&mut self, event_loop: &ActiveEventLoop, event: winit::event::KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        // A modifier press is its own event. It can never be bound, so it must
        // not break a pending multi-key sequence or produce an error echo.
        if is_modifier_key(event.logical_key.as_ref()) {
            return;
        }

        if self.command_menu.is_some() {
            self.keymap.reset();
            self.handle_command_menu_key(event_loop, &event);
            return;
        }

        if !matches!(self.prompt, Prompt::None) {
            self.keymap.reset();
            self.handle_prompt_key(&event);
            return;
        }

        if matches!(event.logical_key.as_ref(), Key::Character(ch) if ch == ":") {
            self.open_command_menu();
            return;
        }

        if self.outline_open {
            if self.handle_outline_key(&event) {
                self.keymap.reset();
                return;
            }
            self.keymap.reset();
        }

        if let Resolution::Action(action) = self.keymap.handle(&event, self.modifiers) {
            self.run_action(event_loop, action);
        }
    }

    fn run_action(&mut self, event_loop: &ActiveEventLoop, action: Action) {
        if action != Action::Quit {
            self.finish_animation();
        }
        match action {
            Action::Quit => event_loop.exit(),
            Action::NextPage => self.next_page(),
            Action::PrevPage => self.prev_page(),
            Action::FirstPage => self.first_page(),
            Action::LastPage => self.last_page(),
            Action::ScrollDown => self.scroll_down_or_next(),
            Action::ScrollUp => self.scroll_up_or_prev(),
            Action::ScrollDownScreen => self.scroll_down_screen(),
            Action::ScrollUpScreen => self.scroll_up_screen(),
            Action::ScrollLeft => self.scroll_horizontal(-60.0),
            Action::ScrollRight => self.scroll_horizontal(60.0),
            Action::ScrollLeftMost => self.scroll_left_most(),
            Action::ScrollRightMost => self.scroll_right_most(),
            Action::Outline => self.open_outline(),
            Action::SearchPage => self.start_search_prompt(SearchScope::Page),
            Action::SearchBook => self.start_search_prompt(SearchScope::Book),
            Action::ZoomIn => self.zoom_in(),
            Action::ZoomOut => self.zoom_out(),
            Action::FontIncrease => self.font_increase(),
            Action::FontDecrease => self.font_decrease(),
            Action::ResetView => self.reset_view(),
            Action::RotateRight => self.rotate(true),
            Action::RotateLeft => self.rotate(false),
            Action::ToggleSpread => self.toggle_spread(),
            Action::Copy => self.copy_selection(),
            Action::Back => self.go_back(),
            Action::Forward => self.go_forward(),
        }
    }

    fn install_page(&mut self, page: RenderedPage) {
        let page_changed = self.page != page.page
            || self.pages != page.pages
            || self.rotation != page.rotation
            || (self.font_scale - page.font_scale).abs() > 1.0e-4;
        let new_scale = page.scale;
        let zoom_anchor = if page_changed {
            self.pending_zooms.clear();
            None
        } else {
            self.pending_zooms
                .front()
                .filter(|pending| (pending.expected_scale - new_scale).abs() < 1.0e-4)
                .map(|pending| (pending.anchor, pending.screen))
        };
        if zoom_anchor.is_some() {
            self.pending_zooms.pop_front();
        }

        self.page = page.page;
        self.pages = page.pages;
        self.page_widths = page.page_widths;
        self.spread_gap = page.gap;
        self.page_count = page.page_count;
        self.scale = new_scale;
        self.page_fit = page.fit;
        self.font_scale = page.font_scale;
        self.reflowable = page.reflowable;
        self.rotation = page.rotation;
        self.highlights = page.highlights;
        self.links = page.links;
        self.text = page.text;
        let page_background = page.bitmap.pixels.first().copied().unwrap_or(0x00ff_ffff);
        // A color-free CSS lets the page decide the theme. Chapters with a
        // different background update the whole UI instead of leaving the
        // built-in dark shell in place.
        let theme_changed = self.follow_document_theme && self.theme.window != page_background;
        self.page_background = page_background;
        self.bitmap = Some(page.bitmap);
        self.page_revision = self.page_revision.wrapping_add(1);
        self.resize_preview = false;
        self.window_index = window_index_for(&self.pages, self.page);
        self.spread_offset = self.normal_offset();
        if page_changed {
            self.clear_selection();
            self.scroll_x = 0.0;
            self.scroll_y = 0.0;
        } else if let Some((anchor, screen)) = zoom_anchor {
            self.place_page_point(anchor, screen);
        }
        if self.pending_zooms.is_empty() {
            self.last_requested_scale = new_scale;
        }
        self.status = if self.highlights.is_empty() {
            None
        } else {
            Some(format!("{} match(es)", self.highlights.len()))
        };
        self.clamp_scroll();
        if theme_changed {
            self.theme = Theme::from_document(page_background);
            self.invalidate_theme();
        }
        // A previewed page may change the effective background (for example a
        // CSS-styled chapter). Refresh the baked outline tint so the rail and
        // veil keep following the page.
        if self.outline_open {
            self.ensure_outline_window();
        }
        self.update_spread_layout();
        // Outline entries are previews until Enter keeps them, so only a
        // settled page belongs in the progress file.
        if !self.outline_open && self.recorded_page != Some(self.page) {
            self.progress.record(&self.path, self.page);
            self.recorded_page = Some(self.page);
        }
        self.update_title();
        self.request_redraw();
    }

    fn handle_page(&mut self, page: RenderedPage) {
        self.pending_turn = None;
        self.install_page(page);
        if let Some(resume) = self.resume_page.take() {
            if self.page == resume {
                self.status = Some(format!("resumed at page {resume}"));
                self.update_title();
                self.request_redraw();
            }
        }
    }

    fn can_slide(&self, direction: TurnDirection) -> bool {
        if !self.spread_active {
            return false;
        }
        match direction {
            TurnDirection::Next => {
                self.window_index + 2 < self.page_widths.len()
                    && self.page_count.is_none_or(|count| self.page < count)
            }
            TurnDirection::Prev => self.window_index > 0 && self.page > 1,
        }
    }

    fn start_slide(&mut self, direction: TurnDirection) {
        let target_index = match direction {
            TurnDirection::Next => self.window_index + 1,
            TurnDirection::Prev => self.window_index - 1,
        };
        let from = self.spread_offset;
        let to = self.offset_for_index(target_index);
        self.animation = Some(SpreadAnimation {
            started: Instant::now(),
            from,
            to,
            duration: self.page_turn_duration,
            command: direction,
        });
        self.request_redraw();
    }

    fn tick_animation(&mut self) {
        let Some(animation) = self.animation.as_ref() else {
            return;
        };
        let elapsed = animation.started.elapsed();
        let duration = animation.duration;
        let from = animation.from;
        let to = animation.to;
        if elapsed >= duration {
            self.finish_animation();
            return;
        }
        let t = elapsed.as_secs_f32() / duration.as_secs_f32();
        // Kindle-like ease-in-out: a gentle start, a short glide, and a
        // gentle landing rather than the abrupt ease-out of a key repeat.
        let eased = if t < 0.5 {
            4.0 * t.powi(3)
        } else {
            1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
        };
        self.spread_offset = from + (to - from) * eased;
        self.request_redraw();
    }

    fn finish_animation(&mut self) {
        let Some(animation) = self.animation.take() else {
            return;
        };
        self.spread_offset = animation.to;
        self.pending_turn = Some(animation.command);
        self.send(match animation.command {
            TurnDirection::Next => Command::NextPage,
            TurnDirection::Prev => Command::PrevPage,
        });
        self.request_redraw();
    }

    fn handle_worker_event(&mut self, event_loop: &ActiveEventLoop, event: WorkerEvent) {
        match event {
            WorkerEvent::Page(page) => {
                self.handle_page(page);
            }
            WorkerEvent::Outline(entries) => {
                self.outline = entries;
                if self.outline.is_empty() {
                    if self.outline_open {
                        self.outline_open = false;
                        self.outline_preview = None;
                        self.outline_preview_origin = None;
                        self.outline_origin_entry = None;
                        self.send_viewport();
                    }
                } else if self.outline_open {
                    self.outline_preview = None;
                    let origin_page = self.outline_preview_origin.unwrap_or(self.page);
                    self.outline_origin_entry = outline_entry_for_page(&self.outline, origin_page);
                    self.outline_selected = outline_selection_for_page(&self.outline, origin_page);
                    self.ensure_outline_visible();
                }
                self.update_title();
                self.request_redraw();
            }
            WorkerEvent::OutlineBitmap {
                start,
                selected,
                bitmap,
            } => {
                let Some(selected) = selected else {
                    return;
                };
                let expected = OutlineRequest {
                    width: self.outline_width(),
                    start,
                    selected,
                    background: self.page_background,
                };
                if self.outline_requested != Some(expected) {
                    return;
                }
                self.outline_bitmap_width = expected.width;
                self.outline_bitmap_background = expected.background;
                self.outline_bitmap = Some(bitmap);
                self.outline_revision = self.outline_revision.wrapping_add(1);
                self.outline_bitmap_start = start;
                self.outline_bitmap_selected = Some(selected);
                self.outline_requested = None;
                // Only fetch the part of the outline that is visible. Do not
                // move the scroll position here: mouse-wheel scrolling must
                // not snap back to a selection that has scrolled off-screen.
                self.ensure_outline_window();
                self.request_redraw();
            }
            WorkerEvent::Status(status) => {
                self.outline_requested = None;
                self.status = Some(status);
                self.update_title();
            }
            WorkerEvent::Error(e) => {
                eprintln!("{TITLE}: {e}");
                // A failed render would otherwise leave a page-turn or
                // outline request waiting forever.
                self.pending_turn = None;
                self.outline_requested = None;
                self.status = Some(e);
                self.update_title();
            }
            WorkerEvent::Fatal(e) => {
                eprintln!("{TITLE}: {e}");
                event_loop.exit();
            }
        }
    }

    fn page_vertical_offset(&self, page_height: u32) -> i32 {
        let (_, viewport_h) = self.viewport();
        let height = (page_height as f32 * self.display_scale()).round() as i32;
        ((viewport_h as i32 - height) / 2).max(0)
    }

    fn page_origin(&self) -> Option<(i32, i32)> {
        let bitmap = self.bitmap.as_ref()?;
        let scale = self.display_scale();
        let visible = self.visible_width();
        Some((
            self.page_left_offset(visible)
                - (self.spread_offset * scale).round() as i32
                - (self.scroll_x * scale).round() as i32,
            self.page_vertical_offset(bitmap.height) - (self.scroll_y * scale).round() as i32,
        ))
    }

    fn page_left_offset(&self, page_width: u32) -> i32 {
        let width = (page_width as f32 * self.display_scale()).round().max(1.0) as u32;
        page_left_offset(self.viewport(), self.content_left(), width)
    }

    fn page_coords(&self, position: PhysicalPosition<f64>) -> Option<(f32, f32)> {
        if position.x < self.content_left() as f64 {
            return None;
        }
        let page = self.bitmap.as_ref()?;
        let scale = self.display_scale();
        let (x0, y0) = self.page_origin()?;
        let x = (position.x as f32 - x0 as f32) / scale;
        let y = (position.y as f32 - y0 as f32) / scale;
        let left = self.spread_offset;
        let right = left + self.visible_width() as f32;
        if x < left || x >= right || y < 0.0 || y >= page.height as f32 {
            return None;
        }
        Some((x, y))
    }

    fn link_at(&self, position: PhysicalPosition<f64>) -> Option<&RenderedLink> {
        let (x, y) = self.page_coords(position)?;
        self.links.iter().find(|link| {
            x >= link.bounds.x0 as f32
                && x < link.bounds.x1 as f32
                && y >= link.bounds.y0 as f32
                && y < link.bounds.y1 as f32
        })
    }

    fn clear_selection(&mut self) {
        self.selection = None;
        self.selecting = false;
        self.selection_dragged = false;
    }

    fn handle_cursor_moved(&mut self, position: PhysicalPosition<f64>) {
        let previous = self.cursor;
        self.cursor = Some(position);
        if self.command_menu.is_some() || self.resize_preview {
            return;
        }
        if self.panning {
            if let Some(previous) = previous {
                self.scroll_horizontal(-(position.x - previous.x) as f32);
                self.scroll_vertical(-(position.y - previous.y) as f32);
            }
            return;
        }
        if !self.selecting {
            return;
        }
        let Some((x, y)) = self.page_coords(position) else {
            return;
        };
        let Some(index) = self.text.position_at(x, y) else {
            return;
        };
        if let Some(selection) = self.selection.as_mut() {
            if selection.focus != index {
                selection.focus = index;
                self.selection_dragged = true;
                self.request_redraw();
            }
        }
    }

    fn handle_mouse_input(&mut self, state: ElementState) {
        if self.command_menu.is_some() || self.resize_preview {
            return;
        }
        match state {
            ElementState::Pressed => {
                self.finish_animation();
                // Retry a startup activation that the host could not complete
                // before the window was ready. Once successful this is a no-op.
                if let Some(window) = self.window.as_ref() {
                    self.activation.activate(window);
                }
                self.clear_selection();
                let Some(position) = self.cursor else {
                    return;
                };
                if let Some(link) = self.link_at(position).cloned() {
                    self.follow_link(link.target);
                    return;
                }
                let Some((x, y)) = self.page_coords(position) else {
                    return;
                };
                let Some(index) = self.text.position_at(x, y) else {
                    return;
                };
                self.selection = Some(TextSelection {
                    anchor: index,
                    focus: index,
                });
                self.selecting = true;
                self.request_redraw();
            }
            ElementState::Released => {
                if self.selecting {
                    self.selecting = false;
                    if !self.selection_dragged {
                        self.selection = None;
                    }
                    self.request_redraw();
                }
            }
        }
    }

    fn copy_selection(&mut self) {
        let Some(selection) = self.selection else {
            self.status = Some("no text selected".to_string());
            self.update_title();
            return;
        };
        let (start, end) = selection.bounds();
        let text = self.text.text_for_range(start, end);
        if text.is_empty() {
            self.status = Some("no text selected".to_string());
            self.update_title();
            return;
        }

        if self.clipboard.is_none() {
            match ClipboardHandle::spawn(self.proxy.clone()) {
                Ok(clipboard) => self.clipboard = Some(clipboard),
                Err(e) => {
                    self.status = Some(e);
                    self.update_title();
                    return;
                }
            }
        }
        if self
            .clipboard
            .as_ref()
            .is_some_and(|clipboard| clipboard.copy(text))
        {
            self.status = Some("copying…".to_string());
        } else {
            self.status = Some("clipboard unavailable".to_string());
        }
        self.update_title();
    }

    fn handle_mouse_wheel(&mut self, delta: MouseScrollDelta) {
        if self.command_menu.is_some() || self.resize_preview {
            return;
        }
        self.finish_animation();
        let (dx, dy) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (x * 60.0, y * 60.0),
            MouseScrollDelta::PixelDelta(pos) => (pos.x as f32, pos.y as f32),
        };
        if self.outline_open {
            self.outline_scroll = (self.outline_scroll - dy).clamp(0.0, self.outline_max_scroll());
            self.ensure_outline_window();
            self.request_redraw();
            return;
        }
        if self.modifiers.control_key() {
            if dy > 0.0 {
                self.zoom_in();
            } else if dy < 0.0 {
                self.zoom_out();
            }
        } else if self.modifiers.shift_key() {
            let amount = if dx.abs() > 0.0 { dx } else { dy };
            self.scroll_horizontal(amount);
        } else {
            self.scroll_vertical(-dy);
        }
    }

    fn draw(&mut self) {
        self.tick_animation();
        let Some(window) = self.window.clone() else {
            return;
        };
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        if self.command_menu.is_some() {
            self.ensure_command_menu_bitmap((size.width, size.height));
        }
        self.ensure_prompt_bitmap((size.width, size.height));
        self.ensure_loading_bitmap((size.width, size.height));

        let viewport = (size.width, size.height);
        let content_left = self.content_left();
        let vw = size.width as i32;
        let vh = size.height as i32;
        let full = Rect::new(0, 0, vw, vh);
        let mut list = DrawList::new();

        if self.resize_preview {
            if let Some(page) = self.bitmap.as_ref() {
                let visible = self.visible_width().max(1) as f32;
                let content_w = (vw - content_left).max(1) as f32;
                let scale = (content_w / visible).min(vh as f32 / page.height.max(1) as f32);
                let target_w = (page.width as f32 * scale).round().max(1.0) as i32;
                let target_h = (page.height as f32 * scale).round().max(1.0) as i32;
                let visible_w = (visible * scale).round().max(1.0) as i32;
                let window_left = page_left_offset(viewport, content_left, visible_w as u32);
                let origin_x = window_left - (self.spread_offset * scale).round() as i32;
                let y0 = ((vh - target_h) / 2).max(0);
                let clip = Rect::new(window_left, 0, visible_w.min(vw), vh);
                list.texture(
                    PAGE_TEXTURE,
                    Rect::new(origin_x, y0, target_w, target_h),
                    clip,
                );
            }
        } else if let Some(page) = self.bitmap.as_ref() {
            let scale = self.display_scale();
            let bw = (page.width as f32 * scale).round().max(1.0) as i32;
            let bh = (page.height as f32 * scale).round().max(1.0) as i32;
            let visible_width = self.visible_width() as i32;
            let display_width = (visible_width as f32 * scale).round().max(1.0) as i32;
            let window_left = page_left_offset(viewport, content_left, display_width as u32)
                - (self.scroll_x * scale).round() as i32;
            let origin_x = window_left - (self.spread_offset * scale).round() as i32;
            let y0 =
                self.page_vertical_offset(page.height) - (self.scroll_y * scale).round() as i32;
            let clip = Rect::new(window_left, 0, display_width, vh);
            list.texture(PAGE_TEXTURE, Rect::new(origin_x, y0, bw, bh), clip);

            let scaled = |value: u32| (value as f32 * scale).round() as i32;
            for highlight in &self.highlights {
                list.solid(
                    Rect::new(
                        origin_x + scaled(highlight.x0),
                        y0 + scaled(highlight.y0),
                        (scaled(highlight.x1) - scaled(highlight.x0)).max(1),
                        (scaled(highlight.y1) - scaled(highlight.y0)).max(1),
                    ),
                    rgb(self.theme.search_highlight, self.theme.search_alpha),
                    clip,
                );
            }

            if let Some(selection) = self.selection {
                let (start, end) = selection.bounds();
                if start < self.text.chars.len() {
                    let end = end.min(self.text.chars.len() - 1);
                    for ch in &self.text.chars[start..=end] {
                        list.solid(
                            Rect::new(
                                origin_x + scaled(ch.rect.x0),
                                y0 + scaled(ch.rect.y0),
                                (scaled(ch.rect.x1) - scaled(ch.rect.x0)).max(1),
                                (scaled(ch.rect.y1) - scaled(ch.rect.y0)).max(1),
                            ),
                            rgb(self.theme.selection_highlight, self.theme.selection_alpha),
                            clip,
                        );
                    }
                }
            }
        }

        if let Some(loading) = self.loading_bitmap.as_ref() {
            let panel_x = ((vw - loading.width as i32) / 2).max(0);
            let panel_y = ((vh - loading.height as i32) / 2).max(0);
            list.texture(
                LOADING_TEXTURE,
                Rect::new(
                    panel_x,
                    panel_y,
                    loading.width as i32,
                    loading.height as i32,
                ),
                full,
            );
        }

        if self.outline_open && !self.outline.is_empty() {
            let panel_w = (self.outline_width() as i32).min(vw);
            let panel_clip = Rect::new(0, 0, panel_w, vh);
            list.solid(panel_clip, rgb(self.theme.outline.panel, 1.0), full);

            if let Some(panel) = self.outline_bitmap.as_ref() {
                let row_height = OUTLINE_ROW_HEIGHT as i32;
                let top =
                    self.outline_bitmap_start as i32 * row_height - self.outline_scroll as i32;
                list.texture(
                    OUTLINE_TEXTURE,
                    Rect::new(0, top, panel.width as i32, panel.height as i32),
                    panel_clip,
                );
            }

            if panel_w > 0 {
                list.solid(
                    Rect::new(panel_w - 1, 0, 1, vh),
                    rgb(self.theme.outline_border, 1.0),
                    full,
                );
            }
        }

        if let (Some(menu), Some((panel_x, panel_y))) = (
            self.command_menu.as_ref(),
            self.command_menu_position(viewport),
        ) {
            if let Some(panel) = menu.bitmap.as_ref() {
                list.texture(
                    MENU_TEXTURE,
                    Rect::new(panel_x, panel_y, panel.width as i32, panel.height as i32),
                    full,
                );
            }
        }

        if let Some(prompt) = self.prompt_bitmap.as_ref() {
            let panel_x = ((vw - prompt.width as i32) / 2).max(0);
            let panel_y = (vh - prompt.height as i32 - 40).max(0);
            list.texture(
                PROMPT_TEXTURE,
                Rect::new(panel_x, panel_y, prompt.width as i32, prompt.height as i32),
                full,
            );
        }

        let Some(gpu) = self.gpu.as_mut() else {
            return;
        };
        gpu.resize(size.width, size.height);
        if let Some(page) = self.bitmap.as_ref() {
            gpu.upload(PAGE_TEXTURE, self.page_revision, page);
        }
        if let Some(panel) = self.outline_bitmap.as_ref() {
            gpu.upload(OUTLINE_TEXTURE, self.outline_revision, panel);
        }
        if let Some(menu) = self.command_menu.as_ref() {
            if let Some(panel) = menu.bitmap.as_ref() {
                gpu.upload(MENU_TEXTURE, menu.revision, panel);
            }
        }
        if let Some(prompt) = self.prompt_bitmap.as_ref() {
            gpu.upload(PROMPT_TEXTURE, prompt.revision, &prompt.bitmap);
        }
        if let Some(loading) = self.loading_bitmap.as_ref() {
            gpu.upload(LOADING_TEXTURE, loading.revision, &loading.bitmap);
        }

        window.pre_present_notify();
        match gpu.render(rgb(self.theme.window, 1.0), &list) {
            Ok(true) => self.show_window(),
            Ok(false) => {
                // A hidden window can miss a frame on some compositors. Ask
                // for another frame instead of leaving it invisible.
                if !self.window_shown {
                    window.request_redraw();
                }
            }
            Err(e) => {
                eprintln!("{TITLE}: present failed: {e}");
                self.show_window();
            }
        }
    }

    fn show_window(&mut self) {
        if self.window_shown {
            return;
        }
        if let Some(window) = self.window.as_ref() {
            window.set_visible(true);
        }
        self.window_shown = true;
        self.request_redraw();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let title = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| TITLE.to_string());
        let attributes = Window::default_attributes()
            .with_title(title)
            .with_inner_size(LogicalSize::new(1100.0, 900.0))
            .with_min_inner_size(LogicalSize::new(320.0, 240.0))
            // Keep the surface unmapped while wgpu initializes, then show it
            // only after a real frame is available.
            .with_visible(false);
        #[cfg(target_os = "linux")]
        let attributes = attributes.with_name("mureader", "mureader");

        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(e) => {
                eprintln!("{TITLE}: create window failed: {e}");
                event_loop.exit();
                return;
            }
        };
        let (viewport_w, viewport_h) = {
            let size = window.inner_size();
            (size.width.max(1), size.height.max(1))
        };
        // Start opening the document before the GPU finishes initializing, so
        // MuPDF's open/layout work overlaps surface and pipeline setup.
        let worker = match WorkerHandle::spawn(self.proxy.clone()) {
            Ok(worker) => worker,
            Err(e) => {
                eprintln!("{TITLE}: {e}");
                event_loop.exit();
                return;
            }
        };
        let content_w = viewport_w;
        worker.send(Command::Viewport(content_w, viewport_h));
        self.last_viewport_sent = Some((content_w, viewport_h));
        worker.send(Command::Open {
            path: self.path.clone(),
            fonts: self.fonts.clone(),
            css: self.css.clone(),
            pretty: self.pretty_text,
            page: self.resume_page,
            background: self.theme.window,
        });
        self.worker = Some(worker);

        let gpu = match GpuRenderer::new(window.clone(), viewport_w, viewport_h) {
            Ok(gpu) => gpu,
            Err(e) => {
                eprintln!("{TITLE}: {e}");
                event_loop.exit();
                return;
            }
        };

        self.window = Some(window);
        self.gpu = Some(gpu);
        self.update_title();
        // Present the loading panel before mapping the window. If the
        // compositor refuses that first hidden frame, show the window anyway
        // and let the queued redraw fill it in.
        self.draw();
        self.show_window();
        if let Some(window) = self.window.as_ref() {
            window.set_ime_allowed(true);
            self.activation.activate(window);
        }
        self.request_redraw();
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Worker(event) => self.handle_worker_event(event_loop, event),
            UserEvent::Clipboard(notice) => match notice {
                ClipboardNotice::Copied(chars) => {
                    self.status = Some(format!("copied {chars} character(s)"));
                    self.update_title();
                }
                ClipboardNotice::Error(e) => {
                    eprintln!("{TITLE}: {e}");
                    self.clipboard = None;
                    self.status = Some(e);
                    self.update_title();
                }
            },
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.animation.is_some() {
            self.request_redraw();
        }
        let now = Instant::now();
        let mut deadline = None;

        match self.pending_viewport {
            Some(at) if now >= at => {
                self.send_viewport();
                self.clamp_scroll();
                self.request_redraw();
            }
            Some(at) => deadline = Some(at),
            None => {}
        }

        if self
            .outline_preview
            .as_ref()
            .is_some_and(|preview| now >= preview.at)
        {
            let preview = self.outline_preview.take().expect("outline preview is due");
            if self.outline_open
                && self.outline_preview_origin.is_some()
                && preview.page != self.page
            {
                self.send(Command::GotoPage(preview.page));
            }
        }
        if let Some(preview) = self.outline_preview.as_ref() {
            deadline = Some(deadline.map_or(preview.at, |current| current.min(preview.at)));
        }

        if let Some(deadline) = deadline {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else {
            // `WaitUntil` persists after its deadline: leaving it set makes
            // the event loop wake immediately on every iteration and burn
            // CPU. Go back to sleeping once the debounce has run.
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                self.finish_animation();
                self.spread_blocked = false;
                if let Some(menu) = self.command_menu.as_mut() {
                    menu.dirty = true;
                }
                if size.width > 0 && size.height > 0 {
                    self.pending_zooms.clear();
                    self.last_requested_scale = self.scale;
                    self.schedule_viewport();
                    self.clamp_scroll();
                    // The outline width follows the window, so re-lay out its
                    // rows while the resize is already being debounced.
                    if self.outline_open {
                        self.ensure_outline_window();
                    }
                    self.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
            WindowEvent::Ime(Ime::Commit(text)) => self.commit_text(&text),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::KeyboardInput { event, .. } => self.handle_key(event_loop, event),
            WindowEvent::CursorMoved { position, .. } => self.handle_cursor_moved(position),
            WindowEvent::CursorLeft { .. } => {
                // A button release may not be delivered after the pointer
                // leaves the surface; stop extending the selection.
                self.selecting = false;
                self.panning = false;
            }
            WindowEvent::Focused(false) => {
                self.selecting = false;
                self.panning = false;
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => self.handle_mouse_input(state),
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Middle,
                ..
            } => {
                if self.command_menu.is_none() {
                    self.panning = state == ElementState::Pressed;
                }
            }
            WindowEvent::MouseWheel { delta, .. } => self.handle_mouse_wheel(delta),
            _ => {}
        }
    }
}

fn open_external(uri: &str) -> Result<(), String> {
    let scheme = uri
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(scheme.as_str(), "http" | "https" | "mailto") {
        return Err(format!("unsupported external link: {uri}"));
    }

    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.args(["/C", "start", ""]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");

    let mut child = command
        .arg(uri)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot open {uri}: {error}"))?;
    // Reap the handler without blocking the UI thread.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn scroll_menu_to_selection(menu: &mut CommandMenu, count: usize) {
    let count = count.max(1);
    let rows = menu.rows.clamp(1, count);
    let selected = menu.selected.min(count.saturating_sub(1));
    if selected < menu.scroll {
        menu.scroll = selected;
    } else if selected >= menu.scroll + rows {
        menu.scroll = selected + 1 - rows;
    }
    menu.scroll = menu.scroll.min(count.saturating_sub(rows));
    menu.rows = rows;
    menu.selected = selected;
}

/// Decide whether the outline actually needs a new bitmap window. The
/// returned window covers the viewport plus a margin, so even a window taller
/// than the old fixed 256-row request settles after one round trip.
fn outline_window_request(
    len: usize,
    scroll: f32,
    viewport_h: u32,
    row_height: u32,
    current_start: usize,
    current_rows: usize,
) -> Option<(usize, usize)> {
    if len == 0 {
        return None;
    }
    let row_height = row_height.max(1) as f32;
    let scroll = scroll.max(0.0);
    let viewport_rows = (viewport_h as f32 / row_height).ceil() as usize + 1;
    let first = (scroll / row_height).floor() as usize;
    let last = (((scroll + viewport_h as f32) / row_height).ceil() as usize).min(len);
    let current_end = current_start.saturating_add(current_rows).min(len);
    if current_rows > 0 && first >= current_start && last <= current_end {
        return None;
    }
    let start = first.saturating_sub(16).min(len - 1);
    let count = viewport_rows
        .saturating_add(64)
        .max(OUTLINE_WINDOW_MIN_ROWS)
        .min(len - start)
        .max(1);
    Some((start, count))
}

/// Index of the outline entry that contains `page`: the last entry whose
/// chapter starts at or before it. Returns `None` for pages before the first
/// chapter. Keeping this distinct from the selected entry means a page in the
/// middle of a chapter stays attached to that chapter instead of the next one.
fn outline_entry_for_page(outline: &[OutlineEntry], page: usize) -> Option<usize> {
    outline
        .iter()
        .rposition(|entry| matches!(entry.page, Some(entry_page) if entry_page <= page))
}

/// Entry to highlight when the outline opens. Pages before the first chapter
/// select the first chapter, whose own page is then the navigation target.
fn outline_selection_for_page(outline: &[OutlineEntry], page: usize) -> usize {
    outline_entry_for_page(outline, page)
        .or_else(|| outline.iter().position(|entry| entry.page.is_some()))
        .unwrap_or(0)
}

/// Page to show for an outline entry. The entry that contains the page where
/// the outline was opened resolves to that remembered page, so leaving the
/// chapter and coming back does not drop the reader at the chapter's first
/// page.
fn outline_target_page(
    outline: &[OutlineEntry],
    index: usize,
    origin_entry: Option<usize>,
    origin_page: Option<usize>,
) -> Option<usize> {
    if origin_entry == Some(index) {
        if let Some(origin) = origin_page {
            return Some(origin);
        }
    }
    outline.get(index).and_then(|entry| entry.page)
}

/// Live previews are cheap at fit-page scale. At an explicit zoom the page
/// bitmap can already be huge, so only preview while it stays modest.
fn outline_preview_allowed(fit: Fit, bitmap: Option<&PageBitmap>) -> bool {
    match fit {
        Fit::Page => true,
        Fit::None => bitmap.is_some_and(|bitmap| {
            u64::from(bitmap.width) * u64::from(bitmap.height) <= OUTLINE_PREVIEW_MAX_PIXELS
        }),
    }
}

fn page_left_offset(viewport: (u32, u32), content_left: i32, page_width: u32) -> i32 {
    let content_w = (viewport.0 as i32 - content_left).max(1);
    content_left + ((content_w - page_width as i32) as f32 / 2.0).round() as i32
}

/// Whether a viewport update already ends the resize preview. At an explicit
/// zoom the worker intentionally keeps the current bitmap, and an unchanged
/// viewport produces no new render either way.
fn resize_preview_done(fit: Fit, unchanged: bool) -> bool {
    unchanged || fit == Fit::None
}

/// Slot of the current page in the rendered strip. Pages can be
/// non-contiguous when blank reflow pages are filtered out, so this must not
/// be computed from the page-number difference.
fn window_index_for(pages: &[usize], page: usize) -> usize {
    pages
        .iter()
        .position(|&candidate| candidate == page)
        .unwrap_or(0)
}

#[cfg(test)]
fn page_point_at_viewport_center(
    viewport: (u32, u32),
    content_left: i32,
    page: (u32, u32),
    scroll: (f32, f32),
) -> (f32, f32) {
    let content_w = (viewport.0 as i32 - content_left).max(1) as f32;
    page_point_at_screen(
        viewport,
        content_left,
        page,
        scroll,
        (
            content_left as f32 + content_w / 2.0,
            viewport.1 as f32 / 2.0,
        ),
    )
}

#[cfg(test)]
fn page_point_at_screen(
    viewport: (u32, u32),
    content_left: i32,
    page: (u32, u32),
    scroll: (f32, f32),
    screen: (f32, f32),
) -> (f32, f32) {
    let origin_x = page_left_offset(viewport, content_left, page.0) as f32 - scroll.0;
    let origin_y = MARGIN as f32 - scroll.1;
    (
        (screen.0 - origin_x) / page.0.max(1) as f32,
        (screen.1 - origin_y) / page.1.max(1) as f32,
    )
}

#[cfg(test)]
fn scroll_for_page_point(
    viewport: (u32, u32),
    content_left: i32,
    page: (u32, u32),
    point: (f32, f32),
) -> (f32, f32) {
    let content_w = (viewport.0 as i32 - content_left).max(1) as f32;
    scroll_for_page_point_at_screen(
        viewport,
        content_left,
        page,
        point,
        (
            content_left as f32 + content_w / 2.0,
            viewport.1 as f32 / 2.0,
        ),
    )
}

#[cfg(test)]
fn scroll_for_page_point_at_screen(
    viewport: (u32, u32),
    content_left: i32,
    page: (u32, u32),
    point: (f32, f32),
    screen: (f32, f32),
) -> (f32, f32) {
    let left = page_left_offset(viewport, content_left, page.0) as f32;
    (
        left + point.0 * page.0.max(1) as f32 - screen.0,
        MARGIN as f32 + point.1 * page.1.max(1) as f32 - screen.1,
    )
}

/// Compose the stylesheet the reader actually applies. The eye-comfort palette
/// is a transformation of the theme implied by the user's CSS, so its rules
/// come after that CSS. When disabled, both values stay exactly untouched.
fn configured_styles(config: &Config) -> (Option<String>, Theme) {
    let mut css = config.reflowable_css();
    let mut theme = Theme::from_css(css.as_deref());
    if config.perfect_palette.enable {
        let palette = palette::tune(theme, config.perfect_palette.options());
        let tuned = palette::css(&palette);
        css = Some(match css {
            Some(user) => format!("{user}\n{tuned}"),
            None => tuned,
        });
        theme = Theme::from_css(css.as_deref());
    }
    (css, theme)
}

fn run(path: Option<PathBuf>) -> Result<(), Box<dyn Error>> {
    let config: Config = config::load();
    let path = match path {
        Some(path) => path,
        None => {
            let Some(path) = startup::pick_document() else {
                return Ok(());
            };
            path
        }
    };
    let fonts = Arc::new(FontCatalog::new(config.font_family().as_deref())?);
    mupdf::set_font_loader(EpubFontLoader::new(fonts.clone()));
    let keymap = Keymap::new(&config.keys);
    let (css, theme) = configured_styles(&config);

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(
        path,
        proxy,
        fonts,
        keymap,
        css,
        theme,
        config.reflowable.pretty,
        Duration::from_millis(config.animation.page_turn_ms),
        Duration::from_millis(config.animation.window_fit_ms),
        config.progress.max_documents,
    );
    event_loop.run_app(&mut app)?;
    Ok(())
}

fn main() {
    // Validate the command line before detaching so usage errors still reach
    // the invoking shell with a non-zero status.
    let path = match startup::document_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("{TITLE}: {e}");
            std::process::exit(2);
        }
    };
    if let Err(e) = startup::detach() {
        eprintln!("{TITLE}: detach failed: {e}");
        std::process::exit(1);
    }
    if let Err(e) = run(path) {
        eprintln!("{TITLE}: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1.0e-4
    }

    #[test]
    fn navigation_history_roundtrips_destination_jumps() {
        let mut history = NavigationHistory::default();
        history.record(3);
        assert_eq!(history.go_back(9), Some(3));
        assert_eq!(history.go_back(3), None);
        assert_eq!(history.go_forward(3), Some(9));
        assert_eq!(history.go_forward(9), None);

        // A new jump replaces the forward branch, like a browser.
        history.record(3);
        history.record(12);
        assert_eq!(history.go_back(20), Some(12));
        history.record(30);
        assert_eq!(history.go_forward(30), None);
    }

    #[test]
    fn external_links_reject_unsafe_schemes() {
        assert!(open_external("javascript:alert(1)").is_err());
        assert!(open_external("file:///etc/passwd").is_err());
        assert!(open_external("not-a-uri").is_err());
    }

    #[test]
    fn disabled_palette_leaves_css_and_theme_untouched() {
        let mut config = Config::default();
        config.reflowable.css = Some("body { background: #f2e8d5; color: #3a2e24; }".to_string());
        let (css, theme) = configured_styles(&config);
        assert_eq!(css, config.reflowable_css());
        assert_eq!(theme, Theme::from_css(css.as_deref()));
    }

    #[test]
    fn enabled_palette_appends_a_tuned_stylesheet() {
        let mut config = Config::default();
        config.reflowable.css = Some("body { background: #f2e8d5; color: #3a2e24; }".to_string());
        config.perfect_palette.enable = true;
        let (css, theme) = configured_styles(&config);
        let css = css.unwrap();
        let user = css.find("body { background").unwrap();
        let tuned = css.find("--mureader-palette-0").unwrap();
        assert!(user < tuned, "the tuned palette must follow the user CSS");
        assert!(
            !config
                .reflowable_css()
                .unwrap()
                .contains("--mureader-palette"),
            "config CSS stays exactly the user's stylesheet"
        );
        assert_ne!(
            theme,
            Theme::from_css(config.reflowable_css().as_deref()),
            "the UI theme follows the tuned palette"
        );
    }

    #[test]
    fn command_catalog_has_distinct_actions() {
        let mut seen = std::collections::HashSet::new();
        for command in COMMANDS {
            assert!(
                seen.insert(command.action),
                "duplicate command action {:?}",
                command.action
            );
            assert!(!command.label.is_empty());
        }
        assert!(!COMMANDS.is_empty());
    }

    #[test]
    fn modifier_presses_are_not_treated_as_commands() {
        for name in [
            NamedKey::Shift,
            NamedKey::Control,
            NamedKey::Alt,
            NamedKey::Super,
            NamedKey::CapsLock,
        ] {
            assert!(is_modifier_key(Key::<&str>::Named(name)));
        }
        assert!(!is_modifier_key(Key::<&str>::Character("g")));
        assert!(!is_modifier_key(Key::<&str>::Named(NamedKey::Escape)));
    }

    #[test]
    fn loading_name_is_truncated_by_display_width() {
        assert_eq!(truncate_display("short.epub", 40), "short.epub");
        let cjk = truncate_display("小王子.epub", 6);
        assert!(cjk.ends_with('…'));
        assert!(cjk.chars().map(display_units).sum::<usize>() <= 6);
        let long = truncate_display("a-very-long-document-name.pdf", 12);
        assert!(long.ends_with('…'));
        assert!(long.chars().map(display_units).sum::<usize>() <= 12);
    }

    #[test]
    fn command_menu_rows_fit_the_window() {
        let count = COMMANDS.len();
        assert_eq!(menu_visible_rows(900, count), count);
        let small = menu_visible_rows(240, count);
        assert!(small >= 1 && small < count);
        assert_eq!(menu_visible_rows(0, count), 1);
        assert_eq!(menu_visible_rows(900, 3), 3);
    }

    #[test]
    fn menu_selection_scrolls_into_view() {
        let mut menu = CommandMenu {
            selected: 12,
            scroll: 0,
            rows: 5,
            columns: 1,
            echo: None,
            dirty: false,
            bitmap: None,
            revision: 0,
        };
        scroll_menu_to_selection(&mut menu, 20);
        assert!(menu.scroll <= menu.selected && menu.selected < menu.scroll + menu.rows);

        menu.selected = 0;
        scroll_menu_to_selection(&mut menu, 20);
        assert_eq!(menu.scroll, 0);
    }

    #[test]
    fn outline_window_covers_tall_viewports_and_settles() {
        // More rows than a fixed 256-row window; the request must still cover
        // the viewport so the next response does not re-request forever.
        let (start, count) =
            outline_window_request(1000, 0.0, 10_000, OUTLINE_ROW_HEIGHT, 0, 0).unwrap();
        assert_eq!(start, 0);
        assert!(count > 256);
        assert_eq!(
            outline_window_request(1000, 0.0, 10_000, OUTLINE_ROW_HEIGHT, start, count),
            None
        );
    }

    #[test]
    fn outline_window_follows_scrolling() {
        let (start, count) =
            outline_window_request(1000, 10_000.0, 900, OUTLINE_ROW_HEIGHT, 0, 256).unwrap();
        assert_eq!(start, 341);
        assert_eq!(
            outline_window_request(1000, 10_000.0, 900, OUTLINE_ROW_HEIGHT, start, count),
            None
        );
    }

    #[test]
    fn outline_window_clamps_at_the_end() {
        let (start, count) =
            outline_window_request(10, 0.0, 900, OUTLINE_ROW_HEIGHT, 0, 0).unwrap();
        assert_eq!((start, count), (0, 10));
        assert_eq!(
            outline_window_request(10, 0.0, 900, OUTLINE_ROW_HEIGHT, start, count),
            None
        );
    }

    fn outline_entries() -> Vec<OutlineEntry> {
        vec![
            OutlineEntry {
                title: "one".to_string(),
                page: Some(1),
                depth: 0,
            },
            OutlineEntry {
                title: "two".to_string(),
                page: Some(100),
                depth: 0,
            },
            OutlineEntry {
                title: "three".to_string(),
                page: Some(200),
                depth: 0,
            },
        ]
    }

    #[test]
    fn outline_entry_is_the_chapter_containing_the_page() {
        let outline = outline_entries();
        assert_eq!(outline_entry_for_page(&outline, 1), Some(0));
        assert_eq!(outline_entry_for_page(&outline, 99), Some(0));
        assert_eq!(outline_entry_for_page(&outline, 100), Some(1));
        assert_eq!(outline_entry_for_page(&outline, 105), Some(1));
        assert_eq!(outline_entry_for_page(&outline, 250), Some(2));
        // Before the first chapter there is no containing entry, so opening
        // selects the first chapter and targets its own page.
        assert_eq!(outline_entry_for_page(&outline, 0), None);
        assert_eq!(outline_selection_for_page(&outline, 0), 0);
        assert_eq!(outline_target_page(&outline, 0, None, Some(0)), Some(1));
    }

    #[test]
    fn outline_origin_entry_keeps_the_page_inside_the_chapter() {
        let outline = outline_entries();
        let origin = outline_entry_for_page(&outline, 105).unwrap();
        assert_eq!(origin, 1);

        // The origin entry resolves to the exact page where the outline
        // opened, while other entries keep their own destination.
        assert_eq!(
            outline_target_page(&outline, origin, Some(origin), Some(105)),
            Some(105)
        );
        assert_eq!(
            outline_target_page(&outline, 2, Some(origin), Some(105)),
            Some(200)
        );

        // Focusing another chapter and returning must not fall back to the
        // origin chapter's first page.
        let _ = outline_target_page(&outline, 2, Some(origin), Some(105));
        assert_eq!(
            outline_target_page(&outline, origin, Some(origin), Some(105)),
            Some(105)
        );

        // Without a remembered origin the entry's own page is used.
        assert_eq!(outline_target_page(&outline, origin, None, None), Some(100));
        assert_eq!(
            outline_target_page(&outline, 99, Some(origin), Some(105)),
            None
        );
    }

    #[test]
    fn outline_preview_skips_oversized_zoomed_pages() {
        let small = PageBitmap {
            width: 1000,
            height: 1000,
            pixels: Vec::new(),
        };
        let large = PageBitmap {
            width: 3000,
            height: 2000,
            pixels: Vec::new(),
        };
        assert!(outline_preview_allowed(Fit::Page, Some(&large)));
        assert!(outline_preview_allowed(Fit::None, Some(&small)));
        assert!(!outline_preview_allowed(Fit::None, Some(&large)));
        assert!(!outline_preview_allowed(Fit::None, None));
    }

    #[test]
    fn zoom_center_roundtrips_with_scaled_bitmaps() {
        let viewport = (1100, 900);
        let old_page = (634, 1200);
        let new_page = (793, 1500);
        let scroll = (0.0, 100.0);

        let point = page_point_at_viewport_center(viewport, 0, old_page, scroll);
        let zoomed_scroll = scroll_for_page_point(viewport, 0, new_page, point);
        let zoomed_point = page_point_at_viewport_center(viewport, 0, new_page, zoomed_scroll);
        assert!(close(point.0, zoomed_point.0));
        assert!(close(point.1, zoomed_point.1));

        let restored_scroll = scroll_for_page_point(viewport, 0, old_page, zoomed_point);
        assert!(close(scroll.0, restored_scroll.0));
        assert!(close(scroll.1, restored_scroll.1));
    }

    #[test]
    fn cursor_anchored_zoom_roundtrips() {
        let viewport = (1100, 900);
        let old_page = (634, 1200);
        let new_page = (793, 1500);
        let scroll = (20.0, 80.0);
        let cursor = (280.0, 210.0);

        let point = page_point_at_screen(viewport, 0, old_page, scroll, cursor);
        let zoomed_scroll = scroll_for_page_point_at_screen(viewport, 0, new_page, point, cursor);
        let zoomed_point = page_point_at_screen(viewport, 0, new_page, zoomed_scroll, cursor);
        assert!(close(point.0, zoomed_point.0));
        assert!(close(point.1, zoomed_point.1));

        let restored = scroll_for_page_point_at_screen(viewport, 0, old_page, zoomed_point, cursor);
        assert!(close(scroll.0, restored.0));
        assert!(close(scroll.1, restored.1));
    }

    #[test]
    fn zoom_out_centers_a_short_page_and_roundtrips() {
        let viewport = (1100, 900);
        let fit_page = (634, 900);
        let short_page = (508, 720);

        let point = page_point_at_viewport_center(viewport, 0, fit_page, (0.0, 0.0));
        let short_scroll = scroll_for_page_point(viewport, 0, short_page, point);
        assert!(close(short_scroll.0, 0.0));
        assert!(close(short_scroll.1, -90.0));

        let short_point = page_point_at_viewport_center(viewport, 0, short_page, short_scroll);
        let restored = scroll_for_page_point(viewport, 0, fit_page, short_point);
        assert!(close(restored.0, 0.0));
        assert!(close(restored.1, 0.0));
    }

    #[test]
    fn resize_preview_finishes_without_a_render_at_explicit_zoom() {
        assert!(resize_preview_done(Fit::None, false));
        assert!(resize_preview_done(Fit::Page, true));
        assert!(!resize_preview_done(Fit::Page, false));
    }

    #[test]
    fn window_index_handles_non_contiguous_reflow_pages() {
        assert_eq!(window_index_for(&[1, 2, 3, 4], 3), 2);
        assert_eq!(window_index_for(&[1, 3, 4, 6], 3), 1);
        assert_eq!(window_index_for(&[2, 4], 2), 0);
        assert_eq!(window_index_for(&[], 7), 0);
    }

    #[test]
    fn visible_window_anchor_ignores_strip_offset() {
        // The page bitmap for a spread is wider than the visible window. The
        // anchor must be relative to the window edge, not the strip origin.
        let viewport = (1100, 900);
        let visible_window = (1100, 900);
        let scroll = (100.0, 0.0);
        let point = page_point_at_screen(viewport, 0, visible_window, scroll, (450.0, 450.0));
        assert!(close(point.0, 0.5));
        assert!(close(point.1, 0.5));
        let restored =
            scroll_for_page_point_at_screen(viewport, 0, visible_window, point, (450.0, 450.0));
        assert!(close(restored.0, scroll.0));
        assert!(close(restored.1, 0.0));
    }
}
