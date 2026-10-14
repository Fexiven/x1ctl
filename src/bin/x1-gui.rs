//! x1-gui — desktop control panel and tray icon for the Sound Blaster X1.
//!
//! A pure client of the `x1d` daemon (D-Bus `io.github.x1ctl`):
//!   x1-gui          the control window
//!   x1-gui --tray   a StatusNotifier tray icon (KDE/Plasma, GNOME with AppIndicator, …)
//!
//! Both follow device changes live (Super X-Fi button, volume knob, other clients).

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, CornerRadius, RichText, Stroke, Vec2};
use x1::device::State;
use x1::{presets, proto};
use zbus::blocking::{Connection, Proxy};

const APP_ID: &str = "io.github.x1ctl.X1Gui";
const GUI_BUS_NAME: &str = "io.github.x1ctl.Gui";
/// The X1 moves its headphone level in 440/256 dB steps.
const VOL_STEP: f64 = proto::LEVEL_STEP_DB;

// =============================================================================================
// Client side of x1d
// =============================================================================================

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Daemon {
    Unknown,
    NotRunning,
    Running,
}

struct Shared {
    daemon: Daemon,
    snap: Option<State>,
    error: Option<(String, Instant)>,
}

#[derive(Debug, Clone)]
enum Cmd {
    Sxfi(bool),
    EqOn(bool),
    Band(u8, f64),
    Preamp(f64),
    Preset(String),
    Volume(f64),
    Mute(bool),
    FactoryReset,
    StartDaemon,
}

type Notify = Arc<Mutex<Vec<Box<dyn Fn() + Send>>>>;

#[derive(Clone)]
struct Client {
    shared: Arc<Mutex<Shared>>,
    tx: Sender<Cmd>,
    notify: Notify,
}

impl Client {
    fn start() -> Self {
        let shared = Arc::new(Mutex::new(Shared {
            daemon: Daemon::Unknown,
            snap: None,
            error: None,
        }));
        let notify: Notify = Arc::new(Mutex::new(Vec::new()));
        let (tx, rx) = mpsc::channel();
        let c = Client { shared, tx, notify };
        {
            let c = c.clone();
            std::thread::Builder::new()
                .name("x1-cmd".into())
                .spawn(move || c.cmd_loop(rx))
                .unwrap();
        }
        {
            let c = c.clone();
            std::thread::Builder::new()
                .name("x1-poll".into())
                .spawn(move || c.poll_loop())
                .unwrap();
        }
        {
            let c = c.clone();
            std::thread::Builder::new()
                .name("x1-signals".into())
                .spawn(move || c.signal_loop())
                .unwrap();
        }
        c
    }

    fn on_change(&self, f: impl Fn() + Send + 'static) {
        self.notify.lock().unwrap().push(Box::new(f));
    }

    fn fire(&self) {
        for f in self.notify.lock().unwrap().iter() {
            f();
        }
    }

    fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }

    fn snapshot(&self) -> (Daemon, Option<State>, Option<String>) {
        let s = self.shared.lock().unwrap();
        let err = s
            .error
            .as_ref()
            .filter(|(_, t)| t.elapsed() < Duration::from_secs(6))
            .map(|(e, _)| e.clone());
        (s.daemon, s.snap.clone(), err)
    }

    fn set_state(&self, daemon: Daemon, snap: Option<State>) {
        {
            let mut s = self.shared.lock().unwrap();
            s.daemon = daemon;
            if snap.is_some() || daemon != Daemon::Running {
                s.snap = snap;
            }
        }
        self.fire();
    }

    fn proxy(conn: &Connection) -> zbus::Result<Proxy<'static>> {
        zbus::blocking::proxy::Builder::<Proxy<'static>>::new(conn)
            .destination(x1::DBUS_NAME)?
            .path(x1::DBUS_PATH)?
            .interface(x1::DBUS_IFACE)?
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
    }

    fn fetch(conn: &Connection) -> (Daemon, Option<State>) {
        let up = zbus::blocking::fdo::DBusProxy::new(conn)
            .and_then(|d| {
                d.name_has_owner(x1::DBUS_NAME.try_into().unwrap())
                    .map_err(Into::into)
            })
            .unwrap_or(false);
        if !up {
            return (Daemon::NotRunning, None);
        }
        let st = Self::proxy(conn)
            .and_then(|p| p.get_property::<String>("StateJSON"))
            .ok()
            .and_then(|j| serde_json::from_str::<State>(&j).ok());
        (Daemon::Running, st)
    }

    /// Slow safety net: notices the daemon starting/stopping and resyncs every few seconds.
    fn poll_loop(&self) {
        loop {
            match Connection::session() {
                Ok(conn) => loop {
                    let (d, s) = Self::fetch(&conn);
                    self.set_state(d, s);
                    std::thread::sleep(Duration::from_secs(if d == Daemon::Running {
                        5
                    } else {
                        2
                    }));
                },
                Err(e) => {
                    self.error(format!("no session D-Bus: {e}"));
                    std::thread::sleep(Duration::from_secs(5));
                }
            }
        }
    }

    /// Fast path: x1d publishes `StateJSON` with every change; apply it as it arrives.
    fn signal_loop(&self) {
        loop {
            let res = (|| -> zbus::Result<()> {
                let conn = Connection::session()?;
                let props = zbus::blocking::fdo::PropertiesProxy::builder(&conn)
                    .destination(x1::DBUS_NAME)?
                    .path(x1::DBUS_PATH)?
                    .build()?;
                for sig in props.receive_properties_changed()? {
                    let Ok(args) = sig.args() else { continue };
                    if let Some(v) = args.changed_properties().get("StateJSON") {
                        if let Ok(j) = <&str>::try_from(v) {
                            if let Ok(st) = serde_json::from_str::<State>(j) {
                                self.set_state(Daemon::Running, Some(st));
                            }
                        }
                    }
                }
                Ok(())
            })();
            if let Err(e) = res {
                log::debug!("signal loop: {e}");
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    fn error(&self, e: String) {
        log::warn!("{e}");
        self.shared.lock().unwrap().error = Some((e, Instant::now()));
        self.fire();
    }

    fn cmd_loop(&self, rx: Receiver<Cmd>) {
        let mut conn: Option<Connection> = None;
        while let Ok(cmd) = rx.recv() {
            if let Cmd::StartDaemon = cmd {
                let ok = std::process::Command::new("systemctl")
                    .args(["--user", "start", "x1d.service"])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                if !ok {
                    self.error("could not start x1d (systemctl --user start x1d)".into());
                }
                continue;
            }
            if conn.is_none() {
                conn = Connection::session().ok();
            }
            let Some(c) = conn.as_ref() else {
                self.error("no session D-Bus".into());
                continue;
            };
            let r = Self::proxy(c).and_then(|p| {
                let r = match &cmd {
                    Cmd::Sxfi(on) => p.call_method("SetSuperXFi", &(*on,)),
                    Cmd::EqOn(on) => p.call_method("SetEqEnabled", &(*on,)),
                    Cmd::Band(i, db) => p.call_method("SetBand", &(*i, *db)),
                    Cmd::Preamp(db) => p.call_method("SetPreamp", &(*db,)),
                    Cmd::Preset(n) => p.call_method("ApplyPreset", &(n.as_str(),)),
                    Cmd::Volume(db) => p.call_method("SetVolume", &(*db,)),
                    Cmd::Mute(on) => p.call_method("SetMuted", &(*on,)),
                    Cmd::FactoryReset => p.call_method("FactoryReset", &()),
                    Cmd::StartDaemon => unreachable!(),
                };
                r.map(|_| ())
            });
            if let Err(e) = r {
                let msg = match e {
                    zbus::Error::MethodError(_, Some(m), _) => m,
                    other => other.to_string(),
                };
                self.error(msg);
                conn = None;
            }
            // pick up the result right away (signals also arrive, this just makes it snappy)
            if let Some(c) = conn.as_ref() {
                let (d, s) = Self::fetch(c);
                self.set_state(d, s);
            }
        }
    }
}

/// Which preset (if any) the current EQ equals.
fn matching_preset(st: &State) -> Option<String> {
    let bands: Vec<f32> = st.bands.iter().map(|b| b.unwrap_or(f32::NAN)).collect();
    let pre = st.preamp.unwrap_or(0.0);
    let user = presets::load_user();
    let ordered = user.into_iter().chain(presets::builtin_ordered());
    for (name, p) in ordered {
        if (p.preamp - pre).abs() < 0.05
            && p.bands
                .iter()
                .zip(&bands)
                .all(|(a, b)| (a - b).abs() < 0.05)
        {
            return Some(name);
        }
    }
    None
}

// =============================================================================================
// Window
// =============================================================================================

type ImportSlot = Arc<Mutex<Option<Result<usize, String>>>>;

struct Edit {
    bands: [f32; 10],
    preamp: f32,
    volume: f64,
    hold_until: Instant,
}

struct App {
    client: Client,
    /// owns io.github.x1ctl.Gui while the window lives
    _instance: Option<Connection>,
    edit: Edit,
    save_name: String,
    preset_filter: String,
    preset_names: Vec<(String, bool)>,
    confirm_reset: bool,
    has_creative: bool,
    /// result of a background import: Ok(count) / Err(message)
    importing: Option<ImportSlot>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, client: Client, instance: Option<Connection>) -> Self {
        let ctx = cc.egui_ctx.clone();
        client.on_change(move || ctx.request_repaint());
        style(&cc.egui_ctx);
        Self {
            client,
            _instance: instance,
            edit: Edit {
                bands: [0.0; 10],
                preamp: 0.0,
                volume: -20.0,
                hold_until: Instant::now(),
            },
            save_name: String::new(),
            preset_filter: String::new(),
            preset_names: Self::load_names(),
            confirm_reset: false,
            has_creative: presets::load_imported().is_some(),
            importing: None,
        }
    }

    fn load_names() -> Vec<(String, bool)> {
        let user = presets::load_user();
        let mut v: Vec<(String, bool)> = user.keys().map(|n| (n.clone(), true)).collect();
        v.extend(
            presets::builtin_ordered()
                .into_iter()
                .filter(|(n, _)| !user.contains_key(n))
                .map(|(n, _)| (n, false)),
        );
        v
    }

    fn hold(&mut self) {
        self.edit.hold_until = Instant::now() + Duration::from_millis(900);
    }
}

fn style(ctx: &egui::Context) {
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = Vec2::new(10.0, 8.0);
        s.spacing.button_padding = Vec2::new(12.0, 6.0);
        s.spacing.slider_width = 180.0;
        s.visuals.widgets.inactive.corner_radius = CornerRadius::same(6);
        s.visuals.widgets.hovered.corner_radius = CornerRadius::same(6);
        s.visuals.widgets.active.corner_radius = CornerRadius::same(6);
        s.visuals.widgets.noninteractive.corner_radius = CornerRadius::same(6);
        s.visuals.selection.bg_fill = ACCENT;
        s.visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    });
}

