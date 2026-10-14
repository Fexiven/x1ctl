//! x1d — session daemon that owns the X1's control port and exposes it on D-Bus.
//!
//! Bus name `io.github.x1ctl`, object `/io/github/x1ctl/Device`, interface
//! `io.github.x1ctl.Device`. All state is published as properties with change
//! notifications, so panels/widgets can bind to it directly.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser as _;
use x1::device::{Device, Notice, State};
use x1::{Error, presets, proto};
use zbus::fdo;
use zbus::interface;

#[derive(clap::Parser)]
#[command(version, about = "Sound Blaster X1 control daemon (D-Bus service)")]
struct Args {
    /// Use this tty instead of auto-detecting 041e:3286
    #[arg(long)]
    port: Option<std::path::PathBuf>,
    /// Use the system bus instead of the session bus
    #[arg(long)]
    system: bool,
}

#[derive(Default)]
struct Inner {
    dev: Mutex<Option<Arc<Device>>>,
    last: Mutex<State>,
}

#[derive(Clone)]
struct Svc(Arc<Inner>);

fn fe(e: Error) -> fdo::Error {
    match e {
        Error::Refused(m) => fdo::Error::InvalidArgs(m),
        Error::NotFound | Error::Disconnected => fdo::Error::Failed(format!("{e}")),
        other => fdo::Error::Failed(other.to_string()),
    }
}

impl Svc {
    fn dev(&self) -> fdo::Result<Arc<Device>> {
        self.0
            .dev
            .lock()
            .unwrap()
            .clone()
            .filter(|d| d.is_alive())
            .ok_or_else(|| fe(Error::NotFound))
    }
    /// Live device state when connected (no lag behind writes), else the last snapshot.
    fn st(&self) -> State {
        match self.0.dev.lock().unwrap().as_ref().filter(|d| d.is_alive()) {
            Some(d) => d.state(),
            None => self.0.last.lock().unwrap().clone(),
        }
    }
    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Device) -> x1::Result<T> + Send + 'static,
    ) -> fdo::Result<T> {
        let d = self.dev()?;
        blocking::unblock(move || f(&d)).await.map_err(fe)
    }
}

