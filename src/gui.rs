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

#[derive(Clone)]
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
    /// A frame has reached the pump. Later failures give up sooner than the login retry.
    delivered: bool,
    retries: u8,
    /// One picture, sent again so the panel does not fall back to its built-in clip.
    hold: bool,
    paused: bool,
}

/// The panel drops a live picture about a second after the frames stop.
const HOLD_MS: u64 = 500;

fn hold_of(jpeg: Vec<u8>) -> Playing {
    Playing {
        frames: vec![jpeg],
        frame_ms: HOLD_MS,
        index: 0,
        next_at: Instant::now() + Duration::from_millis(HOLD_MS),
        delivered: true,
        retries: 0,
        hold: true,
        paused: false,
    }
}

/// `next_index` is the frame the loop would send next.
fn last_sent_index(next_index: usize, len: usize, delivered: bool) -> Option<usize> {
    if !delivered || len == 0 || next_index >= len {
        return None;
    }
    Some(if next_index == 0 {
        len - 1
    } else {
        next_index - 1
    })
}

fn hold_last(play: Playing) -> Option<Playing> {
    let index = last_sent_index(play.index, play.frames.len(), play.delivered)?;
    let jpeg = play.frames.get(index)?.clone();
    Some(hold_of(jpeg))
}

fn open_panel() -> Option<Panel> {
    Panel::open()
        .and_then(|mut panel| {
            panel.connect()?;
            Ok(panel)
        })
        .ok()
}

enum AfterCommand {
    Pause,
    Wake,
    /// Brightness and rotation can kick the panel back to its built-in clip.
    Rewake,
    Keep,
}

fn worker_loop(rx: Receiver<Request>, cancel: Arc<AtomicBool>, speed: Arc<AtomicU32>) {
    let mut panel = open_panel();
    let mut playing: Option<Playing> = None;
    loop {
        let wait = match &playing {
            Some(play) if play.paused => Duration::from_secs(3600),
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
                    playing = playing.take().and_then(hold_last);
                    cancel.store(false, Ordering::Relaxed);
                }
                Request::Play(frames, frame_ms) => {
                    playing = Some(Playing {
                        frames,
                        frame_ms: frame_ms.max(1),
                        index: 0,
                        next_at: Instant::now(),
                        delivered: false,
                        retries: 0,
                        hold: false,
                        paused: false,
                    });
                    cancel.store(false, Ordering::Relaxed);
                }
                Request::ShowStill(jpeg, reply) => {
                    cancel.store(false, Ordering::Relaxed);
                    playing = None;
                    if panel.is_none() {
                        panel = open_panel();
                    }
                    let result = (|| {
                        let panel = panel.as_mut().context("pump screen is not connected")?;
                        panel.show_jpeg(&jpeg, &|| cancel.load(Ordering::Relaxed))
                    })();
                    if result.is_ok() && !cancel.load(Ordering::Relaxed) {
                        playing = Some(hold_of(jpeg));
                    }
                    let _ = reply.send(result);
                }
                other => {
                    let effect = match &other {
                        Request::Power("suspend", _) => AfterCommand::Pause,
                        Request::Power("resume", _) => AfterCommand::Wake,
                        Request::Brightness(_, _) | Request::Rotate(_, _) => AfterCommand::Rewake,
                        _ => AfterCommand::Keep,
                    };
                    if panel.is_none() {
                        panel = open_panel();
                    }
                    dispatch(&mut panel, other);
                    match effect {
                        AfterCommand::Pause => {
                            if let Some(play) = playing.as_mut() {
                                play.paused = true;
                            }
                        }
                        AfterCommand::Wake => {
                            if let Some(play) = playing.as_mut() {
                                play.paused = false;
                                play.delivered = false;
                                play.retries = 0;
                                play.next_at = Instant::now();
                            }
                        }
                        AfterCommand::Rewake => {
                            if let Some(play) = playing.as_mut() {
                                if !play.paused {
                                    play.delivered = false;
                                    play.retries = 0;
                                    play.next_at = Instant::now();
                                }
                            }
                        }
                        AfterCommand::Keep => {}
                    }
                }
            }
        }
        if cancel.load(Ordering::Relaxed) {
            playing = playing.take().and_then(hold_last);
            cancel.store(false, Ordering::Relaxed);
            continue;
        }
        if playing.as_ref().is_some_and(|play| play.frames.is_empty()) {
            playing = None;
            continue;
        }
        if playing.as_ref().is_some_and(|play| play.paused) {
            continue;
        }
        let ready = match playing.as_ref() {
            Some(play) if Instant::now() < play.next_at => continue,
            // Open the picture channel once. Repeating that for every frame
            // of a one-frame hold blanks the panel.
            Some(play) => Some((!play.delivered, play.index)),
            None => None,
        };
        let Some((prepare, index)) = ready else {
            continue;
        };
        if panel.is_none() {
            panel = open_panel();
        }
        if panel.is_none() {
            if let Some(play) = playing.as_mut() {
                play.next_at = Instant::now() + Duration::from_millis(500);
            }
            continue;
        }
        if playing
            .as_ref()
            .is_some_and(|play| play.frames.get(index).is_none())
        {
            playing = None;
            continue;
        }
        let began = Instant::now();
        let result = {
            let panel = panel.as_mut().expect("panel is open");
            let jpeg = &playing.as_ref().expect("clip is loaded").frames[index];
            if prepare {
                panel.show_jpeg(jpeg, &|| cancel.load(Ordering::Relaxed))
            } else {
                panel.push_jpeg(jpeg, &|| cancel.load(Ordering::Relaxed))
            }
        };
        if result.is_err() {
            // At sign-in the pump USB is often still enumerating. Keep the clip
            // and try again. Once frames are flowing, a few failures mean it left.
            let give_up = match playing.as_mut() {
                Some(play) => {
                    let limit = if play.delivered { 3 } else { 40 };
                    play.retries = play.retries.saturating_add(1);
                    if play.retries > limit {
                        true
                    } else {
                        play.next_at = Instant::now() + Duration::from_millis(500);
                        false
                    }
                }
                None => true,
            };
            if give_up {
                playing = None;
            }
            continue;
        }
        if let Some(play) = playing.as_mut() {
            play.delivered = true;
            play.retries = 0;
            let thousandths = speed.load(Ordering::Relaxed).clamp(250, 2000) as f32 / 1000.0;
            let gap = Duration::from_millis(play.frame_ms);
            let gap = if play.hold {
                gap
            } else {
                gap.div_f32(thousandths)
            };
            let spent = began.elapsed();
            // If a frame already used up its slot, wait out another full slot.
            // Sending the next one immediately pegs a core trying to catch up.
            play.next_at = Instant::now() + next_frame_delay(gap, spent);
            let count = play.frames.len();
            play.index = (play.index + 1) % count.max(1);
        }
    }
}

