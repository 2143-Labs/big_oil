//! Layer-shell overlay rendering.
//!
//! A single surface on the `overlay` layer — niri (and wlroots compositors
//! generally) keep that layer above fullscreen windows, which the `top` layer
//! does not. The surface has an empty input region, so every click still lands
//! in the game.

use anyhow::{Context, Result};
use smithay_client_toolkit::reexports::client::{
    Connection, QueueHandle,
    globals::registry_queue_init,
    protocol::{wl_output, wl_shm::Format, wl_surface},
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        WaylandSurface,
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
    },
    shm::{Shm, ShmHandler, slot::SlotPool},
};

use super::{Mark, Rect};

pub fn run(area: Rect, marks: Vec<Mark>) -> Result<()> {
    let conn =
        Connection::connect_to_env().context("connecting to Wayland (WAYLAND_DISPLAY unset?)")?;
    let (globals, mut event_queue) = registry_queue_init::<Overlay>(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).context("binding wl_compositor")?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .context("compositor does not support wlr-layer-shell (needed for an overlay)")?;
    let shm = Shm::bind(&globals, &qh).context("binding wl_shm")?;
    let output_state = OutputState::new(&globals, &qh);

    let pool = SlotPool::new((area.w as usize * area.h as usize * 4).max(4), &shm)
        .context("allocating the overlay buffer")?;

    let mut state = Overlay {
        registry_state: RegistryState::new(&globals),
        output_state,
        shm,
        pool,
        layer: None,
        width: area.w,
        height: area.h,
        origin: (area.x, area.y),
        marks,
        buffer: None,
        exit: false,
    };

    // Let the output geometry arrive before choosing which monitor to cover;
    // without this the surface defaults to the focused output, which may not be
    // the one the game is on.
    event_queue.roundtrip(&mut state)?;
    let output = pick_output(&state.output_state, area);
    if output.is_none() {
        tracing::warn!(
            area = ?(area.x, area.y, area.w, area.h),
            "no output contains the stash region; using the focused output"
        );
    }

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("big_oil"),
        output.as_ref(),
    );
    layer.set_anchor(Anchor::TOP | Anchor::LEFT);
    layer.set_margin(area.y, 0, 0, area.x);
    layer.set_size(area.w, area.h);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.set_exclusive_zone(-1);

    // Empty input region: the overlay never steals a click from the game.
    let empty = Region::new(&compositor)?;
    layer.wl_surface().set_input_region(Some(empty.wl_region()));
    layer.commit();
    state.layer = Some(layer);

    while !state.exit {
        event_queue
            .blocking_dispatch(&mut state)
            .context("dispatching Wayland events")?;
    }
    Ok(())
}

fn pick_output(outputs: &OutputState, area: Rect) -> Option<wl_output::WlOutput> {
    let (x, y) = (area.x, area.y);
    outputs.outputs().into_iter().find(|output| {
        outputs.info(output).is_some_and(|info| {
            let Some((ox, oy)) = info.logical_position else {
                return false;
            };
            let Some((ow, oh)) = info.logical_size else {
                return false;
            };
            x >= ox && x < ox + ow && y >= oy && y < oy + oh
        })
    })
}

struct Overlay {
    registry_state: RegistryState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    width: u32,
    height: u32,
    origin: (i32, i32),
    marks: Vec<Mark>,
    buffer: Option<smithay_client_toolkit::shm::slot::Buffer>,
    exit: bool,
}

impl Overlay {
    fn draw(&mut self) {
        let (w, h) = (self.width.max(1), self.height.max(1));
        let stride = (w as i32) * 4;
        let (buffer, canvas) =
            match self
                .pool
                .create_buffer(w as i32, h as i32, stride, Format::Argb8888)
            {
                Ok(created) => created,
                Err(e) => {
                    tracing::error!(error = %e, "could not allocate overlay buffer");
                    return;
                }
            };
        canvas.fill(0);
        tracing::debug!(w, h, marks = self.marks.len(), "drawing overlay frame");
        for mark in &self.marks {
            draw_mark(canvas, w, h, mark, self.origin);
        }

        let Some(layer) = &self.layer else { return };
        if let Err(e) = buffer.attach_to(layer.wl_surface()) {
            tracing::error!(error = %e, "attaching overlay buffer");
            return;
        }
        // Keep the previous buffer alive until the next frame.
        self.buffer = Some(buffer);
        layer.wl_surface().damage_buffer(0, 0, w as i32, h as i32);
        layer.commit();
    }
}

/// Draw a filled cell with a bright border. `canvas` is ARGB8888 little-endian.
fn draw_mark(canvas: &mut [u8], sw: u32, sh: u32, mark: &Mark, origin: (i32, i32)) {
    let x0 = (mark.rect.x - origin.0).max(0);
    let y0 = (mark.rect.y - origin.1).max(0);
    let x1 = (mark.rect.x - origin.0 + mark.rect.w as i32).min(sw as i32);
    let y1 = (mark.rect.y - origin.1 + mark.rect.h as i32).min(sh as i32);
    if x1 <= x0 || y1 <= y0 {
        return;
    }

    let (r, g, b) = mark.colour.rgb();
    let fill = premultiply(r, g, b, 44);
    let border = premultiply(r, g, b, 235);
    const EDGE: i32 = 3;

    for y in y0..y1 {
        for x in x0..x1 {
            let on_border = x < x0 + EDGE || x >= x1 - EDGE || y < y0 + EDGE || y >= y1 - EDGE;
            put(canvas, sw, x, y, if on_border { border } else { fill });
        }
    }
}

fn put(canvas: &mut [u8], stride_px: u32, x: i32, y: i32, argb: u32) {
    let idx = ((y as usize) * (stride_px as usize) + (x as usize)) * 4;
    if idx + 3 < canvas.len() {
        canvas[idx] = (argb & 0xff) as u8;
        canvas[idx + 1] = ((argb >> 8) & 0xff) as u8;
        canvas[idx + 2] = ((argb >> 16) & 0xff) as u8;
        canvas[idx + 3] = ((argb >> 24) & 0xff) as u8;
    }
}

/// Wayland ARGB8888 expects premultiplied alpha.
fn premultiply(r: u8, g: u8, b: u8, a: u8) -> u32 {
    let scale = |c: u8| ((c as u32 * a as u32) / 255) & 0xff;
    ((a as u32) << 24) | (scale(r) << 16) | (scale(g) << 8) | scale(b)
}

impl CompositorHandler for Overlay {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl ShmHandler for Overlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl OutputHandler for Overlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Overlay {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let (w, h) = configure.new_size;
        tracing::debug!(w, h, "layer surface configured");
        if w > 0 && h > 0 {
            self.width = w;
            self.height = h;
        }
        self.draw();
    }
}

impl ProvidesRegistryState for Overlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

delegate_compositor!(Overlay);
delegate_output!(Overlay);
delegate_shm!(Overlay);
delegate_layer!(Overlay);
delegate_registry!(Overlay);
