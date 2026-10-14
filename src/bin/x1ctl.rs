//! x1ctl — command-line control for the Sound Blaster X1.
//!
//! Talks to `x1d` over D-Bus when it is running; otherwise opens the device directly.

use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;
use x1::device::{Device, Notice, State};
use x1::{presets, proto};
use zbus::blocking::{Connection, Proxy};

#[derive(Parser)]
#[command(
    version,
    about = "Control a Creative Sound Blaster X1 (Super X-Fi, EQ, levels)"
)]
struct Cli {
    /// Open the device directly even if x1d is running (x1d must not hold the port)
    #[arg(long, global = true, conflicts_with = "dbus")]
    direct: bool,
    /// Require the x1d daemon (fail instead of falling back to direct access)
    #[arg(long, global = true)]
    dbus: bool,
    /// tty to use in direct mode (default: auto-detect 041e:3286)
    #[arg(long, global = true)]
    port: Option<std::path::PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOffToggle {
    On,
    Off,
    Toggle,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show device state
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Super X-Fi on/off/toggle (no argument: show)
    Sxfi { state: Option<OnOffToggle> },
    /// Graphic equalizer
    Eq {
        #[command(subcommand)]
        cmd: Option<EqCmd>,
    },
    /// Headphone level in dB: absolute (`volume -20`) or relative (`volume -r -- -3`, `volume -r 2`)
    #[command(allow_negative_numbers = true)]
    Volume {
        db: Option<String>,
        /// Treat the value as a change relative to the current level
        #[arg(short, long)]
        relative: bool,
    },
    /// Headphone mute on/off/toggle
    Mute { state: Option<OnOffToggle> },
    /// Print events (Super X-Fi button, EQ slot dumps, knob turns) until interrupted
    Monitor,
    /// Show the device's EnableUAC2 flag, or change it (the X1 re-enumerates on USB)
    Uac2 {
        state: Option<OnOff>,
        /// Required to change the flag
        #[arg(long)]
        force: bool,
    },
    /// Super X-Fi capabilities and the headphone profile stored on the device
    SxfiInfo,
    /// Everything the X1 reports about itself (serial, firmware, chip, USB, capabilities)
    Info,
    /// Reset the X1 to factory defaults (EQ in both Super X-Fi slots, Super X-Fi, levels)
    FactoryReset {
        /// Required: confirms you want to reset
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum EqCmd {
    /// Show EQ (default)
    Show,
    /// Enable the EQ
    On,
    /// Disable the EQ
    Off,
    /// Set one band: index 0-9 or frequency (31, 62, 125, 250, 500, 1k, 2k, 4k, 8k, 16k)
    #[command(allow_negative_numbers = true)]
    Set { band: String, db: f32 },
    /// Set the preamp (-12…+12 dB)
    #[command(allow_negative_numbers = true)]
    Preamp { db: f32 },
    /// Set all 10 bands at once (31 Hz → 16 kHz)
    #[command(allow_negative_numbers = true)]
    Bands {
        #[arg(num_args = 10)]
        db: Vec<f32>,
        #[arg(long, default_value_t = 0.0)]
        preamp: f32,
    },
    /// Apply a preset or sound mode by name (case/punctuation-insensitive)
    Preset {
        #[arg(num_args = 1.., required = true)]
        name: Vec<String>,
    },
    /// List presets
    Presets,
    /// List Creative's sound modes for the X1 and the EQ preset each one uses
    Modes,
    /// Import Creative's official X1 presets: downloads Creative's product package, or reads
    /// a ZZ0090.zip (or its extracted folder) you already have
    Import {
        path: Option<std::path::PathBuf>,
        /// Download from this URL instead of Creative's default package URL
        #[arg(long)]
        url: Option<String>,
    },
    /// Save the current EQ as a user preset
    Save { name: String },
}

fn parse_band(s: &str) -> Result<usize, String> {
    if let Ok(i) = s.parse::<usize>() {
        if i < 10 {
            return Ok(i);
        }
        if let Some(p) = proto::BAND_HZ.iter().position(|&hz| hz as usize == i) {
            return Ok(p);
        }
    }
    let l = s.to_ascii_lowercase().replace("hz", "");
    let hz = if let Some(k) = l.strip_suffix('k') {
        k.parse::<f32>().map(|v| (v * 1000.0) as u32).ok()
    } else {
        l.parse::<u32>().ok()
    };
    hz.and_then(|hz| proto::BAND_HZ.iter().position(|&b| b == hz))
        .ok_or_else(|| format!("unknown band '{s}' (use 0-9 or 31,62,125,250,500,1k,2k,4k,8k,16k)"))
}

type R<T> = Result<T, String>;

// ---- backends ----------------------------------------------------------------------------

enum Backend {
    Direct(Box<Device>),
    Bus(Proxy<'static>),
}

impl Backend {
    fn open(cli: &Cli) -> R<Self> {
        if !cli.direct {
            match bus_proxy() {
                Ok(p) => return Ok(Backend::Bus(p)),
                Err(e) if cli.dbus => return Err(format!("x1d not reachable on D-Bus: {e}")),
                Err(e) => log::debug!("x1d not available ({e}); using direct access"),
            }
        }
        let d = match &cli.port {
            Some(p) => Device::connect(p),
            None => Device::connect_auto(),
        }
        .map_err(|e| match e {
            x1::Error::Io(ref io) if io.kind() == std::io::ErrorKind::PermissionDenied => format!(
                "{e}\nhint: install the udev rule (see README) or add yourself to the 'dialout' group"
            ),
            x1::Error::Io(ref io) if io.raw_os_error() == Some(libc_ebusy()) => {
                format!("{e}\nhint: the port is busy - is x1d running? (use it without --direct)")
            }
            other => other.to_string(),
        })?;
        Ok(Backend::Direct(Box::new(d)))
    }

    fn state(&self) -> R<Value> {
        match self {
            Backend::Direct(d) => Ok(serde_json::to_value(d.state()).unwrap()),
            Backend::Bus(p) => {
                let s: String = p.get_property("StateJSON").map_err(es)?;
                serde_json::from_str(&s).map_err(|e| e.to_string())
            }
        }
    }

    fn call<B>(
        &self,
        method: &str,
        body: &B,
        direct: impl FnOnce(&Device) -> x1::Result<()>,
    ) -> R<()>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
    {
        match self {
            Backend::Direct(d) => direct(d).map_err(|e| e.to_string()),
            Backend::Bus(p) => p.call_method(method, body).map(|_| ()).map_err(es),
        }
    }
}

fn libc_ebusy() -> i32 {
    16
}

fn es(e: zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(m), _) => m,
        zbus::Error::FDO(f) => match *f {
            zbus::fdo::Error::Failed(m) | zbus::fdo::Error::InvalidArgs(m) => m,
            other => other.to_string(),
        },
        other => other.to_string(),
    }
}

fn bus_proxy() -> zbus::Result<Proxy<'static>> {
    let conn = Connection::session()?;
    let dbus = zbus::blocking::fdo::DBusProxy::new(&conn)?;
    if !dbus.name_has_owner(x1::DBUS_NAME.try_into()?)? {
        return Err(zbus::Error::Failure("x1d is not running".into()));
    }
    // no property cache: we read right after writing and must not see stale values
    let p = zbus::blocking::proxy::Builder::<Proxy<'static>>::new(&conn)
        .destination(x1::DBUS_NAME)?
        .path(x1::DBUS_PATH)?
        .interface(x1::DBUS_IFACE)?
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()?;
    let connected: bool = p.get_property("Connected")?;
    if !connected {
        return Err(zbus::Error::Failure(
            "x1d is running but the X1 is not connected".into(),
        ));
    }
    Ok(p)
}

// ---- output ------------------------------------------------------------------------------

fn f(v: &Value) -> Option<f64> {
    v.as_f64()
}
fn onoff(v: &Value) -> &'static str {
    match v.as_bool() {
        Some(true) => "on",
        Some(false) => "off",
        None => "?",
    }
}

