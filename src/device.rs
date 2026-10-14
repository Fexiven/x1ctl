//! Session with a Sound Blaster X1: handshake, background reader, request/response
//! matching tolerant of interleaved events, and a state cache fed by *every* frame
//! (replies, echoes and unsolicited events alike).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::auth;
use crate::error::{Error, Result};
use crate::proto::{self, Event, Frame, Parser};
use crate::serial::{self, Port};

const REPLY_TIMEOUT: Duration = Duration::from_millis(1500);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    pub connected: bool,
    pub port: String,
    pub vid: u16,
    pub pid: u16,
    pub firmware: String,
    pub serial: String,
    /// Malcolm sub-features on board (X1: GraphicEQ only).
    pub dsp_features: Vec<String>,
    /// FeatureControl features supported (X1: EnableUAC2 only).
    pub features: Vec<String>,
    pub buttons: Vec<u16>,
    pub super_xfi: Option<bool>,
    pub eq_enabled: Option<bool>,
    pub preamp: Option<f32>,
    pub bands: [Option<f32>; proto::EQ_BANDS],
    /// Active device EQ profile reported by `1A` (follows Super X-Fi).
    pub eq_profile: Option<u8>,
    pub headphone_index: Option<u8>,
    pub mic_index: Option<u8>,
    pub volume_db: Option<f64>,
    pub muted: Option<bool>,
    pub mic_db: Option<f64>,
    pub mic_muted: Option<bool>,
    pub volume_range_db: Option<(f64, f64, f64)>,
    pub mic_range_db: Option<(f64, f64, f64)>,
    /// `FeatureControl` EnableUAC2 currently set.
    pub uac2_forced: Option<bool>,
    pub operation_mode: Option<u8>,
    pub sxfi_caps: Option<String>,
    pub headphone_profile: Option<(String, String)>,
    /// Last `6F 05 xx` notification payload (meaning unknown).
    pub sxfi_notify: Option<Vec<u8>>,
    /// Internal firmware version (`09/01`), matches Creative's update metadata.
    pub fw_version_code: Option<u32>,
    /// MCU/chip type (`09/01`), = `chipId` in Creative's metadata.
    pub mcu_type: Option<u8>,
    pub max_payload: Option<u16>,
    pub usb: Option<serial::UsbInfo>,
    /// Increments whenever a full EQ dump finishes (second dump frame, `more = 0`).
    #[serde(skip)]
    pub dump_seq: u64,
}

impl State {
    fn apply(&mut self, ev: &Event) {
        match ev {
            Event::SuperXFi(on) => self.super_xfi = Some(*on),
            Event::Params(b) => {
                for &(id, v) in &b.params {
                    match id {
                        proto::EQ_ENABLE => self.eq_enabled = Some(v >= 0.5),
                        proto::EQ_PREAMP => self.preamp = Some(v),
                        id if (proto::EQ_BAND0..proto::EQ_BAND0 + 10).contains(&id) => {
                            self.bands[(id - proto::EQ_BAND0) as usize] = Some(v)
                        }
                        _ => {}
                    }
                }
                if !b.more && b.params.len() > 1 {
                    self.dump_seq += 1;
                }
            }
            Event::Profile { kind, index } if *kind == proto::PROFILE_TYPE_DEVICE => {
                self.eq_profile = Some(*index)
            }
            Event::Level { index, raw } => {
                if Some(*index) == self.headphone_index.or(Some(0)) {
                    self.volume_db = Some(proto::level_to_db(*raw));
                } else if Some(*index) == self.mic_index {
                    self.mic_db = Some(proto::level_to_db(*raw));
                }
            }
            Event::Mute { index, muted } => {
                if Some(*index) == self.headphone_index.or(Some(0)) {
                    self.muted = Some(*muted);
                } else if Some(*index) == self.mic_index {
                    self.mic_muted = Some(*muted);
                }
            }
            Event::SxfiNotify(p) => self.sxfi_notify = Some(p.clone()),
            _ => {}
        }
    }
}

#[derive(Debug, Clone)]
pub enum Notice {
    Event(Event),
    Disconnected,
}

pub type Listener = Box<dyn Fn(&Notice, &State) + Send + Sync>;

struct Waiter {
    pred: Box<dyn Fn(&Frame) -> bool + Send>,
    tx: SyncSender<Frame>,
}

