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

mod activation;
mod clipboard;
mod config;
mod document;
mod fonts;
mod keymap;
mod outline_text;
mod pretty_text;
mod startup;
mod worker;

use std::collections::VecDeque;
use std::error::Error;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use activation::Activation;
use clipboard::{ClipboardHandle, ClipboardNotice};
use config::Config;
use document::{
    Fit, HighlightRect, OutlineEntry, PageBitmap, RenderedLink, RenderedPage, SearchScope,
    TextLayer, TextSelection, MARGIN, MAX_SCALE, MIN_SCALE, ZOOM_IN_FACTOR, ZOOM_OUT_FACTOR,
};
use fontdb::Weight;
use fonts::{EpubFontLoader, FontCatalog};
use keymap::{Action, Keymap, Resolution};
use outline_text::{TextEngine, TextTarget};
use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{Window, WindowId};

use worker::{Command, Event as WorkerEvent, WorkerHandle};

const BACKGROUND: u32 = 0x00181818;
const TITLE: &str = "MuReader";
const OUTLINE_WIDTH: u32 = 320;
const OUTLINE_ROW_HEIGHT: u32 = 28;
const MENU_HEADER_HEIGHT: u32 = 36;
const MENU_ROW_HEIGHT: u32 = 30;
const MENU_MAX_ROWS: usize = 10;
const MENU_PADDING: u32 = 16;

type SoftContext = Context<Arc<Window>>;
type SoftSurface = Surface<Arc<Window>, Arc<Window>>;

pub enum UserEvent {
    Worker(WorkerEvent),
    Clipboard(ClipboardNotice),
}