/// Sleep until the next frame. A send that already ran long still waits a full
/// slot, so playback cannot busy-loop to catch up.
fn next_frame_delay(gap: Duration, spent: Duration) -> Duration {
    if gap > spent {
        gap - spent
    } else {
        gap
    }
}

fn dispatch(panel: &mut Option<Panel>, request: Request) {
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
        Request::ShowStill(_, reply) => {
            let _ = reply.send(Err(anyhow::anyhow!(
                "the picture was not handed to the pump"
            )));
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
    // `conn` is the handshake. Sending it again drops the live picture.
    let identity = panel.identity()?;
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

const ACCENT: egui::Color32 = egui::Color32::from_rgb(196, 54, 78);

pub fn run(start_in_tray: bool) -> eframe::Result<()> {
    if !settings::claim_single_instance() {
        return Ok(());
    }
    let (tray_tx, tray_rx) = std::sync::mpsc::sync_channel(64);
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([1000.0, 640.0])
        .with_min_inner_size([900.0, 540.0])
        .with_resizable(true)
        .with_decorations(true)
        .with_title("Pump screen");
    #[cfg(windows)]
    {
        viewport = viewport.with_transparent(true);
    }
    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };
    eframe::run_native(
        "Pump screen",
        options,
        Box::new(move |cc| {
            install_theme(&cc.egui_ctx);
            let _ = apply_window_material();
            let ctx = cc.egui_ctx.clone();
            settings::on_show_request({
                let ctx = ctx.clone();
                move || ctx.request_repaint()
            });
            Ok(Box::new(App::new(start_in_tray, tray_tx, tray_rx, ctx)))
        }),
    )
}

enum TrayAction {
    Show,
    Stop,
    Quit,
}

#[cfg(windows)]
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
    /// Window is parked off-screen. It stays visible to Windows so paints still arrive.
    in_tray: bool,
    resume_when_ready: bool,
    tray_rx: std::sync::mpsc::Receiver<TrayAction>,
    material_applied: bool,
    /// The saved window from the tall layout is resized once.
    window_fitted: bool,
}

struct TrayMenu {
    available: bool,
    #[cfg(windows)]
    _icon: tray_icon::TrayIcon,
    #[cfg(windows)]
    _show: tray_icon::menu::MenuItem,
    #[cfg(windows)]
    _stop: tray_icon::menu::MenuItem,
    #[cfg(windows)]
    _quit: tray_icon::menu::MenuItem,
    #[cfg(target_os = "linux")]
    _handle: Option<ksni::blocking::Handle<PumpTray>>,
}

enum JobResult {
    Snapshot(Result<Snapshot>),
    Done(Result<()>),
    /// The worker is already playing. The UI only needs the count for its status line.
    Playing(usize),
    Failed(String),
}

