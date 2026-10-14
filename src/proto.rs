//! Creative CDC control protocol as spoken by the Sound Blaster X1.
//!
//! Frame:  `5A <opcode> <len:u8> <payload[len]>`  (little-endian, floats are f32)
//! Ack:    `5A 02 0A <opcode> <status> <8 bytes>`
//! `5B <sub> <len:u16> …` frames exist (mode announcement, firmware transfer);
//! we parse them but never send them.
//!
//! Payload layouts come from Creative App 1.24.05 (`Creative.Platform.Devices.dll`,
//! `CDCRawLibrary` structs) cross-checked against USB captures of the X1.

use crate::error::{Error, Result};

// ---- opcodes (Creative `CDCRawCommand`) ---------------------------------------------------
pub const OP_ACK: u8 = 0x02;
pub const OP_MAX_PAYLOAD: u8 = 0x03;
pub const OP_DEVICE_INFO: u8 = 0x09;
pub const OP_SUBFEATURE_SUPPORT: u8 = 0x10;
pub const OP_GET_PARAM: u8 = 0x11;
pub const OP_SET_PARAM: u8 = 0x12;
pub const OP_ACTIVE_PROFILE: u8 = 0x1A;
pub const OP_PROFILE_INFO: u8 = 0x1D;
pub const OP_AUDIO_CONTROL_INFO: u8 = 0x21;
pub const OP_AUDIO_LEVEL_RANGES: u8 = 0x22;
pub const OP_AUDIO_LEVEL: u8 = 0x23;
pub const OP_AUDIO_MUTE: u8 = 0x24;
pub const OP_HW_BUTTON: u8 = 0x26;
pub const OP_FEATURE_CONTROL: u8 = 0x39;
pub const OP_OPERATION_MODE: u8 = 0x6D;
pub const OP_SUPERXFI: u8 = 0x6F;
pub const OP_FACTORY_RESET: u8 = 0x9B;

/// EQ parameters live in Malcolm module 0x96 (PlaybackManager).
pub const MODULE_PLAYBACK: u8 = 0x96;

/// Parameter ids (Creative `CDCRawEventMsg`): 0x09 enable, 0x0A preamp, 0x0B..=0x14 bands.
pub const EQ_ENABLE: u8 = 0x09;
pub const EQ_PREAMP: u8 = 0x0A;
pub const EQ_BAND0: u8 = 0x0B;
pub const EQ_BANDS: usize = 10;
pub const BAND_HZ: [u32; EQ_BANDS] = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
pub const BAND_LIMIT_DB: f32 = 9.0;
pub const PREAMP_LIMIT_DB: f32 = 12.0;

/// `ButtonIDV2::SXFiOnOff`.
pub const BUTTON_SXFI: u16 = 0x001E;
/// `FeatureMask::EnableUAC2` (bit 26).
pub const FEATURE_ENABLE_UAC2: u32 = 0x0400_0000;
/// `MalcolmProfileType::Device`.
pub const PROFILE_TYPE_DEVICE: u8 = 2;

/// Headphone level on the X1, in raw 1/256 dB units: it moves in 440-unit steps
/// (1.71875 dB) from −12760 (−49.84 dB) to +2200 (+8.59 dB). Values above the maximum are
/// not rejected — the X1 goes silent — so they must never be sent.
pub const LEVEL_STEP_RAW: i16 = 440;
pub const LEVEL_MIN_RAW: i16 = -12760;
pub const LEVEL_MAX_RAW: i16 = 2200;
pub const LEVEL_MIN_DB: f64 = LEVEL_MIN_RAW as f64 / 256.0;
pub const LEVEL_MAX_DB: f64 = LEVEL_MAX_RAW as f64 / 256.0;
pub const LEVEL_STEP_DB: f64 = LEVEL_STEP_RAW as f64 / 256.0;

/// Nearest valid headphone level for a dB value (clamped, snapped to the device grid).
pub fn snap_level(db: f64) -> i16 {
    let steps = (db * 256.0 / LEVEL_STEP_RAW as f64).round();
    let raw = (steps * LEVEL_STEP_RAW as f64).clamp(LEVEL_MIN_RAW as f64, LEVEL_MAX_RAW as f64);
    raw as i16
}

// ---- frames -------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// `5A op len payload`
    Cmd { op: u8, payload: Vec<u8> },
    /// `5B sub len16 payload` (only ever received)
    Ext { sub: u8, payload: Vec<u8> },
}

impl Frame {
    pub fn op(&self) -> Option<u8> {
        match self {
            Frame::Cmd { op, .. } => Some(*op),
            Frame::Ext { .. } => None,
        }
    }
    pub fn payload(&self) -> &[u8] {
        match self {
            Frame::Cmd { payload, .. } | Frame::Ext { payload, .. } => payload,
        }
    }
    /// `(acked opcode, status)` if this is an `02` acknowledge frame.
    pub fn ack(&self) -> Option<(u8, u8)> {
        match self {
            Frame::Cmd {
                op: OP_ACK,
                payload,
            } if payload.len() >= 2 => Some((payload[0], payload[1])),
            _ => None,
        }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Frame::Cmd { op, payload } => {
                let mut v = vec![0x5A, *op, payload.len() as u8];
                v.extend_from_slice(payload);
                v
            }
            Frame::Ext { sub, payload } => {
                let mut v = vec![0x5B, *sub];
                v.extend_from_slice(&(payload.len() as u16).to_le_bytes());
                v.extend_from_slice(payload);
                v
            }
        }
    }
}