const ACCENT: Color32 = Color32::from_rgb(0x1E, 0x9B, 0xD7);

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::group(ui.style())
        .fill(ui.visuals().faint_bg_color)
        .stroke(Stroke::new(
            1.0,
            ui.visuals().widgets.noninteractive.bg_stroke.color,
        ))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

/// iOS-style switch (from the egui demo).
fn toggle(ui: &mut egui::Ui, on: &mut bool, size: f32) -> egui::Response {
    let desired = egui::vec2(2.0, 1.0) * size;
    let (rect, mut resp) = ui.allocate_exact_size(desired, egui::Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(resp.id, *on);
        let visuals = ui.style().interact_selectable(&resp, *on);
        let rect = rect.expand(visuals.expansion);
        let radius = 0.5 * rect.height();
        let bg = if *on { ACCENT } else { visuals.bg_fill };
        ui.painter().rect(
            rect,
            radius,
            bg,
            visuals.bg_stroke,
            egui::StrokeKind::Inside,
        );
        let cx = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
        ui.painter().circle(
            egui::pos2(cx, rect.center().y),
            0.78 * radius,
            Color32::WHITE,
            Stroke::NONE,
        );
    }
    resp
}

fn hz_label(hz: u32) -> String {
    if hz >= 1000 {
        format!("{}k", hz / 1000)
    } else {
        hz.to_string()
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some(slot) = &self.importing {
            let done = slot.lock().unwrap().take();
            if let Some(r) = done {
                self.importing = None;
                match r {
                    Ok(_) => {
                        self.has_creative = true;
                        self.preset_names = Self::load_names();
                    }
                    Err(e) => self.client.error(format!(
                        "import failed: {e} — download ZZ0090.zip in a browser and run `x1ctl eq import <file>`"
                    )),
                }
            }
        }
        let (daemon, snap, err) = self.client.snapshot();
        let connected = snap.as_ref().is_some_and(|s| s.connected);
        let dragging = ui.ctx().dragged_id().is_some();

        // adopt device state unless the user is mid-edit
        if let Some(st) = &snap {
            if !dragging && Instant::now() > self.edit.hold_until {
                for (i, b) in st.bands.iter().enumerate() {
                    if let Some(v) = b {
                        self.edit.bands[i] = *v;
                    }
                }
                self.edit.preamp = st.preamp.unwrap_or(0.0);
                if let Some(v) = st.volume_db {
                    self.edit.volume = v;
                }
            }
        }

        egui::CentralPanel::default_margins().show(ui, |ui| {
          egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // ---- header -------------------------------------------------------------------
            ui.horizontal(|ui| {
                ui.heading(RichText::new("Sound Blaster X1").strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (txt, col) = match (daemon, connected) {
                        (Daemon::Running, true) => ("connected", Color32::from_rgb(0x3D, 0xAE, 0x5A)),
                        (Daemon::Running, false) => ("not plugged in", Color32::from_rgb(0xD9, 0x8E, 0x1E)),
                        (Daemon::NotRunning, _) => ("x1d not running", Color32::from_rgb(0xD6, 0x45, 0x45)),
                        (Daemon::Unknown, _) => ("connecting…", ui.visuals().weak_text_color()),
                    };
                    ui.label(RichText::new(txt).color(col));
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
                    ui.painter().circle_filled(r.center(), 4.5, col);
                    if let Some(st) = snap.as_ref().filter(|s| s.connected) {
                        ui.label(RichText::new(format!("firmware {}", st.firmware)).weak().small());
                    }
                });
            });
            ui.add_space(6.0);

            if daemon == Daemon::NotRunning {
                card(ui, |ui| {
                    ui.label("The x1d service isn't running. It owns the X1's control port and is what this window talks to.");
                    ui.horizontal(|ui| {
                        if ui.button("Start x1d").clicked() {
                            self.client.send(Cmd::StartDaemon);
                        }
                        ui.label(RichText::new("systemctl --user start x1d").monospace().weak());
                    });
                });
            } else if daemon == Daemon::Running && !connected {
                card(ui, |ui| {
                    ui.label("Plug in the Sound Blaster X1 — it will appear here automatically.");
                });
            }

            ui.add_enabled_ui(connected, |ui| {
                let st = snap.clone().unwrap_or_default();

                // ---- Super X-Fi ----------------------------------------------------------
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        let mut on = st.super_xfi.unwrap_or(false);
                        if toggle(ui, &mut on, 26.0).changed() {
                            self.client.send(Cmd::Sxfi(on));
                        }
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Super X-Fi").size(18.0).strong());
                            ui.label(
                                RichText::new(
                                    "Headphone holography. The X1 keeps a separate EQ for on and off — \
                                     the equalizer below edits the active one.",
                                )
                                .weak()
                                .small(),
                            );
                        });
                    });
                });

                // ---- Equalizer ------------------------------------------------------------
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        let mut on = st.eq_enabled.unwrap_or(false);
                        if toggle(ui, &mut on, 18.0).changed() {
                            self.client.send(Cmd::EqOn(on));
                        }
                        ui.label(RichText::new("Equalizer").size(18.0).strong());
                        let slot = if st.super_xfi.unwrap_or(false) { "Super X-Fi on" } else { "Super X-Fi off" };
                        ui.label(RichText::new(format!("· {slot} slot")).weak());
                        if st.eq_enabled == Some(false) {
                            ui.label(RichText::new("(bypassed)").color(Color32::from_rgb(0xD9, 0x8E, 0x1E)).small());
                        }
                    });
                    ui.add_space(4.0);

                    // preset row
                    ui.horizontal(|ui| {
                        let current = matching_preset(&st).unwrap_or_else(|| "Custom".into());
                        ui.label("Preset");
                        egui::ComboBox::from_id_salt("preset")
                            .selected_text(&current)
                            .width(260.0)
                            .height(420.0)
                            .show_ui(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.preset_filter)
                                        .hint_text("filter…")
                                        .desired_width(240.0),
                                );
                                let f = presets::slug(&self.preset_filter);
                                let mut last_user = None;
                                for (name, user) in self.preset_names.clone() {
                                    if !f.is_empty() && !presets::slug(&name).contains(&f) {
                                        continue;
                                    }
                                    if last_user == Some(true) && !user {
                                        ui.separator();
                                    }
                                    last_user = Some(user);
                                    let label = if user { format!("★ {name}") } else { name.clone() };
                                    if ui.selectable_label(name == current, label).clicked() {
                                        self.client.send(Cmd::Preset(name.clone()));
                                        self.edit.hold_until = Instant::now();
                                    }
                                }
                            });
                        if ui.button("Flat").clicked() {
                            self.client.send(Cmd::Preset("Flat".into()));
                        }
                        if !self.has_creative {
                            let busy = self.importing.is_some();
                            let b = ui.add_enabled(!busy, egui::Button::new(if busy {
                                "Importing…"
                            } else {
                                "Import Creative presets"
                            }));
                            if b.on_hover_text("Downloads Creative's official X1 presets (71 EQ presets, 60 sound modes) from Creative's server").clicked() {
                                let slot = Arc::new(Mutex::new(None));
                                let s2 = slot.clone();
                                let ctx = ui.ctx().clone();
                                std::thread::spawn(move || {
                                    let r = presets::download_package(presets::PACKAGE_URL).and_then(|imp| {
                                        presets::save_imported(&imp).map(|_| imp.eq_presets.len()).map_err(|e| e.to_string())
                                    });
                                    *s2.lock().unwrap() = Some(r);
                                    ctx.request_repaint();
                                });
                                self.importing = Some(slot);
                            }
                        }
                        ui.separator();
                        ui.add(egui::TextEdit::singleline(&mut self.save_name).hint_text("name").desired_width(120.0));
                        let can_save = !self.save_name.trim().is_empty() && st.bands.iter().all(|b| b.is_some());
                        if ui.add_enabled(can_save, egui::Button::new("Save preset")).clicked() {
                            let p = presets::Preset { bands: self.edit.bands, preamp: self.edit.preamp };
                            match presets::save_user(self.save_name.trim(), p) {
                                Ok(_) => {
                                    self.save_name.clear();
                                    self.preset_names = Self::load_names();
                                }
                                Err(e) => self.client.error(format!("saving preset: {e}")),
                            }
                        }
                    });
                    ui.add_space(8.0);

                    // sliders
                    let mut send: Vec<Cmd> = vec![];
                    ui.horizontal(|ui| {
                        ui.spacing_mut().slider_width = 170.0;
                        let col = |ui: &mut egui::Ui, v: &mut f32, lim: f32, label: &str, strong: bool| -> (bool, bool) {
                            let mut commit = false;
                            let mut reset = false;
                            ui.vertical(|ui| {
                                ui.set_width(46.0);
                                ui.vertical_centered(|ui| {
                                    ui.label(RichText::new(format!("{:+.1}", v)).monospace().small());
                                    let r = ui.add(
                                        egui::Slider::new(v, -lim..=lim)
                                            .vertical()
                                            .step_by(0.5)
                                            .show_value(false)
                                            .trailing_fill(true),
                                    );
                                    if r.double_clicked() {
                                        *v = 0.0;
                                        reset = true;
                                    }
                                    commit = r.drag_stopped() || (r.changed() && !r.dragged()) || reset;
                                    let t = RichText::new(label).small();
                                    ui.label(if strong { t.strong() } else { t });
                                });
                            });
                            (commit, reset)
                        };
                        let (c, _) = col(ui, &mut self.edit.preamp, proto::PREAMP_LIMIT_DB, "pre", true);
                        if c {
                            send.push(Cmd::Preamp(self.edit.preamp as f64));
                        }
                        ui.separator();
                        for i in 0..10 {
                            let (c, _) = col(ui, &mut self.edit.bands[i], proto::BAND_LIMIT_DB, &hz_label(proto::BAND_HZ[i]), false);
                            if c {
                                send.push(Cmd::Band(i as u8, self.edit.bands[i] as f64));
                            }
                        }
                    });
                    if !send.is_empty() {
                        self.hold();
                        for c in send {
                            self.client.send(c);
                        }
                    }
                    ui.label(RichText::new("dB · double-click a slider to reset it · range ±9 dB (preamp ±12)").weak().small());
                });

                // ---- Output -------------------------------------------------------------
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Headphones").size(18.0).strong());
                        ui.add_space(8.0);
                        let mut muted = st.muted.unwrap_or(false);
                        let label = if muted { "🔇 Muted" } else { "🔊 Mute" };
                        if ui.toggle_value(&mut muted, label).changed() {
                            self.client.send(Cmd::Mute(muted));
                        }
                        ui.spacing_mut().slider_width = 300.0;
                        let r = ui.add(
                            egui::Slider::new(&mut self.edit.volume, proto::LEVEL_MIN_DB..=proto::LEVEL_MAX_DB)
                                .step_by(VOL_STEP)
                                .suffix(" dB")
                                .fixed_decimals(1)
                                .trailing_fill(true),
                        );
                        if r.drag_stopped() || (r.changed() && !r.dragged()) {
                            self.hold();
                            self.client.send(Cmd::Volume(self.edit.volume));
                        }
                    });
                    if let Some(m) = st.mic_muted {
                        ui.label(
                            RichText::new(format!(
                                "Mic {} · {}",
                                st.mic_db.map(|d| format!("{d:+.1} dB")).unwrap_or_default(),
                                if m { "muted" } else { "live" }
                            ))
                            .weak()
                            .small(),
                        );
                    }
                });
            });

            if let Some(st) = snap.as_ref().filter(|s| s.connected) {
                card(ui, |ui| {
                    egui::CollapsingHeader::new(RichText::new("Device").size(18.0).strong())
                        .id_salt("device")
                        .default_open(false)
                        .show(ui, |ui| device_info(ui, st, &mut self.confirm_reset));
                });
            }

            if let Some(e) = err {
                ui.add_space(4.0);
                ui.colored_label(Color32::from_rgb(0xD6, 0x45, 0x45), e);
            }
            if self.confirm_reset {
                let m = egui::Modal::new(egui::Id::new("confirm-reset")).show(ui.ctx(), |ui| {
                    ui.set_width(380.0);
                    ui.heading("Reset the X1 to factory defaults?");
                    ui.add_space(4.0);
                    ui.label(
                        "This resets the settings stored on the device: the equalizer in both Super X-Fi \
                         slots, the Super X-Fi state and the levels. Your saved presets on this computer \
                         are kept. The X1 may disconnect and reconnect.",
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let reset = egui::Button::new(RichText::new("Reset").color(Color32::WHITE))
                            .fill(Color32::from_rgb(0xC0, 0x39, 0x2B));
                        if ui.add(reset).clicked() {
                            self.client.send(Cmd::FactoryReset);
                            self.edit.hold_until = Instant::now();
                            self.confirm_reset = false;
                        }
                        if ui.button("Cancel").clicked() {
                            self.confirm_reset = false;
                        }
                    });
                });
                if m.should_close() {
                    self.confirm_reset = false;
                }
            }

            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Surround, Crystalizer, Bass and Smart Volume from Creative's sound modes run on Windows, \
                     not in the X1 — use EasyEffects for those.",
                )
                .weak()
                .small(),
            );
          });
        });
    }
}