impl App {
    fn new(
        start_in_tray: bool,
        tray_tx: std::sync::mpsc::SyncSender<TrayAction>,
        tray_rx: std::sync::mpsc::Receiver<TrayAction>,
        ctx: egui::Context,
    ) -> Self {
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
            tray: TrayMenu::build(tray_tx, ctx),
            quit: false,
            hide_once: start_in_tray,
            in_tray: false,
            resume_when_ready,
            tray_rx,
            material_applied: false,
            window_fitted: false,
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

    fn show_window(&mut self, ctx: &egui::Context) {
        self.in_tray = false;
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
                TrayAction::Stop => {
                    self.worker.stop_playback();
                    self.status = "Stopped. The last frame stays until you quit.".into();
                }
                TrayAction::Quit => {
                    self.quit = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
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
                    self.status = if self.tray.available {
                        "Ready.".into()
                    } else {
                        "Ready. This desktop has no tray, so closing the window quits.".into()
                    };
                }
            }
            JobResult::Snapshot(Err(error)) => {
                self.busy = false;
                self.status = format!("{error:#}");
                self.maybe_resume();
            }
            JobResult::Done(Ok(())) => {
                self.status = "On the pump. It stays while this app is open.".into();
                self.pending = Some(spawn_snapshot(&self.worker));
                self.busy = true;
            }
            JobResult::Done(Err(error)) => {
                self.status = format!("{error:#}");
            }
            JobResult::Playing(count) => {
                let stay = if self.settings.close_to_tray {
                    "Closing the window leaves it running in the tray."
                } else {
                    "The window has to stay open."
                };
                self.status = format!("Playing {count} frames at {:.2}×. {stay}", self.speed);
            }
            JobResult::Failed(error) => {
                self.status = error;
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
                    self.preview =
                        Some(ctx.load_texture("preview", color, egui::TextureOptions::LINEAR));
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
        if settings::take_show_request() {
            self.show_window(ctx);
        }
        self.poll_tray(ctx);
        if self.hide_once && self.tray.available && park_pump_window(ctx) {
            self.hide_once = false;
            self.in_tray = true;
        } else if self.hide_once {
            if self.tray.available {
                ctx.request_repaint();
            } else {
                self.hide_once = false;
            }
        }
        if ctx.input(|input| input.viewport().close_requested())
            && !self.quit
            && self.settings.close_to_tray
            && self.tray.available
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            if park_pump_window(ctx) {
                self.in_tray = true;
            }
        }
        // Windows puts the window back on the taskbar unless the tool style is
        // applied again. Linux hides the window once; repeating that wakes the loop.
        #[cfg(windows)]
        if self.in_tray {
            if pump_window_is_on_screen() {
                self.in_tray = false;
            } else {
                let _ = park_pump_window(ctx);
            }
        }
        if !self.material_applied {
            self.material_applied = apply_window_material();
        }
        self.fit_window(ctx);
        self.poll(ctx);
        self.poll_preview(ctx);
        self.refresh_asus_flag();
        let waiting = self.pending.is_some() || self.preview_rx.is_some();
        if waiting {
            ctx.request_repaint_after(Duration::from_millis(200));
        } else if !self.in_tray {
            ctx.request_repaint_after(Duration::from_secs(3));
        }
        egui::CentralPanel::default()
            .frame(shell_frame())
            .show(ctx, |ui| {
                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        self.header(ui);
                        if self.asus_running {
                            ui.add_space(6.0);
                            ui.colored_label(
                                egui::Color32::from_rgb(255, 186, 110),
                                "Quit ROG STRIX LC & SLC IV Series. It is holding the pump USB connection.",
                            );
                        }
                        ui.add_space(8.0);
                        ui.columns_const(|[left, right]| {
                            self.picture_card(left, ctx);
                            self.controls_card(right);
                        });
                        ui.add_space(8.0);
                        ui.label(egui::RichText::new(&self.status).size(13.5));
                    });
            });
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        #[cfg(windows)]
        {
            [0.0, 0.0, 0.0, 0.0]
        }
        #[cfg(not(windows))]
        {
            egui::Color32::from_rgb(16, 14, 20).to_normalized_gamma_f32()
        }
    }
}

fn install_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = pump_visuals();
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 7.0);
    style.spacing.interact_size.y = 30.0;
    ctx.set_style(style);
}