impl std::fmt::Display for Frame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", hex(&self.to_bytes()))
    }
}

pub fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Incremental, length-based frame parser. Skips garbage (e.g. CRLF from the mode probe,
/// stray text-mode bytes) until the next `5A`/`5B`.
#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
}

impl Parser {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            let skip = self
                .buf
                .iter()
                .position(|&b| b == 0x5A || b == 0x5B)
                .unwrap_or(self.buf.len());
            if skip > 0 {
                log::trace!(
                    "skipping {} stray byte(s): {}",
                    skip,
                    hex(&self.buf[..skip])
                );
                self.buf.drain(..skip);
            }
            if self.buf.is_empty() {
                return None;
            }
            match self.buf[0] {
                0x5A => {
                    if self.buf.len() < 3 {
                        return None;
                    }
                    let total = 3 + self.buf[2] as usize;
                    if self.buf.len() < total {
                        return None;
                    }
                    let f: Vec<u8> = self.buf.drain(..total).collect();
                    return Some(Frame::Cmd {
                        op: f[1],
                        payload: f[3..].to_vec(),
                    });
                }
                _ => {
                    if self.buf.len() < 4 {
                        return None;
                    }
                    let len = u16::from_le_bytes([self.buf[2], self.buf[3]]) as usize;
                    if len > 4096 {
                        // implausible: treat the 5B as garbage and resync
                        self.buf.remove(0);
                        continue;
                    }
                    let total = 4 + len;
                    if self.buf.len() < total {
                        return None;
                    }
                    let f: Vec<u8> = self.buf.drain(..total).collect();
                    return Some(Frame::Ext {
                        sub: f[1],
                        payload: f[4..].to_vec(),
                    });
                }
            }
        }
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

// ---- outgoing safety gate -----------------------------------------------------------------

/// Every frame we write goes through here. Only commands documented for the X1 with
/// known-safe payloads are allowed. Firmware/debug opcodes (0x53, 0x54, 0x83, 0x98, …)
/// and all `5B` frames are impossible to send through this crate.
pub fn check_outgoing(op: u8, p: &[u8]) -> Result<()> {
    let ok = match op {
        OP_MAX_PAYLOAD | OP_SUBFEATURE_SUPPORT => p.is_empty(),
        // DeviceInformationV2: General(00), FirmwareVersionString(02), Serial(03),
        // Firmware version/type query (01 <index>)
        OP_DEVICE_INFO => matches!(p, [0x00 | 0x02 | 0x03] | [0x01, 0x00]),
        OP_GET_PARAM => {
            p.len() == 3
                && p[0] == 1
                && p[1] == MODULE_PLAYBACK
                && (EQ_ENABLE..EQ_BAND0 + 10).contains(&p[2])
        }
        OP_SET_PARAM => {
            p.len() == 7
                && p[0] == 1
                && p[1] == MODULE_PLAYBACK
                && (EQ_ENABLE..EQ_BAND0 + 10).contains(&p[2])
        }
        // ActiveMalcolmProfile: Get only
        OP_ACTIVE_PROFILE => p.len() == 3 && p[0] == 0x01,
        OP_PROFILE_INFO => p.len() == 3 && p[0] == 0x01,
        OP_AUDIO_CONTROL_INFO => p.len() == 1,
        OP_AUDIO_LEVEL_RANGES => p.len() == 33,
        // AudioLevel / AudioMute: get (01 idx) or set (00 idx value)
        OP_AUDIO_LEVEL => {
            matches!(p.len(), 2 if p[0] == 1)
                || (p.len() == 4 && p[0] == 0 && {
                    let v = i16::from_le_bytes([p[2], p[3]]);
                    (LEVEL_MIN_RAW..=LEVEL_MAX_RAW).contains(&v)
                })
        }
        OP_AUDIO_MUTE => {
            matches!(p.len(), 2 if p[0] == 1) || (p.len() == 3 && p[0] == 0 && p[2] <= 1)
        }
        // HardwareButton: support(05), get(08 FFFF), set Super X-Fi only (07 1E00 vv00)
        OP_HW_BUTTON => matches!(
            p,
            [0x05] | [0x08, 0xFF, 0xFF] | [0x07, 0x1E, 0x00, 0x00 | 0x01, 0x00]
        ),
        // FeatureControl: support(02), get(01 mask), set(00 bit val) only for EnableUAC2 (bit 26)
        OP_FEATURE_CONTROL => {
            matches!(p, [0x02]) || (p.len() == 5 && p[0] == 0x01) || matches!(p, [0x00, 26, 0 | 1])
        }
        // DeviceOperationMode: Get only (Set could put the device in debug/production mode)
        OP_OPERATION_MODE => matches!(p, [0x01]),
        // SuperXFi: QueryCapabilities(00), GetActiveHeadphoneInformation(06)
        OP_SUPERXFI => matches!(p, [0x00] | [0x06]),
        // FactoryReset: support query (02) and the General/software reset (01) that Creative
        // App uses for the X1. The Full reset (03) is never sent.
        OP_FACTORY_RESET => matches!(p, [0x02] | [0x01]),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(Error::Refused(format!(
            "frame 5A {op:02X} {} is not on the X1 allowlist",
            hex(p)
        )))
    }
}