fn device_info(ui: &mut egui::Ui, st: &State, confirm_reset: &mut bool) {
    let button_name = |b: u16| match b {
        0x001E => "Super X-Fi".to_string(),
        0x400A => "play/pause".into(),
        0x400B => "previous".into(),
        0x400C => "next".into(),
        o => format!("0x{o:04X}"),
    };
    let mut rows: Vec<(&str, String)> = vec![
        ("Model", "Creative Sound Blaster X1 (ZZ0090)".into()),
        ("Serial number", st.serial.clone()),
        ("Firmware", st.firmware.clone()),
    ];
    if let Some(v) = st.fw_version_code {
        rows.push(("Firmware build", format!("0x{v:08X}")));
    }
    if let Some(v) = st.mcu_type {
        rows.push(("Chip type", format!("0x{v:02X} ({v})")));
    }
    rows.push(("USB ID", format!("{:04x}:{:04x}", st.vid, st.pid)));
    if let Some(u) = &st.usb {
        rows.push((
            "USB link",
            format!(
                "USB {} · {} Mbit/s · bus {}",
                u.usb_version, u.speed_mbps, u.bus_path
            ),
        ));
        rows.push(("USB release", u.bcd_device.clone()));
    }
    rows.push(("Control port", st.port.clone()));
    rows.push(("On-board DSP", st.dsp_features.join(", ")));
    rows.push((
        "Buttons",
        st.buttons
            .iter()
            .map(|&b| button_name(b))
            .collect::<Vec<_>>()
            .join(", "),
    ));
    if let Some(c) = &st.sxfi_caps {
        rows.push(("Super X-Fi engine", c.clone()));
    }
    rows.push((
        "Personal SXFI profile",
        match &st.headphone_profile {
            Some((id, _)) if !id.is_empty() => id.clone(),
            Some(_) => "none — generic profile".into(),
            None => "?".into(),
        },
    ));
    rows.push((
        "UAC2 flag",
        if st.uac2_forced == Some(true) {
            "on".into()
        } else {
            "off".into()
        },
    ));
    egui::Grid::new("devinfo")
        .num_columns(2)
        .striped(true)
        .spacing([18.0, 4.0])
        .show(ui, |ui| {
            for (k, v) in rows {
                ui.label(RichText::new(k).weak());
                ui.add(egui::Label::new(RichText::new(v).monospace()).selectable(true));
                ui.end_row();
            }
        });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Factory reset…").clicked() {
            *confirm_reset = true;
        }
        let note = "Same as Creative App's reset: EQ (both Super X-Fi slots), Super X-Fi and levels go back to defaults.";
        ui.label(RichText::new(note).weak().small());
    });
}