fn pump_visuals() -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    visuals.window_fill = egui::Color32::TRANSPARENT;
    visuals.panel_fill = egui::Color32::TRANSPARENT;
    visuals.faint_bg_color = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 14);
    visuals.extreme_bg_color = egui::Color32::from_rgba_unmultiplied(18, 14, 20, 170);
    visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_rgb(176, 170, 176);
    visuals.widgets.noninteractive.bg_stroke.color =
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 28);
    let mut idle = visuals.widgets.inactive;
    idle.bg_fill = egui::Color32::from_rgb(58, 50, 58);
    idle.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 18);
    idle.bg_stroke = egui::Stroke::new(
        1.0_f32,
        egui::Color32::from_rgba_unmultiplied(255, 255, 255, 36),
    );
    idle.corner_radius = egui::CornerRadius::same(8);
    idle.fg_stroke.color = egui::Color32::from_rgb(240, 236, 238);
    let mut hovered = idle;
    hovered.weak_bg_fill = egui::Color32::from_rgba_unmultiplied(255, 255, 255, 30);
    hovered.bg_fill = egui::Color32::from_rgb(78, 66, 74);
    let mut active = hovered;
    active.bg_fill = ACCENT;
    active.weak_bg_fill = ACCENT;
    active.fg_stroke.color = egui::Color32::WHITE;
    visuals.widgets.inactive = idle;
    visuals.widgets.hovered = hovered;
    visuals.widgets.active = active;
    visuals.widgets.open = hovered;
    visuals.selection.bg_fill = egui::Color32::from_rgb(190, 52, 74);
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, egui::Color32::from_rgb(230, 140, 152));
    visuals.hyperlink_color = egui::Color32::from_rgb(255, 176, 186);
    visuals.window_corner_radius = egui::CornerRadius::same(12);
    visuals.menu_corner_radius = egui::CornerRadius::same(10);
    visuals
}

fn shell_frame() -> egui::Frame {
    #[cfg(windows)]
    let fill = egui::Color32::from_rgba_unmultiplied(16, 12, 18, 150);
    #[cfg(not(windows))]
    let fill = egui::Color32::from_rgb(16, 14, 20);
    egui::Frame::new()
        .fill(fill)
        .inner_margin(egui::Margin::same(16))
}

fn card_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(egui::Color32::from_rgba_unmultiplied(34, 28, 36, 214))
        .stroke(egui::Stroke::new(
            1.0_f32,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 32),
        ))
        .corner_radius(egui::CornerRadius::same(16))
        .inner_margin(egui::Margin::same(14))
        .outer_margin(egui::Margin::symmetric(4, 0))
        .shadow(egui::Shadow {
            offset: [0, 8],
            blur: 18,
            spread: 0,
            color: egui::Color32::from_black_alpha(80),
        })
}

fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    card_frame().show(ui, |ui| {
        ui.label(egui::RichText::new(title).strong().size(15.0));
        ui.add_space(8.0);
        add(ui);
    });
}

fn dim(text: &str) -> egui::RichText {
    egui::RichText::new(text).color(egui::Color32::from_rgb(186, 180, 186))
}

fn status_pill(ui: &mut egui::Ui, text: &str, color: egui::Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.22))
        .stroke(egui::Stroke::new(1.0_f32, color.gamma_multiply(0.5)))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::symmetric(8, 3))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).color(color).size(12.0));
        });
}