enum Prompt {
    None,
    Goto(String),
    Outline(String),
    Search(String, SearchScope),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MenuAction {
    NextPage,
    PrevPage,
    FirstPage,
    LastPage,
    GotoPage,
    Outline,
    SearchPage,
    SearchBook,
    ZoomIn,
    ZoomOut,
    ResetView,
    RotateRight,
    RotateLeft,
    Copy,
    Quit,
}

struct CommandEntry {
    label: &'static str,
    keywords: &'static str,
    action: MenuAction,
}

const COMMANDS: &[CommandEntry] = &[
    CommandEntry {
        label: "Next page",
        keywords: "next forward n page turn",
        action: MenuAction::NextPage,
    },
    CommandEntry {
        label: "Previous page",
        keywords: "previous prev back p page turn",
        action: MenuAction::PrevPage,
    },
    CommandEntry {
        label: "First page",
        keywords: "first beginning start home",
        action: MenuAction::FirstPage,
    },
    CommandEntry {
        label: "Last page",
        keywords: "last end finish",
        action: MenuAction::LastPage,
    },
    CommandEntry {
        label: "Go to page…",
        keywords: "goto jump page number",
        action: MenuAction::GotoPage,
    },
    CommandEntry {
        label: "Show outline",
        keywords: "outline contents toc table",
        action: MenuAction::Outline,
    },
    CommandEntry {
        label: "Search this page…",
        keywords: "search find page",
        action: MenuAction::SearchPage,
    },
    CommandEntry {
        label: "Search the whole book…",
        keywords: "search find book whole",
        action: MenuAction::SearchBook,
    },
    CommandEntry {
        label: "Zoom in",
        keywords: "zoom in larger magnify",
        action: MenuAction::ZoomIn,
    },
    CommandEntry {
        label: "Zoom out",
        keywords: "zoom out smaller",
        action: MenuAction::ZoomOut,
    },
    CommandEntry {
        label: "Reset view",
        keywords: "reset restore fit zoom pan",
        action: MenuAction::ResetView,
    },
    CommandEntry {
        label: "Rotate right",
        keywords: "rotate right turn",
        action: MenuAction::RotateRight,
    },
    CommandEntry {
        label: "Rotate left",
        keywords: "rotate left turn",
        action: MenuAction::RotateLeft,
    },
    CommandEntry {
        label: "Copy selection",
        keywords: "copy selection text clipboard",
        action: MenuAction::Copy,
    },
    CommandEntry {
        label: "Quit",
        keywords: "quit exit close",
        action: MenuAction::Quit,
    },
];

fn filtered_commands(query: &str) -> Vec<usize> {
    let query = query.to_ascii_lowercase();
    let parts: Vec<&str> = query.split_whitespace().collect();
    COMMANDS
        .iter()
        .enumerate()
        .filter(|(_, command)| {
            if parts.is_empty() {
                return true;
            }
            let haystack = format!(
                "{} {}",
                command.label.to_ascii_lowercase(),
                command.keywords
            );
            parts.iter().all(|part| haystack.contains(part))
        })
        .map(|(index, _)| index)
        .collect()
}

struct CommandMenu {
    query: String,
    selected: usize,
    scroll: usize,
    dirty: bool,
    bitmap: Option<PageBitmap>,
}

struct PendingZoom {
    anchor: (f32, f32),
    screen: (f32, f32),
    expected_scale: f32,
}

#[derive(Clone, Copy)]
struct ViewState {
    scale: f32,
    scroll: (f32, f32),
}

struct PendingRestore {
    scale: f32,
    scroll: (f32, f32),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TurnDirection {
    Next,
    Prev,
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
    // Drop order matters: surface before context before window.
    surface: Option<SoftSurface>,
    context: Option<SoftContext>,
    window: Option<Arc<Window>>,
    surface_size: (u32, u32),
    bitmap: Option<PageBitmap>,
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
    outline: Vec<OutlineEntry>,
    outline_open: bool,
    outline_selected: usize,
    outline_bitmap: Option<PageBitmap>,
    outline_bitmap_start: usize,
    outline_requested_start: Option<usize>,
    outline_scroll: f32,
    page: usize,
    pages: Vec<usize>,
    page_widths: Vec<u32>,
    spread_gap: u32,
    spread_offset: f32,
    window_index: usize,
    page_count: Option<usize>,
    scale: f32,
    page_fit: Fit,
    rotation: u16,
    scroll_x: f32,
    scroll_y: f32,
    modifiers: ModifiersState,
    prompt: Prompt,
    command_menu: Option<CommandMenu>,
    menu_text: Option<TextEngine>,
    status: Option<String>,
    fonts: Arc<FontCatalog>,
    keymap: Keymap,
    pending_zooms: VecDeque<PendingZoom>,
    last_requested_scale: f32,
    zoom_baseline: Option<ViewState>,
    pending_restore: Option<PendingRestore>,
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
    pending_turn: Option<TurnDirection>,
    animation: Option<SpreadAnimation>,
}

impl App {
    #[allow(clippy::too_many_arguments)]
    fn new(
        path: PathBuf,
        proxy: EventLoopProxy<UserEvent>,
        fonts: Arc<FontCatalog>,
        keymap: Keymap,
        css: Option<String>,
        pretty_text: bool,
        page_turn_duration: Duration,
        window_fit_duration: Duration,
    ) -> Self {
        Self {
            path,
            proxy,
            worker: None,
            surface: None,
            context: None,
            window: None,
            surface_size: (0, 0),
            bitmap: None,
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
            outline: Vec::new(),
            outline_open: false,
            outline_selected: 0,
            outline_bitmap: None,
            outline_bitmap_start: 0,
            outline_requested_start: None,
            outline_scroll: 0.0,
            page: 1,
            pages: Vec::new(),
            page_widths: Vec::new(),
            spread_gap: 0,
            spread_offset: 0.0,
            window_index: 0,
            page_count: None,
            scale: 1.0,
            page_fit: Fit::Page,
            rotation: 0,
            scroll_x: 0.0,
            scroll_y: 0.0,
            modifiers: ModifiersState::empty(),
            prompt: Prompt::None,
            command_menu: None,
            menu_text: None,
            status: None,
            fonts,
            keymap,
            pending_zooms: VecDeque::new(),
            last_requested_scale: 1.0,
            zoom_baseline: None,
            pending_restore: None,
            css,
            pretty_text,
            page_turn_duration,
            window_fit_duration,
            pending_viewport: None,
            resize_preview: false,
            last_viewport_sent: None,
            spread_active: false,
            spread_blocked: false,
            pending_turn: None,
            animation: None,
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

    fn request_redraw(&self) {
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }

    fn min_scroll_y(&self) -> f32 {
        let (_, viewport_h) = self.viewport();
        let page_h = self.bitmap.as_ref().map(|b| b.height as f32).unwrap_or(0.0);
        ((page_h + MARGIN as f32 * 2.0 - viewport_h as f32) / 2.0).min(0.0)
    }

    fn max_scroll_y(&self) -> f32 {
        let (_, viewport_h) = self.viewport();
        let page_h = self.bitmap.as_ref().map(|b| b.height as f32).unwrap_or(0.0);
        (page_h + MARGIN as f32 * 2.0 - viewport_h as f32).max(0.0)
    }

    fn page_fits_height(&self) -> bool {
        let (_, viewport_h) = self.viewport();
        self.bitmap
            .as_ref()
            .is_some_and(|page| page.height <= viewport_h.saturating_add(1))
    }

    fn content_left(&self) -> i32 {
        if self.outline_open {
            (OUTLINE_WIDTH as i32).min(self.viewport().0 as i32)
        } else {
            0
        }
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
        let page_w = self.visible_width() as f32;
        ((page_w - content_w) / 2.0).max(0.0)
    }

    fn spread_outline_width(&self) -> u32 {
        if self.outline_open {
            OUTLINE_WIDTH
        } else {
            0
        }
    }

    fn maybe_enable_spread(&mut self) {
        if self.spread_active || self.spread_blocked || self.pages.len() >= 2 {
            return;
        }
        if self.page_count.unwrap_or(0) < 2 {
            return;
        }
        let Some(page) = self.bitmap.as_ref() else {
            return;
        };
        if self.page_fit != Fit::Page {
            return;
        }
        // The outline is not opened automatically. It still reserves the
        // panel width when the user opens it with `o`.
        let required = page
            .width
            .saturating_mul(2)
            .saturating_add(self.spread_outline_width());
        if self.viewport().0 < required {
            return;
        }

        self.spread_active = true;
        self.send(Command::SetSpread(true));
        self.update_title();
    }

    fn update_spread_layout(&mut self) {
        if self.spread_active {
            if self.pages.len() < 2 {
                return;
            }
            if self.bitmap.is_none() {
                return;
            }
            if self.page_fit != Fit::Page {
                return;
            }
            // The visual gap is not part of the fit decision. Using the same
            // gap-free width as maybe_enable_spread also prevents a narrow
            // window from repeatedly enabling and disabling the spread when
            // the outline reduces the available width.
            let required = self
                .spread_pages_width()
                .saturating_add(self.spread_outline_width());
            if self.viewport().0 < required {
                self.spread_active = false;
                self.spread_blocked = true;
                self.send(Command::SetSpread(false));
            }
        } else {
            self.maybe_enable_spread();
        }
    }

    fn send_viewport(&mut self) {
        self.pending_viewport = None;
        let (width, height) = self.viewport();
        if width == 0 || height == 0 {
            return;
        }
        let width = if self.outline_open {
            width.saturating_sub(OUTLINE_WIDTH).max(1)
        } else {
            width
        };
        let target = (width, height);
        if self.last_viewport_sent == Some(target) {
            self.resize_preview = false;
        }
        self.last_viewport_sent = Some(target);
        self.send(Command::Viewport(width, height));
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
        self.scroll_y += delta;
        self.clamp_scroll();
        self.request_redraw();
    }

    fn scroll_horizontal(&mut self, delta: f32) {
        self.scroll_x += delta;
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

    fn request_outline_window(&mut self, start: usize) {
        if self.outline.is_empty() {
            return;
        }
        let start = start.min(self.outline.len() - 1);
        if self.outline_requested_start == Some(start) {
            return;
        }
        let count = 256.min(self.outline.len() - start);
        self.outline_requested_start = Some(start);
        self.send(Command::RenderOutline {
            width: OUTLINE_WIDTH,
            row_height: OUTLINE_ROW_HEIGHT,
            start,
            count,
        });
    }

    fn ensure_outline_window(&mut self) {
        if self.outline.is_empty() {
            return;
        }
        let (_, viewport_h) = self.viewport();
        let row_height = OUTLINE_ROW_HEIGHT as f32;
        let first = (self.outline_scroll / row_height).floor() as usize;
        let last = ((self.outline_scroll + viewport_h as f32) / row_height).ceil() as usize;
        let current_start = self.outline_bitmap_start;
        let current_rows = self
            .outline_bitmap
            .as_ref()
            .map(|bitmap| (bitmap.height as usize / OUTLINE_ROW_HEIGHT as usize).max(1))
            .unwrap_or(0);
        let selected_outside = self.outline_selected < current_start
            || self.outline_selected >= current_start + current_rows;
        let needed_first = first;
        let needed_last = last.min(self.outline.len());
        let current_end = (current_start + current_rows).min(self.outline.len());
        if current_rows == 0
            || selected_outside
            || needed_first < current_start
            || needed_last > current_end
        {
            let start = if selected_outside {
                self.outline_selected.saturating_sub(32)
            } else {
                first.saturating_sub(16)
            };
            self.request_outline_window(start);
        }
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
        self.outline_open = false;
        self.spread_blocked = false;
        self.pending_zooms.clear();
        self.pending_restore = None;
        self.zoom_baseline = None;
        self.last_requested_scale = self.scale;
        self.send_viewport();
        self.update_title();
        self.request_redraw();
    }

    fn move_outline(&mut self, delta: isize) {
        if self.outline.is_empty() {
            return;
        }
        let last = self.outline.len() - 1;
        let next = (self.outline_selected as isize + delta).clamp(0, last as isize) as usize;
        if next == self.outline_selected {
            return;
        }
        self.outline_selected = next;
        self.ensure_outline_visible();
        self.update_title();
        self.request_redraw();
    }

    fn activate_outline(&mut self) {
        self.finish_animation();
        let page = self
            .outline
            .get(self.outline_selected)
            .and_then(|entry| entry.page);
        if let Some(page) = page {
            self.send(Command::GotoPage(page));
            self.close_outline();
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
                self.outline_selected = 0;
                self.ensure_outline_visible();
                self.update_title();
                self.request_redraw();
                true
            }
            Key::Named(NamedKey::End) => {
                self.outline_selected = self.outline.len().saturating_sub(1);
                self.ensure_outline_visible();
                self.update_title();
                self.request_redraw();
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

    fn zoom_in(&mut self) {
        self.zoom(ZOOM_IN_FACTOR, Command::ZoomIn);
    }

    fn zoom_out(&mut self) {
        self.zoom(ZOOM_OUT_FACTOR, Command::ZoomOut);
    }

    fn reset_view(&mut self) {
        self.finish_animation();
        let Some(baseline) = self.zoom_baseline.take() else {
            return;
        };
        self.pending_zooms.clear();
        self.pending_restore = Some(PendingRestore {
            scale: baseline.scale,
            scroll: baseline.scroll,
        });
        self.last_requested_scale = baseline.scale;
        self.send(Command::SetScale(baseline.scale));
    }

    fn zoom(&mut self, factor: f32, command: Command) {
        self.finish_animation();
        self.pending_restore = None;
        if self.zoom_baseline.is_none() {
            self.zoom_baseline = Some(ViewState {
                scale: self.scale,
                scroll: (self.scroll_x, self.scroll_y),
            });
        }
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
        let (x0, y0) = self.page_origin()?;
        let width = self.visible_width();
        if width == 0 || page.height == 0 {
            return None;
        }
        Some((
            (screen.0 - x0 as f32) / width as f32,
            (screen.1 - y0 as f32) / page.height as f32,
        ))
    }

    /// Move the new bitmap so that a normalized page point sits under the
    /// screen point where the zoom was requested.
    fn place_page_point(&mut self, point: (f32, f32), screen: (f32, f32)) {
        let Some(page) = self.bitmap.as_ref() else {
            return;
        };
        let (scroll_x, scroll_y) = scroll_for_page_point_at_screen(
            self.viewport(),
            self.content_left(),
            (self.visible_width(), page.height),
            point,
            screen,
        );
        self.scroll_x = scroll_x;
        self.scroll_y = scroll_y;
        self.clamp_scroll();
    }

    fn rotate(&mut self, right: bool) {
        self.finish_animation();
        self.spread_blocked = false;
        self.scroll_x = 0.0;
        self.scroll_y = 0.0;
        self.pending_zooms.clear();
        self.pending_restore = None;
        self.zoom_baseline = None;
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
            window.set_title(&format!("{name} — Command: {}", menu.query));
            return;
        }

        match &self.prompt {
            Prompt::Goto(input) => {
                window.set_title(&format!("{name} — Goto page: {input}"));
                return;
            }
            Prompt::Outline(input) => {
                window.set_title(&format!(
                    "{name} — Outline page (empty for outline): {input}"
                ));
                return;
            }
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
            let page = entry
                .page
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
        let status = self
            .status
            .as_ref()
            .map(|s| format!(" — {s}"))
            .unwrap_or_default();
        window.set_title(&format!(
            "{name} — {page} — {:.0}%{rotation}{status}",
            self.scale * 100.0
        ));
    }

    fn start_goto_prompt(&mut self) {
        self.prompt = Prompt::Goto(String::new());
        self.update_title();
    }

    fn start_outline_prompt(&mut self) {
        self.prompt = Prompt::Outline(String::new());
        self.update_title();
    }

    fn start_search_prompt(&mut self, scope: SearchScope) {
        self.prompt = Prompt::Search(String::new(), scope);
        self.update_title();
    }

    fn open_command_menu(&mut self) {
        self.finish_animation();
        self.prompt = Prompt::None;
        self.command_menu = Some(CommandMenu {
            query: String::new(),
            selected: 0,
            scroll: 0,
            dirty: true,
            bitmap: None,
        });
        self.keymap.reset();
        self.update_title();
        self.request_redraw();
    }

    fn close_command_menu(&mut self) {
        self.command_menu = None;
        self.update_title();
        self.request_redraw();
    }

    fn menu_query_changed(&mut self) {
        if let Some(menu) = self.command_menu.as_mut() {
            menu.selected = 0;
            menu.scroll = 0;
            menu.dirty = true;
        }
        self.update_title();
        self.request_redraw();
    }

    fn move_menu_selection(&mut self, delta: isize) {
        let Some(menu) = self.command_menu.as_ref() else {
            return;
        };
        let count = filtered_commands(&menu.query).len();
        if count == 0 {
            return;
        }
        let selected =
            (menu.selected as isize + delta).clamp(0, count.saturating_sub(1) as isize) as usize;
        let visible = count.min(MENU_MAX_ROWS);
        let menu = self.command_menu.as_mut().expect("menu is open");
        menu.selected = selected;
        if selected < menu.scroll {
            menu.scroll = selected;
        } else if selected >= menu.scroll + visible {
            menu.scroll = selected + 1 - visible;
        }
        menu.scroll = menu.scroll.min(count.saturating_sub(visible));
        menu.dirty = true;
        self.request_redraw();
    }

    fn execute_menu_command(&mut self, event_loop: &ActiveEventLoop) {
        let action = self.command_menu.as_ref().and_then(|menu| {
            let commands = filtered_commands(&menu.query);
            commands
                .get(menu.selected)
                .map(|index| COMMANDS[*index].action)
        });
        let Some(action) = action else {
            return;
        };
        self.close_command_menu();
        self.run_menu_action(event_loop, action);
    }

    fn run_menu_action(&mut self, event_loop: &ActiveEventLoop, action: MenuAction) {
        match action {
            MenuAction::NextPage => self.next_page(),
            MenuAction::PrevPage => self.prev_page(),
            MenuAction::FirstPage => self.first_page(),
            MenuAction::LastPage => self.last_page(),
            MenuAction::GotoPage => self.start_goto_prompt(),
            MenuAction::Outline => self.open_outline(None),
            MenuAction::SearchPage => self.start_search_prompt(SearchScope::Page),
            MenuAction::SearchBook => self.start_search_prompt(SearchScope::Book),
            MenuAction::ZoomIn => self.zoom_in(),
            MenuAction::ZoomOut => self.zoom_out(),
            MenuAction::ResetView => self.reset_view(),
            MenuAction::RotateRight => self.rotate(true),
            MenuAction::RotateLeft => self.rotate(false),
            MenuAction::Copy => self.copy_selection(),
            MenuAction::Quit => event_loop.exit(),
        }
    }

    fn handle_command_menu_key(
        &mut self,
        event_loop: &ActiveEventLoop,
        event: &winit::event::KeyEvent,
    ) {
        if self.modifiers.control_key() {
            match event.logical_key.as_ref() {
                Key::Character("g") => {
                    self.close_command_menu();
                    return;
                }
                Key::Character("u") => {
                    if let Some(menu) = self.command_menu.as_mut() {
                        menu.query.clear();
                    }
                    self.menu_query_changed();
                    return;
                }
                Key::Character("n") => {
                    self.move_menu_selection(1);
                    return;
                }
                Key::Character("p") => {
                    self.move_menu_selection(-1);
                    return;
                }
                _ => {}
            }
        }

        match event.logical_key.as_ref() {
            Key::Named(NamedKey::Escape) => self.close_command_menu(),
            Key::Named(NamedKey::Enter) => self.execute_menu_command(event_loop),
            Key::Named(NamedKey::ArrowDown) => self.move_menu_selection(1),
            Key::Named(NamedKey::ArrowUp) => self.move_menu_selection(-1),
            Key::Named(NamedKey::PageDown) => self.move_menu_selection(MENU_MAX_ROWS as isize),
            Key::Named(NamedKey::PageUp) => self.move_menu_selection(-(MENU_MAX_ROWS as isize)),
            Key::Named(NamedKey::Home) => {
                if let Some(menu) = self.command_menu.as_mut() {
                    menu.selected = 0;
                    menu.scroll = 0;
                    menu.dirty = true;
                }
                self.request_redraw();
            }
            Key::Named(NamedKey::End) => {
                let last = self
                    .command_menu
                    .as_ref()
                    .map(|menu| filtered_commands(&menu.query).len().saturating_sub(1))
                    .unwrap_or(0);
                if let Some(menu) = self.command_menu.as_mut() {
                    menu.selected = last;
                    menu.dirty = true;
                }
                self.move_menu_selection(0);
                self.request_redraw();
            }
            Key::Named(NamedKey::Backspace) => {
                let empty = self
                    .command_menu
                    .as_ref()
                    .is_none_or(|menu| menu.query.is_empty());
                if empty {
                    self.close_command_menu();
                } else {
                    if let Some(menu) = self.command_menu.as_mut() {
                        menu.query.pop();
                    }
                    self.menu_query_changed();
                }
            }
            Key::Named(NamedKey::Space) => {
                if let Some(menu) = self.command_menu.as_mut() {
                    menu.query.push(' ');
                }
                self.menu_query_changed();
            }
            Key::Character(ch) if !self.modifiers.alt_key() => {
                let mut changed = false;
                for c in ch.chars() {
                    if !c.is_control() {
                        if let Some(menu) = self.command_menu.as_mut() {
                            menu.query.push(c);
                        }
                        changed = true;
                    }
                }
                if changed {
                    self.menu_query_changed();
                }
            }
            _ => {}
        }
    }

    fn ensure_command_menu_bitmap(&mut self, viewport: (u32, u32)) {
        let Some(menu) = self.command_menu.as_ref() else {
            return;
        };
        if !menu.dirty {
            return;
        }

        let query = menu.query.clone();
        let selected = menu.selected;
        let filtered = filtered_commands(&query);
        let visible = filtered.len().clamp(1, MENU_MAX_ROWS);
        let mut scroll = menu.scroll.min(filtered.len().saturating_sub(visible));
        if !filtered.is_empty() && selected >= scroll + visible {
            scroll = selected + 1 - visible;
        }
        let rows = visible;
        let width = viewport
            .0
            .saturating_sub(80)
            .clamp(280, 680)
            .min(viewport.0.max(1));
        let height = MENU_HEADER_HEIGHT + rows as u32 * MENU_ROW_HEIGHT + 8;
        let mut pixels = vec![0x001c1c1c; width as usize * height as usize];

        for y in 0..MENU_HEADER_HEIGHT.min(height) {
            let row = y as usize * width as usize;
            pixels[row..row + width as usize].fill(0x00242424);
        }

        for row in 0..rows {
            let Some(&index) = filtered.get(scroll + row) else {
                continue;
            };
            if index == selected {
                let top = MENU_HEADER_HEIGHT + row as u32 * MENU_ROW_HEIGHT;
                for y in top..(top + MENU_ROW_HEIGHT).min(height) {
                    let start = y as usize * width as usize;
                    pixels[start..start + width as usize].fill(0x00304a70);
                }
            }
        }

        if self.menu_text.is_none() {
            self.menu_text = TextEngine::new(self.fonts.clone()).ok();
        }
        if let Some(text) = self.menu_text.as_mut() {
            let query_label = format!(":{query}");
            let mut target = TextTarget::new(&mut pixels, width, height, 0, MENU_HEADER_HEIGHT);
            text.draw_line(
                &query_label,
                15.0,
                Weight::NORMAL,
                &mut target,
                MENU_PADDING as i32,
                0,
                MENU_HEADER_HEIGHT,
                (230, 230, 230),
            );

            if filtered.is_empty() {
                let top = MENU_HEADER_HEIGHT;
                let mut target =
                    TextTarget::new(&mut pixels, width, height, top as i32, MENU_ROW_HEIGHT);
                text.draw_line(
                    "No matching commands",
                    14.0,
                    Weight::NORMAL,
                    &mut target,
                    MENU_PADDING as i32 + 4,
                    top as i32,
                    MENU_ROW_HEIGHT,
                    (160, 160, 160),
                );
            } else {
                for row in 0..rows {
                    let Some(&index) = filtered.get(scroll + row) else {
                        continue;
                    };
                    let top = MENU_HEADER_HEIGHT + row as u32 * MENU_ROW_HEIGHT;
                    let color = if index == selected {
                        (255, 255, 255)
                    } else {
                        (215, 215, 215)
                    };
                    let mut target =
                        TextTarget::new(&mut pixels, width, height, top as i32, MENU_ROW_HEIGHT);
                    text.draw_line(
                        COMMANDS[index].label,
                        14.0,
                        Weight::NORMAL,
                        &mut target,
                        MENU_PADDING as i32 + 4,
                        top as i32,
                        MENU_ROW_HEIGHT,
                        color,
                    );
                }
            }
        }

        let border = 0x00484848;
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
        menu.scroll = scroll;
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

    fn open_outline(&mut self, target: Option<usize>) {
        self.finish_animation();
        let target = target.map(|page| {
            let page = page.max(1);
            self.page_count.map_or(page, |count| page.min(count))
        });
        if let Some(page) = target {
            self.send(Command::GotoPage(page));
        }
        if self.outline.is_empty() {
            self.status = Some("this document has no outline".to_string());
            self.update_title();
            return;
        }
        let reference = target.unwrap_or(self.page);
        self.outline_selected = self
            .outline
            .iter()
            .position(|entry| matches!(entry.page, Some(page) if page >= reference))
            .unwrap_or_else(|| self.outline.len().saturating_sub(1));
        self.outline_open = true;
        self.pending_zooms.clear();
        self.pending_restore = None;
        self.zoom_baseline = None;
        self.last_requested_scale = self.scale;
        // Opening the panel narrows the content area. Drop the spread in the
        // same event rather than waiting for the worker's single-page reply,
        // which would otherwise briefly re-enable it and cause a render loop.
        if self.spread_active
            && self.viewport().0 < self.spread_pages_width().saturating_add(OUTLINE_WIDTH)
        {
            self.spread_active = false;
            self.spread_blocked = true;
            self.send(Command::SetSpread(false));
        }
        self.send_viewport();
        self.ensure_outline_visible();
        self.update_title();
        self.request_redraw();
    }

    fn cancel_prompt(&mut self) {
        self.prompt = Prompt::None;
        self.update_title();
    }

    fn commit_prompt(&mut self) {
        match std::mem::replace(&mut self.prompt, Prompt::None) {
            Prompt::Goto(input) => {
                if let Ok(page) = input.parse::<usize>() {
                    self.goto_page(page);
                }
            }
            Prompt::Outline(input) => {
                let page = input.parse::<usize>().ok();
                self.open_outline(page);
            }
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
    }

    fn handle_prompt_key(&mut self, event: &winit::event::KeyEvent) {
        if self.modifiers.control_key()
            && matches!(event.logical_key.as_ref(), Key::Character(ch) if ch == "g")
        {
            self.cancel_prompt();
            return;
        }

        match event.logical_key.as_ref() {
            Key::Named(NamedKey::Escape) => self.cancel_prompt(),
            Key::Named(NamedKey::Enter) => self.commit_prompt(),
            Key::Named(NamedKey::Space) => {
                if let Prompt::Search(input, _) = &mut self.prompt {
                    input.push(' ');
                    self.update_title();
                }
            }
            Key::Named(NamedKey::Backspace) => match &mut self.prompt {
                Prompt::Goto(input) | Prompt::Outline(input) | Prompt::Search(input, _) => {
                    input.pop();
                    self.update_title();
                }
                Prompt::None => {}
            },
            Key::Character(ch) => match &mut self.prompt {
                Prompt::Goto(input) | Prompt::Outline(input) => {
                    for c in ch.chars() {
                        if c.is_ascii_digit() {
                            input.push(c);
                        }
                    }
                    self.update_title();
                }
                Prompt::Search(input, _) => {
                    for c in ch.chars() {
                        if !c.is_control() {
                            input.push(c);
                        }
                    }
                    self.update_title();
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
            Action::GotoPage => self.start_goto_prompt(),
            Action::Outline => self.start_outline_prompt(),
            Action::SearchPage => self.start_search_prompt(SearchScope::Page),
            Action::SearchBook => self.start_search_prompt(SearchScope::Book),
            Action::ZoomIn => self.zoom_in(),
            Action::ZoomOut => self.zoom_out(),
            Action::ResetView => self.reset_view(),
            Action::RotateRight => self.rotate(true),
            Action::RotateLeft => self.rotate(false),
            Action::Copy => self.copy_selection(),
        }
    }

    fn install_page(&mut self, page: RenderedPage) {
        let page_changed =
            self.page != page.page || self.pages != page.pages || self.rotation != page.rotation;
        let new_scale = page.scale;
        let restore = if page_changed {
            self.pending_zooms.clear();
            self.pending_restore = None;
            self.zoom_baseline = None;
            None
        } else {
            self.pending_restore
                .take_if(|pending| (pending.scale - new_scale).abs() < 1.0e-4)
        };
        let zoom_anchor = if restore.is_some() || page_changed {
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
        self.rotation = page.rotation;
        self.highlights = page.highlights;
        self.links = page.links;
        self.text = page.text;
        self.bitmap = Some(page.bitmap);
        self.resize_preview = false;
        self.window_index = self
            .page
            .saturating_sub(self.pages.first().copied().unwrap_or(self.page));
        self.spread_offset = self.normal_offset();
        if page_changed {
            self.clear_selection();
            self.scroll_x = 0.0;
            self.scroll_y = 0.0;
        } else if let Some(restore) = restore {
            self.scroll_x = restore.scroll.0;
            self.scroll_y = restore.scroll.1;
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
        self.update_spread_layout();
        self.update_title();
        self.request_redraw();
    }

    fn handle_page(&mut self, page: RenderedPage) {
        self.pending_turn = None;
        self.install_page(page);
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
                        self.send_viewport();
                    }
                } else if self.outline_open {
                    self.outline_selected = self
                        .outline
                        .iter()
                        .position(|entry| matches!(entry.page, Some(page) if page >= self.page))
                        .unwrap_or_else(|| self.outline.len().saturating_sub(1));
                    self.ensure_outline_visible();
                }
                self.update_title();
                self.request_redraw();
            }
            WorkerEvent::OutlineBitmap { start, bitmap } => {
                if self
                    .outline_requested_start
                    .is_some_and(|requested| requested != start)
                {
                    return;
                }
                self.outline_bitmap = Some(bitmap);
                self.outline_bitmap_start = start;
                self.outline_requested_start = None;
                self.ensure_outline_visible();
                self.request_redraw();
            }
            WorkerEvent::Status(status) => {
                self.status = Some(status);
                self.update_title();
            }
            WorkerEvent::Error(e) => {
                eprintln!("{TITLE}: {e}");
                self.status = Some(e);
                self.update_title();
            }
            WorkerEvent::Fatal(e) => {
                eprintln!("{TITLE}: {e}");
                event_loop.exit();
            }
        }
    }

    fn page_origin(&self) -> Option<(i32, i32)> {
        self.bitmap.as_ref()?;
        let visible = self.visible_width();
        Some((
            self.page_left_offset(visible)
                - self.spread_offset.round() as i32
                - self.scroll_x.round() as i32,
            MARGIN as i32 - self.scroll_y.round() as i32,
        ))
    }

    fn page_left_offset(&self, page_width: u32) -> i32 {
        page_left_offset(self.viewport(), self.content_left(), page_width)
    }

    fn page_coords(&self, position: PhysicalPosition<f64>) -> Option<(f32, f32)> {
        if position.x < self.content_left() as f64 {
            return None;
        }
        let page = self.bitmap.as_ref()?;
        let (x0, y0) = self.page_origin()?;
        let x = position.x as f32 - x0 as f32;
        let y = position.y as f32 - y0 as f32;
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
                if let Some(link) = self.link_at(position) {
                    if let Some(target) = link.target_page {
                        let target = self.page_count.map_or(target, |count| target.min(count));
                        self.status = Some(format!("link → page {target}"));
                        self.goto_page(target);
                    } else {
                        self.status = Some(format!("external link: {}", link.uri));
                    }
                    self.update_title();
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
        let (Some(width), Some(height)) =
            (NonZeroU32::new(size.width), NonZeroU32::new(size.height))
        else {
            return;
        };
        if self.command_menu.is_some() {
            self.ensure_command_menu_bitmap((size.width, size.height));
        }
        let content_left = self.content_left();
        let viewport = self.viewport();
        let bitmap = self.bitmap.as_ref();
        let highlights = &self.highlights;
        let scroll_x = self.scroll_x;
        let scroll_y = self.scroll_y;
        let spread_offset = self.spread_offset;
        let visible_width = self.visible_width() as i32;
        let menu_position = self.command_menu_position(viewport);
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        if self.surface_size != (size.width, size.height) {
            if let Err(e) = surface.resize(width, height) {
                eprintln!("{TITLE}: resize failed: {e}");
                return;
            }
            self.surface_size = (size.width, size.height);
        }

        let vw = size.width as i32;
        let vh = size.height as i32;
        let mut buffer = match surface.buffer_mut() {
            Ok(buffer) => buffer,
            Err(e) => {
                eprintln!("{TITLE}: buffer failed: {e}");
                return;
            }
        };
        buffer.fill(BACKGROUND);

        if self.resize_preview {
            if let Some(page) = bitmap {
                draw_page_preview(&mut buffer, (size.width, size.height), page);
            }
        } else if let Some(page) = bitmap {
            let bw = page.width as i32;
            let bh = page.height as i32;
            let window_left = page_left_offset(viewport, content_left, visible_width.max(1) as u32)
                - scroll_x.round() as i32;
            let origin_x = window_left - spread_offset.round() as i32;
            let y0 = MARGIN as i32 - scroll_y.round() as i32;

            let src_left = (spread_offset.round() as i32).max(0);
            let src_right = (spread_offset.round() as i32 + visible_width).min(bw);
            let dst_left = window_left;
            let clip_left = (-dst_left).max(0);
            let src_start_x = src_left + clip_left;
            let dst_x = dst_left + clip_left;
            let copy_w = (src_right - src_start_x).min(vw - dst_x).max(0);
            let clip_x0 = window_left;
            let clip_x1 = window_left + visible_width;
            if copy_w > 0 {
                for y in 0..bh {
                    let dy = y0 + y;
                    if dy < 0 || dy >= vh {
                        continue;
                    }
                    let src_start = y as usize * page.width as usize + src_start_x as usize;
                    let src_end = src_start + copy_w as usize;
                    let dst_start = dy as usize * size.width as usize + dst_x as usize;
                    buffer[dst_start..dst_start + copy_w as usize]
                        .copy_from_slice(&page.pixels[src_start..src_end]);
                }

                for highlight in highlights {
                    overlay_rect(
                        &mut buffer,
                        (vw, vh),
                        (origin_x, y0),
                        *highlight,
                        (255, 230, 30),
                        45,
                        clip_x0,
                        clip_x1,
                    );
                }

                if let Some(selection) = self.selection {
                    let (start, end) = selection.bounds();
                    if start < self.text.chars.len() {
                        let end = end.min(self.text.chars.len() - 1);
                        for ch in &self.text.chars[start..=end] {
                            overlay_rect(
                                &mut buffer,
                                (vw, vh),
                                (origin_x, y0),
                                ch.rect,
                                (80, 120, 210),
                                38,
                                clip_x0,
                                clip_x1,
                            );
                        }
                    }
                }
            }
        }

        if self.outline_open && !self.outline.is_empty() {
            let panel_w = (OUTLINE_WIDTH as i32).min(vw);
            let scroll = self.outline_scroll as i32;
            for y in 0..vh {
                let row = y as usize * size.width as usize;
                for x in 0..panel_w {
                    buffer[row + x as usize] = 0x00202020;
                }
            }

            if let Some(panel) = self.outline_bitmap.as_ref() {
                let copy_w = panel_w.min(panel.width as i32);
                let row_height = OUTLINE_ROW_HEIGHT as i32;
                let rows = panel.height as i32 / row_height;
                let start = self.outline_bitmap_start as i32;
                for row in 0..rows {
                    let top = (start + row) * row_height - scroll;
                    for line in 0..row_height {
                        let dy = top + line;
                        if dy < 0 || dy >= vh {
                            continue;
                        }
                        let src_y = row * row_height + line;
                        let src_start = src_y as usize * panel.width as usize;
                        let dst_start = dy as usize * size.width as usize;
                        buffer[dst_start..dst_start + copy_w as usize]
                            .copy_from_slice(&panel.pixels[src_start..src_start + copy_w as usize]);
                    }
                }
            }

            let row_top = self.outline_selected as i32 * OUTLINE_ROW_HEIGHT as i32 - scroll;
            let row_bottom = row_top + OUTLINE_ROW_HEIGHT as i32;
            let hy0 = row_top.max(0);
            let hy1 = row_bottom.min(vh);
            if hy0 < hy1 {
                for y in hy0..hy1 {
                    let row = y as usize * size.width as usize;
                    for x in 0..panel_w {
                        let idx = row + x as usize;
                        let dst = buffer[idx];
                        let dr = (dst >> 16) & 0xff;
                        let dg = (dst >> 8) & 0xff;
                        let db = dst & 0xff;
                        let r = (dr * 62 + 70 * 38) / 100;
                        let g = (dg * 62 + 110 * 38) / 100;
                        let b = (db * 62 + 180 * 38) / 100;
                        buffer[idx] = (r << 16) | (g << 8) | b;
                    }
                }
            }

            let border_x = panel_w - 1;
            if border_x >= 0 {
                for y in 0..vh {
                    let idx = y as usize * size.width as usize + border_x as usize;
                    buffer[idx] = 0x003c3c3c;
                }
            }
        }

        if let (Some(panel), Some((panel_x, panel_y))) = (
            self.command_menu
                .as_ref()
                .and_then(|menu| menu.bitmap.as_ref()),
            menu_position,
        ) {
            let copy_w = (panel.width as i32).min(vw - panel_x);
            let copy_h = (panel.height as i32).min(vh - panel_y);
            if copy_w > 0 && copy_h > 0 {
                for row in 0..copy_h {
                    let dst_y = panel_y + row;
                    if dst_y < 0 || dst_y >= vh {
                        continue;
                    }
                    let src_start = row as usize * panel.width as usize;
                    let dst_start = dst_y as usize * size.width as usize + panel_x as usize;
                    buffer[dst_start..dst_start + copy_w as usize]
                        .copy_from_slice(&panel.pixels[src_start..src_start + copy_w as usize]);
                }
            }
        }

        window.pre_present_notify();
        if let Err(e) = buffer.present() {
            eprintln!("{TITLE}: present failed: {e}");
        }
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
            .with_min_inner_size(LogicalSize::new(320.0, 240.0));
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
        let context = match Context::new(window.clone()) {
            Ok(context) => context,
            Err(e) => {
                eprintln!("{TITLE}: softbuffer context failed: {e}");
                event_loop.exit();
                return;
            }
        };
        let surface = match Surface::new(&context, window.clone()) {
            Ok(surface) => surface,
            Err(e) => {
                eprintln!("{TITLE}: softbuffer surface failed: {e}");
                event_loop.exit();
                return;
            }
        };

        let (viewport_w, viewport_h) = {
            let size = window.inner_size();
            (size.width.max(1), size.height.max(1))
        };
        let path = self.path.clone();

        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
        self.update_title();
        if let Some(window) = self.window.as_ref() {
            self.activation.activate(window);
        }

        let worker = WorkerHandle::spawn(self.proxy.clone());
        let viewport_w = if self.outline_open {
            viewport_w.saturating_sub(OUTLINE_WIDTH).max(1)
        } else {
            viewport_w
        };
        worker.send(Command::Viewport(viewport_w, viewport_h));
        self.last_viewport_sent = Some((viewport_w, viewport_h));
        worker.send(Command::Open {
            path,
            fonts: self.fonts.clone(),
            css: self.css.clone(),
            pretty: self.pretty_text,
        });
        self.worker = Some(worker);
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
        if let Some(deadline) = self.pending_viewport {
            if Instant::now() >= deadline {
                self.send_viewport();
                self.clamp_scroll();
                self.request_redraw();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
            }
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
                self.surface_size = (0, 0);
                self.spread_blocked = false;
                if let Some(menu) = self.command_menu.as_mut() {
                    menu.dirty = true;
                }
                if size.width > 0 && size.height > 0 {
                    self.pending_zooms.clear();
                    self.pending_restore = None;
                    self.zoom_baseline = None;
                    self.last_requested_scale = self.scale;
                    self.schedule_viewport();
                    self.clamp_scroll();
                    self.request_redraw();
                }
            }
            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }
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

fn draw_page_preview(buffer: &mut [u32], viewport: (u32, u32), page: &PageBitmap) {
    if page.width == 0 || page.height == 0 || viewport.0 == 0 || viewport.1 == 0 {
        return;
    }
    let vw = viewport.0 as f32;
    let vh = viewport.1 as f32;
    let scale = (vw / page.width as f32).min(vh / page.height as f32);
    let target_w = (page.width as f32 * scale).round().max(1.0) as u32;
    let target_h = (page.height as f32 * scale).round().max(1.0) as u32;
    let target_w = target_w.min(viewport.0);
    let target_h = target_h.min(viewport.1);
    let left = (viewport.0 - target_w) / 2;
    let top = (viewport.1 - target_h) / 2;
    let viewport_w = viewport.0 as usize;

    for dy in 0..target_h {
        let sy = (dy as u64 * page.height as u64 / target_h as u64) as usize;
        let src_row = sy * page.width as usize;
        let dst_row = (top + dy) as usize * viewport_w + left as usize;
        for dx in 0..target_w {
            let sx = (dx as u64 * page.width as u64 / target_w as u64) as usize;
            buffer[dst_row + dx as usize] = page.pixels[src_row + sx];
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn overlay_rect(
    buffer: &mut [u32],
    (viewport_w, viewport_h): (i32, i32),
    (origin_x, origin_y): (i32, i32),
    rect: HighlightRect,
    color: (u32, u32, u32),
    alpha_percent: u32,
    clip_x0: i32,
    clip_x1: i32,
) {
    let x0 = (origin_x + rect.x0 as i32).max(clip_x0).max(0);
    let y0 = (origin_y + rect.y0 as i32).max(0);
    let x1 = (origin_x + rect.x1 as i32).min(clip_x1).min(viewport_w);
    let y1 = (origin_y + rect.y1 as i32).min(viewport_h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }
    let alpha = alpha_percent.min(100);
    let inverse = 100 - alpha;
    let (cr, cg, cb) = color;
    for y in y0..y1 {
        let row = y as usize * viewport_w as usize;
        for x in x0..x1 {
            let idx = row + x as usize;
            let dst = buffer[idx];
            let dr = (dst >> 16) & 0xff;
            let dg = (dst >> 8) & 0xff;
            let db = dst & 0xff;
            let r = (dr * inverse + cr * alpha) / 100;
            let g = (dg * inverse + cg * alpha) / 100;
            let b = (db * inverse + cb * alpha) / 100;
            buffer[idx] = (r << 16) | (g << 8) | b;
        }
    }
}

fn page_left_offset(viewport: (u32, u32), content_left: i32, page_width: u32) -> i32 {
    let content_w = (viewport.0 as i32 - content_left).max(1);
    content_left + ((content_w - page_width as i32) as f32 / 2.0).round() as i32
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

fn run() -> Result<(), Box<dyn Error>> {
    let config: Config = config::load();
    let Some(path) = startup::pick_document() else {
        return Ok(());
    };
    let fonts = Arc::new(FontCatalog::new(config.font_family().as_deref())?);
    mupdf::set_font_loader(EpubFontLoader::new(fonts.clone()));
    let keymap = Keymap::new(&config.keys);
    let css = config.reflowable_css();

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut app = App::new(
        path,
        proxy,
        fonts,
        keymap,
        css,
        config.reflowable.pretty,
        Duration::from_millis(config.animation.page_turn_ms),
        Duration::from_millis(config.animation.window_fit_ms),
    );
    event_loop.run_app(&mut app)?;
    Ok(())
}

fn main() {
    if let Err(e) = startup::detach() {
        eprintln!("{TITLE}: detach failed: {e}");
        std::process::exit(1);
    }
    if let Err(e) = run() {
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
    fn command_menu_filters_with_intuitive_names() {
        let all = filtered_commands("");
        assert_eq!(all.len(), COMMANDS.len());
        assert_eq!(
            filtered_commands("rotate")
                .into_iter()
                .map(|index| COMMANDS[index].label)
                .collect::<Vec<_>>(),
            vec!["Rotate right", "Rotate left"]
        );
        assert_eq!(
            filtered_commands("toc")
                .into_iter()
                .map(|index| COMMANDS[index].label)
                .collect::<Vec<_>>(),
            vec!["Show outline"]
        );
        assert!(filtered_commands("nonsense").is_empty());
        assert_eq!(filtered_commands("right")[0], 11);
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
}
