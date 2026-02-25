//! Wayland overlay using wlr-layer-shell
//!
//! Creates a transparent, click-through overlay for displaying lyrics.

use anyhow::Result;
use cosmic_text::{
    Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache,
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_registry, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer as WlBuffer, SlotPool},
        Shm, ShmHandler,
    },
};
use tokio::sync::watch;
use tracing::{debug, info};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_output, wl_region, wl_shm, wl_surface},
    Connection, QueueHandle,
};

use std::env;

fn get_env_u32(name: &str, default: u32) -> u32 {
    env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn get_env_i32(name: &str, default: i32) -> i32 {
    env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn get_env_f32(name: &str, default: f32) -> f32 {
    env::var(name).ok().and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn overlay_width() -> u32 { get_env_u32("LYRICS_WIDTH", 600) }
fn overlay_height() -> u32 { get_env_u32("LYRICS_HEIGHT", 80) }
fn top_margin() -> i32 { get_env_i32("LYRICS_TOP_MARGIN", 50) }  // Higher default
fn right_margin() -> i32 { get_env_i32("LYRICS_RIGHT_MARGIN", 10) }
fn font_size() -> f32 { get_env_f32("LYRICS_FONT_SIZE", 22.0) }

struct TextRenderer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    buffer: Buffer,
    font_size: f32,
}

impl TextRenderer {
    fn new() -> Self {
        let mut font_system = FontSystem::new();
        let swash_cache = SwashCache::new();

        let fsize = font_size();
        let metrics = Metrics::new(fsize, fsize * 1.2);
        let buffer = Buffer::new(&mut font_system, metrics);

        Self {
            font_system,
            swash_cache,
            buffer,
            font_size: fsize,
        }
    }

    fn render(&mut self, canvas: &mut [u8], width: u32, height: u32, text: &str) {
        // Set up the text buffer
        self.buffer.set_size(&mut self.font_system, Some(width as f32), Some(height as f32));

        let attrs = Attrs::new()
            .family(Family::SansSerif)
            .color(Color::rgb(255, 255, 255));

        self.buffer.set_text(&mut self.font_system, text, attrs, Shaping::Advanced);

        // Shape the text
        self.buffer.shape_until_scroll(&mut self.font_system, false);

        // First line is centered vertically (same position whether 1 or multiple lines)
        let y_offset = ((height as f32 - self.font_size) / 2.0) as i32;

        // Render shadow first (each line right-aligned individually)
        self.render_glyphs(canvas, width, height, y_offset + 2, 2, 0, 0, 0, 160);

        // Render main text
        self.render_glyphs(canvas, width, height, y_offset, 0, 255, 255, 255, 255);
    }

    fn render_glyphs(
        &mut self,
        canvas: &mut [u8],
        width: u32,
        height: u32,
        y_offset: i32,
        shadow_offset: i32,
        r: u8,
        g: u8,
        b: u8,
        base_alpha: u8,
    ) {
        // Get first line's line_y to use as baseline offset
        let first_line_y = self.buffer.layout_runs().next().map(|r| r.line_y).unwrap_or(0.0);

        for run in self.buffer.layout_runs() {
            // Calculate this line's width for right-alignment
            let mut line_width = 0.0f32;
            for glyph in run.glyphs.iter() {
                line_width = line_width.max(glyph.x + glyph.w);
            }

            // Right-align this line individually
            let x_offset = (width as f32 - line_width - 15.0).max(5.0) as i32 + shadow_offset;

            // Position first line at y_offset, subsequent lines below
            let line_y = y_offset + (run.line_y - first_line_y) as i32 + shadow_offset;

            for glyph in run.glyphs.iter() {
                let physical_glyph = glyph.physical((0.0, 0.0), 1.0);

                let Some(image) = self.swash_cache.get_image(&mut self.font_system, physical_glyph.cache_key) else {
                    continue;
                };

                let glyph_x = x_offset + physical_glyph.x + image.placement.left;
                let glyph_y = line_y + physical_glyph.y - image.placement.top + self.font_size as i32;

                for py in 0..image.placement.height as i32 {
                    for px in 0..image.placement.width as i32 {
                        let canvas_x = glyph_x + px;
                        let canvas_y = glyph_y + py;

                        if canvas_x >= 0 && canvas_x < width as i32
                            && canvas_y >= 0 && canvas_y < height as i32
                        {
                            let src_idx = (py * image.placement.width as i32 + px) as usize;
                            if src_idx < image.data.len() {
                                let coverage = image.data[src_idx];
                                if coverage > 0 {
                                    let dst_idx = ((canvas_y as u32 * width + canvas_x as u32) * 4) as usize;

                                    // Gamma-correct alpha blending for smoother edges
                                    let src_a = (coverage as u32 * base_alpha as u32 / 255) as u8;
                                    let dst_a = canvas[dst_idx + 3];

                                    // Use standard porter-duff "over" compositing
                                    let inv_src_a = 255 - src_a;
                                    let out_a = src_a as u32 + (dst_a as u32 * inv_src_a as u32 / 255);

                                    if out_a > 0 {
                                        // Premultiplied alpha blending
                                        let out_r = (r as u32 * src_a as u32 / 255) + (canvas[dst_idx + 2] as u32 * inv_src_a as u32 / 255);
                                        let out_g = (g as u32 * src_a as u32 / 255) + (canvas[dst_idx + 1] as u32 * inv_src_a as u32 / 255);
                                        let out_b = (b as u32 * src_a as u32 / 255) + (canvas[dst_idx] as u32 * inv_src_a as u32 / 255);

                                        canvas[dst_idx] = out_b.min(255) as u8;
                                        canvas[dst_idx + 1] = out_g.min(255) as u8;
                                        canvas[dst_idx + 2] = out_r.min(255) as u8;
                                        canvas[dst_idx + 3] = out_a.min(255) as u8;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

struct LyricsOverlay {
    registry_state: RegistryState,
    compositor_state: CompositorState,
    output_state: OutputState,
    shm: Shm,
    layer_shell: LayerShell,

    layer_surface: Option<LayerSurface>,
    pool: Option<SlotPool>,
    buffer: Option<WlBuffer>,

    width: u32,
    height: u32,
    configured: bool,
    exit: bool,

    current_lyric: String,
    lyric_rx: watch::Receiver<String>,

    text_renderer: TextRenderer,
}

impl LyricsOverlay {
    fn new(
        registry_state: RegistryState,
        compositor_state: CompositorState,
        output_state: OutputState,
        shm: Shm,
        layer_shell: LayerShell,
        lyric_rx: watch::Receiver<String>,
    ) -> Self {
        info!("Initializing text renderer with system fonts");
        let text_renderer = TextRenderer::new();

        info!(
            "Overlay config: {}x{}, top_margin={}, right_margin={}, font_size={}",
            overlay_width(), overlay_height(), top_margin(), right_margin(), font_size()
        );

        Self {
            registry_state,
            compositor_state,
            output_state,
            shm,
            layer_shell,
            layer_surface: None,
            pool: None,
            buffer: None,
            width: overlay_width(),
            height: overlay_height(),
            configured: false,
            exit: false,
            current_lyric: String::new(),
            lyric_rx,
            text_renderer,
        }
    }

    fn create_layer_surface(&mut self, qh: &QueueHandle<Self>) {
        let surface = self.compositor_state.create_surface(qh);

        let layer_surface = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("lyrics-overlay"),
            None, // Use default output
        );

        // Anchor to top-right
        layer_surface.set_anchor(Anchor::TOP | Anchor::RIGHT);

        // Set margins (configurable)
        layer_surface.set_margin(top_margin(), right_margin(), 0, 0);

        // Set size
        layer_surface.set_size(self.width, self.height);

        // Don't take exclusive zone (don't push other windows)
        layer_surface.set_exclusive_zone(-1);

        // No keyboard input
        layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);

        // Commit to apply settings
        layer_surface.commit();

        self.layer_surface = Some(layer_surface);
    }

    fn set_click_through(&self, qh: &QueueHandle<Self>) {
        if let Some(ref layer_surface) = self.layer_surface {
            let surface = layer_surface.wl_surface();

            // Create an empty region for click-through
            let compositor = self.compositor_state.wl_compositor();
            let empty_region = compositor.create_region(qh, ());

            // Set the empty region as input region
            surface.set_input_region(Some(&empty_region));

            // Destroy the region object (compositor keeps a copy)
            empty_region.destroy();

            surface.commit();
        }
    }

    fn draw(&mut self, _qh: &QueueHandle<Self>) {
        if !self.configured {
            return;
        }

        let Some(ref layer_surface) = self.layer_surface else {
            return;
        };

        let surface = layer_surface.wl_surface();

        // Ensure we have a pool with enough space for double buffering
        if self.pool.is_none() {
            let pool = SlotPool::new(
                (self.width * self.height * 4 * 2) as usize, // Double buffer
                &self.shm,
            )
            .expect("Failed to create slot pool");
            self.pool = Some(pool);
        }

        let pool = self.pool.as_mut().unwrap();

        // Create a fresh buffer
        let (buffer, canvas) = pool
            .create_buffer(
                self.width as i32,
                self.height as i32,
                (self.width * 4) as i32,
                wl_shm::Format::Argb8888,
            )
            .expect("Failed to create buffer");

        // Clear entire canvas to transparent (use fill for efficiency)
        canvas.fill(0);

        // Only render if visible (toggle via SIGUSR1)
        if crate::VISIBLE.load(std::sync::atomic::Ordering::Relaxed) {
            // Render text if we have lyrics
            let lyric = self.current_lyric.clone();
            if !lyric.is_empty() {
                self.text_renderer.render(canvas, self.width, self.height, &lyric);
            }
        }

        // Attach and commit
        buffer.attach_to(surface).expect("Failed to attach buffer");
        surface.damage_buffer(0, 0, self.width as i32, self.height as i32);
        surface.commit();

        // Drop old buffer to free memory
        self.buffer = Some(buffer);
    }

    fn check_lyric_update(&mut self) -> bool {
        if self.lyric_rx.has_changed().unwrap_or(false) {
            self.current_lyric = self.lyric_rx.borrow_and_update().clone();
            true
        } else {
            false
        }
    }
}

impl CompositorHandler for LyricsOverlay {
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
        qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // Always check for updates and redraw (handles both lyric changes and visibility toggle)
        self.check_lyric_update();
        self.draw(qh);

        // Request next frame
        if let Some(ref layer_surface) = self.layer_surface {
            layer_surface
                .wl_surface()
                .frame(qh, layer_surface.wl_surface().clone());
            layer_surface.wl_surface().commit();
        }
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

impl OutputHandler for LyricsOverlay {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for LyricsOverlay {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        debug!("Layer surface configured: {:?}", configure);

        if configure.new_size.0 > 0 {
            self.width = configure.new_size.0;
        }
        if configure.new_size.1 > 0 {
            self.height = configure.new_size.1;
        }

        self.configured = true;

        // Set click-through after configuration
        self.set_click_through(qh);

        // Initial draw
        self.draw(qh);

        // Request frame callbacks
        layer.wl_surface().frame(qh, layer.wl_surface().clone());
        layer.wl_surface().commit();
    }
}

impl ShmHandler for LyricsOverlay {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for LyricsOverlay {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers!(OutputState);
}

// Implement region handler (just needs to exist)
impl wayland_client::Dispatch<wl_region::WlRegion, ()> for LyricsOverlay {
    fn event(
        _state: &mut Self,
        _proxy: &wl_region::WlRegion,
        _event: wl_region::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // wl_region has no events
    }
}

delegate_compositor!(LyricsOverlay);
delegate_output!(LyricsOverlay);
delegate_layer!(LyricsOverlay);
delegate_shm!(LyricsOverlay);
delegate_registry!(LyricsOverlay);

pub fn run_overlay(lyric_rx: watch::Receiver<String>) -> Result<()> {
    info!("Initializing Wayland connection");

    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    // Initialize states
    let compositor_state =
        CompositorState::bind(&globals, &qh).expect("wl_compositor not available");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr-layer-shell not available");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm not available");
    let output_state = OutputState::new(&globals, &qh);
    let registry_state = RegistryState::new(&globals);

    let mut overlay = LyricsOverlay::new(
        registry_state,
        compositor_state,
        output_state,
        shm,
        layer_shell,
        lyric_rx,
    );

    // Create the layer surface
    overlay.create_layer_surface(&qh);

    info!("Starting Wayland event loop");

    // Event loop
    while !overlay.exit {
        event_queue.blocking_dispatch(&mut overlay)?;
    }

    Ok(())
}