fn apply_window_material() -> bool {
    #[cfg(windows)]
    {
        return apply_windows_material();
    }
    #[cfg(not(windows))]
    {
        true
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
    let max_w = ui.available_width().min(280.0);
    let aspect = source_width / source_height;
    let (disp_w, disp_h) = if aspect >= 1.0 {
        (max_w, max_w / aspect)
    } else {
        (max_w * aspect, max_w)
    };
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(disp_w, disp_h), egui::Sense::click_and_drag());
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
        egui::Rect::from_min_max(
            egui::pos2(rect.min.x, crop_rect.min.y),
            crop_rect.left_bottom(),
        ),
        0.0,
        shade,
    );
    painter.rect_filled(
        egui::Rect::from_min_max(
            crop_rect.right_top(),
            egui::pos2(rect.max.x, crop_rect.max.y),
        ),
        0.0,
        shade,
    );
    painter.rect_stroke(
        crop_rect,
        2.0,
        egui::Stroke::new(2.0_f32, egui::Color32::from_rgb(255, 228, 232)),
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
    fn fit_window(&mut self, ctx: &egui::Context) {
        if self.window_fitted || self.in_tray {
            return;
        }
        self.window_fitted = true;
        let narrow = ctx.input(|input| {
            input
                .viewport()
                .inner_rect
                .map(|rect| rect.width() < 900.0)
                .unwrap_or(true)
        });
        if narrow {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(1000.0, 640.0)));
        }
    }

    fn header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 22.0), egui::Sense::hover());
            let mark = if self.snapshot.is_some() {
                ACCENT
            } else {
                egui::Color32::from_rgb(120, 116, 124)
            };
            ui.painter().circle_filled(rect.center(), 5.0, mark);
            ui.heading("Pump screen");
            ui.label(dim("ROG Strix LC IV"));
            let (text, color) = if self.snapshot.is_some() {
                ("Connected", egui::Color32::from_rgb(126, 196, 154))
            } else if self.busy {
                ("Connecting", egui::Color32::from_rgb(230, 186, 120))
            } else {
                ("Offline", egui::Color32::from_rgb(176, 170, 176))
            };
            status_pill(ui, text, color);
            if let Some(shot) = &self.snapshot {
                ui.label(dim(&format!(
                    "{}   {}   {} free",
                    shot.serial, shot.firmware, shot.free_space
                )));
            }
        });
    }

    fn picture_card(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        section(ui, "Picture", |ui| {
            ui.horizontal(|ui| {
                if ui.button("Choose file…").clicked() && !self.busy {
                    self.choose_file(ctx);
                }
                if let Some(path) = &self.chosen {
                    let name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("chosen file");
                    ui.label(egui::RichText::new(name).strong())
                        .on_hover_text(path.display().to_string());
                }
            });
            if let Some(preview) = &self.preview {
                ui.label(dim("Drag the square to crop."));
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
            }
            ui.horizontal(|ui| {
                let zoom_slider = ui.add(egui::Slider::new(&mut self.zoom, 1.0..=4.0).text("Zoom"));
                if zoom_slider.drag_stopped() {
                    self.persist();
                }
                if ui.button("Reset").clicked() {
                    self.zoom = 1.0;
                    self.pan_x = 0.0;
                    self.pan_y = 0.0;
                    self.persist();
                }
            });
            ui.horizontal(|ui| {
                let speed_slider = ui.add(
                    egui::Slider::new(&mut self.speed, 0.25..=2.0)
                        .fixed_decimals(2)
                        .text("Speed")
                        .suffix("×"),
                );
                if speed_slider.drag_stopped() {
                    self.worker.set_speed(self.speed);
                    self.persist();
                } else if speed_slider.changed() {
                    self.worker.set_speed(self.speed);
                }
            });
            ui.label(dim(
                "1× matches the video. Crop applies the next time you press Show.",
            ));
            ui.horizontal(|ui| {
                let can_send = self.chosen.is_some() && !self.busy;
                let show = egui::Button::new(
                    egui::RichText::new("Show on pump")
                        .strong()
                        .color(egui::Color32::WHITE),
                )
                .fill(ACCENT)
                .corner_radius(egui::CornerRadius::same(10))
                .min_size(egui::vec2(160.0, 34.0));
                if ui.add_enabled(can_send, show).clicked() {
                    self.send_chosen();
                }
                if ui.button("Stop video").clicked() {
                    self.worker.stop_playback();
                    self.status = "Stopped. The last frame stays until you quit.".into();
                }
            });
        });
    }

    fn controls_card(&mut self, ui: &mut egui::Ui) {
        section(ui, "Screen", |ui| {
            ui.horizontal(|ui| {
                ui.label(dim("Brightness"));
                let slider = ui.add(egui::Slider::new(&mut self.brightness, 0..=100).suffix("%"));
                if slider.drag_stopped() && !self.busy {
                    self.apply_brightness();
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(dim("Rotation"));
                let current = self.snapshot.as_ref().map(|shot| shot.rotation);
                for degrees in [0u16, 90, 180, 270] {
                    let selected = current == Some(degrees);
                    let mut button = egui::Button::new(format!("{degrees}°"));
                    if selected {
                        button = button.fill(ACCENT);
                    }
                    if ui.add_enabled(!self.busy, button).clicked() {
                        self.apply_rotation(degrees);
                    }
                }
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
        });
        ui.add_space(8.0);
        section(ui, "Startup and tray", |ui| {
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
                    sign_in_startup_label(),
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
            ui.label(dim(
                "Click the tray icon to show this window. Right-click it to stop or quit.",
            ));
        });
    }

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
                &[
                    "png", "jpg", "jpeg", "gif", "bmp", "mp4", "mov", "avi", "mkv", "webm",
                ],
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
        let crop = Crop {
            zoom: self.zoom,
            pan_x: self.pan_x,
            pan_y: self.pan_y,
        };
        self.worker.set_speed(self.speed);
        if media::is_video(&path) {
            self.status = "Preparing the video…".into();
            let player = self.worker.clone();
            thread::spawn(move || match video_jpegs(&path, &crop) {
                Ok(clip) => {
                    let count = clip.frames.len();
                    let frame_ms = clip.frame_ms;
                    // Play from this thread. The window may be parked off-screen, and a
                    // parked window must not have to paint before the pump gets frames.
                    if let Err(error) = player.play(clip.frames, frame_ms) {
                        let _ = tx.send(JobResult::Failed(format!("{error:#}")));
                        return;
                    }
                    let _ = tx.send(JobResult::Playing(count));
                }
                Err(error) => {
                    let _ = tx.send(JobResult::Failed(format!("{error:#}")));
                }
            });
        } else {
            self.status = "Sending the photo…".into();
            let worker_tx = self.worker.tx.clone();
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
    fn build(tx: std::sync::mpsc::SyncSender<TrayAction>, ctx: egui::Context) -> Self {
        #[cfg(windows)]
        {
            build_windows_tray(tx, ctx)
        }
        #[cfg(target_os = "linux")]
        {
            build_linux_tray(tx, ctx)
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = (tx, ctx);
            Self { available: false }
        }
    }
}

#[cfg(windows)]
fn build_windows_tray(tx: std::sync::mpsc::SyncSender<TrayAction>, ctx: egui::Context) -> TrayMenu {
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
    let show = MenuItem::with_id("show", "Show", true, None);
    let stop = MenuItem::with_id("stop", "Stop video", true, None);
    let quit = MenuItem::with_id("quit", "Quit", true, None);
    let menu = Menu::new();
    let _ = menu.append(&show);
    let _ = menu.append(&stop);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&quit);
    let show_id = show.id().clone();
    let stop_id = stop.id().clone();
    let quit_id = quit.id().clone();
    let click_tx = tx.clone();
    let click_ctx = ctx.clone();
    tray_icon::TrayIconEvent::set_event_handler(Some(move |event| {
        if tray_click_shows_window(&event) {
            reveal_pump_window();
            let _ = click_tx.try_send(TrayAction::Show);
        }
        click_ctx.request_repaint();
    }));
    tray_icon::menu::MenuEvent::set_event_handler(Some(
        move |event: tray_icon::menu::MenuEvent| {
            let action = if event.id == show_id {
                TrayAction::Show
            } else if event.id == stop_id {
                TrayAction::Stop
            } else if event.id == quit_id {
                TrayAction::Quit
            } else {
                return;
            };
            let _ = tx.try_send(action);
            ctx.request_repaint();
        },
    ));
    let icon = tray_icon_image();
    let tray = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip("Pump screen")
        .with_icon(icon)
        .build()
        .expect("the tray icon could not be created");
    TrayMenu {
        available: true,
        _icon: tray,
        _show: show,
        _stop: stop,
        _quit: quit,
    }
}

#[cfg(target_os = "linux")]
struct PumpTray {
    tx: std::sync::mpsc::SyncSender<TrayAction>,
    ctx: egui::Context,
}

#[cfg(target_os = "linux")]
impl PumpTray {
    fn send(&mut self, action: TrayAction) {
        let _ = self.tx.try_send(action);
        self.ctx.request_repaint();
    }
}

#[cfg(target_os = "linux")]
impl ksni::Tray for PumpTray {
    fn id(&self) -> String {
        "strix-lc-screen".into()
    }

    fn title(&self) -> String {
        "Pump screen".into()
    }

    fn icon_name(&self) -> String {
        "video-display".into()
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![pump_tray_pixmap()]
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Pump screen".into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayAction::Show);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Show".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayAction::Show)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Stop video".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayAction::Stop)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|tray: &mut Self| tray.send(TrayAction::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

#[cfg(target_os = "linux")]
fn build_linux_tray(tx: std::sync::mpsc::SyncSender<TrayAction>, ctx: egui::Context) -> TrayMenu {
    use ksni::blocking::TrayMethods;
    let tray = PumpTray { tx, ctx };
    match tray.spawn() {
        Ok(handle) => TrayMenu {
            available: true,
            _handle: Some(handle),
        },
        Err(error) => {
            eprintln!("tray icon: {error}");
            TrayMenu {
                available: false,
                _handle: None,
            }
        }
    }
}

fn sign_in_startup_label() -> &'static str {
    #[cfg(windows)]
    {
        "Start when I sign in to Windows"
    }
    #[cfg(not(windows))]
    {
        "Start when I sign in"
    }
}

#[cfg(target_os = "linux")]
fn pump_tray_pixmap() -> ksni::Icon {
    let size = 32i32;
    let mut data = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let dx = x - 15;
            let dy = y - 15;
            if dx * dx + dy * dy <= 13 * 13 {
                let index = ((y * size + x) * 4) as usize;
                data[index] = 255;
                data[index + 1] = 214;
                data[index + 2] = 64;
                data[index + 3] = 38;
            }
        }
    }
    ksni::Icon {
        width: size,
        height: size,
        data,
    }
}