struct Shared {
    waiter: Mutex<Option<Waiter>>,
    state: Mutex<State>,
    cond: Condvar,
    alive: AtomicBool,
    stop: AtomicBool,
    sxfi_bit: Mutex<u8>,
    listener: Mutex<Option<Listener>>,
}

pub struct Device {
    port: Mutex<Port>,
    req: Mutex<()>,
    sh: Arc<Shared>,
    reader: Option<JoinHandle<()>>,
}

impl Drop for Device {
    fn drop(&mut self) {
        self.sh.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
    }
}

// ---- handshake (text mode) --------------------------------------------------------------------

fn has_frame(bytes: &[u8], op: u8) -> bool {
    let mut p = Parser::default();
    p.push(bytes);
    std::iter::from_fn(|| p.next_frame()).any(|f| f.op() == Some(op))
}

enum AuthReply {
    Challenge(auth::Challenge),
    AlreadyUnlocked,
    NotYet,
    Garbage,
}

fn classify_auth(b: &[u8]) -> AuthReply {
    // Skip a possible echo of our own "whoareyou\r\n": use the first occurrence that is
    // followed by a full challenge rather than by CRLF.
    let mut i = 0;
    while let Some(off) = auth::find(&b[i..], auth::WHOAREYOU) {
        let s = i + off;
        let rest = &b[s + auth::WHOAREYOU.len()..];
        if rest.len() >= 36 && !rest.starts_with(b"\r\n") {
            if let Ok(c) = auth::parse_challenge(&b[s..]) {
                return AuthReply::Challenge(c);
            }
        }
        i = s + 1;
    }
    if auth::find(b, b"Unknown command").is_some() {
        AuthReply::AlreadyUnlocked
    } else if auth::find(b, b"NotYet").is_some() {
        AuthReply::NotYet
    } else {
        AuthReply::Garbage
    }
}

fn auth_complete(b: &[u8]) -> bool {
    !matches!(classify_auth(b), AuthReply::Garbage)
}

fn probe_binary(port: &mut Port) -> Result<bool> {
    // MaxPayloadSize + CRLF: answered in binary mode; in text mode the CRLF flushes the
    // device's line buffer so the next text command lands cleanly.
    port.write_all(&[0x5A, 0x03, 0x00, b'\r', b'\n'])?;
    let got = port.read_until(Duration::from_millis(700), |b| {
        has_frame(b, proto::OP_MAX_PAYLOAD)
    })?;
    log::debug!("probe reply: {}", proto::hex(&got));
    Ok(has_frame(&got, proto::OP_MAX_PAYLOAD))
}

