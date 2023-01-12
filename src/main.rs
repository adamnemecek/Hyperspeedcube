//! A keyboard-controlled speedcube simulator.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![warn(clippy::if_then_some_else_none, missing_docs)]
#![allow(
    clippy::collapsible_match,
    clippy::match_like_matches_macro,
    clippy::single_match,
    clippy::useless_format,
    missing_docs, // TODO: remove
)]

#[macro_use]
extern crate ambassador;
#[macro_use]
extern crate enum_dispatch;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate strum;

use instant::{Duration, Instant};
use std::sync::Arc;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;
use winit::event::{ElementState, Event, KeyboardInput, WindowEvent};
use winit::event_loop::{ControlFlow, EventLoopBuilder};
#[cfg(target_arch = "wasm32")]
use winit::platform::web::WindowBuilderExtWebSys;
use winit::window::Icon;

#[macro_use]
mod debug;
mod app;
mod commands;
mod gui;
mod logfile;
mod preferences;
pub mod puzzle;
mod render;
mod serde_impl;
mod util;
#[cfg(target_arch = "wasm32")]
mod web_workarounds;

use app::App;

const TITLE: &str = "Hyperspeedcube";
const ICON_32: &[u8] = include_bytes!("../resources/icon/hyperspeedcube_32x32.png");

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    // Initialize logging.
    env_logger::builder()
        .filter_module(
            "hyperspeedcube",
            if cfg!(debug_assertions) {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Warn
            },
        )
        .init();

    let human_panic_metadata = human_panic::Metadata {
        name: TITLE.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        authors: env!("CARGO_PKG_AUTHORS").into(),
        homepage: env!("CARGO_PKG_REPOSITORY").into(),
    };

    let std_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let file_path = human_panic::handle_dump(&human_panic_metadata, info);
        human_panic::print_msg(file_path.as_ref(), &human_panic_metadata)
            .expect("human-panic: printing error message to console failed");

        rfd::MessageDialog::new()
            .set_title(&format!("{TITLE} crashed"))
            .set_description(&match file_path {
                Some(fp) => format!(
                    "A crash report has been saved to \"{}\"\n\n\
                     Please submit this to the developer",
                    fp.display(),
                ),
                None => format!("Error saving crash report"),
            })
            .set_level(rfd::MessageLevel::Error)
            .show();

        std_panic_hook(info);
    }));

    pollster::block_on(run());
}

#[cfg(target_arch = "wasm32")]
fn main() {
    // Initialize logging.
    wasm_logger::init(wasm_logger::Config::default());

    // Log panics using `console.error`.
    console_error_panic_hook::set_once();

    // Redirect tracing to console.log and friends:
    tracing_wasm::set_as_global_default();

    wasm_bindgen_futures::spawn_local(run());
}