/// Puts the window back on the primary monitor and on the taskbar.
pub fn reveal_pump_window() {
    #[cfg(windows)]
    unsafe {
        let Some(hwnd) = pump_hwnd() else {
            return;
        };
        set_taskbar_button(hwnd, true);
        if !pump_window_is_on_screen() {
            place_on_primary(hwnd);
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
    let _ = apply_window_material();
}

/// Moves the window off the virtual screen without hiding it.
///
/// `ShowWindow(SW_HIDE)` makes Windows drop `WM_PAINT`. eframe then leaves its
/// event loop in `Poll` after a missed redraw, which burns one core and never
/// reaches the code that starts playback.
fn park_pump_window(ctx: &egui::Context) -> bool {
    #[cfg(windows)]
    {
        let _ = ctx;
        unsafe {
            let Some(hwnd) = pump_hwnd() else {
                return false;
            };
            if !pump_window_is_on_screen() && is_tool_window(hwnd) {
                return true;
            }
            set_taskbar_button(hwnd, false);
            let rect = window_rect(hwnd);
            let width = (rect.right - rect.left).max(1);
            let height = (rect.bottom - rect.top).max(1);
            let x = GetSystemMetrics(76)
                .saturating_sub(width)
                .saturating_sub(80);
            let y = GetSystemMetrics(77)
                .saturating_sub(height)
                .saturating_sub(80);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                x,
                y,
                0,
                0,
                0x0010 | 0x0001 | 0x0004 | 0x0020,
            );
            // Refresh the taskbar button, then leave the window visible so paints still arrive.
            ShowWindow(hwnd, 0);
            ShowWindow(hwnd, 8);
            return true;
        }
    }
    #[cfg(not(windows))]
    {
        // Hiding is safe here. The one-core spin came from Win32 dropping WM_PAINT
        // after SW_HIDE. Playback itself runs on the worker thread.
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        true
    }
}

#[cfg(windows)]
fn pump_window_is_on_screen() -> bool {
    unsafe {
        let Some(hwnd) = pump_hwnd() else {
            return false;
        };
        let rect = window_rect(hwnd);
        let cx = rect.left.saturating_add(rect.right) / 2;
        let cy = rect.top.saturating_add(rect.bottom) / 2;
        let vx = GetSystemMetrics(76);
        let vy = GetSystemMetrics(77);
        let vw = GetSystemMetrics(78);
        let vh = GetSystemMetrics(79);
        cx >= vx && cy >= vy && cx < vx.saturating_add(vw) && cy < vy.saturating_add(vh)
    }
}

#[cfg(windows)]
fn pump_hwnd() -> Option<*mut std::ffi::c_void> {
    let title: Vec<u16> = "Pump screen"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let hwnd = unsafe { FindWindowW(std::ptr::null(), title.as_ptr()) };
    if hwnd.is_null() {
        None
    } else {
        Some(hwnd)
    }
}

#[cfg(windows)]
fn window_rect(hwnd: *mut std::ffi::c_void) -> Rect {
    let mut rect = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe {
        GetWindowRect(hwnd, &mut rect);
    }
    rect
}

#[cfg(windows)]
fn place_on_primary(hwnd: *mut std::ffi::c_void) {
    unsafe {
        let rect = window_rect(hwnd);
        let width = (rect.right - rect.left).max(200);
        let height = (rect.bottom - rect.top).max(200);
        let screen_w = GetSystemMetrics(0).max(width);
        let screen_h = GetSystemMetrics(1).max(height);
        let x = (screen_w - width) / 2;
        let y = (screen_h - height) / 2;
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            x,
            y,
            0,
            0,
            0x0010 | 0x0001 | 0x0004 | 0x0020,
        );
    }
}

