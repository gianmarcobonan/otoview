//! egui front-end: live view, status, battery, LED, rotation, photos.

use crate::detect::{self, Detection, Detector, MIN_SCORE};
use crate::protocol::{Config, Device, Event, Family, Info, NetIface, candidate_ifaces, find_device_iface};
use crate::record::{Recorder, find_ffmpeg};
use crate::tr;
use crate::wifi::{self, is_endoscope};
use eframe::egui::{self, Color32, ColorImage, Key, RichText, TextureHandle, TextureOptions, Vec2};
use image::RgbImage;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, SyncSender, channel, sync_channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

const GREEN: Color32 = Color32::from_rgb(60, 200, 90);
const RED: Color32 = Color32::from_rgb(230, 70, 60);

enum Msg {
    Frame { image: ColorImage, rgb: Arc<RgbImage>, jpeg: Vec<u8>, angle: f32 },
    Event(Event),
}

/// Eardrum detection state: model loading/import, worker, latest results.
struct Ai {
    enabled: bool,
    loading: bool,
    input: Option<SyncSender<Arc<RgbImage>>>,
    model_tx: Sender<Result<Detector, String>>,
    model_rx: Receiver<Result<Detector, String>>,
    out_tx: Sender<Vec<Detection>>,
    out_rx: Receiver<Vec<Detection>>,
    detections: Vec<Detection>,
    at: Instant,
}

impl Ai {
    fn new() -> Ai {
        let (model_tx, model_rx) = channel();
        let (out_tx, out_rx) = channel();
        let mut ai = Ai {
            enabled: true,
            loading: false,
            input: None,
            model_tx,
            model_rx,
            out_tx,
            out_rx,
            detections: Vec::new(),
            at: Instant::now(),
        };
        let path = detect::model_path();
        if path.is_file() {
            ai.load_bg(move || Detector::load(std::fs::read(&path).map_err(|e| e.to_string())?));
        }
        ai
    }

    /// Model parsing/optimisation takes a moment: keep it off the UI thread.
    fn load_bg(&mut self, f: impl FnOnce() -> Result<Detector, String> + Send + 'static) {
        self.loading = true;
        let tx = self.model_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
    }

    /// Returns a message for the toast when a load/import finished.
    fn poll(&mut self, ctx: &egui::Context) -> Option<String> {
        while let Ok(d) = self.out_rx.try_recv() {
            self.detections = d;
            self.at = Instant::now();
        }
        let res = self.model_rx.try_recv().ok()?;
        self.loading = false;
        match res {
            Ok(det) => {
                // One inference at a time; frames arriving meanwhile are skipped.
                let (tx, rx) = sync_channel::<Arc<RgbImage>>(1);
                let (out, ctx) = (self.out_tx.clone(), ctx.clone());
                std::thread::spawn(move || {
                    for img in rx {
                        if let Ok(d) = det.detect(&img) {
                            let _ = out.send(d);
                            ctx.request_repaint();
                        }
                    }
                });
                self.input = Some(tx); // replaces (and ends) any previous worker
                Some(tr("Modello AI pronto", "AI model ready").into())
            }
            Err(e) => Some(format!("{} {e}", tr("Modello AI non valido:", "Invalid AI model:"))),
        }
    }

    fn feed(&self, img: &Arc<RgbImage>) {
        if let (true, Some(tx)) = (self.enabled, &self.input) {
            let _ = tx.try_send(img.clone());
        }
    }

    /// Detections above the app's threshold, if fresh.
    fn hits(&self) -> impl Iterator<Item = &Detection> {
        let fresh = self.enabled && self.input.is_some() && self.at.elapsed() < Duration::from_secs(1);
        self.detections.iter().filter(move |d| fresh && d.score >= MIN_SCORE)
    }
}

/// Owns the `Device` and handles several interfaces on the device subnet
/// (e.g. Ethernet to a home router that is also 192.168.1.1).
struct Net {
    cfg: Config,
    events: Sender<Event>,
    dev: Result<Arc<Device>, String>,
    candidates: Vec<NetIface>,
    /// Manual pick; None = automatic.
    choice: Option<NetIface>,
    finder: Option<Receiver<Option<NetIface>>>,
    last_scan: Instant,
    last_probe: Instant,
}

