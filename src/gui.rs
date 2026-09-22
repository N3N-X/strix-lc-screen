//! Window for brightness, rotation, and pictures on the pump screen.

use crate::media::{self, photo_jpeg, video_jpegs, Crop};
use crate::session::Panel;
use crate::settings::{self, Settings};
use anyhow::{Context, Result};
use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

enum Request {
    Refresh(Sender<Result<Snapshot>>),
    Brightness(u8, Sender<Result<Snapshot>>),
    Rotate(u16, Sender<Result<Snapshot>>),
    Power(&'static str, Sender<Result<Snapshot>>),
    ShowStill(Vec<u8>, Sender<Result<()>>),
    Play(Vec<Vec<u8>>, u64),
    Stop,
}

#[derive(Clone)]
struct Snapshot {
    serial: String,
    firmware: String,
    brightness: u8,
    rotation: u16,
    free_space: String,
}

struct Worker {
    pub(crate) tx: Sender<Request>,
    cancel: Arc<AtomicBool>,
    /// 1000 means 1×. The playback loop reads this between frames.
    speed: Arc<AtomicU32>,
}

impl Worker {
    fn start() -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let speed = Arc::new(AtomicU32::new(1000));
        let flag = Arc::clone(&cancel);
        let pace = Arc::clone(&speed);
        thread::spawn(move || worker_loop(rx, flag, pace));
        Self { tx, cancel, speed }
    }

    fn play(&self, frames: Vec<Vec<u8>>, frame_ms: u64) -> Result<()> {
        self.cancel.store(false, Ordering::Relaxed);
        self.tx
            .send(Request::Play(frames, frame_ms))
            .context("screen worker stopped")?;
        Ok(())
    }

    fn set_speed(&self, speed: f32) {
        let thousandths = (speed.clamp(0.25, 2.0) * 1000.0).round() as u32;
        self.speed.store(thousandths.max(250), Ordering::Relaxed);
    }

    fn stop_playback(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Request::Stop);
    }
}

struct Playing {
    frames: Vec<Vec<u8>>,
    frame_ms: u64,
    index: usize,
    next_at: Instant,
}

fn worker_loop(rx: Receiver<Request>, cancel: Arc<AtomicBool>, speed: Arc<AtomicU32>) {
    let mut panel = match Panel::open().and_then(|mut panel| {
        panel.connect()?;
        Ok(panel)
    }) {
        Ok(panel) => Some(panel),
        Err(_) => None,
    };
    let mut playing: Option<Playing> = None;
    loop {
        let wait = match &playing {
            Some(play) => play.next_at.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(3600),
        };
        let incoming = if playing.is_some() {
            rx.recv_timeout(wait).ok()
        } else {
            match rx.recv() {
                Ok(request) => Some(request),
                Err(_) => return,
            }
        };
        if let Some(request) = incoming {
            match request {
                Request::Stop => {
                    playing = None;
                    cancel.store(false, Ordering::Relaxed);
                }
                Request::Play(frames, frame_ms) => {
                    playing = Some(Playing {
                        frames,
                        frame_ms: frame_ms.max(1),
                        index: 0,
                        next_at: Instant::now(),
                    });
                    cancel.store(false, Ordering::Relaxed);
                }
                other => {
                    playing = None;
                    cancel.store(false, Ordering::Relaxed);
                    if panel.is_none() {
                        panel = Panel::open()
                            .and_then(|mut panel| {
                                panel.connect()?;
                                Ok(panel)
                            })
                            .ok();
                    }
                    dispatch(&mut panel, other, &cancel);
                }
            }
        }
        if cancel.load(Ordering::Relaxed) {
            playing = None;
            continue;
        }
        let Some(play) = playing.as_mut() else {
            continue;
        };
        if play.frames.is_empty() {
            playing = None;
            continue;
        }
        if Instant::now() < play.next_at {
            continue;
        }
        if panel.is_none() {
            panel = Panel::open()
                .and_then(|mut panel| {
                    panel.connect()?;
                    Ok(panel)
                })
                .ok();
        }
        let Some(panel) = panel.as_mut() else {
            play.next_at = Instant::now() + Duration::from_millis(500);
            continue;
        };
        let prepare = play.index == 0;
        let jpeg = &play.frames[play.index];
        let began = Instant::now();
        let result = if prepare {
            panel.show_jpeg(jpeg, &|| cancel.load(Ordering::Relaxed))
        } else {
            panel.push_jpeg(jpeg, &|| cancel.load(Ordering::Relaxed))
        };
        if result.is_err() {
            playing = None;
            continue;
        }
        let thousandths = speed.load(Ordering::Relaxed).clamp(250, 2000) as f32 / 1000.0;
        let gap = Duration::from_millis(play.frame_ms).div_f32(thousandths);
        let spent = began.elapsed();
        play.next_at = if gap > spent {
            Instant::now() + (gap - spent)
        } else {
            Instant::now()
        };
        play.index = (play.index + 1) % play.frames.len();
    }
}