#[cfg(windows)]
fn is_tool_window(hwnd: *mut std::ffi::c_void) -> bool {
    unsafe {
        const GWL_EXSTYLE: i32 = -20;
        const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
        const WS_EX_APPWINDOW: isize = 0x0004_0000;
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        style & WS_EX_TOOLWINDOW != 0 && style & WS_EX_APPWINDOW == 0
    }
}

#[cfg(windows)]
fn set_taskbar_button(hwnd: *mut std::ffi::c_void, show: bool) {
    unsafe {
        const GWL_EXSTYLE: i32 = -20;
        const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
        const WS_EX_APPWINDOW: isize = 0x0004_0000;
        let mut style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if show {
            style &= !WS_EX_TOOLWINDOW;
            style |= WS_EX_APPWINDOW;
        } else {
            style |= WS_EX_TOOLWINDOW;
            style &= !WS_EX_APPWINDOW;
        }
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
    }
}

#[cfg(windows)]
#[repr(C)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[cfg(windows)]
#[repr(C)]
struct DwmMargins {
    cx_left_width: i32,
    cx_right_width: i32,
    cy_top_height: i32,
    cy_bottom_height: i32,
}

#[cfg(windows)]
fn apply_windows_material() -> bool {
    let Some(hwnd) = pump_hwnd() else {
        return false;
    };
    unsafe {
        let dark: u32 = 1;
        // DWMWA_USE_IMMERSIVE_DARK_MODE
        let _ = DwmSetWindowAttribute(hwnd, 20, &dark as *const u32 as _, 4);
        if is_windows_11() {
            // DWMSBT_TRANSIENTWINDOW is the acrylic blur. Mica is 2, mica alt is 4.
            let backdrop: i32 = 3;
            let _ = DwmSetWindowAttribute(hwnd, 38, &backdrop as *const i32 as _, 4);
            // DWMWCP_ROUND
            let corners: i32 = 2;
            let _ = DwmSetWindowAttribute(hwnd, 33, &corners as *const i32 as _, 4);
            // COLORREF is 0x00BBGGRR.
            let border: u32 = 0x00302832;
            let caption: u32 = 0x00181016;
            let text: u32 = 0x00F2EEF0;
            let _ = DwmSetWindowAttribute(hwnd, 34, &border as *const u32 as _, 4);
            let _ = DwmSetWindowAttribute(hwnd, 35, &caption as *const u32 as _, 4);
            let _ = DwmSetWindowAttribute(hwnd, 36, &text as *const u32 as _, 4);
            let margins = DwmMargins {
                cx_left_width: -1,
                cx_right_width: -1,
                cy_top_height: -1,
                cy_bottom_height: -1,
            };
            let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        } else {
            enable_acrylic(hwnd);
        }
    }
    true
}