impl Net {
    fn new(cfg: Config, events: Sender<Event>) -> Net {
        let mut net = Net {
            dev: Err(String::new()),
            cfg,
            events,
            candidates: Vec::new(),
            choice: None,
            finder: None,
            last_scan: Instant::now(),
            last_probe: Instant::now() - Duration::from_secs(60),
        };
        net.scan();
        net.restart(None);
        net
    }

    fn scan(&mut self) {
        self.last_scan = Instant::now();
        if let IpAddr::V4(host) = self.cfg.host {
            self.candidates = candidate_ifaces(host);
        }
    }

    fn restart(&mut self, iface: Option<NetIface>) {
        if let Ok(d) = &self.dev {
            d.stop();
        }
        let cfg = Config { iface, ..self.cfg.clone() };
        // Shown in the window instead of panicking (e.g. port 61501 taken by another instance).
        self.dev = Device::start(cfg, self.events.clone()).map_err(|e| e.to_string());
        if let Ok(d) = &self.dev {
            d.open();
        }
    }

    fn conflict(&self) -> bool {
        self.candidates.len() > 1
    }

    fn current_iface(&self) -> Option<&NetIface> {
        self.dev.as_ref().ok().and_then(|d| d.iface.as_ref())
    }

    /// Automatic mode: while there is a conflict and no video, probe every candidate
    /// and rebind to the one where the endoscope answers. Returns a toast message.
    fn tick(&mut self) -> Option<String> {
        if self.last_scan.elapsed() > Duration::from_secs(3) {
            self.scan();
        }
        if let Some(rx) = &self.finder {
            match rx.try_recv() {
                Ok(found) => {
                    self.finder = None;
                    if let Some(i) = found.filter(|i| self.current_iface() != Some(i)) {
                        let msg = format!("{} {} ({})", tr("Endoscopio trovato sulla scheda", "Endoscope found on adapter"), i.name, i.ip);
                        self.restart(Some(i));
                        return Some(msg);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => self.finder = None,
            }
        }
        let connected = self.dev.as_ref().is_ok_and(|d| d.connected());
        let idle = self.finder.is_none() && self.last_probe.elapsed() > Duration::from_secs(3);
        if self.conflict() && self.choice.is_none() && !connected && idle {
            self.last_probe = Instant::now();
            let (tx, rx) = channel();
            let (host, cands) = (self.cfg.host, self.candidates.clone());
            std::thread::spawn(move || {
                let _ = tx.send(find_device_iface(host, &cands, Duration::from_secs(1)));
            });
            self.finder = Some(rx);
        }
        None
    }

    /// Warning + interface picker, only when several interfaces share the subnet.
    fn ui(&mut self, ui: &mut egui::Ui) {
        if !self.conflict() {
            return;
        }
        ui.separator();
        ui.label(RichText::new(tr("⚠ Più schede di rete sulla rete del dispositivo", "⚠ Several network adapters on the device network")).color(RED))
            .on_hover_text(tr(
                "Es. cavo Ethernet verso un router di casa che è anch'esso 192.168.1.1.\nIn automatico l'app usa la scheda su cui risponde l'endoscopio.",
                "E.g. an Ethernet cable to a home router that is also 192.168.1.1.\nIn automatic mode the app uses the adapter where the endoscope answers.",
            ));
        let label = |i: &NetIface| format!("{} ({})", i.name, i.ip);
        let selected = self.choice.as_ref().map_or(tr("Automatica", "Automatic").to_string(), label);
        let mut pick: Option<Option<NetIface>> = None;
        egui::ComboBox::from_label(tr("Scheda", "Adapter")).selected_text(selected).show_ui(ui, |ui| {
            if ui.selectable_label(self.choice.is_none(), tr("Automatica", "Automatic")).clicked() {
                pick = Some(None);
            }
            for c in &self.candidates {
                if ui.selectable_label(self.choice.as_ref() == Some(c), label(c)).clicked() {
                    pick = Some(Some(c.clone()));
                }
            }
        });
        if let Some(p) = pick {
            self.choice = p.clone();
            self.last_probe = Instant::now() - Duration::from_secs(60); // probe right away in auto
            self.restart(p);
        }
    }
}

enum WifiMsg {
    Current(Option<String>),
    Found(Result<Vec<String>, String>),
    Done(Result<(), String>),
}

/// The PC must join the endoscope's own Wi-Fi. Scans for it while there is no video,
/// offers "Connect", and remembers the previous network to go back to (internet).
struct Wifi {
    tx: Sender<WifiMsg>,
    rx: Receiver<WifiMsg>,
    current: Option<String>,
    /// Last non-endoscope network seen.
    // ponytail: in memory only; persist it if users close the app while on the endoscope Wi-Fi.
    home: Option<String>,
    found: Vec<String>,
    scanning: bool,
    polling: bool,
    /// Running connect/disconnect, shown with a spinner.
    busy: Option<String>,
    error: Option<String>,
    last_poll: Instant,
    last_scan: Option<Instant>,
}

impl Wifi {
    fn new() -> Wifi {
        let (tx, rx) = channel();
        Wifi {
            tx,
            rx,
            current: None,
            home: None,
            found: Vec::new(),
            scanning: false,
            polling: false,
            busy: None,
            error: None,
            last_poll: Instant::now() - Duration::from_secs(60),
            last_scan: None,
        }
    }

    fn bg(&self, ctx: &egui::Context, f: impl FnOnce() -> WifiMsg + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(f());
            ctx.request_repaint();
        });
    }