fn authenticate(port: &mut Port) -> Result<Option<u16>> {
    for attempt in 0..10 {
        port.drain()?;
        port.write_all(b"whoareyou\r\n")?;
        let got = port.read_until(Duration::from_millis(1500), auth_complete)?;
        match classify_auth(&got) {
            AuthReply::AlreadyUnlocked => {
                log::info!("already unlocked");
                return Ok(None);
            }
            AuthReply::Challenge(c) => {
                log::debug!("challenge header {:02X?} pid {:04X}", c.header, c.pid());
                let r = auth::compute_response(&c)?;
                let mut msg = b"unlock".to_vec();
                msg.extend_from_slice(&r);
                msg.extend_from_slice(b"\r\n");
                port.write_all(&msg)?;
                let ok = port.read_until(Duration::from_millis(1500), |b| {
                    auth::find(b, b"unlock_OK").is_some() || auth::find(b, b"\r\n").is_some()
                })?;
                if auth::find(&ok, b"unlock_OK").is_some() {
                    log::info!("unlock_OK");
                    return Ok(Some(c.pid()));
                }
                return Err(Error::Auth(format!(
                    "device answered {:?}",
                    String::from_utf8_lossy(&ok)
                )));
            }
            AuthReply::NotYet | AuthReply::Garbage => {
                log::info!(
                    "device not ready (attempt {}): {:?}",
                    attempt + 1,
                    String::from_utf8_lossy(&got)
                );
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    Err(Error::NotReady)
}

// ---- device -------------------------------------------------------------------------------

impl Device {
    /// Find the X1, unlock it if needed, switch to binary mode, and read its state.
    pub fn connect_auto() -> Result<Self> {
        Self::connect(&serial::find_x1()?)
    }

    pub fn connect(path: &Path) -> Result<Self> {
        let mut port = Port::open(path)?;
        let _ = port.drain()?;
        if probe_binary(&mut port)? {
            log::info!("{}: already in binary mode", path.display());
        } else {
            log::info!("{}: text mode, authenticating", path.display());
            authenticate(&mut port)?;
            port.write_all(b"SW_MODE1\r\n")?;
            std::thread::sleep(Duration::from_millis(150));
            let _ = port.drain()?; // 5B 02 … mode-change announcement
        }
        let dev = Self::start(port)?;
        dev.ping()
            .map_err(|e| Error::Protocol(format!("no binary reply after mode switch: {e}")))?;
        dev.refresh_all();
        Ok(dev)
    }

    fn start(port: Port) -> Result<Self> {
        let sh = Arc::new(Shared {
            waiter: Mutex::new(None),
            state: Mutex::new(State {
                connected: true,
                port: port.path().display().to_string(),
                ..Default::default()
            }),
            cond: Condvar::new(),
            alive: AtomicBool::new(true),
            stop: AtomicBool::new(false),
            sxfi_bit: Mutex::new(0),
            listener: Mutex::new(None),
        });
        let mut rport = port.try_clone()?;
        let sh2 = sh.clone();
        let reader = std::thread::Builder::new()
            .name("x1-reader".into())
            .spawn(move || reader_loop(&mut rport, &sh2))?;
        Ok(Self {
            port: Mutex::new(port),
            req: Mutex::new(()),
            sh,
            reader: Some(reader),
        })
    }

    pub fn set_listener(&self, l: Listener) {
        *self.sh.listener.lock().unwrap() = Some(l);
    }

    pub fn is_alive(&self) -> bool {
        self.sh.alive.load(Ordering::SeqCst)
    }

    pub fn state(&self) -> State {
        self.sh.state.lock().unwrap().clone()
    }

    pub fn port_path(&self) -> PathBuf {
        self.port.lock().unwrap().path().to_path_buf()
    }

    fn send(&self, f: &Frame) -> Result<()> {
        let Frame::Cmd { op, payload } = f else {
            return Err(Error::Refused("5B frames are never sent".into()));
        };
        proto::check_outgoing(*op, payload)?;
        log::trace!("TX {f}");
        let r = self.port.lock().unwrap().write_all(&f.to_bytes());
        if let Err(Error::Disconnected) = r {
            // the reader may not have noticed yet; let the supervisor reconnect right away
            self.sh.alive.store(false, Ordering::SeqCst);
            self.sh.stop.store(true, Ordering::SeqCst);
        }
        r
    }

    /// Send `f` and wait for the first frame satisfying `pred`. A NAK for the same opcode
    /// also completes the request (as an error).
    fn request(&self, f: &Frame, pred: impl Fn(&Frame) -> bool + Send + 'static) -> Result<Frame> {
        if !self.is_alive() {
            return Err(Error::Disconnected);
        }
        let _g = self.req.lock().unwrap();
        let op = f.op().unwrap_or(0);
        let (tx, rx) = mpsc::sync_channel(1);
        *self.sh.waiter.lock().unwrap() = Some(Waiter {
            pred: Box::new(move |fr| {
                pred(fr) || matches!(fr.ack(), Some((o, s)) if o == op && s != 0)
            }),
            tx,
        });
        let res = self
            .send(f)
            .and_then(|_| match rx.recv_timeout(REPLY_TIMEOUT) {
                Ok(fr) => match fr.ack() {
                    Some((o, s)) if o == op && s != 0 => Err(Error::Nak(o, s)),
                    _ => Ok(fr),
                },
                Err(RecvTimeoutError::Timeout) if self.is_alive() => {
                    Err(Error::Timeout("device reply"))
                }
                Err(_) => Err(Error::Disconnected),
            });
        *self.sh.waiter.lock().unwrap() = None;
        res
    }

    fn request_data(&self, f: &Frame, sub: Option<u8>) -> Result<Frame> {
        let op = f.op().unwrap();
        self.request(f, move |fr| {
            fr.op() == Some(op) && sub.is_none_or(|s| fr.payload().first() == Some(&s))
        })
    }

    fn request_ack(&self, f: &Frame) -> Result<()> {
        let op = f.op().unwrap();
        self.request(f, move |fr| matches!(fr.ack(), Some((o, _)) if o == op))
            .map(|_| ())
    }

    fn update(&self, ev: Event) {
        let mut st = self.sh.state.lock().unwrap();
        st.apply(&ev);
        let snap = st.clone();
        drop(st);
        self.sh.cond.notify_all();
        if let Some(l) = self.sh.listener.lock().unwrap().as_ref() {
            l(&Notice::Event(ev), &snap);
        }
    }

    fn set_state(&self, f: impl FnOnce(&mut State)) {
        f(&mut self.sh.state.lock().unwrap());
    }

    // ---- queries ----------------------------------------------------------------------------

    pub fn ping(&self) -> Result<u16> {
        proto::parse_max_payload(self.request_data(&proto::req_ping(), None)?.payload())
    }

    pub fn ids(&self) -> Result<(u16, u16, u16)> {
        proto::parse_ids(
            self.request_data(&proto::req_info(0x00), Some(0x00))?
                .payload(),
        )
    }

    pub fn firmware(&self) -> Result<String> {
        proto::parse_info_string(
            self.request_data(&proto::req_info(0x02), Some(0x02))?
                .payload(),
        )
    }

    pub fn serial(&self) -> Result<String> {
        proto::parse_info_string(
            self.request_data(&proto::req_info(0x03), Some(0x03))?
                .payload(),
        )
    }

    pub fn dsp_features(&self) -> Result<u32> {
        proto::parse_subfeatures(
            self.request_data(&proto::req_subfeatures(), None)?
                .payload(),
        )
    }

    pub fn buttons(&self) -> Result<Vec<u16>> {
        let b = proto::parse_buttons(
            self.request_data(&proto::req_buttons(), Some(0x05))?
                .payload(),
        )?;
        if let Some(i) = b.iter().position(|&x| x == proto::BUTTON_SXFI) {
            *self.sh.sxfi_bit.lock().unwrap() = i as u8;
        }
        Ok(b)
    }

    pub fn super_xfi(&self) -> Result<bool> {
        let f = self.request_data(&proto::req_button_state(), Some(0x08))?;
        let (_, mask) = proto::parse_button_state(f.payload())?;
        Ok(mask & (1u64 << *self.sh.sxfi_bit.lock().unwrap()) != 0)
    }

    pub fn get_param(&self, id: u8) -> Result<f32> {
        let f = self.request(&proto::req_get_param(id), move |fr| {
            fr.op() == Some(proto::OP_GET_PARAM)
                && proto::parse_params(fr.payload())
                    .is_ok_and(|b| b.params.iter().any(|&(i, _)| i == id))
        })?;
        let b = proto::parse_params(f.payload())?;
        Ok(b.params
            .iter()
            .find(|&&(i, _)| i == id)
            .map(|&(_, v)| v)
            .unwrap())
    }

    /// Read EQ enable, preamp and all 10 bands (reader thread updates the cache).
    pub fn read_eq(&self) -> Result<()> {
        for id in proto::EQ_ENABLE..proto::EQ_BAND0 + 10 {
            self.get_param(id)?;
        }
        Ok(())
    }

    pub fn eq_profile(&self) -> Result<u8> {
        let f = self.request_data(&proto::req_active_profile(), None)?;
        let p = f.payload();
        if p.len() < 2 {
            return Err(Error::Protocol(format!(
                "ActiveMalcolmProfile: {}",
                proto::hex(p)
            )));
        }
        Ok(p[1])
    }

    pub fn audio_controls(&self) -> Result<Vec<proto::AudioControl>> {
        proto::parse_audio_controls(
            self.request_data(&proto::req_audio_controls(), None)?
                .payload(),
        )
    }

    pub fn level_ranges(&self, idx: &[u8]) -> Result<Vec<proto::LevelRange>> {
        proto::parse_level_ranges(
            self.request_data(&proto::req_level_ranges(idx), None)?
                .payload(),
        )
    }

    pub fn level(&self, idx: u8) -> Result<f64> {
        let f = self.request(&proto::req_get_level(idx), move |fr| {
            fr.op() == Some(proto::OP_AUDIO_LEVEL)
                && fr.payload().len() == 3
                && fr.payload()[0] == idx
        })?;
        let p = f.payload();
        Ok(proto::level_to_db(i16::from_le_bytes([p[1], p[2]])))
    }

    pub fn mute(&self, idx: u8) -> Result<bool> {
        let f = self.request(&proto::req_get_mute(idx), move |fr| {
            fr.op() == Some(proto::OP_AUDIO_MUTE)
                && fr.payload().len() == 2
                && fr.payload()[0] == idx
        })?;
        Ok(f.payload()[1] != 0)
    }

    pub fn feature_support(&self) -> Result<(u32, u32)> {
        proto::parse_feature_support(
            self.request_data(&proto::req_feature_support(), Some(0x02))?
                .payload(),
        )
    }

    pub fn uac2_forced(&self) -> Result<bool> {
        let f = self.request_data(
            &proto::req_feature_get(proto::FEATURE_ENABLE_UAC2),
            Some(0x01),
        )?;
        Ok(proto::parse_feature_get(f.payload())? & proto::FEATURE_ENABLE_UAC2 != 0)
    }

    pub fn operation_mode(&self) -> Result<u8> {
        let f = self.request_data(&proto::req_operation_mode(), Some(0x01))?;
        f.payload()
            .get(1)
            .copied()
            .ok_or_else(|| Error::Protocol("DeviceOperationMode: short".into()))
    }

    pub fn sxfi_caps(&self) -> Result<proto::SxfiCaps> {
        proto::parse_sxfi_caps(
            self.request_data(&proto::req_sxfi_caps(), Some(0x00))?
                .payload(),
        )
    }

    pub fn fw_version(&self) -> Result<(u32, u8, u8)> {
        proto::parse_fw_version(
            self.request_data(&proto::req_fw_version(), Some(0x01))?
                .payload(),
        )
    }

    /// Creative App's "Reset to factory defaults": `5A 9B 01 01` (FactoryReset, type General),
    /// exactly what Creative's CTCDC.dll sends for `CTCDCCMD_FactoryReset`. Like Creative App,
    /// no support query first — the X1 rejects the `9B 02` query but that says nothing about
    /// the reset itself. The X1 may reboot and re-enumerate afterwards, ending this session.
    pub fn factory_reset(&self) -> Result<()> {
        let op = proto::OP_FACTORY_RESET;
        match self.request(&proto::req_factory_reset(), move |fr| {
            fr.op() == Some(op) || matches!(fr.ack(), Some((o, _)) if o == op)
        }) {
            Ok(fr) => {
                log::info!("factory reset reply: {fr}");
                Ok(())
            }
            // the device may reset before answering
            Err(Error::Timeout(_)) | Err(Error::Disconnected) => Ok(()),
            Err(Error::Nak(_, 0x81)) => Err(Error::Refused(
                "the X1 rejected the factory reset (not supported)".into(),
            )),
            Err(e) => Err(e),
        }
    }

    pub fn headphone_profile(&self) -> Result<(String, String)> {
        proto::parse_hp_info(
            self.request_data(&proto::req_sxfi_hp_info(), Some(0x06))?
                .payload(),
        )
    }

    /// Read everything we know how to read; individual failures are logged, not fatal.
    pub fn refresh_all(&self) {
        fn log_err<T>(what: &str, r: Result<T>) -> Option<T> {
            r.map_err(|e| log::warn!("{what}: {e}")).ok()
        }
        if let Some((pid, _, vid)) = log_err("ids", self.ids()) {
            self.set_state(|s| (s.pid, s.vid) = (pid, vid));
        }
        if let Some(v) = log_err("firmware", self.firmware()) {
            self.set_state(|s| s.firmware = v);
        }
        if let Some(v) = log_err("serial", self.serial()) {
            self.set_state(|s| s.serial = v);
        }
        if let Some(v) = log_err("buttons", self.buttons()) {
            self.set_state(|s| s.buttons = v);
        }
        if let Some(m) = log_err("dsp features", self.dsp_features()) {
            self.set_state(|s| {
                s.dsp_features = proto::subfeature_names(m)
                    .into_iter()
                    .map(String::from)
                    .collect()
            });
        }
        if let Some((m, _)) = log_err("feature support", self.feature_support()) {
            self.set_state(|s| {
                s.features = proto::feature_names(m)
                    .into_iter()
                    .map(String::from)
                    .collect()
            });
            if m & proto::FEATURE_ENABLE_UAC2 != 0 {
                if let Some(v) = log_err("uac2", self.uac2_forced()) {
                    self.set_state(|s| s.uac2_forced = Some(v));
                }
            }
        }
        if let Some(v) = log_err("operation mode", self.operation_mode()) {
            self.set_state(|s| s.operation_mode = Some(v));
        }
        if let Some(ctrls) = log_err("audio controls", self.audio_controls()) {
            let hp = ctrls
                .iter()
                .find(|c| c.kind() == 2 || c.kind() == 1)
                .map(|c| c.index);
            let mic = ctrls.iter().find(|c| c.is_recording()).map(|c| c.index);
            self.set_state(|s| (s.headphone_index, s.mic_index) = (hp, mic));
            let idx: Vec<u8> = ctrls.iter().map(|c| c.index).collect();
            if let Some(r) = log_err("level ranges", self.level_ranges(&idx)) {
                let conv = |x: &proto::LevelRange| {
                    (
                        proto::level_to_db(x.min),
                        proto::level_to_db(x.max),
                        x.step as f64 / 256.0,
                    )
                };
                self.set_state(|s| {
                    s.volume_range_db = r.iter().find(|x| Some(x.index) == hp).map(conv);
                    s.mic_range_db = r.iter().find(|x| Some(x.index) == mic).map(conv);
                });
            }
            for (i, is_hp) in [(hp, true), (mic, false)] {
                let Some(i) = i else { continue };
                if let Some(v) = log_err("level", self.level(i)) {
                    self.set_state(|s| {
                        if is_hp {
                            s.volume_db = Some(v)
                        } else {
                            s.mic_db = Some(v)
                        }
                    });
                }
                if let Some(v) = log_err("mute", self.mute(i)) {
                    self.set_state(|s| {
                        if is_hp {
                            s.muted = Some(v)
                        } else {
                            s.mic_muted = Some(v)
                        }
                    });
                }
            }
        }
        if let Some(v) = log_err("super x-fi", self.super_xfi()) {
            self.set_state(|s| s.super_xfi = Some(v));
        }
        if let Some(v) = log_err("eq profile", self.eq_profile()) {
            self.set_state(|s| s.eq_profile = Some(v));
        }
        if let Some((v, mcu, _)) = log_err("firmware version code", self.fw_version()) {
            self.set_state(|s| (s.fw_version_code, s.mcu_type) = (Some(v), Some(mcu)));
        }
        if let Some(v) = log_err("max payload", self.ping()) {
            self.set_state(|s| s.max_payload = Some(v));
        }
        let usb = serial::usb_info(&self.port_path());
        self.set_state(|s| s.usb = usb);
        if let Some(hp) = log_err("headphone profile", self.headphone_profile()) {
            self.set_state(|s| s.headphone_profile = Some(hp));
        }
        if let Some(c) = log_err("sxfi caps", self.sxfi_caps()) {
            self.set_state(|s| {
                s.sxfi_caps = Some(format!(
                    "room-response v{} channels 0x{:02X}, headphone-compensation v{} formats 0x{:02X}",
                    c.rr_metadata_version, c.rr_channel_support, c.hpc_metadata_version, c.hpc_format_support
                ))
            });
        }
        let _ = log_err("eq", self.read_eq());
    }

    // ---- setters ----------------------------------------------------------------------------

    pub fn set_super_xfi(&self, on: bool) -> Result<()> {
        let before = self.sh.state.lock().unwrap().dump_seq;
        self.request_ack(&proto::req_set_sxfi(on))?;
        self.set_state(|s| s.super_xfi = Some(on));
        // The X1 swaps EQ slots and dumps the new one; wait for it so callers see final EQ.
        let st = self.sh.state.lock().unwrap();
        let (st, to) = self
            .sh
            .cond
            .wait_timeout_while(st, Duration::from_millis(1500), |s| s.dump_seq == before)
            .unwrap();
        drop(st);
        if to.timed_out() {
            log::debug!("no EQ dump after Super X-Fi change; reading EQ explicitly");
            self.read_eq()?;
        }
        Ok(())
    }

    fn set_param(&self, id: u8, v: f32) -> Result<()> {
        if !v.is_finite() {
            return Err(Error::Refused("value is not a number".into()));
        }
        self.request_ack(&proto::req_set_param(id, v))?;
        self.update(Event::Params(proto::ParamBlock {
            more: false,
            params: vec![(id, v)],
        }));
        Ok(())
    }

    pub fn set_eq_enabled(&self, on: bool) -> Result<()> {
        self.set_param(proto::EQ_ENABLE, if on { 1.0 } else { 0.0 })
    }

    pub fn set_preamp(&self, db: f32) -> Result<()> {
        self.set_param(
            proto::EQ_PREAMP,
            db.clamp(-proto::PREAMP_LIMIT_DB, proto::PREAMP_LIMIT_DB),
        )
    }

    pub fn set_band(&self, band: usize, db: f32) -> Result<()> {
        if band >= proto::EQ_BANDS {
            return Err(Error::Refused(format!(
                "band index {band} out of range 0..=9"
            )));
        }
        self.set_param(
            proto::EQ_BAND0 + band as u8,
            db.clamp(-proto::BAND_LIMIT_DB, proto::BAND_LIMIT_DB),
        )
    }

    /// Same order Creative App uses for presets: bands low→high, then preamp.
    pub fn set_eq(&self, bands: &[f32; proto::EQ_BANDS], preamp: f32) -> Result<()> {
        for (i, &b) in bands.iter().enumerate() {
            self.set_band(i, b)?;
        }
        self.set_preamp(preamp)
    }

    fn hp_index(&self) -> u8 {
        self.sh.state.lock().unwrap().headphone_index.unwrap_or(0)
    }

    /// The X1 moves its level in 440-unit steps (1.71875 dB; e.g. −12.03, −10.31, −8.59),
    /// rounds other values up, and goes silent above +8.59 dB. Snap and clamp to its grid,
    /// then read back what the device actually applied.
    pub fn set_volume_db(&self, db: f64) -> Result<f64> {
        let raw = proto::snap_level(db);
        let idx = self.hp_index();
        self.request_ack(&proto::req_set_level(idx, raw))?;
        let actual = self.level(idx)?;
        self.update(Event::Level {
            index: idx,
            raw: proto::db_to_level(actual),
        });
        Ok(actual)
    }

    pub fn set_muted(&self, on: bool) -> Result<()> {
        let idx = self.hp_index();
        self.request_ack(&proto::req_set_mute(idx, on))?;
        self.update(Event::Mute {
            index: idx,
            muted: on,
        });
        Ok(())
    }

    /// `FeatureControl` EnableUAC2. The X1 re-enumerates on USB afterwards, which ends this
    /// session. Linux already uses the X1 as a UAC2 device, so this is rarely needed.
    pub fn set_uac2_forced(&self, on: bool) -> Result<()> {
        self.request_ack(&proto::req_feature_set(26, on))
    }
}

fn reader_loop(port: &mut Port, sh: &Arc<Shared>) {
    let mut parser = Parser::default();
    let mut buf = [0u8; 1024];
    while !sh.stop.load(Ordering::SeqCst) {
        let n = match port.read_some(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                log::warn!("reader: {e}");
                break;
            }
        };
        if n == 0 {
            if port.hung_up() {
                log::warn!("reader: port hung up (X1 unplugged or reset)");
                break;
            }
            continue;
        }
        parser.push(&buf[..n]);
        while let Some(f) = parser.next_frame() {
            log::trace!("RX {f}");
            // 1. hand to a pending request
            let mut claimed = false;
            if let Some(w) = sh.waiter.lock().unwrap().as_ref() {
                if (w.pred)(&f) {
                    claimed = true;
                    let _ = w.tx.try_send(f.clone());
                }
            }
            // 2. fold into state and notify
            let bit = *sh.sxfi_bit.lock().unwrap();
            let ev = proto::classify(&f, bit);
            if let (Event::Other(fr), false) = (&ev, claimed) {
                log::debug!("unhandled frame {fr}");
            }
            let snap = {
                let mut st = sh.state.lock().unwrap();
                st.apply(&ev);
                st.clone()
            };
            sh.cond.notify_all();
            if !matches!(ev, Event::Ack { .. } | Event::Other(_)) {
                if let Some(l) = sh.listener.lock().unwrap().as_ref() {
                    l(&Notice::Event(ev), &snap);
                }
            }
        }
    }
    sh.alive.store(false, Ordering::SeqCst);
    let snap = {
        let mut st = sh.state.lock().unwrap();
        st.connected = false;
        st.clone()
    };
    sh.cond.notify_all();
    if !sh.stop.load(Ordering::SeqCst) {
        if let Some(l) = sh.listener.lock().unwrap().as_ref() {
            l(&Notice::Disconnected, &snap);
        }
    }
}

/// Wait helper for callers that want to block until some condition on the state.
pub fn wait_until(dev: &Device, timeout: Duration, cond: impl Fn(&State) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    let mut st = dev.sh.state.lock().unwrap();
    while !cond(&st) {
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        st = dev.sh.cond.wait_timeout(st, deadline - now).unwrap().0;
    }
    true
}
