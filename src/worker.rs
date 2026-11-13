// SPDX-License-Identifier: AGPL-3.0-only
//! Render worker.
//!
//! The worker thread is the only place that opens, uses, or drops a MuPDF
//! `Document`. The UI thread sends commands and receives finished pages. This
//! keeps `Document`'s `!Send` nature out of the application state entirely.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread;

use winit::event_loop::EventLoopProxy;

use crate::document::{
    Fit, LayoutIntent, OutlineEntry, PageBitmap, RenderEngine, RenderedPage, SearchScope,
    ZOOM_IN_FACTOR, ZOOM_OUT_FACTOR,
};
use crate::fonts::FontCatalog;
use crate::theme::OutlineStyle;
use crate::UserEvent;

pub enum Command {
    Viewport(u32, u32),
    /// Set the user's reading-model preference. The engine resolves the
    /// effective layout from it and the current window geometry. `reset_fit`
    /// returns to fit-page first, so `Auto` can choose a spread even when the
    /// view was zoomed.
    SetLayoutIntent {
        intent: LayoutIntent,
        reset_fit: bool,
    },
    SetBackground(u32),
    Open {
        path: PathBuf,
        fonts: Arc<FontCatalog>,
        css: Option<String>,
        document_css: bool,
        pretty: bool,
        page: Option<usize>,
        background: u32,
        layout: LayoutIntent,
    },
    NextPage,
    PrevPage,
    FirstPage,
    LastPage,
    GotoPage(usize),
    /// Slide the scroll window around an anchor page without treating it as a
    /// page jump. `before` is how many pages to render above the anchor.
    ScrollTo {
        anchor: usize,
        before: usize,
    },
    ZoomIn,
    ZoomOut,
    FontScale(f32),
    FitPage,
    RotateRight,
    RotateLeft,
    Search {
        query: String,
        scope: SearchScope,
    },
    RenderOutline {
        width: u32,
        row_height: u32,
        start: usize,
        count: usize,
        selected: Option<usize>,
        veil: Option<u32>,
        rail: Option<u32>,
        style: OutlineStyle,
    },
    Quit,
}

pub enum Event {
    Page(Box<RenderedPage>),
    Outline(Vec<OutlineEntry>),
    OutlineBitmap {
        start: usize,
        selected: Option<usize>,
        bitmap: PageBitmap,
    },
    Status(String),
    Error(String),
    Fatal(String),
}

pub struct WorkerHandle {
    tx: Sender<Command>,
    cancel: Arc<AtomicBool>,
}

impl WorkerHandle {
    pub fn spawn(proxy: EventLoopProxy<UserEvent>) -> Result<Self, String> {
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        thread::Builder::new()
            .name("mureader-render".to_string())
            .spawn(move || run(rx, proxy, worker_cancel))
            .map_err(|e| format!("spawn render worker: {e}"))?;
        Ok(Self { tx, cancel })
    }

    pub fn send(&self, command: Command) {
        // Every command cancels an in-flight book search. Starting a new
        // search clears the flag again on the worker thread.
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.tx.send(command);
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
    }
}