pub fn cmd(op: u8, payload: Vec<u8>) -> Result<Frame> {
    check_outgoing(op, &payload)?;
    Ok(Frame::Cmd { op, payload })
}

// ---- request builders -------------------------------------------------------------------------

pub fn req_ping() -> Frame {
    Frame::Cmd {
        op: OP_MAX_PAYLOAD,
        payload: vec![],
    }
}
pub fn req_info(sub: u8) -> Frame {
    Frame::Cmd {
        op: OP_DEVICE_INFO,
        payload: vec![sub],
    }
}
pub fn req_subfeatures() -> Frame {
    Frame::Cmd {
        op: OP_SUBFEATURE_SUPPORT,
        payload: vec![],
    }
}
pub fn req_get_param(id: u8) -> Frame {
    Frame::Cmd {
        op: OP_GET_PARAM,
        payload: vec![0x01, MODULE_PLAYBACK, id],
    }
}
pub fn req_set_param(id: u8, v: f32) -> Frame {
    let mut p = vec![0x01, MODULE_PLAYBACK, id];
    p.extend_from_slice(&v.to_le_bytes());
    Frame::Cmd {
        op: OP_SET_PARAM,
        payload: p,
    }
}
pub fn req_active_profile() -> Frame {
    Frame::Cmd {
        op: OP_ACTIVE_PROFILE,
        payload: vec![0x01, PROFILE_TYPE_DEVICE, 0x00],
    }
}
pub fn req_audio_controls() -> Frame {
    Frame::Cmd {
        op: OP_AUDIO_CONTROL_INFO,
        payload: vec![0x00],
    }
}
pub fn req_level_ranges(indices: &[u8]) -> Frame {
    let mut p = vec![0u8; 33];
    p[0] = indices.len().min(32) as u8;
    for (i, &x) in indices.iter().take(32).enumerate() {
        p[1 + i] = x;
    }
    Frame::Cmd {
        op: OP_AUDIO_LEVEL_RANGES,
        payload: p,
    }
}
pub fn req_get_level(idx: u8) -> Frame {
    Frame::Cmd {
        op: OP_AUDIO_LEVEL,
        payload: vec![0x01, idx],
    }
}
pub fn req_set_level(idx: u8, raw: i16) -> Frame {
    let b = raw.to_le_bytes();
    Frame::Cmd {
        op: OP_AUDIO_LEVEL,
        payload: vec![0x00, idx, b[0], b[1]],
    }
}
pub fn req_get_mute(idx: u8) -> Frame {
    Frame::Cmd {
        op: OP_AUDIO_MUTE,
        payload: vec![0x01, idx],
    }
}
pub fn req_set_mute(idx: u8, on: bool) -> Frame {
    Frame::Cmd {
        op: OP_AUDIO_MUTE,
        payload: vec![0x00, idx, on as u8],
    }
}
pub fn req_buttons() -> Frame {
    Frame::Cmd {
        op: OP_HW_BUTTON,
        payload: vec![0x05],
    }
}
pub fn req_button_state() -> Frame {
    Frame::Cmd {
        op: OP_HW_BUTTON,
        payload: vec![0x08, 0xFF, 0xFF],
    }
}
pub fn req_set_sxfi(on: bool) -> Frame {
    Frame::Cmd {
        op: OP_HW_BUTTON,
        payload: vec![0x07, 0x1E, 0x00, on as u8, 0x00],
    }
}
pub fn req_feature_support() -> Frame {
    Frame::Cmd {
        op: OP_FEATURE_CONTROL,
        payload: vec![0x02],
    }
}
pub fn req_feature_get(mask: u32) -> Frame {
    let mut p = vec![0x01];
    p.extend_from_slice(&mask.to_le_bytes());
    Frame::Cmd {
        op: OP_FEATURE_CONTROL,
        payload: p,
    }
}
pub fn req_feature_set(bit: u8, on: bool) -> Frame {
    Frame::Cmd {
        op: OP_FEATURE_CONTROL,
        payload: vec![0x00, bit, on as u8],
    }
}
pub fn req_operation_mode() -> Frame {
    Frame::Cmd {
        op: OP_OPERATION_MODE,
        payload: vec![0x01],
    }
}
pub fn req_sxfi_caps() -> Frame {
    Frame::Cmd {
        op: OP_SUPERXFI,
        payload: vec![0x00],
    }
}
pub fn req_fw_version() -> Frame {
    Frame::Cmd {
        op: OP_DEVICE_INFO,
        payload: vec![0x01, 0x00],
    }
}
pub fn req_factory_reset_support() -> Frame {
    Frame::Cmd {
        op: OP_FACTORY_RESET,
        payload: vec![0x02],
    }
}
pub fn req_factory_reset() -> Frame {
    Frame::Cmd {
        op: OP_FACTORY_RESET,
        payload: vec![0x01],
    }
}
pub fn req_sxfi_hp_info() -> Frame {
    Frame::Cmd {
        op: OP_SUPERXFI,
        payload: vec![0x06],
    }
}