#[cfg(windows)]
fn is_windows_11() -> bool {
    #[repr(C)]
    struct OsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        csd: [u16; 128],
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
    }
    unsafe {
        let mut info = std::mem::zeroed::<OsVersionInfoW>();
        info.size = std::mem::size_of::<OsVersionInfoW>() as u32;
        if RtlGetVersion(&mut info) != 0 {
            return false;
        }
        info.build >= 22000
    }
}

#[cfg(windows)]
fn enable_acrylic(hwnd: *mut std::ffi::c_void) {
    #[repr(C)]
    struct AccentPolicy {
        state: u32,
        flags: u32,
        color: u32,
        animation: u32,
    }
    #[repr(C)]
    struct CompositionAttribute {
        attribute: u32,
        data: *mut std::ffi::c_void,
        size: usize,
    }
    unsafe extern "system" {
        fn GetModuleHandleW(name: *const u16) -> *mut std::ffi::c_void;
        fn GetProcAddress(module: *mut std::ffi::c_void, name: *const u8) -> *mut std::ffi::c_void;
    }
    type SetComposition =
        unsafe extern "system" fn(*mut std::ffi::c_void, *mut CompositionAttribute) -> i32;
    unsafe {
        let module_name: Vec<u16> = "user32.dll"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let module = GetModuleHandleW(module_name.as_ptr());
        if module.is_null() {
            return;
        }
        let proc = GetProcAddress(module, b"SetWindowCompositionAttribute\0".as_ptr());
        if proc.is_null() {
            return;
        }
        let set: SetComposition = std::mem::transmute(proc);
        let mut policy = AccentPolicy {
            state: 4,
            flags: 2,
            color: 0xCC1A1218,
            animation: 0,
        };
        let mut data = CompositionAttribute {
            attribute: 19,
            data: &mut policy as *mut AccentPolicy as *mut std::ffi::c_void,
            size: std::mem::size_of::<AccentPolicy>(),
        };
        let _ = set(hwnd, &mut data);
    }
}

#[cfg(windows)]
#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmSetWindowAttribute(
        hwnd: *mut std::ffi::c_void,
        attribute: u32,
        data: *const std::ffi::c_void,
        size: u32,
    ) -> i32;
    fn DwmExtendFrameIntoClientArea(hwnd: *mut std::ffi::c_void, margins: *const DwmMargins)
        -> i32;
}

#[cfg(windows)]
unsafe extern "system" {
    fn FindWindowW(class: *const u16, window: *const u16) -> *mut std::ffi::c_void;
    fn ShowWindow(hwnd: *mut std::ffi::c_void, cmd: i32) -> i32;
    fn SetForegroundWindow(hwnd: *mut std::ffi::c_void) -> i32;
    fn BringWindowToTop(hwnd: *mut std::ffi::c_void) -> i32;
    fn IsIconic(hwnd: *mut std::ffi::c_void) -> i32;
    fn GetForegroundWindow() -> *mut std::ffi::c_void;
    fn GetWindowThreadProcessId(hwnd: *mut std::ffi::c_void, process: *mut u32) -> u32;
    fn GetCurrentThreadId() -> u32;
    fn AttachThreadInput(from: u32, to: u32, attach: i32) -> i32;
    fn GetWindowRect(hwnd: *mut std::ffi::c_void, rect: *mut Rect) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn SetWindowPos(
        hwnd: *mut std::ffi::c_void,
        insert_after: *mut std::ffi::c_void,
        x: i32,
        y: i32,
        cx: i32,
        cy: i32,
        flags: u32,
    ) -> i32;
    fn GetWindowLongPtrW(hwnd: *mut std::ffi::c_void, index: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: *mut std::ffi::c_void, index: i32, value: isize) -> isize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_frame_that_just_played_is_the_one_before_the_cursor() {
        assert_eq!(last_sent_index(0, 0, true), None);
        assert_eq!(last_sent_index(0, 4, false), None);
        assert_eq!(last_sent_index(1, 4, true), Some(0));
        assert_eq!(last_sent_index(0, 4, true), Some(3));
        assert_eq!(last_sent_index(0, 1, true), Some(0));
    }

    #[test]
    fn a_slow_frame_still_waits_a_full_slot() {
        let gap = Duration::from_millis(125);
        assert_eq!(
            next_frame_delay(gap, Duration::from_millis(40)),
            Duration::from_millis(85)
        );
        assert_eq!(next_frame_delay(gap, Duration::from_millis(125)), gap);
        assert_eq!(next_frame_delay(gap, Duration::from_millis(400)), gap);
    }
}

#[cfg(windows)]
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
