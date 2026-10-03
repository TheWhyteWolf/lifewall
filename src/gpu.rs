// SPDX-License-Identifier: GPL-3.0-or-later
// GLES2 renderer for --layer. One EGL context for every output; each frame
// uploads only the cell grid (one RGBA texel per cell: colour + glyph slot)
// and draws one quad whose fragment shader finds each pixel's cell, its glyph
// coverage in the atlas texture, and blends cell colour over the background.
// The glyph atlas is uploaded once per output scale.
//
// GLES2 / GLSL ES 1.00 on purpose: it is what every Mesa driver, old Intel
// iGPUs included, can do.

use crate::glyphs::CellAtlas;
use glow::HasContext;
use khronos_egl as egl;
use std::ffi::c_void;

const VERT: &str = "attribute vec2 a_pos;
void main() { gl_Position = vec4(a_pos, 0.0, 1.0); }";

const FRAG: &str = "#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
uniform sampler2D u_cells;   // cols x rows: rgb = cell colour, a = glyph slot / 255
uniform sampler2D u_atlas;   // glyph coverage, slots packed in a grid
uniform vec2 u_size;         // buffer size, px
uniform vec2 u_origin;       // grid's top-left, px
uniform vec2 u_cell;         // cell size, px
uniform vec2 u_grid;         // cols, rows
uniform vec2 u_atlas_grid;   // atlas slots per row, rows of slots
uniform vec3 u_bg;
void main() {
    vec2 p = vec2(gl_FragCoord.x, u_size.y - gl_FragCoord.y) - u_origin; // y down
    vec2 cell = floor(p / u_cell);
    if (cell.x < 0.0 || cell.y < 0.0 || cell.x >= u_grid.x || cell.y >= u_grid.y) {
        gl_FragColor = vec4(u_bg, 1.0);
        return;
    }
    vec4 c = texture2D(u_cells, (cell + 0.5) / u_grid);
    float slot = floor(c.a * 255.0 + 0.5);
    vec2 g = vec2(mod(slot, u_atlas_grid.x), floor(slot / u_atlas_grid.x));
    vec2 inner = p - cell * u_cell; // pixel centre within the cell
    float cov = texture2D(u_atlas, (g * u_cell + inner) / (u_atlas_grid * u_cell)).a;
    gl_FragColor = vec4(mix(u_bg, c.rgb, cov), 1.0);
}";

pub struct Gpu {
    egl: &'static egl::Instance<egl::Static>,
    display: egl::Display,
    config: egl::Config,
    context: egl::Context,
    pub gl: glow::Context,
    program: glow::Program,
    quad: glow::Buffer,
}

/// One output's EGL surface.
pub struct Target {
    window: *mut wayland_sys::egl::wl_egl_window,
    surface: egl::Surface,
    display: egl::Display,
    pub w: i32,
    pub h: i32,
}

impl Drop for Target {
    fn drop(&mut self) {
        let _ = egl::API.destroy_surface(self.display, self.surface);
        // SAFETY: created by wl_egl_window_create, destroyed exactly once,
        // after the EGL surface that used it.
        unsafe { wayland_sys::egl::wl_egl_window_destroy(self.window) };
    }
}

/// A packed glyph atlas on the GPU.
pub struct AtlasTex {
    tex: glow::Texture,
    per_row: usize,
    rows: usize,
    cw: usize,
    ch: usize,
}

/// The per-output cell grid texture.
pub struct GridTex {
    tex: glow::Texture,
    pub cols: usize,
    pub rows: usize,
}

fn err(what: &str) -> impl Fn(egl::Error) -> String + '_ {
    move |e| format!("{what}: {e}")
}