// ---- reply / event decoding -------------------------------------------------------------------

fn need(p: &[u8], n: usize, what: &str) -> Result<()> {
    if p.len() < n {
        Err(Error::Protocol(format!(
            "{what}: short payload ({} < {n}): {}",
            p.len(),
            hex(p)
        )))
    } else {
        Ok(())
    }
}
fn u16le(p: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([p[i], p[i + 1]])
}
fn u32le(p: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([p[i], p[i + 1], p[i + 2], p[i + 3]])
}

pub fn parse_max_payload(p: &[u8]) -> Result<u16> {
    need(p, 2, "MaxPayloadSize")?;
    Ok(u16le(p, 0))
}

/// `09 / 00`: `00 <pid:u16> <ver:u16> <vid:u16> …`
pub fn parse_ids(p: &[u8]) -> Result<(u16, u16, u16)> {
    need(p, 7, "DeviceInformation/General")?;
    Ok((u16le(p, 1), u16le(p, 3), u16le(p, 5)))
}

/// `09 / 02` (`02 <len> chars`) and `09 / 03` (`03 [len] chars 00`).
pub fn parse_info_string(p: &[u8]) -> Result<String> {
    need(p, 2, "DeviceInformation string")?;
    let mut rest = &p[1..];
    if rest[0] < 0x20 && (rest[0] as usize) < rest.len() {
        let n = rest[0] as usize;
        rest = &rest[1..=n.min(rest.len() - 1)];
    }
    let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    Ok(String::from_utf8_lossy(&rest[..end]).trim().to_string())
}

/// `10`: u32 Malcolm sub-feature mask.
pub fn parse_subfeatures(p: &[u8]) -> Result<u32> {
    need(p, 4, "SubFeatureSupport")?;
    Ok(u32le(p, 0))
}

pub fn subfeature_names(mask: u32) -> Vec<&'static str> {
    const N: [(u32, &str); 24] = [
        (0x1, "Surround"),
        (0x2, "Crystalizer"),
        (0x4, "SVM"),
        (0x8, "XBass"),
        (0x10, "BassManagement"),
        (0x20, "DialogPlus"),
        (0x40, "GraphicEQ"),
        (0x80, "VoiceFx"),
        (0x100, "MicSVM"),
        (0x200, "NoiseCancellation"),
        (0x400, "AEC"),
        (0x800, "VoiceFocus"),
        (0x1000, "Profiles"),
        (0x2000, "DolbyDigitalDecoding"),
        (0x4000, "DynamicRangeCompression"),
        (0x8000, "LineNoiseReduction"),
        (0x10000, "DualMicEndFiring"),
        (0x20000, "SpeakerCalibration"),
        (0x40000, "MicEQ"),
        (0x80000, "OptionalSpeaker"),
        (0x100000, "SVMPlus"),
        (0x200000, "SurroundV2"),
        (0x400000, "DialogPlusV2"),
        (0x800000, "SubwooferBoost"),
    ];
    N.iter()
        .filter(|(b, _)| mask & b != 0)
        .map(|(_, n)| *n)
        .collect()
}

/// `11` replies and dumps: `<count> <more> (96 <id> <f32>)×count`
#[derive(Debug, Clone, PartialEq)]
pub struct ParamBlock {
    pub more: bool,
    pub params: Vec<(u8, f32)>,
}

pub fn parse_params(p: &[u8]) -> Result<ParamBlock> {
    need(p, 2, "MalcolmParameter")?;
    let count = p[0] as usize;
    need(p, 2 + count * 6, "MalcolmParameter")?;
    let mut params = Vec::with_capacity(count);
    for i in 0..count {
        let o = 2 + i * 6;
        let v = f32::from_le_bytes([p[o + 2], p[o + 3], p[o + 4], p[o + 5]]);
        params.push((p[o + 1], v));
    }
    Ok(ParamBlock {
        more: p[1] != 0,
        params,
    })
}

/// `21`: Total, Count, AdditionalPacket, StartIndex, u16 info[Count]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioControl {
    pub index: u8,
    pub raw: u16,
}

impl AudioControl {
    /// Creative `AudioControlType` (low 6 bits).
    pub fn kind(&self) -> u8 {
        (self.raw & 0x3F) as u8
    }
    pub fn kind_name(&self) -> &'static str {
        match self.kind() {
            1 => "speaker",
            2 => "headphone",
            3 => "mic",
            4 => "line-in",
            6 => "usb-in",
            7 => "bluetooth-in",
            9 => "spdif-in",
            10 => "aux-in",
            12 => "external-mic",
            13 => "subwoofer",
            14 => "spdif-out",
            15 => "mic-monitoring",
            18 => "chat",
            19 => "game",
            20 => "headset",
            _ => "other",
        }
    }
    pub fn has_volume(&self) -> bool {
        self.raw & 0x40 != 0
    }
    pub fn has_mute(&self) -> bool {
        self.raw & 0x80 != 0
    }
    pub fn is_source(&self) -> bool {
        self.raw & 0x100 != 0
    }
    pub fn is_recording(&self) -> bool {
        self.raw & 0x200 != 0
    }
    pub fn is_db(&self) -> bool {
        self.raw & 0x800 != 0
    }
}

