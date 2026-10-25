#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod config;
mod controller;
mod dpapi;
#[cfg(test)]
mod gpu_tests;
mod http;
mod logging;
mod native;
mod psn;
mod render;
mod session;
mod tray;
mod ui;
mod updater;
mod upscale;

use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW,
};
use windows::core::{HSTRING, w};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Fullscreen, Icon, Window, WindowId};

use crate::audio::AudioOutput;
use crate::config::{AppConfig, Paths};
use crate::render::Renderer;
use crate::session::StreamController;
use crate::tray::{Tray, TrayAction};
use crate::ui::{App, VideoView, WindowRequest};

const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");
// Caps the redraw loop when Present does not block (e.g. occluded window).
const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(4);

#[derive(Debug, Clone, Copy)]
enum UserEvent {
    Wake,
    Tray(TrayAction),
}

struct Runner {
    proxy: Arc<Mutex<EventLoopProxy<UserEvent>>>,
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    egui_ctx: egui::Context,
    egui_winit: Option<egui_winit::State>,
    app: App,
    tray: Option<Tray>,
    icon: (Vec<u8>, u32),
    next_repaint: Option<Instant>,
    minimized: bool,
    hidden_to_tray: bool,
    last_session: usize,
    last_redraw: Instant,
}

fn decode_icon() -> (Vec<u8>, u32) {
    match image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let size = rgba.width();
            (rgba.into_raw(), size)
        }
        Err(_) => ([0x2f, 0x7d, 0xf6, 0xff].repeat(32 * 32), 32),
    }
}

impl Runner {
    fn window_hwnd(window: &Window) -> Option<HWND> {
        match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Win32(handle) => Some(HWND(handle.hwnd.get() as _)),
            _ => None,
        }
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let (w, h) = self.app.config.ui.window_size.unwrap_or((1280, 760));
        let icon = Icon::from_rgba(self.icon.0.clone(), self.icon.1, self.icon.1).ok();
        let attributes = Window::default_attributes()
            .with_title("PS5 LAN Capture")
            .with_inner_size(PhysicalSize::new(w.max(640), h.max(400)))
            .with_min_inner_size(LogicalSize::new(640.0, 420.0))
            .with_window_icon(icon);
        let window = Arc::new(
            event_loop
                .create_window(attributes)
                .map_err(|e| e.to_string())?,
        );
        let hwnd = Self::window_hwnd(&window).ok_or("No window handle")?;
        let size = window.inner_size();
        let renderer = Renderer::new(hwnd, size.width, size.height)?;
        let egui_winit = egui_winit::State::new(
            self.egui_ctx.clone(),
            self.egui_ctx.viewport_id(),
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        self.renderer = Some(renderer);
        self.egui_winit = Some(egui_winit);
        self.window = Some(window);
        Ok(())
    }

    fn ensure_tray(&mut self) {
        if self.tray.is_some() {
            return;
        }
        let proxy = self.proxy.clone();
        match Tray::new(self.icon.0.clone(), self.icon.1, move |action| {
            let _ = proxy.lock().send_event(UserEvent::Tray(action));
        }) {
            Ok(tray) => self.tray = Some(tray),
            Err(e) => log::warn!("Cannot create tray icon: {e}"),
        }
    }

    fn show_window(&mut self) {
        if let Some(window) = &self.window {
            window.set_visible(true);
            window.set_minimized(false);
            window.focus_window();
            window.request_redraw();
        }
        self.hidden_to_tray = false;
        self.minimized = false;
    }

    fn handle_requests(&mut self) -> bool {
        let requests = std::mem::take(&mut self.app.requests);
        let Some(window) = self.window.clone() else {
            return false;
        };
        let mut quit = false;
        for request in requests {
            match request {
                WindowRequest::ToggleFullscreen => {
                    let fullscreen = window.fullscreen().is_none();
                    window.set_fullscreen(fullscreen.then_some(Fullscreen::Borderless(None)));
                    self.app.fullscreen = fullscreen;
                }
                WindowRequest::ExitFullscreen => {
                    window.set_fullscreen(None);
                    self.app.fullscreen = false;
                }
                WindowRequest::Quit => quit = true,
            }
        }
        quit
    }