fn print_eq(s: &Value) {
    println!(
        "EQ:        {}   preamp {:+.1} dB",
        onoff(&s["eq_enabled"]),
        f(&s["preamp"]).unwrap_or(0.0)
    );
    let bands = s["bands"].as_array().cloned().unwrap_or_default();
    let mut line1 = String::new();
    let mut line2 = String::new();
    for (i, hz) in proto::BAND_HZ.iter().enumerate() {
        let lbl = if *hz >= 1000 {
            format!("{}k", hz / 1000)
        } else {
            hz.to_string()
        };
        line1.push_str(&format!("{lbl:>6}"));
        match bands.get(i).and_then(|b| b.as_f64()) {
            Some(v) => line2.push_str(&format!("{v:>+6.1}")),
            None => line2.push_str(&format!("{:>6}", "?")),
        }
    }
    println!("    Hz {line1}");
    println!("    dB {line2}");
}

fn print_status(s: &Value) {
    println!(
        "Device:    Sound Blaster X1 ({}), firmware {}, serial {}",
        s["port"].as_str().unwrap_or("?"),
        s["firmware"].as_str().unwrap_or("?"),
        s["serial"].as_str().unwrap_or("?")
    );
    println!("Super X-Fi: {}", onoff(&s["super_xfi"]));
    print_eq(s);
    if let Some(on) = s["super_xfi"].as_bool() {
        println!(
            "EQ slot:   {} (the X1 keeps one EQ for Super X-Fi on and one for off)",
            if on {
                "Super X-Fi on"
            } else {
                "Super X-Fi off"
            }
        );
    }
    let vol = f(&s["volume_db"])
        .map(|v| format!("{v:+.1} dB"))
        .unwrap_or("?".into());
    println!("Headphone: {vol}, mute {}", onoff(&s["muted"]));
    if !s["mic_index"].is_null() {
        let mic = f(&s["mic_db"])
            .map(|v| format!("{v:+.1} dB"))
            .unwrap_or("?".into());
        println!("Mic:       {mic}, mute {}", onoff(&s["mic_muted"]));
    }
    println!("UAC2 flag: {}", onoff(&s["uac2_forced"]));
    let feats = |k: &str| {
        s[k].as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    println!(
        "On-board:  DSP [{}], features [{}]",
        feats("dsp_features"),
        feats("features")
    );
    if let Some(c) = s["sxfi_caps"].as_str() {
        println!("SXFI caps: {c}");
    }
}

// ---- main --------------------------------------------------------------------------------

fn run(cli: Cli) -> R<()> {
    // Purely local commands
    if let Cmd::Eq {
        cmd: Some(EqCmd::Presets),
    } = &cli.cmd
    {
        let fmt = |p: &presets::Preset| {
            let b: Vec<String> = p.bands.iter().map(|v| format!("{v:>+5.1}")).collect();
            format!("{}  pre {:+.1}", b.join(""), p.preamp)
        };
        if presets::load_imported().is_none() {
            println!(
                "(Creative's official X1 presets aren't imported yet — run `x1ctl eq import`)\n"
            );
        }
        println!(
            "{:<38}   31   62  125  250  500   1k   2k   4k   8k  16k",
            "Presets"
        );
        for (name, p) in presets::builtin_ordered() {
            println!("{name:<38}{}", fmt(&p));
        }
        let user = presets::load_user();
        if !user.is_empty() {
            println!("\nUser presets ({})", presets::user_path().display());
            for (name, p) in user {
                println!("{name:<38}{}", fmt(&p));
            }
        }
        return Ok(());
    }
    if let Cmd::Eq {
        cmd: Some(EqCmd::Import { path, url }),
    } = &cli.cmd
    {
        let imp = match path {
            Some(p) => presets::read_package(p)?,
            None => {
                let u = url.as_deref().unwrap_or(presets::PACKAGE_URL);
                eprintln!("downloading {u}");
                presets::download_package(u).map_err(|e| {
                    format!(
                        "{e}\nDownload it in a browser instead and run: x1ctl eq import ~/Downloads/ZZ0090.zip"
                    )
                })?
            }
        };
        let path = presets::save_imported(&imp).map_err(|e| e.to_string())?;
        println!(
            "imported {} EQ presets and {} sound modes into {}",
            imp.eq_presets.len(),
            imp.sound_modes.len(),
            path.display()
        );
        return Ok(());
    }
    if let Cmd::Eq {
        cmd: Some(EqCmd::Modes),
    } = &cli.cmd
    {
        if presets::sound_modes().is_empty() {
            return Err("no sound modes yet — run `x1ctl eq import` first".into());
        }
        println!(
            "Sound modes (on the X1 only the EQ part applies; the rest is Windows-side Acoustic Engine):"
        );
        for m in presets::sound_modes() {
            println!(
                "  {:<38} EQ {:<4} → {}",
                m.name,
                if m.eq_enabled { "on" } else { "off" },
                m.eq_preset
            );
        }
        return Ok(());
    }
    if cli.direct {
        if let Ok(p) = bus_proxy() {
            let port: String = p.get_property("Port").unwrap_or_default();
            return Err(format!(
                "x1d currently owns the X1 ({port}); run without --direct, or stop x1d first"
            ));
        }
    }

    let be = Backend::open(&cli)?;
    let cur = || be.state();

    match cli.cmd {
        Cmd::Status { json } => {
            let s = cur()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&s).unwrap());
            } else {
                print_status(&s);
            }
        }
        Cmd::Sxfi { state: None } => println!("{}", onoff(&cur()?["super_xfi"])),
        Cmd::Sxfi { state: Some(t) } => {
            let on = match t {
                OnOffToggle::On => true,
                OnOffToggle::Off => false,
                OnOffToggle::Toggle => !cur()?["super_xfi"].as_bool().unwrap_or(false),
            };
            be.call("SetSuperXFi", &(on,), |d| d.set_super_xfi(on))?;
            let s = cur()?;
            println!("Super X-Fi {}", onoff(&s["super_xfi"]));
            print_eq(&s);
        }
        Cmd::Eq { cmd } => match cmd.unwrap_or(EqCmd::Show) {
            EqCmd::Show => print_eq(&cur()?),
            EqCmd::On => be.call("SetEqEnabled", &(true,), |d| d.set_eq_enabled(true))?,
            EqCmd::Off => be.call("SetEqEnabled", &(false,), |d| d.set_eq_enabled(false))?,
            EqCmd::Set { band, db } => {
                let i = parse_band(&band)?;
                if db.abs() > proto::BAND_LIMIT_DB {
                    eprintln!("note: clamped to ±{} dB", proto::BAND_LIMIT_DB);
                }
                be.call("SetBand", &(i as u8, db as f64), |d| d.set_band(i, db))?;
                print_eq(&cur()?);
            }
            EqCmd::Preamp { db } => {
                be.call("SetPreamp", &(db as f64,), |d| d.set_preamp(db))?;
                print_eq(&cur()?);
            }
            EqCmd::Bands { db, preamp } => {
                let arr: [f32; 10] = db
                    .clone()
                    .try_into()
                    .map_err(|_| "need 10 values".to_string())?;
                let v: Vec<f64> = db.iter().map(|&x| x as f64).collect();
                be.call("SetBands", &(v, preamp as f64), |d| d.set_eq(&arr, preamp))?;
                print_eq(&cur()?);
            }
            EqCmd::Preset { name } => {
                let name = name.join(" ");
                let (canon, p) = presets::find(&name)
                    .ok_or_else(|| format!("no preset '{name}' (see `x1ctl eq presets`)"))?;
                be.call("ApplyPreset", &(canon.as_str(),), |d| {
                    d.set_eq(&p.bands, p.preamp)
                })?;
                println!("applied '{canon}'");
                print_eq(&cur()?);
            }
            EqCmd::Save { name } => {
                let s = cur()?;
                let mut bands = [0f32; 10];
                for (i, b) in bands.iter_mut().enumerate() {
                    *b = s["bands"][i].as_f64().ok_or("EQ not read yet")? as f32;
                }
                let path = presets::save_user(
                    &name,
                    presets::Preset {
                        bands,
                        preamp: f(&s["preamp"]).unwrap_or(0.0) as f32,
                    },
                )
                .map_err(|e| e.to_string())?;
                println!("saved '{name}' to {}", path.display());
            }
            EqCmd::Presets | EqCmd::Modes | EqCmd::Import { .. } => unreachable!(),
        },
        Cmd::Volume { db: None, .. } => {
            let s = cur()?;
            println!(
                "{}",
                f(&s["volume_db"])
                    .map(|v| format!("{v:+.2} dB"))
                    .unwrap_or("?".into())
            );
        }
        Cmd::Volume {
            db: Some(v),
            relative,
        } => {
            let x: f64 = v
                .trim_end_matches("dB")
                .trim()
                .parse()
                .map_err(|_| format!("bad dB value '{v}'"))?;
            let target = if relative {
                f(&cur()?["volume_db"]).ok_or("current level unknown")? + x
            } else {
                x
            };
            be.call("SetVolume", &(target,), |d| {
                d.set_volume_db(target).map(|_| ())
            })?;
            println!(
                "{}",
                f(&cur()?["volume_db"])
                    .map(|v| format!("{v:+.2} dB"))
                    .unwrap_or("?".into())
            );
        }
        Cmd::Mute { state } => {
            let on = match state {
                None => {
                    println!("{}", onoff(&cur()?["muted"]));
                    return Ok(());
                }
                Some(OnOffToggle::On) => true,
                Some(OnOffToggle::Off) => false,
                Some(OnOffToggle::Toggle) => !cur()?["muted"].as_bool().unwrap_or(false),
            };
            be.call("SetMuted", &(on,), |d| d.set_muted(on))?;
            println!("mute {}", if on { "on" } else { "off" });
        }
        Cmd::Uac2 { state: None, .. } => {
            let s = cur()?;
            println!("EnableUAC2 flag: {}", onoff(&s["uac2_forced"]));
        }
        Cmd::Uac2 {
            state: Some(st),
            force,
        } => {
            if !force {
                return Err("changing EnableUAC2 makes the X1 re-enumerate on USB and is not needed on Linux \
                            (it already runs as a UAC2 device). Re-run with --force if you really want it."
                    .into());
            }
            let on = matches!(st, OnOff::On);
            be.call("SetUAC2Forced", &(on,), |d| d.set_uac2_forced(on))?;
            println!(
                "EnableUAC2 set {}; the X1 will now re-enumerate",
                if on { "on" } else { "off" }
            );
        }
        Cmd::SxfiInfo => {
            let s = cur()?;
            println!("capabilities: {}", s["sxfi_caps"].as_str().unwrap_or("?"));
            let hp = match &be {
                Backend::Direct(d) => d.headphone_profile().map_err(|e| e.to_string()),
                Backend::Bus(p) => p
                    .call::<_, _, (String, String)>("HeadphoneProfile", &())
                    .map_err(es),
            };
            match hp {
                Ok((id, file)) => println!("headphone profile: id '{id}', file '{file}'"),
                Err(e) => println!("headphone profile: unavailable ({e})"),
            }
        }
        Cmd::Info => print_info(&cur()?),
        Cmd::FactoryReset { yes } => {
            if !yes {
                return Err("this resets the X1's settings (EQ in both Super X-Fi slots, Super X-Fi, levels) \
                            to factory defaults. Re-run with --yes to do it."
                    .into());
            }
            be.call("FactoryReset", &(), |d| d.factory_reset())?;
            println!("factory reset sent; the X1 may reconnect");
        }
        Cmd::Monitor => monitor(be)?,
    }
    Ok(())
}