fn dispatch(panel: &mut Option<Panel>, request: Request, cancel: &AtomicBool) {
    let stopped = || cancel.load(Ordering::Relaxed);
    match request {
        Request::Refresh(reply) => {
            let _ = reply.send(snapshot(panel));
        }
        Request::Brightness(value, reply) => {
            let result = (|| {
                let panel = panel.as_mut().context("pump screen is not connected")?;
                panel.brightness(value)?;
                read_snapshot(panel)
            })();
            let _ = reply.send(result);
        }
        Request::Rotate(degrees, reply) => {
            let result = (|| {
                let panel = panel.as_mut().context("pump screen is not connected")?;
                panel.rotate(degrees)?;
                read_snapshot(panel)
            })();
            let _ = reply.send(result);
        }
        Request::Power(event, reply) => {
            let result = (|| {
                let panel = panel.as_mut().context("pump screen is not connected")?;
                panel.power(event)?;
                read_snapshot(panel)
            })();
            let _ = reply.send(result);
        }
        Request::ShowStill(jpeg, reply) => {
            let result = (|| {
                let panel = panel.as_mut().context("pump screen is not connected")?;
                panel.show_jpeg(&jpeg, &stopped)
            })();
            let _ = reply.send(result);
        }
        Request::Play(_, _) | Request::Stop => {}
    }
}

fn snapshot(panel: &mut Option<Panel>) -> Result<Snapshot> {
    if panel.is_none() {
        *panel = Some(Panel::open().and_then(|mut panel| {
            panel.connect()?;
            Ok(panel)
        })?);
    }
    read_snapshot(panel.as_mut().context("pump screen is not connected")?)
}

fn read_snapshot(panel: &mut Panel) -> Result<Snapshot> {
    let identity = panel.connect()?;
    let state = panel.state()?;
    let brightness = state["brightness"].as_u64().unwrap_or(0).min(100) as u8;
    let rotation = state["degree"].as_u64().unwrap_or(0) as u16;
    Ok(Snapshot {
        serial: identity["sn"].as_str().unwrap_or("unknown").to_string(),
        firmware: identity["version"]["firmware"]
            .as_str()
            .unwrap_or("unknown")
            .to_string(),
        brightness,
        rotation,
        free_space: state["space"].to_string(),
    })
}

pub fn run(start_in_tray: bool) -> eframe::Result<()> {
    if !settings::claim_single_instance() {
        return Ok(());
    }
    let (tray_tx, tray_rx) = std::sync::mpsc::sync_channel(64);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([480.0, 860.0])
            .with_min_inner_size([420.0, 560.0])
            .with_resizable(true)
            .with_decorations(true)
            .with_title("Pump screen"),
        ..Default::default()
    };
    eframe::run_native(
        "Pump screen",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            let ctx = cc.egui_ctx.clone();
            let click_tx = tray_tx.clone();
            tray_icon::TrayIconEvent::set_event_handler(Some(move |event| {
                if tray_click_shows_window(&event) {
                    reveal_pump_window();
                    let _ = click_tx.try_send(TrayAction::Show);
                }
                ctx.request_repaint();
            }));
            let ctx = cc.egui_ctx.clone();
            let menu_tx = tray_tx.clone();
            tray_icon::menu::MenuEvent::set_event_handler(Some(
                move |event: tray_icon::menu::MenuEvent| {
                    let _ = menu_tx.try_send(TrayAction::Menu(event.id));
                    ctx.request_repaint();
                },
            ));
            Ok(Box::new(App::new(start_in_tray, tray_rx)))
        }),
    )
}

enum TrayAction {
    Show,
    Menu(tray_icon::menu::MenuId),
}