impl Gpu {
    /// `display` is the libwayland `wl_display*` of the connection we use.
    pub fn new(display: *mut c_void) -> Result<Gpu, String> {
        let egl = &egl::API;
        // SAFETY: a live wl_display pointer for the whole process lifetime.
        let display = unsafe { egl.get_display(display) }.ok_or("no EGL display")?;
        egl.initialize(display).map_err(err("eglInitialize"))?;
        egl.bind_api(egl::OPENGL_ES_API).map_err(err("eglBindAPI"))?;
        let attrs = [
            egl::SURFACE_TYPE, egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE, egl::OPENGL_ES2_BIT,
            egl::RED_SIZE, 8, egl::GREEN_SIZE, 8, egl::BLUE_SIZE, 8,
            egl::NONE,
        ];
        let config = egl.choose_first_config(display, &attrs).map_err(err("eglChooseConfig"))?.ok_or("no EGL config")?;
        let ctx_attrs = [egl::CONTEXT_CLIENT_VERSION, 2, egl::NONE];
        let context = egl.create_context(display, config, None, &ctx_attrs).map_err(err("eglCreateContext"))?;
        // Surfaceless is fine for compiling shaders (EGL_KHR_surfaceless_context,
        // which every Mesa driver has).
        egl.make_current(display, None, None, Some(context)).map_err(err("eglMakeCurrent"))?;
        // SAFETY: the loader returns entry points for the context just made current.
        let gl = unsafe {
            glow::Context::from_loader_function(|name| {
                egl.get_proc_address(name).map_or(std::ptr::null(), |f| f as *const c_void)
            })
        };
        let (program, quad) = unsafe { setup(&gl)? };
        Ok(Gpu { egl, display, config, context, gl, program, quad })
    }

    /// An EGL surface for `surface` (a `wl_surface*`) at `w`x`h` buffer pixels.
    pub fn target(&self, surface: *mut c_void, w: i32, h: i32) -> Result<Target, String> {
        // SAFETY: a live wl_surface proxy; the window is destroyed in Target's Drop.
        let window = unsafe { wayland_sys::egl::wl_egl_window_create(surface.cast(), w, h) };
        if window.is_null() {
            return Err("wl_egl_window_create failed".into());
        }
        // SAFETY: the window was just created and outlives the surface (Drop order).
        let made = unsafe { self.egl.create_window_surface(self.display, self.config, window.cast(), None) };
        let surface = match made {
            Ok(s) => s,
            Err(e) => {
                unsafe { wayland_sys::egl::wl_egl_window_destroy(window) };
                return Err(format!("eglCreateWindowSurface: {e}"));
            }
        };
        let t = Target { window, surface, display: self.display, w, h };
        self.bind(&t)?;
        // Never block in eglSwapBuffers: pacing is our own frame callbacks, so
        // a hidden output simply stops being drawn instead of stalling the loop.
        self.egl.swap_interval(self.display, 0).map_err(err("eglSwapInterval"))?;
        Ok(t)
    }

    pub fn bind(&self, t: &Target) -> Result<(), String> {
        self.egl
            .make_current(self.display, Some(t.surface), Some(t.surface), Some(self.context))
            .map_err(err("eglMakeCurrent"))
    }

    pub fn atlas(&self, a: &CellAtlas) -> Result<AtlasTex, String> {
        let slots = a.slots();
        let per_row = (2048 / a.cw).clamp(1, slots);
        let rows = slots.div_ceil(per_row);
        let (w, h) = (per_row * a.cw, rows * a.ch);
        let mut px = vec![0u8; w * h];
        for i in 0..slots {
            let (gx, gy) = ((i % per_row) * a.cw, (i / per_row) * a.ch);
            for (ry, src) in a.slot(i).chunks_exact(a.cw).enumerate() {
                px[(gy + ry) * w + gx..][..a.cw].copy_from_slice(src);
            }
        }
        let tex = unsafe { texture(&self.gl, glow::ALPHA, w, h, Some(&px))? };
        Ok(AtlasTex { tex, per_row, rows, cw: a.cw, ch: a.ch })
    }

