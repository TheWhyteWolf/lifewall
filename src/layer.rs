// SPDX-License-Identifier: GPL-3.0-or-later
// --layer: the board drawn on a wlr-layer-shell background surface, one per
// output, rendered on the GPU (gpu.rs). Replaces running lifewall inside
// `kitten panel`, which cost a whole terminal emulator (~50 MB) to show a grid
// of coloured characters.
//
// Cost model:
//   * Frames are paced by frame callbacks. An output that is off, or fully
//     covered, stops sending them, so its board is not even stepped.
//   * Per frame the CPU computes each cell's colour and glyph slot (4 bytes a
//     cell) and uploads that grid; the GPU draws the pixels. A frame where no
//     cell changed is not drawn or committed at all.
//   * Glyphs are pre-rasterized once per scale (glyphs.rs); no font stays in
//     memory.
//
// Why not CPU-drawn shm buffers: niri holds an attached shm buffer until a
// different one replaces it, so they must alternate, and alternating buffers
// make it re-upload the whole frame (23 MB on a 3072x1920 panel) on every
// commit. That cost more in total than the kitty panel this replaces.
//
// Pausing stays external: lifebg-toggle.sh SIGSTOPs this process and the
// compositor keeps showing the last frame.

use crate::board::{Config, Engine};
use crate::glyphs::{self, CellAtlas};
use crate::gpu::{AtlasTex, GridTex, Gpu, Target};
use crate::{QUIT, RESEED};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            EventLoop,
        },
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
};
use smithay_client_toolkit::reexports::client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_surface},
    Connection, Proxy, QueueHandle,
};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::Ordering;

/// The drawable state for one output at one size and scale.
struct Canvas {
    target: Target,
    grid: GridTex,
    atlas: Rc<CellAtlas>,
    tex: Rc<AtlasTex>,
    ox: usize,
    oy: usize,
    engine: Engine,
    /// What was last uploaded: RGBA per cell (colour + glyph slot).
    cells: Vec<u8>,
    /// Nothing drawn yet: draw even if no cell changed.
    first: bool,
}

struct Wall {
    // Declared first so it drops first: the EGL surface must go before the
    // wl_surface it renders to.
    canvas: Option<Canvas>,
    output: wl_output::WlOutput,
    layer: LayerSurface,
    size: (u32, u32), // logical, from configure
    scale: i32,
    configured: bool,
    frame_pending: bool,
}

pub struct App {
    compositor: CompositorState,
    output_state: OutputState,
    registry_state: RegistryState,
    layer_shell: LayerShell,
    qh: QueueHandle<Self>,
    cfg: Config,
    walls: Vec<Wall>,
    /// Built once per output scale (usually one) and shared.
    atlases: HashMap<i32, (Rc<CellAtlas>, Rc<AtlasTex>)>,
    // Last: GL objects above are freed through it, and the context must
    // outlive every Target.
    gpu: Gpu,
    failed: bool,
}

pub fn run(cfg: Config) -> i32 {
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lifewall: cannot connect to Wayland: {e}");
            return 1;
        }
    };
    let (globals, event_queue) = match registry_queue_init(&conn) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lifewall: registry init failed: {e}");
            return 1;
        }
    };
    let qh: QueueHandle<App> = event_queue.handle();
    let mut event_loop: EventLoop<App> = EventLoop::try_new().expect("failed to create event loop");

    let layer_shell = match LayerShell::bind(&globals, &qh) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("lifewall: wlr-layer-shell not supported: {e}");
            return 1;
        }
    };
    let gpu = match Gpu::new(conn.backend().display_ptr().cast()) {
        Ok(g) => g,
        Err(e) => {
            eprintln!("lifewall: no GPU rendering ({e}); run it inside a terminal instead");
            return 1;
        }
    };
    let mut power = crate::power::Power::new(std::time::Instant::now());
    let first = power.frame(&cfg);
    let mut app = App {
        compositor: CompositorState::bind(&globals, &qh).expect("wl_compositor"),
        output_state: OutputState::new(&globals, &qh),
        registry_state: RegistryState::new(&globals),
        layer_shell,
        qh: qh.clone(),
        cfg,
        walls: Vec::new(),
        atlases: HashMap::new(),
        gpu,
        failed: false,
    };

    WaylandSource::new(conn, event_queue)
        .insert(event_loop.handle())
        .expect("insert wayland source");
    event_loop
        .handle()
        .insert_source(Timer::from_duration(first), move |_, _, app: &mut App| {
            app.tick();
            // Unplugged: --fps-battery from the next frame on.
            TimeoutAction::ToDuration(power.frame(&app.cfg))
        })
        .expect("insert frame timer");

    while !QUIT.load(Ordering::Relaxed) && !app.failed {
        if event_loop.dispatch(None, &mut app).is_err() {
            eprintln!("lifewall: event loop error");
            return 1;
        }
    }
    i32::from(app.failed)
}

impl App {
    fn tick(&mut self) {
        if RESEED.swap(false, Ordering::Relaxed) {
            for c in self.walls.iter_mut().filter_map(|w| w.canvas.as_mut()) {
                c.engine.reseed_now(&self.cfg);
            }
        }
        for i in 0..self.walls.len() {
            self.draw(i);
        }
    }