fn tray_click_shows_window(event: &tray_icon::TrayIconEvent) -> bool {
    match event {
        tray_icon::TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        }
        | tray_icon::TrayIconEvent::DoubleClick {
            button: tray_icon::MouseButton::Left,
            ..
        } => true,
        _ => false,
    }
}

struct App {
    worker: Worker,
    snapshot: Option<Snapshot>,
    brightness: u8,
    status: String,
    busy: bool,
    chosen: Option<PathBuf>,
    preview: Option<egui::TextureHandle>,
    source_width: u32,
    source_height: u32,
    zoom: f32,
    pan_x: f32,
    pan_y: f32,
    speed: f32,
    pending: Option<Receiver<JobResult>>,
    preview_rx: Option<Receiver<Result<media::Preview>>>,
    asus_running: bool,
    asus_checked: std::time::Instant,
    settings: Settings,
    tray: TrayMenu,
    quit: bool,
    hide_once: bool,
    resume_when_ready: bool,
    tray_rx: std::sync::mpsc::Receiver<TrayAction>,
}

struct TrayMenu {
    _icon: tray_icon::TrayIcon,
    show_id: tray_icon::menu::MenuId,
    stop_id: tray_icon::menu::MenuId,
    quit_id: tray_icon::menu::MenuId,
    _show: tray_icon::menu::MenuItem,
    _stop: tray_icon::menu::MenuItem,
    _quit: tray_icon::menu::MenuItem,
}

enum JobResult {
    Snapshot(Result<Snapshot>),
    Done(Result<()>),
    Frames(Result<media::Clip>),
}

impl App {
    fn new(start_in_tray: bool, tray_rx: std::sync::mpsc::Receiver<TrayAction>) -> Self {
        let saved = Settings::load();
        let worker = Worker::start();
        worker.set_speed(saved.speed);
        let pending = Some(spawn_snapshot(&worker));
        let mut chosen = None;
        let mut preview_rx = None;
        let mut resume_when_ready = false;
        if let Some(path) = saved.last_path.clone() {
            let path = PathBuf::from(path);
            if path.exists() {
                resume_when_ready = saved.resume_last;
                let (tx, rx) = mpsc::channel();
                let preview_path = path.clone();
                thread::spawn(move || {
                    let _ = tx.send(media::preview_jpeg(&preview_path));
                });
                chosen = Some(path);
                preview_rx = Some(rx);
            }
        }
        if saved.start_with_windows {
            let _ = settings::set_run_at_logon(true, saved.start_in_tray);
        }
        Self {
            worker,
            snapshot: None,
            brightness: 100,
            status: "Reading the pump…".into(),
            busy: true,
            chosen,
            preview: None,
            source_width: 0,
            source_height: 0,
            zoom: saved.zoom.clamp(1.0, 4.0),
            pan_x: saved.pan_x.clamp(-1.0, 1.0),
            pan_y: saved.pan_y.clamp(-1.0, 1.0),
            speed: saved.speed.clamp(0.25, 2.0),
            pending,
            preview_rx,
            asus_running: false,
            asus_checked: std::time::Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(std::time::Instant::now),
            settings: saved,
            tray: TrayMenu::build(),
            quit: false,
            hide_once: start_in_tray,
            resume_when_ready,
            tray_rx,
        }
    }

    fn persist(&mut self) {
        self.settings.zoom = self.zoom;
        self.settings.pan_x = self.pan_x;
        self.settings.pan_y = self.pan_y;
        self.settings.speed = self.speed;
        self.settings.last_path = self.chosen.as_ref().map(|path| path.display().to_string());
        if let Err(error) = self.settings.save() {
            self.status = format!("{error:#}");
        }
    }

    fn show_window(&self, ctx: &egui::Context) {
        reveal_pump_window();
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }

    fn poll_tray(&mut self, ctx: &egui::Context) {
        while let Ok(action) = self.tray_rx.try_recv() {
            match action {
                TrayAction::Show => self.show_window(ctx),
                TrayAction::Menu(id) if id == self.tray.show_id => self.show_window(ctx),
                TrayAction::Menu(id) if id == self.tray.stop_id => {
                    self.worker.stop_playback();
                    self.status = "Stopped. The pump keeps the last frame.".into();
                }
                TrayAction::Menu(id) if id == self.tray.quit_id => {
                    self.quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                TrayAction::Menu(_) => {}
            }
        }
    }

    fn maybe_resume(&mut self) {
        if !self.resume_when_ready {
            return;
        }
        self.resume_when_ready = false;
        if self.asus_running {
            self.status = "Quit the ROG STRIX LC app first. The last file was not sent.".into();
            return;
        }
        if self.chosen.as_ref().is_some_and(|path| path.exists()) {
            self.send_chosen();
        }
    }

    fn refresh_asus_flag(&mut self) {
        if self.asus_checked.elapsed() < Duration::from_secs(3) {
            return;
        }
        self.asus_running = asus_app_running();
        self.asus_checked = std::time::Instant::now();
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.pending.as_ref() else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        self.pending = None;
        self.busy = false;
        match result {
            JobResult::Snapshot(Ok(shot)) => {
                self.brightness = shot.brightness;
                self.snapshot = Some(shot);
                self.busy = false;
                if self.resume_when_ready {
                    self.maybe_resume();
                } else if self.status.starts_with("Reading the pump") {
                    self.status = "Ready.".into();
                }
            }
            JobResult::Snapshot(Err(error)) => {
                self.busy = false;
                self.status = format!("{error:#}");
                self.maybe_resume();
            }
            JobResult::Done(Ok(())) => {
                self.status = "On the pump.".into();
                self.pending = Some(spawn_snapshot(&self.worker));
                self.busy = true;
            }
            JobResult::Done(Err(error)) => {
                self.status = format!("{error:#}");
            }
            JobResult::Frames(Ok(clip)) => {
                let count = clip.frames.len();
                let frame_ms = clip.frame_ms;
                if let Err(error) = self.worker.play(clip.frames, frame_ms) {
                    self.status = format!("{error:#}");
                } else {
                    let stay = if self.settings.close_to_tray {
                        "Closing the window leaves it running in the tray."
                    } else {
                        "The window has to stay open."
                    };
                    self.status = format!("Playing {count} frames at {:.2}×. {stay}", self.speed);
                }
            }
            JobResult::Frames(Err(error)) => {
                self.status = format!("{error:#}");
            }
        }
        ctx.request_repaint();
    }

    fn poll_preview(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.preview_rx.as_ref() else {
            return;
        };
        let Ok(result) = rx.try_recv() else {
            return;
        };
        self.preview_rx = None;
        match result {
            Ok(preview) => match image::load_from_memory(&preview.jpeg) {
                Ok(image) => {
                    let decoded = image.to_rgb8();
                    let color = egui::ColorImage::from_rgb(
                        [decoded.width() as usize, decoded.height() as usize],
                        decoded.as_raw(),
                    );
                    self.preview = Some(ctx.load_texture(
                        "preview",
                        color,
                        egui::TextureOptions::LINEAR,
                    ));
                    self.source_width = preview.width;
                    self.source_height = preview.height;
                    self.status = "Drag the square to crop, then show it on the pump.".into();
                }
                Err(error) => {
                    self.status = format!("could not show a preview: {error}");
                }
            },
            Err(error) => {
                self.status = format!("{error:#}");
            }
        }
        ctx.request_repaint();
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_tray(ctx);
        if self.hide_once {
            self.hide_once = false;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        if ctx.input(|input| input.viewport().close_requested()) && !self.quit && self.settings.close_to_tray {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        self.poll(ctx);
        self.poll_preview(ctx);
        self.refresh_asus_flag();
        ctx.request_repaint_after(Duration::from_millis(if self.pending.is_some() || self.preview_rx.is_some() {
            200
        } else {
            500
        }));
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Pump screen");
            ui.add_space(6.0);
            if let Some(shot) = &self.snapshot {
                ui.label(format!("Serial {}", shot.serial));
                ui.label(format!("Firmware {}", shot.firmware));
                ui.label(format!(
                    "On screen: brightness {}, rotation {}°, free {}",
                    shot.brightness, shot.rotation, shot.free_space
                ));
            } else {
                ui.label("Not connected yet.");
            }
            ui.add_space(8.0);
            ui.separator();
            ui.label("Brightness");
            let slider = ui.add(egui::Slider::new(&mut self.brightness, 0..=100).suffix("%"));
            if slider.drag_stopped() && !self.busy {
                self.apply_brightness();
            }
            ui.add_space(6.0);
            ui.label("Rotation");
            ui.horizontal(|ui| {
                for degrees in [0u16, 90, 180, 270] {
                    if ui.button(format!("{degrees}°")).clicked() && !self.busy {
                        self.apply_rotation(degrees);
                    }
                }
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Wake").clicked() && !self.busy {
                    self.apply_power("resume");
                }
                if ui.button("Sleep").clicked() && !self.busy {
                    self.apply_power("suspend");
                }
                if ui.button("Refresh").clicked() && !self.busy {
                    self.busy = true;
                    self.status = "Reading the pump…".into();
                    self.pending = Some(spawn_snapshot(&self.worker));
                }
            });
            ui.add_space(10.0);
            ui.separator();
            ui.label("Photo or video");
            if ui.button("Choose file…").clicked() && !self.busy {
                self.choose_file(ctx);
            }
            if let Some(path) = &self.chosen {
                ui.label(path.display().to_string());
            }
            if let Some(preview) = &self.preview {
                ui.add_space(6.0);
                ui.label("Drag the square to choose the part the pump shows.");
                let crop_settled = crop_view(
                    ui,
                    preview,
                    self.source_width,
                    self.source_height,
                    &mut self.zoom,
                    &mut self.pan_x,
                    &mut self.pan_y,
                );
                if crop_settled {
                    self.persist();
                }
                ui.add_space(4.0);
                let zoom_slider = ui.add(egui::Slider::new(&mut self.zoom, 1.0..=4.0).text("Zoom"));
                if zoom_slider.drag_stopped() {
                    self.persist();
                }
                if ui.button("Reset crop").clicked() {
                    self.zoom = 1.0;
                    self.pan_x = 0.0;
                    self.pan_y = 0.0;
                    self.persist();
                }
            }
            ui.add_space(8.0);
            ui.label("Playback speed");
            let speed_slider = ui.add(
                egui::Slider::new(&mut self.speed, 0.25..=2.0)
                    .fixed_decimals(2)
                    .suffix("×"),
            );
            if speed_slider.drag_stopped() {
                self.worker.set_speed(self.speed);
                self.persist();
            } else if speed_slider.changed() {
                self.worker.set_speed(self.speed);
            }
            ui.label("1× matches the video length. Speed changes while it plays. Crop is used the next time you press Show.");
            ui.add_space(10.0);
            ui.separator();
            ui.label("Startup and tray");
            if ui
                .checkbox(
                    &mut self.settings.close_to_tray,
                    "Keep running in the tray when the window closes",
                )
                .changed()
            {
                self.persist();
            }
            if ui
                .checkbox(
                    &mut self.settings.start_with_windows,
                    "Start when I sign in to Windows",
                )
                .changed()
            {
                if let Err(error) = settings::set_run_at_logon(
                    self.settings.start_with_windows,
                    self.settings.start_in_tray,
                ) {
                    self.settings.start_with_windows = !self.settings.start_with_windows;
                    self.status = format!("{error:#}");
                } else {
                    self.persist();
                }
            }
            if ui
                .checkbox(
                    &mut self.settings.start_in_tray,
                    "At sign-in, stay in the tray instead of opening this window",
                )
                .changed()
            {
                if self.settings.start_with_windows {
                    if let Err(error) =
                        settings::set_run_at_logon(true, self.settings.start_in_tray)
                    {
                        self.status = format!("{error:#}");
                    }
                }
                self.persist();
            }
            if ui
                .checkbox(
                    &mut self.settings.resume_last,
                    "Play the last photo or video when the app opens",
                )
                .changed()
            {
                self.persist();
            }
            ui.label("Click the tray icon to show this window. Right-click it to stop the video or quit. Quitting stops playback.");
            ui.add_space(6.0);
            let can_send = self.chosen.is_some() && !self.busy;
            if ui.add_enabled(can_send, egui::Button::new("Show on pump")).clicked() {
                self.send_chosen();
            }
            if ui.button("Stop video").clicked() {
                self.worker.stop_playback();
                self.status = "Stopped. The pump keeps the last frame.".into();
            }
            ui.add_space(12.0);
            ui.label(&self.status);
            if self.asus_running {
                ui.add_space(6.0);
                ui.colored_label(
                    egui::Color32::from_rgb(255, 180, 80),
                    "Quit ROG STRIX LC & SLC IV Series. It is holding the pump USB connection.",
                );
            }
            });
        });
    }
}