fn print_info(s: &Value) {
    let row = |k: &str, v: String| println!("{k:<22} {v}");
    let st = |k: &str| s[k].as_str().unwrap_or("?").to_string();
    row(
        "Model",
        "Creative Sound Blaster X1 (ZZ0090, codename Allion)".into(),
    );
    row(
        "USB ID",
        format!(
            "{:04x}:{:04x}",
            s["vid"].as_u64().unwrap_or(0),
            s["pid"].as_u64().unwrap_or(0)
        ),
    );
    row("Serial number", st("serial"));
    row("Firmware", st("firmware"));
    if let Some(v) = s["fw_version_code"].as_u64() {
        row("Firmware build code", format!("0x{v:08X}"));
    }
    if let Some(v) = s["mcu_type"].as_u64() {
        row("Chip / MCU type", format!("0x{v:02X} ({v})"));
    }
    if let Some(u) = s["usb"].as_object() {
        let g = |k: &str| u.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
        row(
            "USB product",
            format!("{} — {}", g("manufacturer"), g("product")),
        );
        row("USB device release", g("bcd_device"));
        row(
            "USB link",
            format!(
                "USB {} at {} Mbit/s, bus path {}",
                g("usb_version"),
                g("speed_mbps"),
                g("bus_path")
            ),
        );
    }
    row("Control port", st("port"));
    if let Some(v) = s["max_payload"].as_u64() {
        row("Max payload", format!("{v} bytes"));
    }
    let list = |k: &str| {
        s[k].as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    row("On-board DSP", list("dsp_features"));
    row("Feature flags", list("features"));
    if let Some(b) = s["buttons"].as_array() {
        let names: Vec<String> = b
            .iter()
            .filter_map(|x| x.as_u64())
            .map(|x| match x as u16 {
                0x001E => "Super X-Fi".to_string(),
                0x400A => "play/pause".to_string(),
                0x400B => "previous".to_string(),
                0x400C => "next".to_string(),
                o => format!("0x{o:04X}"),
            })
            .collect();
        row("Hardware buttons", names.join(", "));
    }
    row("Super X-Fi engine", st("sxfi_caps"));
    let hp = s["headphone_profile"].as_array().map(|a| {
        let id = a.first().and_then(|v| v.as_str()).unwrap_or("");
        if id.is_empty() {
            "none (generic profile)".to_string()
        } else {
            id.to_string()
        }
    });
    row("Personal SXFI profile", hp.unwrap_or("?".into()));
    row(
        "Operation mode",
        match s["operation_mode"].as_u64() {
            Some(0) => "normal".into(),
            Some(v) => v.to_string(),
            None => "?".into(),
        },
    );
    row("Factory reset", "x1ctl factory-reset --yes".into());
}

fn monitor(be: Backend) -> R<()> {
    match be {
        Backend::Direct(d) => {
            eprintln!("monitoring {} (Ctrl-C to stop)", d.port_path().display());
            d.set_listener(Box::new(|n: &Notice, s: &State| match n {
                Notice::Event(ev) => println!("{}", describe(ev, s)),
                Notice::Disconnected => println!("disconnected"),
            }));
            while d.is_alive() {
                std::thread::sleep(Duration::from_millis(200));
            }
            Ok(())
        }
        Backend::Bus(p) => {
            eprintln!("monitoring via x1d (Ctrl-C to stop)");
            let props = zbus::blocking::fdo::PropertiesProxy::builder(p.connection())
                .destination(x1::DBUS_NAME)
                .map_err(es)?
                .path(x1::DBUS_PATH)
                .map_err(es)?
                .build()
                .map_err(es)?;
            for sig in props.receive_properties_changed().map_err(es)? {
                let Ok(args) = sig.args() else { continue };
                for (k, v) in args.changed_properties() {
                    if *k == "StateJSON" {
                        continue;
                    }
                    println!("{k} = {}", fmt_variant(v));
                }
            }
            Ok(())
        }
    }
}

fn fmt_variant(v: &zbus::zvariant::Value) -> String {
    use zbus::zvariant::Value as V;
    match v {
        V::Bool(b) => b.to_string(),
        V::F64(x) => format!("{x:+.2}"),
        V::Str(s) => s.to_string(),
        V::I32(i) => i.to_string(),
        V::Array(a) => {
            let items: Vec<String> = a.iter().map(fmt_variant).collect();
            format!("[{}]", items.join(" "))
        }
        other => format!("{other:?}"),
    }
}

fn describe(ev: &proto::Event, s: &State) -> String {
    use proto::Event as E;
    match ev {
        E::SuperXFi(on) => format!("super-xfi {}", if *on { "on" } else { "off" }),
        E::Params(b) => {
            let items: Vec<String> = b
                .params
                .iter()
                .map(|&(id, v)| match id {
                    proto::EQ_ENABLE => format!("eq={}", if v >= 0.5 { "on" } else { "off" }),
                    proto::EQ_PREAMP => format!("preamp={v:+.1}"),
                    id => {
                        let hz = proto::BAND_HZ[(id - proto::EQ_BAND0) as usize];
                        format!("{hz}Hz={v:+.1}")
                    }
                })
                .collect();
            format!("eq {}{}", items.join(" "), if b.more { " …" } else { "" })
        }
        E::Profile { kind, index } => format!("eq-profile type={kind} index={index}"),
        E::Level { index, raw } => {
            let who = if Some(*index) == s.mic_index {
                "mic"
            } else {
                "volume"
            };
            format!("{who} {:+.2} dB", proto::level_to_db(*raw))
        }
        E::Mute { index, muted } => {
            let who = if Some(*index) == s.mic_index {
                "mic-mute"
            } else {
                "mute"
            };
            format!("{who} {}", if *muted { "on" } else { "off" })
        }
        E::SxfiNotify(p) => format!("sxfi-notify {}", proto::hex(p)),
        E::Ack { op, status } => format!("ack {op:02X} {status:02X}"),
        E::Other(f) => format!("frame {f}"),
    }
}

fn main() -> ExitCode {
    // behave like a normal CLI when piped into head/grep
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("x1ctl: {e}");
            ExitCode::FAILURE
        }
    }
}