/// Single-instance: the first window owns `io.github.x1ctl.Gui`; later launches ask it to raise.
struct Raiser {
    ctx: Arc<Mutex<Option<egui::Context>>>,
}

#[zbus::interface(name = "io.github.x1ctl.Gui")]
impl Raiser {
    fn raise(&self) {
        if let Some(ctx) = self.ctx.lock().unwrap().as_ref() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            ctx.request_repaint();
        }
    }
}

fn run_window() -> eframe::Result {
    let ctx_slot: Arc<Mutex<Option<egui::Context>>> = Arc::new(Mutex::new(None));
    // single instance: register the object first, then try to take the name
    let guard = Connection::session().ok().and_then(|c| {
        c.object_server()
            .at(
                "/io/github/x1ctl/Gui",
                Raiser {
                    ctx: ctx_slot.clone(),
                },
            )
            .ok()?;
        match c
            .request_name_with_flags(GUI_BUS_NAME, zbus::fdo::RequestNameFlags::DoNotQueue.into())
        {
            Ok(zbus::fdo::RequestNameReply::PrimaryOwner) => Some(Ok(c)),
            Ok(_) => Some(Err(c)),
            Err(e) => {
                log::debug!("request_name: {e}");
                Some(Err(c))
            }
        }
    });
    if let Some(Err(c)) = &guard {
        let _ = c.call_method(
            Some(GUI_BUS_NAME),
            "/io/github/x1ctl/Gui",
            Some("io.github.x1ctl.Gui"),
            "Raise",
            &(),
        );
        return Ok(());
    }

    let instance = match guard {
        Some(Ok(c)) => Some(c),
        _ => None,
    };
    let client = Client::start();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("X1 Control")
            .with_app_id(APP_ID)
            .with_inner_size([820.0, 660.0])
            .with_min_inner_size([760.0, 600.0])
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../../packaging/icon-256.png"))
                    .unwrap_or_default(),
            ),
        ..Default::default()
    };
    eframe::run_native(
        "X1 Control",
        options,
        Box::new(move |cc| {
            *ctx_slot.lock().unwrap() = Some(cc.egui_ctx.clone());
            Ok(Box::new(App::new(cc, client, instance)))
        }),
    )
}