fn crop_view(
    ui: &mut egui::Ui,
    texture: &egui::TextureHandle,
    source_width: u32,
    source_height: u32,
    zoom: &mut f32,
    pan_x: &mut f32,
    pan_y: &mut f32,
) -> bool {
    let source_width = source_width.max(1) as f32;
    let source_height = source_height.max(1) as f32;
    let max_w = ui.available_width().min(420.0);
    let aspect = source_width / source_height;
    let (disp_w, disp_h) = if aspect >= 1.0 {
        (max_w, max_w / aspect)
    } else {
        (max_w * aspect, max_w)
    };
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(disp_w, disp_h),
        egui::Sense::click_and_drag(),
    );
    ui.painter().image(
        texture.id(),
        rect,
        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
        egui::Color32::WHITE,
    );
    let crop = Crop {
        zoom: *zoom,
        pan_x: *pan_x,
        pan_y: *pan_y,
    };
    let (side, x, y) = crop.square(source_width as u32, source_height as u32);
    let sx = disp_w / source_width;
    let sy = disp_h / source_height;
    let crop_rect = egui::Rect::from_min_size(
        rect.min + egui::vec2(x as f32 * sx, y as f32 * sy),
        egui::vec2(side as f32 * sx, side as f32 * sy),
    );
    let shade = egui::Color32::from_black_alpha(140);
    let painter = ui.painter();
    painter.rect_filled(
        egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, crop_rect.min.y)),
        0.0,
        shade,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(egui::pos2(rect.min.x, crop_rect.max.y), rect.max),
        0.0,
        shade,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(egui::pos2(rect.min.x, crop_rect.min.y), crop_rect.left_bottom()),
        0.0,
        shade,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(crop_rect.right_top(), egui::pos2(rect.max.x, crop_rect.max.y)),
        0.0,
        shade,
    );
    painter.rect_stroke(
        crop_rect,
        0.0,
        egui::Stroke::new(2.0_f32, egui::Color32::WHITE),
        egui::StrokeKind::Inside,
    );
    if response.dragged() {
        let delta = response.drag_delta();
        let max_x = (source_width - side as f32).max(1.0);
        let max_y = (source_height - side as f32).max(1.0);
        *pan_x = (*pan_x + delta.x / (max_x * sx) * 2.0).clamp(-1.0, 1.0);
        *pan_y = (*pan_y + delta.y / (max_y * sy) * 2.0).clamp(-1.0, 1.0);
    }
    response.drag_stopped()
}

