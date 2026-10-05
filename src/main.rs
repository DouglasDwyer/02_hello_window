use std::sync::Arc;

use wgpu::SurfaceTexture;
use winit::{
    application::ApplicationHandler,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle},
    window::{Window, WindowId},
};

struct State {
    instance: wgpu::Instance,
    window: Arc<Window>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    size: winit::dpi::PhysicalSize<u32>,
    surface: wgpu::Surface<'static>,
    surface_format: wgpu::TextureFormat,
    surface_texture: Option<SurfaceTexture>,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    clear_color: [f32; 3],
    counter: u32,
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

        let mut state = State {
            instance,
            window,
            device,
            queue,
            size,
            surface,
            surface_format,
            surface_texture: None,
            egui_ctx,
            egui_state,
            egui_renderer,
            clear_color: [0.0, 1.0, 0.0],
            counter: 0,
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
            self.queue.present(std::mem::take(&mut self.surface_texture).unwrap());
            println!("dropit");
        }

        self.surface_texture = None;

        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: self.surface_format,
            color_space: wgpu::SurfaceColorSpace::Auto,
            // Request compatibility with the non-sRGB texture view we're going to create later.
            view_formats: vec![self.surface_format.remove_srgb_suffix()],
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
                // egui wants to render in gamma space, so view the surface
                // without the sRGB suffix.
                format: Some(self.surface_format.remove_srgb_suffix()),
                ..Default::default()
            });

        // Run the GUI for this frame.
        let raw_input = self.egui_state.take_egui_input(&self.window);
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
            size_in_pixels: [self.size.width, self.size.height],
            pixels_per_point: full_output.pixels_per_point,
        };

        for (id, image_deltas) in full_output.textures_delta.set.drain() {
            for image_delta in image_deltas {
                self.egui_renderer
                    .update_texture(&self.device, &self.queue, id, &image_delta);
            }
        }

        let mut encoder = self.device.create_command_encoder(&Default::default());
        let user_cmd_bufs = self.egui_renderer.update_buffers(
            &self.device,
            &self.queue,
            &mut encoder,
            &paint_jobs,
            &screen_descriptor,
        );

        // Create the renderpass which will clear the screen and draw the GUI.
        let [r, g, b] = self.clear_color.map(f64::from);
        let mut renderpass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
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
            })
            .forget_lifetime();

        self.egui_renderer
            .render(&mut renderpass, &paint_jobs, &screen_descriptor);

        // End the renderpass.
        drop(renderpass);

        // Submit the command in the queue to execute
        self.queue
            .submit(user_cmd_bufs.into_iter().chain([encoder.finish()]));

        for id in full_output.textures_delta.free.drain() {
            self.egui_renderer.free_texture(&id);
        }
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

        // Let egui see the event first. We still handle close/resize/redraw
        // ourselves regardless of whether egui consumed it.
        let response = state.egui_state.on_window_event(&state.window, &event);
        if response.repaint {
            state.window.request_redraw();
        }

        match event {
            WindowEvent::CloseRequested => {
                println!("The close button was pressed; stopping");
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => {
                state.render();
            }
            WindowEvent::Resized(size) => {
                // Reconfigures the size of the surface. We do not re-render
                // here as this event is always followed up by redraw request.
                state.resize(size);
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