// =============================================================================================
// Tray
// =============================================================================================

struct Tray {
    client: Client,
}

fn open_window() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).spawn();
    }
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "x1ctl".into()
    }
    fn title(&self) -> String {
        "Sound Blaster X1".into()
    }
    fn icon_name(&self) -> String {
        "audio-headphones".into()
    }
    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }
    fn status(&self) -> ksni::Status {
        let (d, s, _) = self.client.snapshot();
        if d == Daemon::Running && s.is_some_and(|s| s.connected) {
            ksni::Status::Active
        } else {
            ksni::Status::Passive
        }
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        let (d, s, _) = self.client.snapshot();
        let description = match (d, s) {
            (Daemon::Running, Some(st)) if st.connected => {
                let eq = if st.eq_enabled == Some(true) {
                    matching_preset(&st).unwrap_or_else(|| "custom".into())
                } else {
                    "off".into()
                };
                format!(
                    "Super X-Fi {} · EQ {} · {}{}",
                    if st.super_xfi == Some(true) {
                        "on"
                    } else {
                        "off"
                    },
                    eq,
                    st.volume_db
                        .map(|v| format!("{v:+.1} dB"))
                        .unwrap_or_default(),
                    if st.muted == Some(true) {
                        " (muted)"
                    } else {
                        ""
                    }
                )
            }
            (Daemon::Running, _) => "not plugged in".into(),
            (Daemon::NotRunning, _) => "x1d is not running".into(),
            _ => String::new(),
        };
        ksni::ToolTip {
            title: "Sound Blaster X1".into(),
            description,
            icon_name: "audio-headphones".into(),
            icon_pixmap: vec![],
        }
    }
    /// Left click: open the window.
    fn activate(&mut self, _x: i32, _y: i32) {
        open_window();
    }
    /// Middle click: toggle Super X-Fi.
    fn secondary_activate(&mut self, _x: i32, _y: i32) {
        let (_, s, _) = self.client.snapshot();
        if let Some(st) = s.filter(|s| s.connected) {
            self.client.send(Cmd::Sxfi(!st.super_xfi.unwrap_or(false)));
        }
    }
    /// Scroll: headphone level in device steps.
    fn scroll(&mut self, delta: i32, orientation: ksni::Orientation) {
        if orientation != ksni::Orientation::Vertical {
            return;
        }
        let (_, s, _) = self.client.snapshot();
        if let Some(v) = s.and_then(|s| s.volume_db) {
            let dir = if delta > 0 { 1.0 } else { -1.0 }; // Plasma: positive = up
            self.client.send(Cmd::Volume(
                (v + dir * VOL_STEP).clamp(proto::LEVEL_MIN_DB, proto::LEVEL_MAX_DB),
            ));
        }
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let (d, s, _) = self.client.snapshot();
        let st = s.filter(|s| s.connected && d == Daemon::Running);
        let live = st.is_some();
        let st = st.unwrap_or_default();
        let current = matching_preset(&st);

        let preset_item = |name: String| -> MenuItem<Self> {
            let checked = current.as_deref() == Some(name.as_str());
            CheckmarkItem {
                label: name.replace('_', "__"),
                checked,
                enabled: live,
                activate: Box::new(move |t: &mut Self| t.client.send(Cmd::Preset(name.clone()))),
                ..Default::default()
            }
            .into()
        };
        let builtin = presets::builtin_ordered();
        // Creative orders general-purpose presets first (Gaming … Vocal), games after.
        let split = builtin
            .iter()
            .position(|(n, _)| n == "64 Audio Aspire Series")
            .unwrap_or(builtin.len());
        let mut preset_menu: Vec<MenuItem<Self>> =
            presets::load_user().into_keys().map(&preset_item).collect();
        if !preset_menu.is_empty() {
            preset_menu.push(MenuItem::Separator);
        }
        preset_menu.extend(builtin[..split].iter().map(|(n, _)| preset_item(n.clone())));
        if split < builtin.len() {
            preset_menu.push(
                SubMenu {
                    label: "Games & more".into(),
                    submenu: builtin[split..]
                        .iter()
                        .map(|(n, _)| preset_item(n.clone()))
                        .collect(),
                    ..Default::default()
                }
                .into(),
            );
        }

        let mut m: Vec<MenuItem<Self>> = vec![
            StandardItem {
                label: match (d, live) {
                    (Daemon::Running, true) => format!(
                        "Sound Blaster X1 · {}",
                        st.volume_db
                            .map(|v| format!("{v:+.1} dB"))
                            .unwrap_or_default()
                    ),
                    (Daemon::Running, false) => "X1 not plugged in".into(),
                    _ => "x1d not running".into(),
                },
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Super X-Fi".into(),
                checked: st.super_xfi == Some(true),
                enabled: live,
                activate: Box::new(|t: &mut Self| {
                    let (_, s, _) = t.client.snapshot();
                    let on = s.and_then(|s| s.super_xfi).unwrap_or(false);
                    t.client.send(Cmd::Sxfi(!on));
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Equalizer".into(),
                checked: st.eq_enabled == Some(true),
                enabled: live,
                activate: Box::new(|t: &mut Self| {
                    let (_, s, _) = t.client.snapshot();
                    let on = s.and_then(|s| s.eq_enabled).unwrap_or(false);
                    t.client.send(Cmd::EqOn(!on));
                }),
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: format!(
                    "EQ preset: {}",
                    current.clone().unwrap_or_else(|| "custom".into())
                ),
                enabled: live,
                submenu: preset_menu,
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Mute headphones".into(),
                checked: st.muted == Some(true),
                enabled: live,
                activate: Box::new(|t: &mut Self| {
                    let (_, s, _) = t.client.snapshot();
                    let on = s.and_then(|s| s.muted).unwrap_or(false);
                    t.client.send(Cmd::Mute(!on));
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open X1 Control…".into(),
                icon_name: "configure".into(),
                activate: Box::new(|_| open_window()),
                ..Default::default()
            }
            .into(),
        ];
        if d == Daemon::NotRunning {
            m.push(
                StandardItem {
                    label: "Start x1d".into(),
                    activate: Box::new(|t: &mut Self| t.client.send(Cmd::StartDaemon)),
                    ..Default::default()
                }
                .into(),
            );
        }
        m.push(
            StandardItem {
                label: "Quit tray icon".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        );
        m
    }
}

fn run_tray() -> Result<(), String> {
    use ksni::blocking::TrayMethods;
    let client = Client::start();
    // assume_sni_available: at login we may start before the panel's tray host; ksni then
    // registers as soon as the StatusNotifierWatcher appears instead of failing.
    let handle = Tray {
        client: client.clone(),
    }
    .assume_sni_available(true)
    .spawn()
    .map_err(|e| format!("tray: {e}"))?;
    let h = handle.clone();
    // coalesce bursts (EQ dumps arrive as several property changes)
    let pending = Arc::new(std::sync::atomic::AtomicBool::new(false));
    client.on_change(move || {
        if !pending.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let h = h.clone();
            let pending = pending.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(80));
                pending.store(false, std::sync::atomic::Ordering::SeqCst);
                h.update(|_| {});
            });
        }
    });
    // the first state may have arrived before we subscribed
    handle.update(|_| {});
    loop {
        std::thread::sleep(Duration::from_secs(3600));
        if handle.is_closed() {
            return Ok(());
        }
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let tray = std::env::args().any(|a| a == "--tray");
    if std::env::args().any(|a| a == "--help" || a == "-h") {
        println!(
            "x1-gui           open the Sound Blaster X1 control window\nx1-gui --tray    run the tray icon"
        );
        return;
    }
    let r = if tray {
        run_tray()
    } else {
        run_window().map_err(|e| e.to_string())
    };
    if let Err(e) = r {
        eprintln!("x1-gui: {e}");
        std::process::exit(1);
    }
}