impl App {
    fn apply_brightness(&mut self) {
        if self.asus_running {
            self.status = "Quit the ROG STRIX LC app first.".into();
            return;
        }
        self.busy = true;
        self.status = "Setting brightness…".into();
        let (tx, rx) = mpsc::channel();
        let value = self.brightness;
        let worker_tx = self.worker.tx.clone();
        thread::spawn(move || {
            let (reply_tx, reply_rx) = mpsc::channel();
            if worker_tx
                .send(Request::Brightness(value, reply_tx))
                .is_err()
            {
                let _ = tx.send(JobResult::Snapshot(Err(anyhow_worker())));
                return;
            }
            match reply_rx.recv() {
                Ok(result) => {
                    let _ = tx.send(JobResult::Snapshot(result));
                }
                Err(_) => {
                    let _ = tx.send(JobResult::Snapshot(Err(anyhow_worker())));
                }
            }
        });
        self.pending = Some(rx);
    }

    fn apply_rotation(&mut self, degrees: u16) {
        if self.asus_running {
            self.status = "Quit the ROG STRIX LC app first.".into();
            return;
        }
        self.busy = true;
        self.status = format!("Rotating to {degrees}°…");
        let (tx, rx) = mpsc::channel();
        let worker_tx = self.worker.tx.clone();
        thread::spawn(move || {
            let (reply_tx, reply_rx) = mpsc::channel();
            if worker_tx.send(Request::Rotate(degrees, reply_tx)).is_err() {
                let _ = tx.send(JobResult::Snapshot(Err(anyhow_worker())));
                return;
            }
            match reply_rx.recv() {
                Ok(result) => {
                    let _ = tx.send(JobResult::Snapshot(result));
                }
                Err(_) => {
                    let _ = tx.send(JobResult::Snapshot(Err(anyhow_worker())));
                }
            }
        });
        self.pending = Some(rx);
    }