#[interface(name = "io.github.x1ctl.Device")]
impl Svc {
    // ---- properties ----
    #[zbus(property)]
    fn connected(&self) -> bool {
        self.st().connected
    }
    #[zbus(property)]
    fn port(&self) -> String {
        self.st().port
    }
    #[zbus(property)]
    fn firmware(&self) -> String {
        self.st().firmware
    }
    #[zbus(property)]
    fn serial(&self) -> String {
        self.st().serial
    }
    #[zbus(property, name = "SuperXFi")]
    fn super_xfi(&self) -> bool {
        self.st().super_xfi.unwrap_or(false)
    }
    #[zbus(property)]
    fn eq_enabled(&self) -> bool {
        self.st().eq_enabled.unwrap_or(false)
    }
    #[zbus(property)]
    fn preamp(&self) -> f64 {
        self.st().preamp.unwrap_or(0.0) as f64
    }
    /// 10 band gains in dB, 31 Hz … 16 kHz.
    #[zbus(property)]
    fn bands(&self) -> Vec<f64> {
        self.st()
            .bands
            .iter()
            .map(|b| b.unwrap_or(0.0) as f64)
            .collect()
    }
    #[zbus(property)]
    fn band_frequencies(&self) -> Vec<u32> {
        proto::BAND_HZ.to_vec()
    }
    /// Active device EQ profile (-1 = unknown). Follows Super X-Fi on the X1.
    #[zbus(property)]
    fn eq_profile(&self) -> i32 {
        self.st().eq_profile.map(i32::from).unwrap_or(-1)
    }
    /// Headphone level in dB (NaN = unknown).
    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.st().volume_db.unwrap_or(f64::NAN)
    }
    #[zbus(property)]
    fn muted(&self) -> bool {
        self.st().muted.unwrap_or(false)
    }
    #[zbus(property)]
    fn mic_level(&self) -> f64 {
        self.st().mic_db.unwrap_or(f64::NAN)
    }
    #[zbus(property)]
    fn mic_muted(&self) -> bool {
        self.st().mic_muted.unwrap_or(false)
    }
    #[zbus(property, name = "UAC2Forced")]
    fn uac2_forced(&self) -> bool {
        self.st().uac2_forced.unwrap_or(false)
    }
    /// Everything as JSON (used by `x1ctl status`).
    #[zbus(property, name = "StateJSON")]
    fn state_json(&self) -> String {
        serde_json::to_string(&self.st()).unwrap_or_default()
    }

    // ---- methods ----
    #[zbus(name = "SetSuperXFi")]
    async fn set_super_xfi(&self, on: bool) -> fdo::Result<()> {
        self.run(move |d| d.set_super_xfi(on)).await
    }
    #[zbus(name = "ToggleSuperXFi")]
    async fn toggle_super_xfi(&self) -> fdo::Result<bool> {
        self.run(|d| {
            let on = !d.super_xfi()?;
            d.set_super_xfi(on).map(|_| on)
        })
        .await
    }
    async fn set_eq_enabled(&self, on: bool) -> fdo::Result<()> {
        self.run(move |d| d.set_eq_enabled(on)).await
    }
    async fn set_preamp(&self, db: f64) -> fdo::Result<()> {
        self.run(move |d| d.set_preamp(db as f32)).await
    }
    async fn set_band(&self, band: u8, db: f64) -> fdo::Result<()> {
        self.run(move |d| d.set_band(band as usize, db as f32))
            .await
    }
    async fn set_bands(&self, bands: Vec<f64>, preamp: f64) -> fdo::Result<()> {
        let b: [f32; 10] = bands
            .iter()
            .map(|&x| x as f32)
            .collect::<Vec<_>>()
            .try_into()
            .map_err(|_| fdo::Error::InvalidArgs("need exactly 10 band values".into()))?;
        self.run(move |d| d.set_eq(&b, preamp as f32)).await
    }
    async fn apply_preset(&self, name: String) -> fdo::Result<()> {
        let (_, p) = presets::find(&name)
            .ok_or_else(|| fdo::Error::InvalidArgs(format!("no preset named '{name}'")))?;
        self.run(move |d| d.set_eq(&p.bands, p.preamp)).await
    }
    /// Official presets in Creative's order, then user presets.
    fn list_presets(&self) -> Vec<String> {
        let mut v: Vec<String> = presets::builtin_ordered()
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let user: Vec<String> = presets::load_user()
            .into_keys()
            .filter(|n| !v.contains(n))
            .collect();
        v.extend(user);
        v
    }
    async fn set_volume(&self, db: f64) -> fdo::Result<()> {
        self.run(move |d| d.set_volume_db(db).map(|_| ())).await
    }
    async fn set_muted(&self, on: bool) -> fdo::Result<()> {
        self.run(move |d| d.set_muted(on)).await
    }
    /// Sets the device's EnableUAC2 flag. The X1 re-enumerates afterwards.
    #[zbus(name = "SetUAC2Forced")]
    async fn set_uac2_forced(&self, on: bool) -> fdo::Result<()> {
        self.run(move |d| d.set_uac2_forced(on)).await
    }
    /// Super X-Fi headphone profile (HPID, file id) as stored on the device.
    async fn headphone_profile(&self) -> fdo::Result<(String, String)> {
        self.run(|d| d.headphone_profile()).await
    }
    /// Reset the X1 to factory defaults (Creative App's "Reset", type General).
    async fn factory_reset(&self) -> fdo::Result<()> {
        self.run(|d| {
            d.factory_reset()?;
            std::thread::sleep(std::time::Duration::from_millis(800));
            if d.is_alive() {
                d.refresh_all();
            }
            Ok(())
        })
        .await
    }
    async fn refresh(&self) -> fdo::Result<()> {
        self.run(|d| {
            d.refresh_all();
            Ok(())
        })
        .await
    }
}