    fn on_endoscope(&self) -> bool {
        self.current.as_deref().is_some_and(is_endoscope)
    }

    fn tick(&mut self, ctx: &egui::Context, video: bool) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                WifiMsg::Current(c) => {
                    self.polling = false;
                    if let Some(s) = c.as_ref().filter(|s| !is_endoscope(s)) {
                        self.home = Some(s.clone());
                    }
                    self.current = c;
                }
                WifiMsg::Found(r) => {
                    self.scanning = false;
                    self.last_scan = Some(Instant::now());
                    match r {
                        Ok(f) => self.found = f,
                        Err(e) => self.error = Some(e),
                    }
                }
                WifiMsg::Done(r) => {
                    self.busy = None;
                    self.error = r.err();
                    self.last_poll = Instant::now() - Duration::from_secs(60); // refresh now
                }
            }
        }
        if !self.polling && self.last_poll.elapsed() > Duration::from_secs(3) {
            self.polling = true;
            self.last_poll = Instant::now();
            self.bg(ctx, || WifiMsg::Current(wifi::current_ssid()));
        }
        let due = self.last_scan.is_none_or(|t| t.elapsed() > Duration::from_secs(15));
        if !video && !self.on_endoscope() && !self.scanning && self.busy.is_none() && due {
            self.scan(ctx);
        }
    }

    fn scan(&mut self, ctx: &egui::Context) {
        self.scanning = true;
        self.error = None;
        self.bg(ctx, || WifiMsg::Found(wifi::scan()));
    }

    fn connect(&mut self, ctx: &egui::Context, ssid: String) {
        self.busy = Some(format!("{} {ssid}…", tr("Connessione a", "Connecting to")));
        self.error = None;
        self.bg(ctx, move || WifiMsg::Done(wifi::connect_open(&ssid)));
    }

    fn go_home(&mut self, ctx: &egui::Context) {
        let Some(home) = self.home.clone() else { return };
        self.busy = Some(format!("{} {home}…", tr("Ritorno a", "Switching back to")));
        self.error = None;
        self.bg(ctx, move || WifiMsg::Done(wifi::connect_known(&home)));
    }
}

pub struct App {
    net: Net,
    wifi: Wifi,
    rx: Receiver<Msg>,
    tex: Option<TextureHandle>,
    jpeg: Vec<u8>,
    angle: f32,
    info: Option<(Family, Info)>,
    battery: Option<u8>,
    connected: bool,
    led: u8,
    auto_rotate: bool,
    invert: bool,
    quarter_turns: u8,
    fps: u32,
    frames: u32,
    fps_since: Instant,
    toast: Option<(String, Instant)>,
    ffmpeg: Option<PathBuf>,
    recorder: Option<(Recorder, Instant)>,
    finishing: Vec<std::thread::JoinHandle<Result<PathBuf, String>>>,
    ai: Ai,
}