    fn add_wall(&mut self, output: wl_output::WlOutput) {
        let scale = self.output_state.info(&output).map(|i| i.scale_factor).unwrap_or(1).max(1);
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Background,
            Some("lifewall"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_exclusive_zone(-1); // the whole output, under any bar
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.set_size(0, 0);
        layer.commit();
        self.walls.push(Wall {
            canvas: None,
            output,
            layer,
            size: (0, 0),
            scale,
            configured: false,
            frame_pending: false,
        });
    }

    /// Forget a wall's canvas (size or scale changed), freeing its GL grid.
    fn reset(&mut self, i: usize) {
        if let Some(c) = self.walls[i].canvas.take() {
            self.gpu.free_grid(c.grid);
        }
    }

    fn atlas(&mut self, scale: i32) -> Option<(Rc<CellAtlas>, Rc<AtlasTex>)> {
        if let Some(a) = self.atlases.get(&scale) {
            return Some(a.clone());
        }
        let cfg = &self.cfg;
        let built = glyphs::build(&cfg.font_family, cfg.font_size, scale, &cfg.glyphs)
            .and_then(|a| Ok((Rc::new(self.gpu.atlas(&a)?), Rc::new(a))));
        match built {
            Ok((tex, a)) => Some(self.atlases.entry(scale).or_insert((a, tex)).clone()),
            Err(e) => {
                eprintln!("lifewall: {e}");
                self.failed = true;
                None
            }
        }
    }

    fn make_canvas(&mut self, i: usize) -> Option<Canvas> {
        let (size, scale) = (self.walls[i].size, self.walls[i].scale);
        let (w, h) = (size.0 as usize * scale as usize, size.1 as usize * scale as usize);
        let surface = self.walls[i].layer.wl_surface().clone();
        surface.set_buffer_scale(scale);
        let target = self
            .gpu
            .target(surface.id().as_ptr().cast(), w as i32, h as i32)
            .map_err(|e| {
                eprintln!("lifewall: {e}");
                self.failed = true;
            })
            .ok()?;
        let (atlas, tex) = self.atlas(scale)?;
        let (cols, rows) = ((w / atlas.cw).max(1), (h / atlas.ch).max(1));
        let grid = self.gpu.grid(cols, rows).map_err(|e| eprintln!("lifewall: {e}")).ok()?;
        Some(Canvas {
            target,
            grid,
            ox: w.saturating_sub(cols * atlas.cw) / 2,
            oy: h.saturating_sub(rows * atlas.ch) / 2,
            atlas,
            tex,
            engine: Engine::new(&self.cfg, cols, rows),
            cells: vec![0; cols * rows * 4],
            first: true,
        })
    }

    fn draw(&mut self, i: usize) {
        {
            let wall = &self.walls[i];
            if !wall.configured || wall.frame_pending || wall.size.0 == 0 || wall.size.1 == 0 {
                return;
            }
        }
        if self.walls[i].canvas.is_none() {
            let Some(c) = self.make_canvas(i) else { return };
            self.walls[i].canvas = Some(c);
        }
        let (cfg, gpu) = (&self.cfg, &self.gpu);
        let wall = &mut self.walls[i];
        let c = wall.canvas.as_mut().unwrap();

        let gen_f = c.engine.advance(cfg);
        let board = &c.engine.board;
        let bg = cfg.bg.to_u8();
        let blank = (c.atlas.slots() - 1) as u8;
        let mut changed = std::mem::take(&mut c.first);
        for (i, px) in c.cells.chunks_exact_mut(4).enumerate() {
            let col = board.color_at(i, gen_f, cfg);
            let slot = if col == bg { blank } else { c.atlas.index(board.glyph_at(i, cfg)) };
            let new = [col[0], col[1], col[2], slot];
            if px != new {
                px.copy_from_slice(&new);
                changed = true;
            }
        }
        if !changed {
            return; // nothing to show: no draw, no commit, no frame callback
        }
        if let Err(e) = gpu.bind(&c.target) {
            eprintln!("lifewall: {e}");
            return;
        }
        gpu.draw(&c.target, &c.grid, &c.tex, &c.cells, (c.ox, c.oy), bg);
        let surface = wall.layer.wl_surface();
        surface.frame(&self.qh, surface.clone());
        if let Err(e) = gpu.present(&c.target) {
            eprintln!("lifewall: {e}");
            return;
        }
        wall.frame_pending = true;
    }

    fn wall_index(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.walls.iter().position(|w| w.layer.wl_surface() == surface)
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // Output gone (or the compositor said so): drop it; a returning output
        // arrives through new_output.
        if let Some(i) = self.wall_index(layer.wl_surface()) {
            self.reset(i);
            self.walls.remove(i);
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(i) = self.wall_index(layer.wl_surface()) else { return };
        if configure.new_size != self.walls[i].size {
            self.walls[i].size = configure.new_size;
            self.reset(i);
        }
        let wall = &mut self.walls[i];
        wall.configured = true;
        // A configure without a commit leaves the surface unmapped: draw now,
        // even if a frame callback is still outstanding from an old buffer.
        wall.frame_pending = false;
        self.draw(i);
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, factor: i32) {
        if let Some(i) = self.wall_index(surface) {
            if self.walls[i].scale != factor.max(1) {
                self.walls[i].scale = factor.max(1);
                self.reset(i);
            }
        }
    }
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, _: u32) {
        if let Some(i) = self.wall_index(surface) {
            self.walls[i].frame_pending = false;
        }
    }
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.add_wall(output);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // A mode or scale change arrives as a configure / scale event on the
        // surface itself; only the scale needs catching here for compositors
        // that don't send preferred_buffer_scale.
        let scale = self.output_state.info(&output).map(|i| i.scale_factor).unwrap_or(1).max(1);
        if let Some(i) = self.walls.iter().position(|w| w.output == output) {
            if self.walls[i].scale != scale {
                self.walls[i].scale = scale;
                self.reset(i);
            }
        }
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if let Some(i) = self.walls.iter().position(|w| w.output == output) {
            self.reset(i);
            self.walls.remove(i);
        }
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_compositor!(App);
smithay_client_toolkit::delegate_output!(App);
smithay_client_toolkit::delegate_layer!(App);
smithay_client_toolkit::delegate_registry!(App);