async fn run() {
    // Initialize window.
    let event_loop = EventLoopBuilder::with_user_event().build();
    #[cfg(not(target_arch = "wasm32"))]
    let window_builder = winit::window::WindowBuilder::new()
        .with_title(crate::TITLE)
        .with_window_icon(load_application_icon());
    #[cfg(target_arch = "wasm32")]
    let window_builder =
        winit::window::WindowBuilder::new().with_canvas(Some(find_canvas_element()));
    let window = window_builder
        .build(&event_loop)
        .expect("failed to initialize window");

    // Initialize graphics state.
    let mut gfx = render::GraphicsState::new(&window).await;
    let mut last_fps = 0;
    let mut frames_this_second = 0;
    let mut last_second = Instant::now();

    // Initialize egui.
    let egui_ctx = egui::Context::default();
    let mut egui_winit_state = egui_winit::State::new(&event_loop);
    match dark_light::detect() {
        dark_light::Mode::Light => switch_to_light_mode(&egui_ctx),
        dark_light::Mode::Dark => switch_to_dark_mode(&egui_ctx),
        dark_light::Mode::Default => switch_to_dark_mode(&egui_ctx),
    };
    let mut egui_renderer = egui_wgpu::Renderer::new(&gfx.device, gfx.config.format, None, 1);
    let puzzle_texture_id = egui_renderer.register_native_texture(
        &gfx.device,
        &gfx.dummy_texture_view(),
        wgpu::FilterMode::Linear,
    );

    let initial_file = std::env::args()
        .skip(1)
        .next()
        .map(|s| std::path::PathBuf::from(s));

    // Initialize app state.
    let mut app = App::new(&event_loop, initial_file);

    if app.prefs.show_welcome_at_startup {
        gui::windows::WELCOME.set_open(&egui_ctx, true);
    }

    #[cfg(target_arch = "wasm32")]
    let mut web_workarounds = web_workarounds::WebWorkarounds::new(&event_loop);

    // Begin main loop.
    let mut next_frame_time = Instant::now();
    event_loop.run(move |ev, _ev_loop, control_flow| {
        let mut event_has_been_captured = false;

        #[cfg(target_arch = "wasm32")]
        let ev = {
            web_workarounds.generate_resize_event(&window);

            if let Event::UserEvent(app::AppEvent::WebWorkaround(
                web_workarounds::Event::EmulateWindowEvent(e),
            )) = ev
            {
                Event::WindowEvent {
                    window_id: window.id(),
                    event: e,
                }
            } else {
                ev
            }
        };

        // Key release events should always be processed by the app to make sure
        // there's no stuck keys.
        let allow_egui_capture = match &ev {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::KeyboardInput {
                    input:
                        KeyboardInput {
                            state: ElementState::Released,
                            ..
                        },
                    ..
                } => false,

                WindowEvent::ModifiersChanged(_) => false,

                _ => true,
            },

            _ => true,
        };

        // Prioritize sending events to the key combo popup.
        match &ev {
            Event::WindowEvent { window_id, event } if *window_id == window.id() => {
                gui::key_combo_popup_handle_event(&egui_ctx, &mut app, event);
                event_has_been_captured |= gui::key_combo_popup_captures_event(&egui_ctx, event);
            }
            _ => (),
        }

        // Handle events for the app.
        match ev {
            // Handle window events.
            Event::WindowEvent { window_id, event } if window_id == window.id() => {
                // If the key combo popup didn't capture the event, then let
                // egui handle it before anything else.
                if !event_has_been_captured {
                    let r = egui_winit_state.on_event(&egui_ctx, &event);
                    event_has_been_captured |= r.consumed && allow_egui_capture;
                    if r.repaint {
                        egui_ctx.request_repaint();
                    }
                }

                match &event {
                    WindowEvent::Resized(new_size) => gfx.resize(*new_size),
                    WindowEvent::ScaleFactorChanged {
                        scale_factor,
                        new_inner_size,
                    } => {
                        gfx.set_scale_factor(*scale_factor as f32);
                        gfx.resize(**new_inner_size);
                    }
                    WindowEvent::ThemeChanged(theme) => match theme {
                        winit::window::Theme::Light => switch_to_light_mode(&egui_ctx),
                        winit::window::Theme::Dark => switch_to_dark_mode(&egui_ctx),
                    },
                    _ => {
                        if !event_has_been_captured {
                            app.handle_window_event(&event);
                        }

                        if matches!(
                            &event,
                            WindowEvent::KeyboardInput { .. }
                                | WindowEvent::ModifiersChanged { .. }
                        ) {
                            egui_ctx.request_repaint();
                        }
                    }
                }
            }

            // Handle application-specific events.
            Event::UserEvent(event) => app.handle_app_event(event, control_flow),

            Event::MainEventsCleared => {
                // RedrawRequested will only trigger once unless we manually
                // request it.
                window.request_redraw();
            }

            Event::RedrawRequested(window_id) if window_id == window.id() => {
                let now = Instant::now();

                // Update scale factor.
                egui_winit_state.set_pixels_per_point(gfx.scale_factor);

                // Start egui frame.
                let egui_input = egui_winit_state.take_egui_input(&window);

                let egui_output = egui_ctx.run(egui_input, |ctx| {
                    // Build all the UI except the puzzle view in the center.
                    gui::build(ctx, &mut app, puzzle_texture_id);
                });

                egui_winit_state.handle_platform_output(
                    &window,
                    &egui_ctx,
                    egui_output.platform_output,
                );

                if app.prefs.needs_save {
                    app.prefs.save();
                }

                // Draw puzzle if necessary.
                if let Some(puzzle_texture) = app.draw_puzzle(&mut gfx) {
                    log::trace!("Repainting puzzle");

                    // Update texture for egui.
                    egui_renderer.update_egui_texture_from_wgpu_texture(
                        &gfx.device,
                        &puzzle_texture,
                        wgpu::FilterMode::Linear,
                        puzzle_texture_id,
                    );

                    // Request a repaint.
                    egui_ctx.request_repaint();
                }

                if egui_output.repaint_after.is_zero() && next_frame_time <= now {
                    let frame_duration = app.prefs.gfx.frame_duration();
                    next_frame_time += frame_duration;
                    if next_frame_time < Instant::now() {
                        // Skip a frame (or several).
                        next_frame_time = now + frame_duration;
                    }
                    // Update app state.
                    app.frame();

                    let output_frame = match gfx.surface.get_current_texture() {
                        Ok(tex) => tex,
                        // Log other errors to the console.
                        Err(e) => {
                            match e {
                                // This error occurs when the app is minimized on
                                // Windows. Silently return here to prevent spamming
                                // the console with "The underlying surface has
                                // changed, and therefore the swap chain must be
                                // updated."
                                wgpu::SurfaceError::Outdated => (),
                                // Reconfigure the surface if lost.
                                wgpu::SurfaceError::Lost => gfx.resize(gfx.size),
                                // The system is out of memory, so quit.
                                wgpu::SurfaceError::OutOfMemory => {
                                    *control_flow = ControlFlow::Exit
                                }
                                // Log other errors.
                                _ => log::warn!("Dropped frame with error: {:?}", e),
                            }
                            return;
                        }
                    };

                    let paint_jobs = egui_ctx.tessellate(egui_output.shapes);
                    let mut encoder =
                        gfx.device
                            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                                label: Some("egui_command_encoder"),
                            });
                    let screen_descriptor = egui_wgpu::renderer::ScreenDescriptor {
                        size_in_pixels: [gfx.config.width, gfx.config.height],
                        pixels_per_point: gfx.scale_factor,
                    };

                    for (id, image_delta) in &egui_output.textures_delta.set {
                        egui_renderer.update_texture(&gfx.device, &gfx.queue, *id, image_delta);
                    }
                    egui_renderer.update_buffers(
                        &gfx.device,
                        &gfx.queue,
                        &mut encoder,
                        &paint_jobs,
                        &screen_descriptor,
                    );

                    // Record egui render passes.
                    {
                        let texture_view = output_frame
                            .texture
                            .create_view(&wgpu::TextureViewDescriptor::default());

                        let mut egui_render_pass =
                            encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                                label: None,
                                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                    view: &texture_view,
                                    resolve_target: None,
                                    ops: wgpu::Operations {
                                        load: wgpu::LoadOp::Clear(wgpu::Color::WHITE),
                                        store: true,
                                    },
                                })],
                                depth_stencil_attachment: None,
                            });

                        egui_renderer.render(
                            &mut egui_render_pass,
                            &paint_jobs,
                            &screen_descriptor,
                        );
                    }

                    // Free unneeded textures.
                    for id in &egui_output.textures_delta.free {
                        egui_renderer.free_texture(id);
                    }

                    // Submit the commands.
                    gfx.queue.submit(std::iter::once(encoder.finish()));

                    // Present the frame.
                    output_frame.present();

                    // Update framerate.
                    frames_this_second += 1;
                    if (Instant::now() - last_second).as_secs() >= 1 {
                        last_fps = frames_this_second;
                        frames_this_second = 0;
                        last_second += Duration::from_secs(1);
                    }
                    // TODO: display framerate somewhere
                    printlnd!("FPS: {}", last_fps);
                }
            }

            // Ignore other events.
            _ => (),
        };
    });
}

