use std::sync::Arc;

use wgpu::SurfaceTexture;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle},
    window::{Window, WindowId},
};

/// Everything produced by [`State::run_egui`] that is needed to draw and clean up the GUI.
struct EguiFrame {
    paint_jobs: Vec<egui::ClippedPrimitive>,
    screen_descriptor: egui_wgpu::ScreenDescriptor,
    user_cmd_bufs: Vec<wgpu::CommandBuffer>,
    textures_to_free: Vec<egui::TextureId>,
}

struct State {
    instance: wgpu::Instance,
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    size: winit::dpi::PhysicalSize<u32>,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
    surface_texture: Option<SurfaceTexture>,
    /// Present mode currently configured on the surface. Immediate while the window is being
    /// resized (see [`State::resize`]), vsync otherwise.
    present_mode: wgpu::PresentMode,
    /// When the last resize happened; `Some` while `present_mode` is the resize-time one.
    last_resize: Option<std::time::Instant>,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    clear_color: [f32; 3],
    counter: u32,
    /// Pre-filled texture (same format as the surface) that the test squares are copied from.
    /// Rows `0..SQUARE_SIZE` hold the fixed-size square's colour, the next `SQUARE_SIZE` rows
    /// hold the proportional bar's colour.
    square_texture: wgpu::Texture,
}

/// Height/width of the fixed square, and height of the proportional bar.
const SQUARE_SIZE: u32 = 50;
/// Master switch for the egui GUI. Disabled while debugging resize stretching with the squares.
const ENABLE_EGUI: bool = false;
/// Present mode for normal rendering.
const NORMAL_PRESENT_MODE: wgpu::PresentMode = wgpu::PresentMode::AutoVsync;
/// Present mode while resizing: frames must be available to DWM immediately (see `State::resize`).
const RESIZE_PRESENT_MODE: wgpu::PresentMode = wgpu::PresentMode::AutoNoVsync;
/// How long after the last `Resized` event we keep the resize present mode.
const RESIZE_SETTLE_TIME: std::time::Duration = std::time::Duration::from_millis(150);
/// Top-left corner (x, y), in pixels, of the fixed square and of the proportional bar.
const SQUARE_POS: [u32; 2] = [400, 200];
const BAR_POS: [u32; 2] = [100, 300];
/// Width of the source texture; caps the width of the proportional bar.
const SQUARE_TEXTURE_WIDTH: u32 = 8192;

/// Creates a source texture for the test squares and fills it with solid colours.
fn create_square_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    let extent = wgpu::Extent3d {
        width: SQUARE_TEXTURE_WIDTH.min(device.limits().max_texture_dimension_2d),
        height: SQUARE_SIZE * 2,
        depth_or_array_layers: 1,
    };
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("square_texture"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });

    // Raw bytes are copied, so the channel order has to match the surface format.
    let pixel = |[r, g, b]: [u8; 3]| match format.remove_srgb_suffix() {
        wgpu::TextureFormat::Bgra8Unorm => [b, g, r, 255],
        wgpu::TextureFormat::Rgba8Unorm => [r, g, b, 255],
        other => panic!("unsupported surface format {other:?}"),
    };
    let mut data = Vec::new();
    for (row, color) in [(0..SQUARE_SIZE, [255, 0, 0]), (SQUARE_SIZE..SQUARE_SIZE * 2, [0, 0, 255])] {
        for _ in row {
            for _ in 0..extent.width {
                data.extend_from_slice(&pixel(color));
            }
        }
    }
    queue.write_texture(
        texture.as_image_copy(),
        &data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(extent.width * 4),
            rows_per_image: Some(extent.height),
        },
        extent,
    );
    texture
}