    fn apply_power(&mut self, event: &'static str) {
        if self.asus_running {
            self.status = "Quit the ROG STRIX LC app first.".into();
            return;
        }
        self.busy = true;
        self.status = match event {
            "resume" => "Waking the screen…".into(),
            _ => "Putting the screen to sleep…".into(),
        };
        let (tx, rx) = mpsc::channel();
        let worker_tx = self.worker.tx.clone();
        thread::spawn(move || {
            let (reply_tx, reply_rx) = mpsc::channel();
            if worker_tx.send(Request::Power(event, reply_tx)).is_err() {
                let _ = tx.send(JobResult::Snapshot(Err(anyhow_worker())));
                return;
            }
            match reply_rx.recv() {
                Ok(result) => {
                    let _ = tx.send(JobResult::Snapshot(result));
                }
                Err(_) => {
                    let _ = tx.send(JobResult::Snapshot(Err(anyhow_worker())));
                }
            }
        });
        self.pending = Some(rx);
    }

    fn choose_file(&mut self, ctx: &egui::Context) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter(
                "Photos and video",
                &["png", "jpg", "jpeg", "gif", "bmp", "mp4", "mov", "avi", "mkv", "webm"],
            )
            .pick_file()
        else {
            return;
        };
        self.preview = None;
        self.source_width = 0;
        self.source_height = 0;
        self.zoom = 1.0;
        self.pan_x = 0.0;
        self.pan_y = 0.0;
        self.chosen = Some(path.clone());
        self.status = "Loading preview…".into();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(media::preview_jpeg(&path));
        });
        self.preview_rx = Some(rx);
        self.persist();
        ctx.request_repaint();
    }

    fn send_chosen(&mut self) {
        if self.asus_running {
            self.status = "Quit the ROG STRIX LC app first.".into();
            return;
        }
        let Some(path) = self.chosen.clone() else {
            return;
        };
        self.worker.stop_playback();
        self.busy = true;
        let (tx, rx) = mpsc::channel();
        let worker_tx = self.worker.tx.clone();
        let crop = Crop {
            zoom: self.zoom,
            pan_x: self.pan_x,
            pan_y: self.pan_y,
        };
        self.worker.set_speed(self.speed);
        if media::is_video(&path) {
            self.status = "Preparing the video…".into();
            thread::spawn(move || match video_jpegs(&path, &crop) {
                Ok(clip) => {
                    let _ = tx.send(JobResult::Frames(Ok(clip)));
                }
                Err(error) => {
                    let _ = tx.send(JobResult::Frames(Err(error)));
                }
            });
        } else {
            self.status = "Sending the photo…".into();
            thread::spawn(move || {
                let jpeg = match photo_jpeg(&path, &crop) {
                    Ok(jpeg) => jpeg,
                    Err(error) => {
                        let _ = tx.send(JobResult::Done(Err(error)));
                        return;
                    }
                };
                let (reply_tx, reply_rx) = mpsc::channel();
                if worker_tx.send(Request::ShowStill(jpeg, reply_tx)).is_err() {
                    let _ = tx.send(JobResult::Done(Err(anyhow_worker())));
                    return;
                }
                match reply_rx.recv() {
                    Ok(result) => {
                        let _ = tx.send(JobResult::Done(result));
                    }
                    Err(_) => {
                        let _ = tx.send(JobResult::Done(Err(anyhow_worker())));
                    }
                }
            });
        }
        self.pending = Some(rx);
    }
}

fn spawn_snapshot(worker: &Worker) -> Receiver<JobResult> {
    let (tx, rx) = mpsc::channel();
    let worker_tx = worker.tx.clone();
    thread::spawn(move || {
        let (reply_tx, reply_rx) = mpsc::channel();
        if worker_tx.send(Request::Refresh(reply_tx)).is_err() {
            let _ = tx.send(JobResult::Snapshot(Err(anyhow_worker())));
            return;
        }
        match reply_rx.recv() {
            Ok(result) => {
                let _ = tx.send(JobResult::Snapshot(result));
            }
            Err(_) => {
                let _ = tx.send(JobResult::Snapshot(Err(anyhow_worker())));
            }
        }
    });
    rx
}