    fn quit(&mut self, event_loop: &ActiveEventLoop) {
        log::info!("Shutting down");
        self.app.shutdown();
        event_loop.exit();
    }

    /// Returns true when the UI asked the app to quit.
    fn redraw(&mut self) -> bool {
        let (Some(window), Some(renderer), Some(egui_winit)) = (
            self.window.clone(),
            self.renderer.as_mut(),
            self.egui_winit.as_mut(),
        ) else {
            return false;
        };
        self.last_redraw = Instant::now();
        let session_id = self.app.stream.session().map(|s| s.id()).unwrap_or(0);
        if session_id != self.last_session {
            if session_id != 0 {
                renderer.drop_video();
            }
            self.last_session = session_id;
        }
        match self.app.stream.session() {
            Some(session) => {
                renderer.update_video(
                    session,
                    render::Enhance {
                        better_chroma: self.app.config.video.better_chroma,
                        deband: self.app.config.video.deband,
                    },
                );
            }
            None if !self.app.stream.state.is_active() => renderer.drop_video(),
            None => {}
        }
        let target = self
            .egui_ctx
            .data(|d| d.get_temp::<[u32; 2]>(egui::Id::new(ui::widgets::VIDEO_TARGET_ID)));
        let settings = &self.app.config.video;
        let video = renderer
            .video_output(settings.upscaling, settings.sharpness, target)
            .map(|v| VideoView {
                texture_id: v.texture_id,
                size: v.size,
                source: v.source,
                scaler: v.scaler,
                hdr: v.hdr,
            });
        self.app.rtx_possible = renderer.rtx_possible();

        let input = egui_winit.take_egui_input(&window);
        let app = &mut self.app;
        let output = self.egui_ctx.run_ui(input, |ui| app.ui(ui, video.as_ref()));
        let repaint_delay = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .unwrap_or(Duration::MAX);
        let (renderer_output, platform_output, _) = egui_directx11::split_output(output);
        egui_winit.handle_platform_output(&window, platform_output);
        if let Err(e) = renderer.render(&self.egui_ctx, renderer_output) {
            log::error!("{e}");
            self.app.stream.disconnect();
            let size = window.inner_size();
            let rebuilt = Self::window_hwnd(&window)
                .ok_or_else(|| "No window handle".to_string())
                .and_then(|hwnd| Renderer::new(hwnd, size.width, size.height));
            match rebuilt {
                Ok(new_renderer) => {
                    *renderer = new_renderer;
                    self.app.notify(
                        "The GPU driver was reset and graphics were recreated. Connect again.",
                    );
                }
                Err(e) => self.app.gpu_error = Some(format!("Cannot recreate graphics: {e}")),
            }
        }
        self.next_repaint =
            (repaint_delay < Duration::from_secs(3600)).then(|| Instant::now() + repaint_delay);
        self.handle_requests()
    }
}