pub fn parse_audio_controls(p: &[u8]) -> Result<Vec<AudioControl>> {
    need(p, 4, "AudioControlInformation")?;
    let count = p[1] as usize;
    let start = p[3];
    need(p, 4 + count * 2, "AudioControlInformation")?;
    Ok((0..count)
        .map(|i| AudioControl {
            index: start + i as u8,
            raw: u16le(p, 4 + i * 2),
        })
        .collect())
}

/// `22`: Count, AdditionalPacket, {idx u8, max i16, min i16, step u16}×Count
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelRange {
    pub index: u8,
    pub max: i16,
    pub min: i16,
    pub step: u16,
}

pub fn parse_level_ranges(p: &[u8]) -> Result<Vec<LevelRange>> {
    need(p, 2, "AudioLevelRanges")?;
    let count = p[0] as usize;
    need(p, 2 + count * 7, "AudioLevelRanges")?;
    Ok((0..count)
        .map(|i| {
            let o = 2 + i * 7;
            LevelRange {
                index: p[o],
                max: u16le(p, o + 1) as i16,
                min: u16le(p, o + 3) as i16,
                step: u16le(p, o + 5),
            }
        })
        .collect())
}

/// 1/256 dB fixed point (same unit as UAC2 volume).
pub fn level_to_db(raw: i16) -> f64 {
    raw as f64 / 256.0
}
pub fn db_to_level(db: f64) -> i16 {
    (db * 256.0).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

/// `26 / 05`: Operation(05) Flag NumButtons StartIndex u16[NumButtons]
pub fn parse_buttons(p: &[u8]) -> Result<Vec<u16>> {
    need(p, 4, "HardwareButton support")?;
    let n = p[2] as usize;
    // Creative's struct has a StartIndex byte; tolerate captures where it is absent.
    let off = if p.len() >= 4 + n * 2 { 4 } else { 3 };
    need(p, off + n * 2, "HardwareButton support")?;
    Ok((0..n).map(|i| u16le(p, off + i * 2)).collect())
}

/// `26 / 08`: Operation(08) ButtonId(u16, FFFF = all) ButtonMask(u64)
pub fn parse_button_state(p: &[u8]) -> Result<(u16, u64)> {
    need(p, 4, "HardwareButton state")?;
    let mut m = [0u8; 8];
    let avail = (p.len() - 3).min(8);
    m[..avail].copy_from_slice(&p[3..3 + avail]);
    Ok((u16le(p, 1), u64::from_le_bytes(m)))
}

/// `39 / 02`: Operation, FeatureMask u32, ResetNeededMask u32
pub fn parse_feature_support(p: &[u8]) -> Result<(u32, u32)> {
    need(p, 5, "FeatureControl support")?;
    let reset = if p.len() >= 9 { u32le(p, 5) } else { 0 };
    Ok((u32le(p, 1), reset))
}

/// `39 / 01`: Operation, enabled-features mask u32
pub fn parse_feature_get(p: &[u8]) -> Result<u32> {
    need(p, 5, "FeatureControl get")?;
    Ok(u32le(p, 1))
}

pub fn feature_names(mask: u32) -> Vec<&'static str> {
    const N: [(u32, &str); 16] = [
        (0x20, "DirectMode"),
        (0x40, "HPHighGain"),
        (0x400, "HRTFOverSpeaker"),
        (0x2000, "SPDIFDirectMode"),
        (0x4000, "DisableIndicatorLEDs"),
        (0x8000, "SPDIFPassthru"),
        (0x20000, "AutoStandby"),
        (0x40000, "LEDBrightness"),
        (0x100000, "SaveHPHighGain"),
        (0x200000, "HPVirtualization"),
        (0x400000, "AudioPrompt"),
        (0x800000, "StereoMic"),
        (0x1000000, "HDMICECStandbyLink"),
        (0x4000000, "EnableUAC2"),
        (0x8000000, "SwitchUSBMux"),
        (0x10000000, "SyncMasterVRToHost"),
    ];
    N.iter()
        .filter(|(b, _)| mask & b != 0)
        .map(|(_, n)| *n)
        .collect()
}

/// `6F / 00`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SxfiCaps {
    pub rr_metadata_version: u8,
    pub rr_channel_support: u8,
    pub hpc_metadata_version: u8,
    pub hpc_format_support: u8,
}

pub fn parse_sxfi_caps(p: &[u8]) -> Result<SxfiCaps> {
    need(p, 7, "SuperXFi capabilities")?;
    Ok(SxfiCaps {
        rr_metadata_version: p[1],
        rr_channel_support: p[3],
        hpc_metadata_version: p[4],
        hpc_format_support: p[6],
    })
}