fn asus_app_running() -> bool {
    #[cfg(windows)]
    {
        return process_exists("ROG STRIX LC & SLC IV Series.exe");
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
fn process_exists(exe_name: &str) -> bool {
    #[repr(C)]
    struct ProcessEntry {
        size: u32,
        usage: u32,
        process_id: u32,
        default_heap_id: usize,
        module_id: u32,
        threads: u32,
        parent_process_id: u32,
        pri_class_base: i32,
        flags: u32,
        exe_file: [u16; 260],
    }
    unsafe extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> *mut core::ffi::c_void;
        fn Process32FirstW(snapshot: *mut core::ffi::c_void, entry: *mut ProcessEntry) -> i32;
        fn Process32NextW(snapshot: *mut core::ffi::c_void, entry: *mut ProcessEntry) -> i32;
        fn CloseHandle(handle: *mut core::ffi::c_void) -> i32;
    }
    const SNAPPROCESS: u32 = 0x0000_0002;
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(SNAPPROCESS, 0);
        if snapshot.is_null() || snapshot == -1isize as *mut core::ffi::c_void {
            return false;
        }
        let mut entry = ProcessEntry {
            size: std::mem::size_of::<ProcessEntry>() as u32,
            usage: 0,
            process_id: 0,
            default_heap_id: 0,
            module_id: 0,
            threads: 0,
            parent_process_id: 0,
            pri_class_base: 0,
            flags: 0,
            exe_file: [0; 260],
        };
        let mut found = false;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                let len = entry
                    .exe_file
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.exe_file.len());
                let name = String::from_utf16_lossy(&entry.exe_file[..len]);
                if name.eq_ignore_ascii_case(exe_name) {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        found
    }
}

fn anyhow_worker() -> anyhow::Error {
    anyhow::anyhow!("the screen worker stopped")
}

impl TrayMenu {
    fn build() -> Self {
        use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
        let show = MenuItem::with_id("show", "Show", true, None);
        let stop = MenuItem::with_id("stop", "Stop video", true, None);
        let quit = MenuItem::with_id("quit", "Quit", true, None);
        let menu = Menu::new();
        let _ = menu.append(&show);
        let _ = menu.append(&stop);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&quit);
        let icon = tray_icon_image();
        let tray = tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("Pump screen")
            .with_icon(icon)
            .build()
            .expect("the tray icon could not be created");
        Self {
            _icon: tray,
            show_id: show.id().clone(),
            stop_id: stop.id().clone(),
            quit_id: quit.id().clone(),
            _show: show,
            _stop: stop,
            _quit: quit,
        }
    }
}

fn reveal_pump_window() {
    #[cfg(windows)]
    unsafe {
        use std::ffi::c_void;
        unsafe extern "system" {
            fn FindWindowW(class: *const u16, window: *const u16) -> *mut c_void;
            fn ShowWindow(hwnd: *mut c_void, cmd: i32) -> i32;
            fn SetForegroundWindow(hwnd: *mut c_void) -> i32;
            fn BringWindowToTop(hwnd: *mut c_void) -> i32;
            fn IsIconic(hwnd: *mut c_void) -> i32;
            fn GetForegroundWindow() -> *mut c_void;
            fn GetWindowThreadProcessId(hwnd: *mut c_void, process: *mut u32) -> u32;
            fn GetCurrentThreadId() -> u32;
            fn AttachThreadInput(from: u32, to: u32, attach: i32) -> i32;
        }
        let title: Vec<u16> = "Pump screen"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
        if hwnd.is_null() {
            return;
        }
        let cmd = if IsIconic(hwnd) != 0 { 9 } else { 5 };
        ShowWindow(hwnd, cmd);
        let foreground = GetForegroundWindow();
        let foreground_thread = GetWindowThreadProcessId(foreground, std::ptr::null_mut());
        let this_thread = GetCurrentThreadId();
        if foreground_thread != 0 && foreground_thread != this_thread {
            AttachThreadInput(foreground_thread, this_thread, 1);
            SetForegroundWindow(hwnd);
            BringWindowToTop(hwnd);
            AttachThreadInput(foreground_thread, this_thread, 0);
        } else {
            SetForegroundWindow(hwnd);
            BringWindowToTop(hwnd);
        }
    }
}

fn tray_icon_image() -> tray_icon::Icon {
    let size = 32u32;
    let mut rgba = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let dx = x as i32 - 15;
            let dy = y as i32 - 15;
            let index = ((y * size + x) * 4) as usize;
            if dx * dx + dy * dy <= 13 * 13 {
                rgba[index] = 214;
                rgba[index + 1] = 64;
                rgba[index + 2] = 38;
                rgba[index + 3] = 255;
            }
        }
    }
    tray_icon::Icon::from_rgba(rgba, size, size).expect("tray icon image")
}