/// Decodes JPEGs off the UI thread and wakes the UI.
fn spawn_decoder(ctx: egui::Context, events: Receiver<Event>, out: Sender<Msg>) {
    std::thread::spawn(move || {
        for ev in events {
            let msg = match ev {
                Event::Frame { jpeg, angle } => {
                    match image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg) {
                        Ok(img) => {
                            let rgb = img.to_rgb8();
                            let size = [rgb.width() as usize, rgb.height() as usize];
                            let image = ColorImage::from_rgb(size, rgb.as_raw());
                            Msg::Frame { image, rgb: Arc::new(rgb), jpeg, angle }
                        }
                        Err(_) => continue, // corrupt frame (lost fragment): skip
                    }
                }
                other => Msg::Event(other),
            };
            if out.send(msg).is_err() {
                return;
            }
            ctx.request_repaint();
        }
    });
}

pub fn photos_dir() -> PathBuf {
    // ponytail: ignores XDG_PICTURES_DIR / localised folder names; add `dirs` if users complain.
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).unwrap_or_default();
    PathBuf::from(home).join("Pictures").join("OtoView")
}

fn open_in_file_manager(path: &PathBuf) {
    let cmd = if cfg!(windows) { "explorer" } else if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let _ = std::process::Command::new(cmd).arg(path).spawn();
}

impl App {
    pub fn new(ctx: egui::Context, cfg: Config) -> Self {
        let (events_tx, events) = channel();
        let (tx, rx) = channel();
        spawn_decoder(ctx, events, tx);
        App {
            net: Net::new(cfg, events_tx),
            wifi: Wifi::new(),
            rx,
            tex: None,
            jpeg: Vec::new(),
            angle: 0.0,
            info: None,
            battery: None,
            connected: false,
            led: 50,
            auto_rotate: true,
            invert: false,
            quarter_turns: 0,
            fps: 0,
            frames: 0,
            fps_since: Instant::now(),
            toast: None,
            ffmpeg: find_ffmpeg(),
            recorder: None,
            finishing: Vec::new(),
            ai: Ai::new(),
        }
    }

    fn toggle_recording(&mut self) {
        if let Some((rec, _)) = self.recorder.take() {
            self.finishing.push(rec.stop());
            return;
        }
        let Some(ffmpeg) = &self.ffmpeg else { return };
        let dir = photos_dir();
        let name = chrono::Local::now().format("VID_%Y%m%d_%H%M%S.mp4").to_string();
        // ponytail: records frames as received (no rotation burned in).
        let started = std::fs::create_dir_all(&dir).and_then(|_| Recorder::start(ffmpeg, dir.join(name)));
        match started {
            Ok(rec) => self.recorder = Some((rec, Instant::now())),
            Err(e) => self.toast = Some((format!("{} {e}", tr("Impossibile avviare ffmpeg:", "Cannot start ffmpeg:")), Instant::now())),
        }
    }