fn run(
    rx: std::sync::mpsc::Receiver<Command>,
    proxy: EventLoopProxy<UserEvent>,
    cancel: Arc<AtomicBool>,
) {
    let mut engine: Option<RenderEngine> = None;
    let mut viewport = (1100, 900);

    while let Ok(command) = rx.recv() {
        match command {
            Command::Viewport(width, height) => {
                let next = (width.max(1), height.max(1));
                if next == viewport {
                    continue;
                }
                viewport = next;
                if let Some(engine) = engine.as_mut() {
                    engine.set_viewport(viewport.0, viewport.1);
                    if engine.fit() != Fit::None {
                        render_and_send(engine, &proxy);
                    }
                }
            }
            Command::SetLayoutIntent { intent, reset_fit } => {
                if let Some(engine) = engine.as_mut() {
                    if reset_fit {
                        engine.fit_page();
                    }
                    engine.set_layout_intent(intent);
                    render_and_send(engine, &proxy);
                }
            }
            Command::SetBackground(background) => {
                if let Some(engine) = engine.as_mut() {
                    engine.set_background(background);
                    render_and_send(engine, &proxy);
                }
            }
            Command::Open {
                path,
                fonts,
                css,
                document_css,
                pretty,
                page,
                background,
                layout,
            } => {
                let mut opened = match RenderEngine::open_with_document_css(
                    &path,
                    fonts,
                    css,
                    pretty,
                    document_css,
                ) {
                    Ok(engine) => engine,
                    Err(e) => {
                        let _ = proxy.send_event(UserEvent::Worker(Event::Fatal(e)));
                        break;
                    }
                };
                opened.set_background(background);
                opened.set_viewport(viewport.0, viewport.1);
                opened.set_layout_intent(layout);
                if let Some(page) = page {
                    opened.goto_page(page);
                }
                engine = Some(opened);
                if let Some(engine) = engine.as_ref() {
                    let _ = proxy
                        .send_event(UserEvent::Worker(Event::Outline(engine.outline().to_vec())));
                }
                if let Some(engine) = engine.as_mut() {
                    render_and_send(engine, &proxy);
                }
            }
            Command::NextPage => {
                if let Some(engine) = engine.as_mut() {
                    if engine.next_page() {
                        render_and_send(engine, &proxy);
                    }
                }
            }
            Command::PrevPage => {
                if let Some(engine) = engine.as_mut() {
                    if engine.prev_page() {
                        render_and_send(engine, &proxy);
                    }
                }
            }
            Command::FirstPage => {
                if let Some(engine) = engine.as_mut() {
                    if engine.first_page() {
                        render_and_send(engine, &proxy);
                    }
                }
            }
            Command::LastPage => {
                if let Some(engine) = engine.as_mut() {
                    if engine.last_page() {
                        render_and_send(engine, &proxy);
                    }
                }
            }
            Command::GotoPage(page) => {
                if let Some(engine) = engine.as_mut() {
                    if engine.goto_page(page) {
                        render_and_send(engine, &proxy);
                    }
                }
            }
            Command::ScrollTo { anchor, before } => {
                if let Some(engine) = engine.as_mut() {
                    engine.scroll_to(anchor, before);
                    // Always answer: the UI drops strips that cannot represent
                    // its current position, so a no-op change still needs a
                    // fresh window around the anchor.
                    render_and_send(engine, &proxy);
                }
            }
            Command::ZoomIn => {
                if let Some(engine) = engine.as_mut() {
                    engine.zoom_by(ZOOM_IN_FACTOR);
                    render_and_send(engine, &proxy);
                }
            }
            Command::ZoomOut => {
                if let Some(engine) = engine.as_mut() {
                    engine.zoom_by(ZOOM_OUT_FACTOR);
                    render_and_send(engine, &proxy);
                }
            }
            Command::FontScale(factor) => {
                if let Some(engine) = engine.as_mut() {
                    match engine.adjust_font(factor) {
                        Ok(true) => {
                            let _ = proxy.send_event(UserEvent::Worker(Event::Outline(
                                engine.outline().to_vec(),
                            )));
                            render_and_send(engine, &proxy);
                        }
                        Ok(false) => {
                            let _ = proxy.send_event(UserEvent::Worker(Event::Status(
                                "text size at its limit".to_string(),
                            )));
                        }
                        Err(e) => {
                            let _ = proxy.send_event(UserEvent::Worker(Event::Error(e)));
                        }
                    }
                }
            }
            Command::FitPage => {
                if let Some(engine) = engine.as_mut() {
                    engine.fit_page();
                    render_and_send(engine, &proxy);
                }
            }
            Command::RotateRight => {
                if let Some(engine) = engine.as_mut() {
                    engine.rotate(90);
                    render_and_send(engine, &proxy);
                }
            }
            Command::RotateLeft => {
                if let Some(engine) = engine.as_mut() {
                    engine.rotate(-90);
                    render_and_send(engine, &proxy);
                }
            }
            Command::Search { query, scope } => {
                if let Some(engine) = engine.as_mut() {
                    cancel.store(false, Ordering::Relaxed);
                    match engine.search(&query, scope, &cancel) {
                        Ok(Some(page)) => {
                            let _ =
                                proxy.send_event(UserEvent::Worker(Event::Page(Box::new(page))));
                        }
                        Ok(None) => {
                            // A newer command interrupted this search; do not
                            // report the old query as having no matches.
                            if cancel.load(Ordering::Relaxed) {
                                continue;
                            }
                            engine.clear_search();
                            render_and_send(engine, &proxy);
                            let target = match scope {
                                SearchScope::Page => "this page",
                                SearchScope::Book => "the book",
                            };
                            let _ = proxy.send_event(UserEvent::Worker(Event::Status(format!(
                                "no match on {target} for {query:?}"
                            ))));
                        }
                        Err(e) => {
                            let _ = proxy.send_event(UserEvent::Worker(Event::Error(e)));
                        }
                    }
                }
            }
            Command::RenderOutline {
                width,
                row_height,
                start,
                count,
                selected,
                veil,
                rail,
                style,
            } => {
                if let Some(engine) = engine.as_mut() {
                    match engine.render_outline(
                        width, row_height, start, count, selected, veil, rail, style,
                    ) {
                        Ok(Some(bitmap)) => {
                            let _ = proxy.send_event(UserEvent::Worker(Event::OutlineBitmap {
                                start,
                                selected,
                                bitmap,
                            }));
                        }
                        Ok(None) => {
                            let _ = proxy.send_event(UserEvent::Worker(Event::Status(
                                "this document has no outline".to_string(),
                            )));
                        }
                        Err(e) => {
                            let _ = proxy.send_event(UserEvent::Worker(Event::Error(e)));
                        }
                    }
                }
            }
            Command::Quit => break,
        }
    }
}

fn render_and_send(engine: &mut RenderEngine, proxy: &EventLoopProxy<UserEvent>) {
    match engine.render() {
        Ok(page) => {
            let _ = proxy.send_event(UserEvent::Worker(Event::Page(Box::new(page))));
        }
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Worker(Event::Error(e)));
        }
    }
}