/// `6F / 06`: Operation, HPID chars, FileID chars (NUL separated)
/// `09 / 01`: Operation(01) Index FirmwareVersion:u32 MCUType Format
/// X1: `01 00 60 E5 32 20 2D 04` → 0x2032E560, MCU 0x2D (= chip 45 in Creative's metadata).
pub fn parse_fw_version(p: &[u8]) -> Result<(u32, u8, u8)> {
    need(p, 8, "Firmware version/type")?;
    Ok((u32le(p, 2), p[6], p[7]))
}

pub fn parse_hp_info(p: &[u8]) -> Result<(String, String)> {
    need(p, 1, "SuperXFi headphone info")?;
    let mut parts = p[1..]
        .split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned());
    Ok((
        parts.next().unwrap_or_default(),
        parts.next().unwrap_or_default(),
    ))
}

// ---- event classification -----------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    Ack {
        op: u8,
        status: u8,
    },
    /// Super X-Fi state from `26 / 08` (reply or unsolicited).
    SuperXFi(bool),
    /// EQ values from `11` (reply, echo or slot dump).
    Params(ParamBlock),
    /// `1A`: active device EQ profile (changes with Super X-Fi).
    Profile {
        kind: u8,
        index: u8,
    },
    /// `23`: level of an audio control (1/256 dB).
    Level {
        index: u8,
        raw: i16,
    },
    /// `24`: mute of an audio control.
    Mute {
        index: u8,
        muted: bool,
    },
    /// `6F 05 xx` — unsolicited; Creative App ignores it. Meaning unknown.
    SxfiNotify(Vec<u8>),
    Other(Frame),
}

/// `sxfi_bit`: index of the Super X-Fi button inside the button-state mask (from `26/05`).
pub fn classify(f: &Frame, sxfi_bit: u8) -> Event {
    let p = f.payload();
    match f.op() {
        Some(OP_ACK) if p.len() >= 2 => Event::Ack {
            op: p[0],
            status: p[1],
        },
        Some(OP_HW_BUTTON) if p.first() == Some(&0x08) && p.len() >= 4 => {
            match parse_button_state(p) {
                Ok((_, mask)) => Event::SuperXFi(mask & (1u64 << sxfi_bit) != 0),
                Err(_) => Event::Other(f.clone()),
            }
        }
        Some(OP_GET_PARAM) => match parse_params(p) {
            Ok(b) => Event::Params(b),
            Err(_) => Event::Other(f.clone()),
        },
        Some(OP_ACTIVE_PROFILE) if p.len() == 2 => Event::Profile {
            kind: p[0],
            index: p[1],
        },
        Some(OP_AUDIO_LEVEL) if p.len() == 3 => Event::Level {
            index: p[0],
            raw: u16le(p, 1) as i16,
        },
        Some(OP_AUDIO_MUTE) if p.len() == 2 => Event::Mute {
            index: p[0],
            muted: p[1] != 0,
        },
        Some(OP_SUPERXFI) if p.first() == Some(&0x05) => Event::SxfiNotify(p[1..].to_vec()),
        _ => Event::Other(f.clone()),
    }
}

