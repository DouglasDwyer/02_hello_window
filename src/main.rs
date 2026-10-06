use std::sync::Arc;

use wgpu::SurfaceTexture;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle},
    window::{Window, WindowId},
};

/// Height/width of the fixed square, and height of the proportional bar.
const SQUARE_SIZE: u32 = 50;
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

struct State {
    instance: wgpu::Instance,
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    size: winit::dpi::PhysicalSize<u32>,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
    surface_texture: Option<SurfaceTexture>,
    /// Pre-filled texture (same format as the surface) that the test squares are copied from.
    /// Rows `0..SQUARE_SIZE` hold the fixed-size square's colour, the next `SQUARE_SIZE` rows
    /// hold the proportional bar's colour.
    square_texture: wgpu::Texture
}

impl State {
    async fn new(display: OwnedDisplayHandle, window: Arc<Window>) -> State {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backend_options: wgpu::BackendOptions {
                dx12: wgpu::Dx12BackendOptions {
                    // Important: smooth resizing does not work without this
                    presentation_system: wgpu::Dx12SwapchainKind::DxgiFromVisual,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..wgpu::InstanceDescriptor::new_with_display_handle(
                Box::new(display),
            )
        });
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
            square_texture
        };

        // Configure surface for the first time
        state.configure_surface();

        state
    }

    fn get_window(&self) -> &Window {
        &self.window
    }

    fn configure_surface(&mut self) {
        if self.surface_texture.is_some() {
            println!("dropit");
        }

        self.surface_texture = None;

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.surface_format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            // Request compatibility with the sRGB-format texture view we‘re going to create later.
            view_formats: vec![self.surface_format.add_srgb_suffix()],
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            width: self.size.width,
            height: self.size.height,
            desired_maximum_frame_latency: 2,
            present_mode: wgpu::PresentMode::AutoVsync,
        };
        self.surface.configure(&self.device, &surface_config);
    }

    fn resize(&mut self, new_size: winit::dpi::PhysicalSize<u32>) {
        self.size = new_size;

        // reconfigure the surface
        self.configure_surface();
    }

    fn render(&mut self) {
        if self.surface_texture.is_none() {
            self.update_surface_texture();
        }

        let Some(surface_texture) = std::mem::take(&mut self.surface_texture)
            else { return };

        let texture_view = surface_texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor {
                // Without add_srgb_suffix() the image we will be working with
                // might not be "gamma correct".
                format: Some(self.surface_format.add_srgb_suffix()),
                ..Default::default()
            });

        // Renders a GREEN screen
        let mut encoder = self.device.create_command_encoder(&Default::default());

        // Clear the screen with an empty render pass.
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &texture_view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::GREEN),
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
        let (width, height) = (surface_texture.texture.size().width, surface_texture.texture.size().height);
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

        // Submit the command in the queue to execute
        self.queue
            .submit([encoder.finish()]);

        self.window.pre_present_notify();

        self.queue.present(surface_texture);
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
                wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Suboptimal(_) => {
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
    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        let state = self.state.as_mut().unwrap();
        state.render();
        state.update_surface_texture();
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        use winit::platform::windows::*;
        // Create window object
        let window = Arc::new(
            event_loop
                .create_window(Window::default_attributes().with_drag_and_drop(false)
                    .with_no_redirection_bitmap(true))
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
        match event {
            WindowEvent::CloseRequested => {
                println!("The close button was pressed; stopping");
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                state.resize(size);
                state.render();
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