/// Publish property changes between two snapshots.
fn emit_changes(iref: &zbus::blocking::object_server::InterfaceRef<Svc>, old: &State, new: &State) {
    let se = iref.signal_emitter();
    let g = iref.get();
    zbus::block_on(async {
        macro_rules! emit {
            ($cond:expr, $m:ident) => {
                if $cond {
                    if let Err(e) = g.$m(se).await {
                        log::warn!("emit {}: {e}", stringify!($m));
                    }
                }
            };
        }
        emit!(old.connected != new.connected, connected_changed);
        emit!(old.port != new.port, port_changed);
        emit!(old.firmware != new.firmware, firmware_changed);
        emit!(old.serial != new.serial, serial_changed);
        emit!(old.super_xfi != new.super_xfi, super_x_fi_changed);
        emit!(old.eq_enabled != new.eq_enabled, eq_enabled_changed);
        emit!(old.preamp != new.preamp, preamp_changed);
        emit!(old.bands != new.bands, bands_changed);
        emit!(old.eq_profile != new.eq_profile, eq_profile_changed);
        emit!(old.volume_db != new.volume_db, volume_changed);
        emit!(old.muted != new.muted, muted_changed);
        emit!(old.mic_db != new.mic_db, mic_level_changed);
        emit!(old.mic_muted != new.mic_muted, mic_muted_changed);
        emit!(old.uac2_forced != new.uac2_forced, u_a_c2_forced_changed);
        let (a, b) = (
            serde_json::to_string(old).ok(),
            serde_json::to_string(new).ok(),
        );
        emit!(a != b, state_j_s_o_n_changed);
    });
}

enum Msg {
    Snapshot(State),
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let svc = Svc(Arc::new(Inner::default()));

    let builder = if args.system {
        zbus::blocking::connection::Builder::system()?
    } else {
        zbus::blocking::connection::Builder::session()?
    };
    let conn = builder
        .name(x1::DBUS_NAME)?
        .serve_at(x1::DBUS_PATH, svc.clone())?
        .build()?;
    let iref = conn.object_server().interface::<_, Svc>(x1::DBUS_PATH)?;
    log::info!(
        "serving {} on the {} bus",
        x1::DBUS_NAME,
        if args.system { "system" } else { "session" }
    );

    // Emitter thread: the only place that talks to D-Bus on behalf of device events.
    let (tx, rx) = mpsc::channel::<Msg>();
    {
        let svc = svc.clone();
        std::thread::Builder::new()
            .name("x1-emit".into())
            .spawn(move || {
                // `prev` is what subscribers have been told so far.
                let mut prev = State::default();
                for Msg::Snapshot(new) in rx {
                    *svc.0.last.lock().unwrap() = new.clone();
                    emit_changes(&iref, &prev, &new);
                    prev = new;
                }
            })?;
    }

    // Supervisor: connect, reconnect after unplug/suspend, idle-poll when absent.
    let mut warned = false;
    loop {
        let alive = svc
            .0
            .dev
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|d| d.is_alive());
        if !alive {
            if svc.0.dev.lock().unwrap().take().is_some() {
                log::info!("X1 disconnected");
                let mut st = svc.st();
                st.connected = false;
                let _ = tx.send(Msg::Snapshot(st));
            }
            let res = match &args.port {
                Some(p) => Device::connect(p),
                None => Device::connect_auto(),
            };
            match res {
                Ok(d) => {
                    warned = false;
                    let d = Arc::new(d);
                    let st = d.state();
                    log::info!(
                        "connected to X1 at {} (firmware {}, Super X-Fi {:?})",
                        st.port,
                        st.firmware,
                        st.super_xfi
                    );
                    let txl = Mutex::new(tx.clone());
                    d.set_listener(Box::new(move |n: &Notice, s: &State| {
                        if let Notice::Event(ev) = n {
                            log::debug!("event {ev:?}");
                        }
                        let _ = txl.lock().unwrap().send(Msg::Snapshot(s.clone()));
                    }));
                    let _ = tx.send(Msg::Snapshot(d.state()));
                    *svc.0.dev.lock().unwrap() = Some(d);
                }
                Err(Error::NotFound) => {
                    if !warned {
                        log::info!("waiting for the X1 to be plugged in");
                        warned = true;
                    }
                }
                Err(e) => log::warn!("connect failed: {e}"),
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}