impl State {
    async fn new(display: OwnedDisplayHandle, window: Arc<Window>) -> State {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            flags: wgpu::InstanceFlags::advanced_debugging(),
            backend_options: wgpu::BackendOptions {
                dx12: wgpu::Dx12BackendOptions {
                    latency_waitable_object: wgpu::Dx12UseFrameLatencyWaitableObject::Wait,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(display),
        )});
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .await
            .unwrap();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default())
            .await
            .unwrap();

        let size = window.inner_size();

        let surface = instance.create_surface(window.clone()).unwrap();
        let cap = surface.get_capabilities(&adapter);
        let surface_format = cap.formats[0];

        // egui expects to render into a gamma-space (non-sRGB) target.
        let egui_renderer = egui_wgpu::Renderer::new(
            &device,
            surface_format.remove_srgb_suffix(),
            egui_wgpu::RendererOptions::default(),
        );
        let egui_ctx = egui::Context::default();
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            window.as_ref(),
            Some(window.scale_factor() as f32),
            window.theme(),
            Some(device.limits().max_texture_dimension_2d as usize),
        );

        let square_texture = create_square_texture(&device, &queue, surface_format);

        let mut state = State {
            instance,
            window,
            device,
            queue,
            size,
            surface,
            surface_format,
            surface_texture: None,
            present_mode: NORMAL_PRESENT_MODE,
            last_resize: None,
            egui_ctx,
            egui_state,
            egui_renderer,
            clear_color: [0.0, 1.0, 0.0],
            counter: 0,
            square_texture,
        };

        // Configure surface for the first time
        state.configure_surface();

        state
    }

    fn get_window(&self) -> &Window {
        &self.window
    }

    fn configure_surface(&mut self) {
        // The texture held here was acquired at the end of the previous frame and never drawn to.
        // It must be released before reconfiguring, but it must NOT be presented: that would put
        // an uninitialized (black) frame on screen on every resize step. Dropping discards it.
        self.surface_texture = None;

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST,
            format: self.surface_format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            // Request compatibility with the non-sRGB texture view we're going to create later.
            view_formats: vec![self.surface_format.remove_srgb_suffix()],
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            width: self.size.width,
            height: self.size.height,
            desired_maximum_frame_latency: 2,
            present_mode: self.present_mode,
        };
        self.surface.configure(&self.device, &surface_config);
    }

    /// Resizes the surface to match the window.
    ///
    /// Windows calls `WM_SIZE` (-> `Resized`) from inside the `SetWindowPos` that resizes the
    /// window, and DWM recomposites the window at its new size right away using whatever buffer
    /// was last presented. With `DXGI_SCALING_STRETCH` that is the old frame stretched. So the new
    /// frame has to be presented from within the `Resized` handler, and it has to be visible to
    /// DWM immediately; a vsync-paced present is queued for the next vsync and loses the race.
    /// Hence the immediate present mode while resizing (switched back in `end_resize_if_settled`).
    fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        self.size = new_size;
        self.present_mode = RESIZE_PRESENT_MODE;
        self.last_resize = Some(std::time::Instant::now());

        // reconfigure the surface
        self.configure_surface();
    }

    /// Returns to the normal (vsync) present mode once resizing has stopped.
    fn end_resize_if_settled(&mut self) {
        if self
            .last_resize
            .is_some_and(|t| t.elapsed() >= RESIZE_SETTLE_TIME)
        {
            self.last_resize = None;
            self.present_mode = NORMAL_PRESENT_MODE;
            self.configure_surface();
        }
    }

    fn render(&mut self) {
        // egui lays out using the window's live `inner_size()`, which can already be ahead of the
        // last `Resized` event we received (self.size). Bring the surface up to date first so the
        // GUI layout, the render target and the projection all use the same size.
        let inner = self.window.inner_size();
        if inner != self.size && inner.width > 0 && inner.height > 0 {
            self.resize(inner);
        }

        if self.surface_texture.is_none() {
            self.update_surface_texture();
        }

        let Some(surface_texture) = std::mem::take(&mut self.surface_texture)
            else { return };

        let texture_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor {
                // egui wants to render in gamma space, so view the surface
                // without the sRGB suffix.
                format: Some(self.surface_format.remove_srgb_suffix()),
                ..Default::default()
            });

        let tex_size = surface_texture.texture.size();
        let mut encoder = self.device.create_command_encoder(&Default::default());

        // Run the GUI for this frame and upload its buffers.
        let mut egui_frame = ENABLE_EGUI.then(|| self.run_egui(&surface_texture, &mut encoder));

        // Clear the screen with an empty render pass.
        let [r, g, b] = self.clear_color.map(f64::from);
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &texture_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        }));

        // Draw the test squares with texture copies (no shaders), at fixed offsets from the
        // top-left corner: a 50x50 square at (SQUARE_POS), and a 20%-of-width x 50 bar at
        // (BAR_POS). If the OS rescales a stale frame during a resize, they visibly move/stretch.
        // Both are clamped to the surface size.
        let (width, height) = (tex_size.width, tex_size.height);
        let bar_w = (width / 5).min(self.square_texture.width());
        for (src_y, [dst_x, dst_y], w) in [
            (0, SQUARE_POS, SQUARE_SIZE),
            (SQUARE_SIZE, BAR_POS, bar_w),
        ] {
            let w = w.min(width.saturating_sub(dst_x));
            let h = SQUARE_SIZE.min(height.saturating_sub(dst_y));
            if w == 0 || h == 0 {
                continue;
            }
            encoder.copy_texture_to_texture(
                wgpu::TexelCopyTextureInfo {
                    origin: wgpu::Origin3d { x: 0, y: src_y, z: 0 },
                    ..self.square_texture.as_image_copy()
                },
                wgpu::TexelCopyTextureInfo {
                    origin: wgpu::Origin3d { x: dst_x, y: dst_y, z: 0 },
                    ..surface_texture.texture.as_image_copy()
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }

        // Draw the GUI on top, keeping what's already in the target.
        let mut user_cmd_bufs = Vec::new();
        if let Some(egui_frame) = &mut egui_frame {
            user_cmd_bufs = std::mem::take(&mut egui_frame.user_cmd_bufs);
            let mut renderpass = encoder
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: None,
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &texture_view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Load,
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                })
                .forget_lifetime();

            self.egui_renderer.render(
                &mut renderpass,
                &egui_frame.paint_jobs,
                &egui_frame.screen_descriptor,
            );
        }

        // Submit the command in the queue to execute
        self.queue
            .submit(user_cmd_bufs.into_iter().chain([encoder.finish()]));

        if let Some(egui_frame) = egui_frame {
            for id in egui_frame.textures_to_free {
                self.egui_renderer.free_texture(&id);
            }
        }
        self.window.pre_present_notify();

        self.queue.present(surface_texture);
    }

    /// Runs the egui UI for one frame, uploads its textures/buffers, and returns what is needed
    /// to draw it.
    fn run_egui(
        &mut self,
        surface_texture: &SurfaceTexture,
        encoder: &mut wgpu::CommandEncoder,
    ) -> EguiFrame {
        let mut raw_input = self.egui_state.take_egui_input(&self.window);
        // `take_egui_input` reads the window's *live* size, which can be newer than the texture we
        // acquired. Lay egui out for exactly the size of the texture we are about to draw into.
        let tex_size = surface_texture.texture.size();
        let ppp = egui_winit::pixels_per_point(&self.egui_ctx, &self.window);
        raw_input.screen_rect = Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(tex_size.width as f32, tex_size.height as f32) / ppp,
        ));
        let mut full_output = self.egui_ctx.run_ui(raw_input, |ui| {
            egui::Window::new("Hello egui").show(ui.ctx(), |ui| {
                ui.label("Hello from egui-wgpu!");
                ui.horizontal(|ui| {
                    ui.label("Background:");
                    ui.color_edit_button_rgb(&mut self.clear_color);
                });
                if ui.button("Click me").clicked() {
                    self.counter += 1;
                }
                ui.label(format!("Clicked {} times", self.counter));
            });
        });
        self.egui_state
            .handle_platform_output(&self.window, full_output.platform_output);
        let paint_jobs = self
            .egui_ctx
            .tessellate(full_output.shapes, full_output.pixels_per_point);
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [tex_size.width, tex_size.height],
            pixels_per_point: full_output.pixels_per_point,
        };

        for (id, image_deltas) in full_output.textures_delta.set.drain() {
            for image_delta in image_deltas {
                self.egui_renderer
                    .update_texture(&self.device, &self.queue, id, &image_delta);
            }
        }

        let user_cmd_bufs = self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            encoder,
            &paint_jobs,
            &screen_descriptor,
        );

        EguiFrame {
            paint_jobs,
            screen_descriptor,
            user_cmd_bufs,
            textures_to_free: full_output.textures_delta.free.drain().collect(),
        }
    }

    fn update_surface_texture(&mut self) {
        loop {
            assert!(self.surface_texture.is_none());

            let startt = std::time::Instant::now();
            let next_tex = self.surface.get_current_texture();
            let endd = startt.elapsed();
            if endd.as_millis() > 100 {
                println!("LAAGGGG {endd:?} {}", std::backtrace::Backtrace::capture());
            }

            match next_tex {
                wgpu::CurrentSurfaceTexture::Success(texture) => {
                    self.surface_texture = Some(texture);
                    break;
                },
                wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => return,
                wgpu::CurrentSurfaceTexture::Outdated => {
                    self.configure_surface();
                }
                wgpu::CurrentSurfaceTexture::Suboptimal(tex) => {
                    panic!("here");
                    self.configure_surface();
                }
                wgpu::CurrentSurfaceTexture::Validation => {
                    unreachable!("No error scope registered, so validation errors will panic")
                }
                wgpu::CurrentSurfaceTexture::Lost => {
                    self.surface = self.instance.create_surface(self.window.clone()).unwrap();
                    self.configure_surface();
                }
            }
        }
    }
}