    /// Reports recordings whose ffmpeg has finished writing.
    fn poll_finished(&mut self) {
        let (done, pending) = std::mem::take(&mut self.finishing).into_iter().partition(|h| h.is_finished());
        self.finishing = pending;
        for h in done {
            let text = match h.join() {
                Ok(Ok(p)) => format!("{} {}", tr("Video salvato:", "Video saved:"), p.file_name().unwrap_or_default().to_string_lossy()),
                Ok(Err(e)) => format!("{} {e}", tr("Errore registrazione:", "Recording error:")),
                Err(_) => tr("Errore registrazione", "Recording error").to_string(),
            };
            self.toast = Some((text, Instant::now()));
        }
    }

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Msg::Frame { image, rgb, jpeg, angle } => {
                    self.frames += 1;
                    self.ai.feed(&rgb);
                    let rec_died = self.recorder.as_ref().is_some_and(|(r, _)| !r.push(&jpeg));
                    if rec_died {
                        self.toggle_recording(); // ffmpeg exited: stop() reports why
                    }
                    self.jpeg = jpeg;
                    self.angle = angle;
                    match &mut self.tex {
                        Some(t) => t.set(image, TextureOptions::LINEAR),
                        None => self.tex = Some(ctx.load_texture("live", image, TextureOptions::LINEAR)),
                    }
                }
                Msg::Event(Event::Info(f, i)) => self.info = Some((f, i)),
                Msg::Event(Event::Battery(b)) => self.battery = Some(b),
                Msg::Event(Event::Connected(c)) => self.connected = c,
                Msg::Event(Event::Frame { .. }) => {}
            }
        }
        if self.fps_since.elapsed() >= Duration::from_secs(1) {
            self.fps = self.frames;
            self.frames = 0;
            self.fps_since = Instant::now();
        }
    }

    fn rotation_rad(&self) -> f32 {
        // Unverified on hardware: units assumed degrees, direction togglable via `invert`.
        let auto = if self.auto_rotate { self.angle } else { 0.0 };
        let auto = if self.invert { -auto } else { auto };
        (auto + 90.0 * self.quarter_turns as f32).to_radians()
    }

    fn take_photo(&mut self) {
        if self.jpeg.is_empty() {
            return;
        }
        let dir = photos_dir();
        let name = chrono::Local::now().format("IMG_%Y%m%d_%H%M%S_%3f.jpg").to_string();
        // ponytail: saves the frame as received (unrotated, no re-encode).
        let result = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(dir.join(&name), &self.jpeg));
        let text = match result {
            Ok(()) => format!("{} {name}", tr("Foto salvata:", "Photo saved:")),
            Err(e) => format!("{} {e}", tr("Errore salvataggio foto:", "Cannot save photo:")),
        };
        self.toast = Some((text, Instant::now()));
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (dot, label) = if self.connected {
                (GREEN, tr("Connesso", "Connected"))
            } else {
                (Color32::from_rgb(220, 160, 40), tr("In attesa del dispositivo", "Waiting for the device"))
            };
            ui.label(RichText::new("●").color(dot));
            ui.label(label);
            if let Some((f, i)) = &self.info {
                ui.separator();
                ui.label(format!("{}  {}×{}  {} fps", i.ssid, i.width, i.height, self.fps));
                ui.weak(format!("({f:?}, fw {})", i.firmware));
            }
            if let Some(i) = self.net.current_iface() {
                ui.weak(format!("via {}", i.name));
            }
            if self.ai.enabled && self.ai.input.is_some() && self.tex.is_some() {
                ui.separator();
                match self.ai.hits().map(|d| d.score).reduce(f32::max) {
                    Some(s) => ui.label(RichText::new(format!("{} {:.0}%", tr("Timpano rilevato", "Eardrum detected"), s * 100.0)).color(GREEN)),
                    None => ui.weak(tr("Timpano non rilevato", "No eardrum detected")),
                };
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                match self.battery {
                    Some(101) => {
                        ui.label(tr("Batteria: in carica", "Battery: charging"));
                    }
                    Some(b) => {
                        let color = if b <= 15 { RED } else { ui.visuals().text_color() };
                        ui.label(RichText::new(format!("{} {b}%", tr("Batteria:", "Battery:"))).color(color));
                    }
                    None => {}
                }
                if let (true, Some(home), None) = (self.wifi.on_endoscope(), self.wifi.home.clone(), &self.wifi.busy) {
                    ui.separator();
                    let b = ui.button(format!("{} {home}", tr("Torna a", "Back to"))).on_hover_text(tr(
                        "Lascia il Wi-Fi dell'endoscopio e torna alla rete di prima (internet).",
                        "Leave the endoscope Wi-Fi and go back to the previous network (internet).",
                    ));
                    if b.clicked() {
                        self.wifi.go_home(ui.ctx());
                    }
                }
            });
        });
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button(tr("Apri cartella", "Open folder")).clicked() {
                let dir = photos_dir();
                let _ = std::fs::create_dir_all(&dir);
                open_in_file_manager(&dir);
            }
            // Device controls only make sense with video (a recording keeps them visible).
            if !self.connected && self.recorder.is_none() {
                return;
            }
            ui.separator();
            let can_shoot = !self.jpeg.is_empty();
            if ui.add_enabled(can_shoot, egui::Button::new(tr("Scatta foto (Spazio)", "Take photo (Space)"))).clicked() {
                self.take_photo();
            }
            let rec_label = match &self.recorder {
                Some((_, t)) => {
                    let s = t.elapsed().as_secs();
                    RichText::new(format!("● {} {:02}:{:02} (R)", tr("Stop REC", "Stop REC"), s / 60, s % 60)).color(RED)
                }
                None => RichText::new(tr("Registra video (R)", "Record video (R)")),
            };
            let can_rec = self.ffmpeg.is_some() && (self.recorder.is_some() || can_shoot);
            let rec = ui.add_enabled(can_rec, egui::Button::new(rec_label));
            let rec = if self.ffmpeg.is_none() { rec.on_disabled_hover_text(tr("ffmpeg non trovato", "ffmpeg not found")) } else { rec };
            if rec.clicked() {
                self.toggle_recording();
            }
            ui.separator();
            ui.label(tr("Luce", "Light"));
            let led = ui.add(egui::Slider::new(&mut self.led, 0..=100).suffix("%"));
            if led.changed()
                && let Ok(d) = &self.net.dev {
                    d.set_led(self.led);
                }
            ui.separator();
            ui.checkbox(&mut self.auto_rotate, tr("Rotazione automatica", "Auto rotate"));
            ui.checkbox(&mut self.invert, tr("Inverti verso", "Reverse direction"));
            if ui.button(tr("Ruota 90°", "Rotate 90°")).clicked() {
                self.quarter_turns = (self.quarter_turns + 1) % 4;
            }
            ui.separator();
            if self.ai.loading {
                ui.spinner();
                ui.label(tr("Caricamento modello AI…", "Loading AI model…"));
            } else if self.ai.input.is_some() {
                ui.checkbox(&mut self.ai.enabled, tr("Rilevamento timpano (AI)", "Eardrum detection (AI)"));
            } else {
                ui.weak(tr("AI: trascina qui l'APK originale per attivarla", "AI: drop the original APK here to enable it"))
                    .on_hover_text(tr(
                        "Il modello non è incluso: viene estratto dall'APK Endscope Tool (assets/data/core.db).",
                        "The model is not bundled: it is extracted from the Endscope Tool APK (assets/data/core.db).",
                    ));
            }
        });
    }

    /// Shown instead of the video while there is no signal.
    fn connect_panel(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        ui.vertical_centered(|ui| {
            ui.add_space((ui.available_height() * 0.18).max(16.0));
            if let Err(e) = &self.net.dev {
                ui.label(RichText::new(format!("{} {e}", tr("Impossibile aprire la rete:", "Cannot open the network:"))).color(RED));
                return;
            }
            ui.label(RichText::new(tr("Collega l'endoscopio", "Connect the endoscope")).size(28.0).strong());
            ui.add_space(6.0);
            ui.label(RichText::new(tr(
                "Accendi l'endoscopio, poi collegati alla sua rete Wi-Fi.",
                "Turn on the endoscope, then join its Wi-Fi network.",
            )).size(16.0));
            ui.add_space(24.0);
            let w = &mut self.wifi;
            let big = |t: String| egui::Button::new(RichText::new(t).size(18.0)).min_size(Vec2::new(320.0, 46.0));
            if let Some(b) = &w.busy {
                ui.spinner();
                ui.label(RichText::new(b).size(16.0));
            } else if let Some(cur) = w.current.clone().filter(|c| is_endoscope(c)) {
                ui.spinner();
                ui.label(RichText::new(format!("{} {cur}", tr("Collegato a", "Connected to"))).size(16.0));
                ui.weak(tr("In attesa del video…", "Waiting for video…"));
            } else if !w.found.is_empty() {
                for ssid in w.found.clone() {
                    if ui.add(big(format!("{} {ssid}", tr("Connetti a", "Connect to")))).clicked() {
                        w.connect(&ctx, ssid);
                    }
                    ui.add_space(8.0);
                }
            } else if w.scanning {
                ui.spinner();
                ui.label(RichText::new(tr("Ricerca degli endoscopi vicini…", "Looking for nearby endoscopes…")).size(16.0));
            } else {
                ui.label(RichText::new(tr(
                    "Nessun endoscopio trovato.\nControlla che sia acceso e vicino al computer.",
                    "No endoscope found.\nCheck that it is on and close to the computer.",
                )).size(16.0));
            }
            ui.add_space(24.0);
            let idle = !w.scanning && w.busy.is_none();
            if ui.add_enabled(idle, egui::Button::new(tr("Cerca di nuovo", "Search again"))).clicked() {
                w.scan(&ctx);
            }
            ui.add_space(4.0);
            if ui.link(tr("Apri le impostazioni Wi-Fi", "Open Wi-Fi settings")).clicked() {
                wifi::open_settings();
            }
            if let Some(e) = &w.error {
                ui.add_space(12.0);
                ui.label(RichText::new(format!("{} {e}", tr("Problema con il Wi-Fi:", "Wi-Fi problem:"))).color(RED));
            }
        });
    }

    fn live_view(&mut self, ui: &mut egui::Ui) {
        let avail = ui.available_rect_before_wrap();
        let Some(tex) = self.tex.as_ref().filter(|_| self.connected) else {
            self.connect_panel(ui);
            return;
        };
        // Largest size whose rotated bounding box still fits the panel.
        let rot = self.rotation_rad();
        let (c, s) = (rot.cos().abs(), rot.sin().abs());
        let [w, h] = tex.size().map(|v| v as f32);
        let scale = (avail.width() / (w * c + h * s)).min(avail.height() / (w * s + h * c));
        let rect = egui::Rect::from_center_size(avail.center(), Vec2::new(w, h) * scale);
        ui.painter().rect_filled(avail, 0.0, Color32::BLACK);
        egui::Image::from_texture(tex).rotate(rot, Vec2::splat(0.5)).paint_at(ui, rect);
        // Detection boxes in normalised image coords, rotated like the image.
        let to_screen = |x: f32, y: f32| {
            let p = Vec2::new((x.clamp(0.0, 1.0) - 0.5) * rect.width(), (y.clamp(0.0, 1.0) - 0.5) * rect.height());
            rect.center() + egui::emath::Rot2::from_angle(rot) * p
        };
        for d in self.ai.hits() {
            let pts = vec![
                to_screen(d.xmin, d.ymin),
                to_screen(d.xmax, d.ymin),
                to_screen(d.xmax, d.ymax),
                to_screen(d.xmin, d.ymax),
            ];
            let label_at = pts[0];
            ui.painter().add(egui::Shape::closed_line(pts, egui::Stroke::new(3.0, GREEN)));
            ui.painter().text(
                label_at,
                egui::Align2::LEFT_BOTTOM,
                format!("{} {:.0}%", tr("Timpano", "Eardrum"), d.score * 100.0),
                egui::FontId::proportional(16.0),
                GREEN,
            );
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain(&ctx);
        self.poll_finished();
        self.wifi.tick(&ctx, self.connected);
        if let Some(msg) = self.ai.poll(&ctx).or_else(|| self.net.tick()) {
            self.toast = Some((msg, Instant::now()));
        }
        let dropped = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        if let Some(path) = dropped {
            self.ai.load_bg(move || detect::import(&path));
        }
        if ctx.input(|i| i.key_pressed(Key::Space)) {
            self.take_photo();
        }
        if ctx.input(|i| i.key_pressed(Key::R)) && self.ffmpeg.is_some() && (self.recorder.is_some() || !self.jpeg.is_empty()) {
            self.toggle_recording();
        }
        egui::Panel::top("status").show(ui, |ui| self.status_bar(ui));
        egui::Panel::bottom("controls").show(ui, |ui| {
            ui.add_space(4.0);
            self.controls(ui);
            ui.horizontal(|ui| self.net.ui(ui));
            if let Some((text, at)) = &self.toast {
                if at.elapsed() < Duration::from_secs(4) {
                    ui.label(text);
                } else {
                    self.toast = None;
                }
            }
            ui.add_space(4.0);
        });
        egui::CentralPanel::default().show(ui, |ui| self.live_view(ui));
        // Keep the fps counter and "signal lost" overlay fresh without video.
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Finalise an open recording so the MP4 is playable after closing the window.
        if let Some((rec, _)) = self.recorder.take() {
            self.finishing.push(rec.stop());
        }
        for h in self.finishing.drain(..) {
            let _ = h.join();
        }
        if let Ok(d) = &self.net.dev {
            d.stop();
        }
    }
}