/// Byte sequences in this module's tests are copied from USB captures of a real X1
/// (firmware 1.0.220525.1100).
#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Vec<u8> {
        s.split_whitespace()
            .map(|x| u8::from_str_radix(x, 16).unwrap())
            .collect()
    }
    fn parse_all(bytes: &[u8]) -> Vec<Frame> {
        let mut p = Parser::default();
        p.push(bytes);
        std::iter::from_fn(|| p.next_frame()).collect()
    }

    #[test]
    fn parser_handles_split_interleaved_and_garbage() {
        let mut bytes = b"\r\nUnknown command\r\n".to_vec();
        bytes.extend(h("5B 02 0A 00 6D 00 00 00 00 00 00 00 00 00"));
        bytes.extend(h("5A 03 02 00 02"));
        bytes.extend(h("5A 23 03 00 04 06"));
        let mut p = Parser::default();
        // byte-by-byte delivery
        let mut out = vec![];
        for b in &bytes {
            p.push(&[*b]);
            while let Some(f) = p.next_frame() {
                out.push(f);
            }
        }
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0],
            Frame::Ext {
                sub: 2,
                payload: h("6D 00 00 00 00 00 00 00 00 00")
            }
        );
        assert_eq!(parse_max_payload(out[1].payload()).unwrap(), 512);
        assert_eq!(
            classify(&out[2], 0),
            Event::Level {
                index: 0,
                raw: 0x0604
            }
        );
    }

    #[test]
    fn encode_matches_captures() {
        assert_eq!(req_ping().to_bytes(), h("5A 03 00"));
        assert_eq!(req_info(2).to_bytes(), h("5A 09 01 02"));
        assert_eq!(req_get_param(0x0B).to_bytes(), h("5A 11 03 01 96 0B"));
        assert_eq!(req_set_sxfi(true).to_bytes(), h("5A 26 05 07 1E 00 01 00"));
        assert_eq!(req_button_state().to_bytes(), h("5A 26 03 08 FF FF"));
        assert_eq!(req_buttons().to_bytes(), h("5A 26 01 05"));
        assert_eq!(req_feature_support().to_bytes(), h("5A 39 01 02"));
        assert_eq!(
            req_feature_get(FEATURE_ENABLE_UAC2).to_bytes(),
            h("5A 39 05 01 00 00 00 04")
        );
        assert_eq!(req_operation_mode().to_bytes(), h("5A 6D 01 01"));
        assert_eq!(req_sxfi_caps().to_bytes(), h("5A 6F 01 00"));
        assert_eq!(req_audio_controls().to_bytes(), h("5A 21 01 00"));
        assert_eq!(req_active_profile().to_bytes(), h("5A 1A 03 01 02 00"));
        let r = req_level_ranges(&[0, 1]).to_bytes();
        assert_eq!(&r[..6], &h("5A 22 21 02 00 01")[..]);
        assert_eq!(r.len(), 36);
        let mut set = h("5A 12 07 01 96 0C");
        set.extend_from_slice(&(-4.5f32).to_le_bytes());
        assert_eq!(req_set_param(0x0C, -4.5).to_bytes(), set);
    }

    #[test]
    fn builders_pass_the_allowlist() {
        for f in [
            req_ping(),
            req_info(0),
            req_info(2),
            req_info(3),
            req_subfeatures(),
            req_get_param(0x14),
            req_set_param(0x09, 1.0),
            req_active_profile(),
            req_audio_controls(),
            req_level_ranges(&[0, 1]),
            req_get_level(0),
            req_set_level(0, -1000),
            req_get_mute(1),
            req_set_mute(0, true),
            req_buttons(),
            req_button_state(),
            req_set_sxfi(false),
            req_feature_support(),
            req_feature_get(FEATURE_ENABLE_UAC2),
            req_feature_set(26, true),
            req_operation_mode(),
            req_sxfi_caps(),
            req_sxfi_hp_info(),
            req_fw_version(),
            req_factory_reset_support(),
            req_factory_reset(),
        ] {
            let Frame::Cmd { op, payload } = &f else {
                unreachable!()
            };
            check_outgoing(*op, payload).unwrap_or_else(|e| panic!("{f}: {e}"));
        }
    }

    #[test]
    fn allowlist_blocks_dangerous_frames() {
        assert!(check_outgoing(0x53, &[]).is_err()); // firmware upgrade
        assert!(check_outgoing(0x54, &[0, 0]).is_err()); // debug memory
        assert!(check_outgoing(0x9B, &[3]).is_err()); // full factory reset
        assert!(check_outgoing(OP_OPERATION_MODE, &[0x00, 0x01]).is_err()); // set debug mode
        assert!(check_outgoing(OP_FEATURE_CONTROL, &[0x00, 5, 1]).is_err()); // other feature bits
        assert!(check_outgoing(OP_HW_BUTTON, &[0x07, 0x02, 0x00, 1, 0]).is_err()); // other buttons
        assert!(check_outgoing(OP_SET_PARAM, &[1, 0x96, 0x00, 0, 0, 0, 0]).is_err()); // surround id
        assert!(check_outgoing(OP_ACTIVE_PROFILE, &[0x00, 2, 1]).is_err()); // profile set
    }

    #[test]
    fn factory_reset_allowlist() {
        assert_eq!(req_factory_reset().to_bytes(), h("5A 9B 01 01"));
        assert_eq!(req_factory_reset_support().to_bytes(), h("5A 9B 01 02"));
        assert!(check_outgoing(OP_FACTORY_RESET, &[0x03]).is_err()); // full reset never
        let f = &parse_all(&h("5A 09 08 01 00 60 E5 32 20 2D 04"))[0];
        assert_eq!(
            parse_fw_version(f.payload()).unwrap(),
            (0x2032E560, 0x2D, 4)
        );
    }

    #[test]
    fn level_snapping_and_limits() {
        assert_eq!(snap_level(-12.0), -3080); // -12.03 dB
        assert_eq!(snap_level(8.59), 2200);
        assert_eq!(snap_level(10.0), 2200); // never above the max: the X1 goes silent
        assert_eq!(snap_level(-80.0), -12760);
        assert!(check_outgoing(OP_AUDIO_LEVEL, &[0, 0, 0x48, 0x0A]).is_err()); // 2632 = +10.3 dB
        assert!(check_outgoing(OP_AUDIO_LEVEL, &[0, 0, 0x98, 0x08]).is_ok()); // 2200
    }

    #[test]
    fn decodes_device_info() {
        let f = &parse_all(&h(
            "5A 09 11 00 86 32 00 01 1E 04 00 00 00 00 00 00 00 00 00 00 00",
        ))[0];
        assert_eq!(parse_ids(f.payload()).unwrap(), (0x3286, 0x0100, 0x041E));
        let mut fw = h("5A 09 12 02 10");
        fw.extend_from_slice(b"1.0.220525.1100");
        fw.push(0);
        let f = &parse_all(&fw)[0];
        assert_eq!(parse_info_string(f.payload()).unwrap(), "1.0.220525.1100");
        let mut sn = h("5A 09 14 03");
        sn.extend_from_slice(b"W6ZZ0090ABCDEFGHIX");
        sn.push(0);
        let f = &parse_all(&sn)[0];
        assert_eq!(
            parse_info_string(f.payload()).unwrap(),
            "W6ZZ0090ABCDEFGHIX"
        );
    }

    #[test]
    fn decodes_support_masks() {
        let f = &parse_all(&h("5A 10 08 40 00 00 00 00 00 00 00"))[0];
        assert_eq!(
            subfeature_names(parse_subfeatures(f.payload()).unwrap()),
            vec!["GraphicEQ"]
        );
        let f = &parse_all(&h("5A 39 09 02 00 00 00 04 00 00 00 00"))[0];
        let (sup, reset) = parse_feature_support(f.payload()).unwrap();
        assert_eq!(feature_names(sup), vec!["EnableUAC2"]);
        assert_eq!(reset, 0);
        let f = &parse_all(&h("5A 39 05 01 00 00 00 00"))[0];
        assert_eq!(
            parse_feature_get(f.payload()).unwrap() & FEATURE_ENABLE_UAC2,
            0
        );
    }

    #[test]
    fn decodes_eq_dump_pair() {
        let mut a = h("5A 11 38 09 01");
        for id in 0x09..=0x11u8 {
            a.extend([0x96, id]);
            a.extend_from_slice(&(id as f32 - 12.0).to_le_bytes());
        }
        let mut b = h("5A 11 14 03 00");
        for id in 0x12..=0x14u8 {
            b.extend([0x96, id]);
            b.extend_from_slice(&1.5f32.to_le_bytes());
        }
        a.extend(b);
        let frames = parse_all(&a);
        let Event::Params(p0) = classify(&frames[0], 0) else {
            panic!()
        };
        let Event::Params(p1) = classify(&frames[1], 0) else {
            panic!()
        };
        assert!(p0.more && !p1.more);
        assert_eq!(p0.params.len(), 9);
        assert_eq!(p0.params[0], (0x09, -3.0));
        assert_eq!(p1.params[2], (0x14, 1.5));
    }

    #[test]
    fn decodes_super_xfi_sequence() {
        let seq = h("5A 26 0B 08 FF FF 01 00 00 00 00 00 00 00 \
                     5A 1A 02 02 00 \
                     5A 02 0A 26 00 07 1E 00 01 00 00 00 00");
        let frames = parse_all(&seq);
        assert_eq!(classify(&frames[0], 0), Event::SuperXFi(true));
        assert_eq!(
            classify(&frames[1], 0),
            Event::Profile { kind: 2, index: 0 }
        );
        assert_eq!(frames[2].ack(), Some((OP_HW_BUTTON, 0)));
        let off = parse_all(&h("5A 26 0B 08 FF FF 00 00 00 00 00 00 00 00"));
        assert_eq!(classify(&off[0], 0), Event::SuperXFi(false));
    }

    #[test]
    fn decodes_button_list_with_and_without_start_index() {
        // Creative layout (Operation, Flag, NumButtons, StartIndex, u16[n]) = 12 bytes
        let f = &parse_all(&h("5A 26 0C 05 00 04 00 1E 00 0A 40 0B 40 0C 40"))[0];
        assert_eq!(
            parse_buttons(f.payload()).unwrap(),
            vec![0x001E, 0x400A, 0x400B, 0x400C]
        );
        // the 11-byte transcription from the earlier notes
        assert_eq!(
            parse_buttons(&h("05 00 04 1E 00 0A 40 0B 40 0C 40")).unwrap()[0],
            0x001E
        );
    }

    #[test]
    fn decodes_audio_controls_and_ranges() {
        let f = &parse_all(&h("5A 21 0C 02 02 00 00 C2 09 C3 0A 01 00 00 00"))[0];
        let c = parse_audio_controls(f.payload()).unwrap();
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].kind_name(), "headphone");
        assert!(c[0].has_volume() && c[0].has_mute() && c[0].is_db() && !c[0].is_recording());
        assert_eq!(c[1].kind_name(), "mic");
        assert!(c[1].is_recording());
        let f = &parse_all(&h(
            "5A 22 16 02 00 00 00 00 28 CE C5 01 01 00 00 00 D0 00 01 00 00 00 D0 00 01",
        ))[0];
        let r = parse_level_ranges(f.payload()).unwrap();
        assert_eq!(r[0].index, 0);
        assert_eq!(r[1].index, 1);
        assert!((level_to_db(r[1].min) + 48.0).abs() < 1e-9);
    }

    #[test]
    fn decodes_sxfi_caps_and_volume_events() {
        let f = &parse_all(&h("5A 6F 08 00 03 01 07 02 01 1C 01"))[0];
        let c = parse_sxfi_caps(f.payload()).unwrap();
        assert_eq!(
            (
                c.rr_metadata_version,
                c.rr_channel_support,
                c.hpc_format_support
            ),
            (3, 7, 0x1C)
        );
        let ev = classify(&parse_all(&h("5A 23 03 00 68 F7"))[0], 0);
        let Event::Level { raw, .. } = ev else {
            panic!()
        };
        assert!((level_to_db(raw) + 8.59).abs() < 0.01);
        assert_eq!(
            classify(&parse_all(&h("5A 6F 02 05 03"))[0], 0),
            Event::SxfiNotify(vec![3])
        );
    }
}