#[derive(Default)]
struct App {
    state: Option<State>,
}

impl ApplicationHandler for App {
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let state = self.state.as_mut().unwrap();
        state.end_resize_if_settled();
        state.render();
        state.update_surface_texture();
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        use winit::platform::windows::*;
        // Create window object
        let window = Arc::new(
            event_loop
                .create_window(Window::default_attributes().with_drag_and_drop(false)
                    .with_no_redirection_bitmap(false))
                .unwrap(),
        );

        let state = pollster::block_on(State::new(
            event_loop.owned_display_handle(),
            window.clone(),
        ));
        self.state = Some(state);

        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let state = self.state.as_mut().unwrap();

        // Let egui see the event first. We still handle close/resize/redraw
        // ourselves regardless of whether egui consumed it.
        if ENABLE_EGUI {
            let response = state.egui_state.on_window_event(&state.window, &event);
            if response.repaint {
                state.window.request_redraw();
            }
        }

        match event {
            WindowEvent::CloseRequested => {
                println!("The close button was pressed; stopping");
                event_loop.exit();
            }
            //WindowEvent::RedrawRequested => {
            //    state.render();
            //}
            WindowEvent::Resized(size) => {
                // Reconfigure the surface and render + present a frame at the new size right
                // here, before returning to the OS (see `State::resize` for why).
                state.resize(size);
                if size.width > 0 && size.height > 0 {
                    state.render();
                }
            }
            _ => (),
        }
    }
}

fn main() {
    // wgpu uses `log` for all of our logging, so we initialize a logger with the `env_logger` crate.
    //
    // To change the log level, set the `RUST_LOG` environment variable. See the `env_logger`
    // documentation for more information.
    env_logger::init();

    let event_loop = EventLoop::new().unwrap();

    // When the current loop iteration finishes, immediately begin a new
    // iteration regardless of whether or not new events are available to
    // process. Preferred for applications that want to render as fast as
    // possible, like games.
    event_loop.set_control_flow(ControlFlow::Poll);

    // When the current loop iteration finishes, suspend the thread until
    // another event arrives. Helps keeping CPU utilization low if nothing
    // is happening, which is preferred if the application might be idling in
    // the background.
    // event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App::default();
    event_loop.run_app(&mut app).unwrap();
}