impl ApplicationHandler<UserEvent> for Runner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        if let Err(e) = self.create_window(event_loop) {
            log::error!("Cannot create window: {e}");
            fatal(&format!("Cannot start Direct3D 11 graphics:\n{e}"));
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wake => {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            UserEvent::Tray(TrayAction::Show) => self.show_window(),
            UserEvent::Tray(TrayAction::Quit) => self.quit(event_loop),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(window) = self.window.clone() else {
            return;
        };
        if let Some(egui_winit) = self.egui_winit.as_mut() {
            let response = egui_winit.on_window_event(&window, &event);
            if response.repaint {
                window.request_redraw();
            }
        }
        match event {
            WindowEvent::CloseRequested => self.quit(event_loop),
            WindowEvent::Resized(size) => {
                self.minimized =
                    size.width == 0 || size.height == 0 || window.is_minimized().unwrap_or(false);
                if self.minimized {
                    if self.app.config.ui.minimize_to_tray && !self.hidden_to_tray {
                        self.ensure_tray();
                        if self.tray.is_some() {
                            window.set_visible(false);
                            self.hidden_to_tray = true;
                        }
                    }
                } else {
                    if let Some(renderer) = self.renderer.as_mut() {
                        renderer.resize(size.width, size.height);
                    }
                    if window.fullscreen().is_none() && !window.is_maximized() {
                        self.app.config.ui.window_size = Some((size.width, size.height));
                        self.app.mark_dirty();
                    }
                    window.request_redraw();
                }
            }
            WindowEvent::RedrawRequested if !self.minimized && self.redraw() => {
                self.quit(event_loop);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let device = self
            .renderer
            .as_ref()
            .map(|r| r.device_ptr())
            .unwrap_or(std::ptr::null_mut());
        if !device.is_null() {
            self.app.tick(device);
        }
        let Some(window) = &self.window else { return };
        if self.minimized || self.hidden_to_tray {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(100),
            ));
            return;
        }
        if self.app.wants_continuous_redraw() {
            let next_frame = self.last_redraw + MIN_FRAME_INTERVAL;
            if Instant::now() >= next_frame {
                window.request_redraw();
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                next_frame.max(Instant::now() + Duration::from_millis(1)),
            ));
            return;
        }
        let tick = Instant::now() + Duration::from_millis(250);
        let wake = match self.next_repaint {
            Some(at) if at <= Instant::now() => {
                window.request_redraw();
                tick
            }
            Some(at) => at.min(tick),
            None => tick,
        };
        if self.app.stream.state.is_active() || self.app.notice.is_some() {
            window.request_redraw();
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
    }
}

fn fatal(message: &str) {
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(message),
            w!("PS5 LAN Capture"),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn single_instance_guard() -> Option<HANDLE> {
    let handle =
        unsafe { CreateMutexW(None, true, w!("Local\\PS5LanCapture.SingleInstance")) }.ok()?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            MessageBoxW(
                None,
                w!("PS5 LAN Capture is already running. Check the taskbar or the system tray."),
                w!("PS5 LAN Capture"),
                MB_OK | MB_ICONINFORMATION,
            );
        }
        std::process::exit(0);
    }
    Some(handle)
}

fn main() {
    let paths = Paths::resolve();
    logging::init(&paths.log_dir);
    std::panic::set_hook(Box::new(|info| {
        log::error!("Fatal error: {info}");
        log::logger().flush();
        fatal(&format!(
            "An unexpected error occurred and the app will close.\n\n{info}"
        ));
    }));
    let _guard = single_instance_guard();
    log::info!("PS5 LAN Capture {} started", env!("CARGO_PKG_VERSION"));

    let native_version = match native::init() {
        Ok(v) => v,
        Err(e) => {
            fatal(&e);
            return;
        }
    };
    log::info!("Core: {native_version}");

    let config = AppConfig::load(&paths.config_file);
    let audio = AudioOutput::start(
        config.audio.device_id.clone(),
        config.audio.volume,
        config.audio.muted,
        config.audio.latency_ms,
    );

    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(l) => l,
        Err(e) => {
            fatal(&format!("Cannot create event loop: {e}"));
            return;
        }
    };
    let proxy = Arc::new(Mutex::new(event_loop.create_proxy()));
    let waker_proxy = proxy.clone();
    let waker: session::Waker = Arc::new(move || {
        let _ = waker_proxy.lock().send_event(UserEvent::Wake);
    });
    let controller =
        controller::ControllerManager::start(&config.controller.settings(), waker.clone());
    let updater = updater::Updater::new(waker.clone());
    let stream = StreamController::new(audio.sink(), controller.shared(), waker);
    let app = App::new(
        paths,
        config,
        audio,
        controller,
        updater,
        stream,
        native_version,
    );

    let egui_ctx = egui::Context::default();
    ui::theme::apply(&egui_ctx);

    let mut runner = Runner {
        proxy,
        window: None,
        renderer: None,
        egui_ctx,
        egui_winit: None,
        app,
        tray: None,
        icon: decode_icon(),
        next_repaint: None,
        minimized: false,
        hidden_to_tray: false,
        last_session: 0,
        last_redraw: Instant::now(),
    };
    if let Err(e) = event_loop.run_app(&mut runner) {
        log::error!("Event loop ended with an error: {e}");
    }
    runner.app.shutdown();
    drop(runner);
    log::info!("Closed");
    log::logger().flush();
}