fn load_application_icon() -> Option<Icon> {
    match png::Decoder::new(crate::ICON_32).read_info() {
        Ok(mut reader) => match reader.output_color_type() {
            (png::ColorType::Rgba, png::BitDepth::Eight) => {
                let mut img_data = vec![0_u8; reader.output_buffer_size()];
                if let Err(err) = reader.next_frame(&mut img_data) {
                    log::warn!("Failed to read icon data: {:?}", err);
                    return None;
                };
                let info = reader.info();
                match Icon::from_rgba(img_data, info.width, info.height) {
                    Ok(icon) => Some(icon),
                    Err(err) => {
                        log::warn!("Failed to construct icon: {:?}", err);
                        None
                    }
                }
            }
            other => {
                log::warn!(
                    "Failed to load icon data due to unknown color format: {:?}",
                    other,
                );
                None
            }
        },
        Err(err) => {
            log::warn!("Failed to load icon data: {:?}", err);
            None
        }
    }
}

fn switch_to_dark_mode(ctx: &egui::Context) {
    ctx.set_style(egui::Style {
        visuals: egui::Visuals::dark(),
        ..Default::default()
    });
    set_style_overrides(ctx);
}
fn switch_to_light_mode(ctx: &egui::Context) {
    ctx.set_style(egui::Style {
        visuals: egui::Visuals::dark(),
        ..Default::default()
    });
    set_style_overrides(ctx);
}
fn set_style_overrides(ctx: &egui::Context) {
    let mut style = ctx.style();
    let style_mut = Arc::make_mut(&mut style);
    style_mut.visuals.widgets.noninteractive.bg_stroke.width *= 2.0;
    style_mut.visuals.widgets.inactive.bg_stroke.width *= 2.0;
    style_mut.visuals.widgets.hovered.bg_stroke.width *= 2.0;
    style_mut.visuals.widgets.active.bg_stroke.width *= 2.0;
    style_mut.visuals.widgets.open.bg_stroke.width *= 2.0;
    // style_mut.visuals.widgets.hovered.bg_stroke.width *= 2.0;
    style_mut.spacing.interact_size.x *= 1.2;
    ctx.set_style(style);
}

#[cfg(target_arch = "wasm32")]
fn find_canvas_element() -> web_sys::HtmlCanvasElement {
    let document = web_sys::window().unwrap().document().unwrap();
    let canvas = document.get_element_by_id("hyperspeedcube_canvas").unwrap();
    canvas
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .map_err(|_| ())
        .expect("failed to find canvas for Hyperspeedcube")
}
