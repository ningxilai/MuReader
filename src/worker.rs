//! Render worker.
//!
//! The worker thread is the only place that opens, uses, or drops a MuPDF
//! `Document`. The UI thread sends commands and receives finished pages. This
//! keeps `Document`'s `!Send` nature out of the application state entirely.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use winit::event_loop::EventLoopProxy;

use crate::document::{
    Fit, OutlineEntry, PageBitmap, RenderEngine, RenderedPage, SearchScope, ZOOM_IN_FACTOR,
    ZOOM_OUT_FACTOR,
};
use crate::fonts::FontCatalog;
use crate::UserEvent;

pub enum Command {
    Viewport(u32, u32),
    SetSpread(bool),
    Open {
        path: PathBuf,
        fonts: Arc<FontCatalog>,
        css: Option<String>,
        pretty: bool,
    },
    NextPage,
    PrevPage,
    FirstPage,
    LastPage,
    GotoPage(usize),
    ZoomIn,
    ZoomOut,
    FontScale(f32),
    SetScale(f32),
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
    },
    Quit,
}

pub enum Event {
    Page(RenderedPage),
    Outline(Vec<OutlineEntry>),
    OutlineBitmap { start: usize, bitmap: PageBitmap },
    Status(String),
    Error(String),
    Fatal(String),
}

pub struct WorkerHandle {
    tx: Sender<Command>,
    join: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    pub fn spawn(proxy: EventLoopProxy<UserEvent>) -> Self {
        let (tx, rx) = channel();
        let join = thread::Builder::new()
            .name("mureader-render".to_string())
            .spawn(move || run(rx, proxy))
            .expect("spawn render worker");
        Self {
            tx,
            join: Some(join),
        }
    }

    pub fn send(&self, command: Command) {
        let _ = self.tx.send(command);
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Quit);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn run(rx: std::sync::mpsc::Receiver<Command>, proxy: EventLoopProxy<UserEvent>) {
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
            Command::SetSpread(spread) => {
                if let Some(engine) = engine.as_mut() {
                    engine.set_spread(spread);
                    render_and_send(engine, &proxy);
                }
            }
            Command::Open {
                path,
                fonts,
                css,
                pretty,
            } => {
                let mut opened = match RenderEngine::open(&path, fonts, css, pretty) {
                    Ok(engine) => engine,
                    Err(e) => {
                        let _ = proxy.send_event(UserEvent::Worker(Event::Fatal(e)));
                        break;
                    }
                };
                opened.set_viewport(viewport.0, viewport.1);
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
            Command::SetScale(scale) => {
                if let Some(engine) = engine.as_mut() {
                    engine.set_scale(scale);
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
                    match engine.search(&query, scope) {
                        Ok(Some(page)) => {
                            let _ = proxy.send_event(UserEvent::Worker(Event::Page(page)));
                        }
                        Ok(None) => {
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
            } => {
                if let Some(engine) = engine.as_mut() {
                    match engine.render_outline(width, row_height, start, count) {
                        Ok(Some(bitmap)) => {
                            let _ = proxy.send_event(UserEvent::Worker(Event::OutlineBitmap {
                                start,
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
            let _ = proxy.send_event(UserEvent::Worker(Event::Page(page)));
        }
        Err(e) => {
            let _ = proxy.send_event(UserEvent::Worker(Event::Error(e)));
        }
    }
}
