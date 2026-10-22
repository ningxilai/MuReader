//! Wayland startup activation for MuReader window.
//!
//! A compositor may launch MuReader with `XDG_ACTIVATION_TOKEN` set. That
//! token is the standard, protocol-legal way for a background-launched client
//! to ask the host to focus its surface. The token is read only on Wayland;
//! `DESKTOP_STARTUP_ID` is an X11 mechanism and is intentionally ignored.

use std::env;

use wayland_client::backend::{Backend, ObjectId};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::{self, XdgActivationV1};
use winit::raw_window_handle::RawWindowHandle;
use winit::raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle};
use winit::window::Window;

/// One-shot host activation for a winit window.
///
/// The token is consumed by the host after a successful `activate` request;
/// later clicks do not need to send it again. If the compositor does not
/// advertise `xdg_activation_v1`, activation remains pending so a later mouse
/// press can retry.
pub struct Activation {
    token: Option<String>,
    activated: bool,
}

impl Activation {
    pub fn from_env() -> Self {
        let token = env::var("XDG_ACTIVATION_TOKEN")
            .ok()
            .filter(|token| !token.is_empty());
        Self {
            token,
            activated: false,
        }
    }

    /// Ask the host to activate `window`, if a startup token is available.
    ///
    /// Returns `true` if activation was requested successfully or had already
    /// been requested. Safe to call from a mouse handler to retry a startup
    /// activation that could not be completed.
    pub fn activate(&mut self, window: &Window) -> bool {
        if self.activated {
            return true;
        }
        let Some(token) = self.token.as_deref() else {
            return false;
        };
        match request_activation(window, token) {
            Ok(true) => {
                self.activated = true;
                true
            }
            Ok(false) => false,
            Err(e) => {
                eprintln!("MuReader: xdg activation failed: {e}");
                false
            }
        }
    }
}

fn request_activation(window: &Window, token: &str) -> Result<bool, String> {
    let RawDisplayHandle::Wayland(display) = window
        .display_handle()
        .map_err(|e| format!("display handle: {e}"))?
        .as_raw()
    else {
        return Ok(false);
    };
    let RawWindowHandle::Wayland(surface) = window
        .window_handle()
        .map_err(|e| format!("window handle: {e}"))?
        .as_raw()
    else {
        return Ok(false);
    };

    // SAFETY: winit owns this display and surface for the lifetime of the
    // window. The temporary Connection is only used to bind a global and send
    // one activate request; it never destroys or mutates winit's objects.
    let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
    let connection = Connection::from_backend(backend);

    struct ActivationState {
        activation: Option<XdgActivationV1>,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for ActivationState {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                if interface == "xdg_activation_v1" && state.activation.is_none() {
                    state.activation = Some(registry.bind(name, version.min(1), qh, ()));
                }
            }
        }
    }

    impl Dispatch<XdgActivationV1, ()> for ActivationState {
        fn event(
            _: &mut Self,
            _: &XdgActivationV1,
            _: xdg_activation_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    let mut queue = connection.new_event_queue::<ActivationState>();
    let qh = queue.handle();
    let _registry = connection.display().get_registry(&qh, ());
    let mut state = ActivationState { activation: None };
    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("registry roundtrip: {e}"))?;

    let Some(activation) = state.activation.as_ref() else {
        return Ok(false);
    };

    // SAFETY: the raw surface pointer comes from winit, is alive, and the
    // wrapped proxy is used only as the target of `activate`.
    let surface_id =
        unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.surface.as_ptr().cast()) }
            .map_err(|e| format!("surface id: {e}"))?;
    let surface =
        WlSurface::from_id(&connection, surface_id).map_err(|e| format!("surface proxy: {e}"))?;

    activation.activate(token.to_string(), &surface);
    connection
        .flush()
        .map_err(|e| format!("activation flush: {e}"))?;
    Ok(true)
}