    pub fn grid(&self, cols: usize, rows: usize) -> Result<GridTex, String> {
        let tex = unsafe { texture(&self.gl, glow::RGBA, cols, rows, None)? };
        Ok(GridTex { tex, cols, rows })
    }

    /// Upload `cells` (cols*rows RGBA) and draw the frame. The caller has bound
    /// the target, requested a frame callback, and swaps with `present`.
    pub fn draw(&self, t: &Target, grid: &GridTex, atlas: &AtlasTex, cells: &[u8], origin: (usize, usize), bg: [u8; 3]) {
        let gl = &self.gl;
        unsafe {
            gl.viewport(0, 0, t.w, t.h);
            gl.use_program(Some(self.program));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(grid.tex));
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D, 0, 0, 0, grid.cols as i32, grid.rows as i32,
                glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(Some(cells)),
            );
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(atlas.tex));
            let u = |n: &str| gl.get_uniform_location(self.program, n);
            gl.uniform_1_i32(u("u_cells").as_ref(), 0);
            gl.uniform_1_i32(u("u_atlas").as_ref(), 1);
            gl.uniform_2_f32(u("u_size").as_ref(), t.w as f32, t.h as f32);
            gl.uniform_2_f32(u("u_origin").as_ref(), origin.0 as f32, origin.1 as f32);
            gl.uniform_2_f32(u("u_cell").as_ref(), atlas.cw as f32, atlas.ch as f32);
            gl.uniform_2_f32(u("u_grid").as_ref(), grid.cols as f32, grid.rows as f32);
            gl.uniform_2_f32(u("u_atlas_grid").as_ref(), atlas.per_row as f32, atlas.rows as f32);
            let f = |c: u8| c as f32 / 255.0;
            gl.uniform_3_f32(u("u_bg").as_ref(), f(bg[0]), f(bg[1]), f(bg[2]));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.quad));
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 0, 0);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    pub fn present(&self, t: &Target) -> Result<(), String> {
        self.egl.swap_buffers(self.display, t.surface).map_err(err("eglSwapBuffers"))
    }

    pub fn free_grid(&self, g: GridTex) {
        unsafe { self.gl.delete_texture(g.tex) };
    }
}

unsafe fn texture(gl: &glow::Context, format: u32, w: usize, h: usize, data: Option<&[u8]>) -> Result<glow::Texture, String> {
    unsafe {
        let tex = gl.create_texture()?;
        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
        for (p, v) in [
            (glow::TEXTURE_MIN_FILTER, glow::NEAREST),
            (glow::TEXTURE_MAG_FILTER, glow::NEAREST),
            (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
            (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
        ] {
            gl.tex_parameter_i32(glow::TEXTURE_2D, p, v as i32);
        }
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.tex_image_2d(
            glow::TEXTURE_2D, 0, format as i32, w as i32, h as i32, 0,
            format, glow::UNSIGNED_BYTE, glow::PixelUnpackData::Slice(data),
        );
        Ok(tex)
    }
}

unsafe fn setup(gl: &glow::Context) -> Result<(glow::Program, glow::Buffer), String> {
    unsafe {
        let program = gl.create_program()?;
        for (kind, src) in [(glow::VERTEX_SHADER, VERT), (glow::FRAGMENT_SHADER, FRAG)] {
            let sh = gl.create_shader(kind)?;
            gl.shader_source(sh, src);
            gl.compile_shader(sh);
            if !gl.get_shader_compile_status(sh) {
                return Err(format!("shader: {}", gl.get_shader_info_log(sh)));
            }
            gl.attach_shader(program, sh);
        }
        gl.bind_attrib_location(program, 0, "a_pos");
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            return Err(format!("link: {}", gl.get_program_info_log(program)));
        }
        let quad = gl.create_buffer()?;
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(quad));
        let verts: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
        let bytes: Vec<u8> = verts.iter().flat_map(|v| v.to_ne_bytes()).collect();
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, &bytes, glow::STATIC_DRAW);
        Ok((program, quad))
    }
}
