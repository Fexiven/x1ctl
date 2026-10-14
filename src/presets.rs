//! EQ presets.
//!
//! * built-in: only "Flat"
//! * Creative's official X1 presets and sound modes: imported by the user from Creative's
//!   own product package (`x1ctl eq import`), stored in `$XDG_CONFIG_HOME/x1ctl/creative-presets.json`
//! * user presets: `$XDG_CONFIG_HOME/x1ctl/presets.json`
//!
//! On the X1 a Creative "sound mode" only changes the graphic EQ — its Surround/Crystalizer/
//! Bass/SVM/Dialog+ parts are Acoustic Engine effects that run on the Windows host.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::proto::EQ_BANDS;

/// Creative's product package for the X1, as fetched by Creative App.
pub const PACKAGE_URL: &str = "http://files.creative.com/creative/bin/apps/swureleases/win/creativeapp/release/product/ZZ0090/1.0.05.00/ZZ0090.zip";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Preset {
    pub bands: [f32; EQ_BANDS],
    #[serde(default)]
    pub preamp: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NamedPreset {
    pub name: String,
    pub preamp: f32,
    pub bands: [f32; EQ_BANDS],
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq)]
pub struct SoundMode {
    pub name: String,
    pub eq_enabled: bool,
    pub eq_preset: String,
    /// Host-side Acoustic Engine settings Creative App applies on Windows (not on the X1).
    #[serde(default)]
    pub host_effects: serde_json::Value,
}

/// What `x1ctl eq import` stores.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq)]
pub struct Imported {
    pub source: String,
    pub eq_presets: Vec<NamedPreset>,
    pub sound_modes: Vec<SoundMode>,
}

fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("x1ctl")
}

pub fn user_path() -> PathBuf {
    config_dir().join("presets.json")
}

pub fn imported_path() -> PathBuf {
    config_dir().join("creative-presets.json")
}

pub fn load_imported() -> Option<Imported> {
    let s = std::fs::read_to_string(imported_path()).ok()?;
    serde_json::from_str(&s)
        .map_err(|e| log::warn!("{}: {e}", imported_path().display()))
        .ok()
}

/// Flat, then Creative's presets in their display order (if imported).
pub fn builtin_ordered() -> Vec<(String, Preset)> {
    let mut v = vec![(
        "Flat".to_string(),
        Preset {
            bands: [0.0; EQ_BANDS],
            preamp: 0.0,
        },
    )];
    if let Some(imp) = load_imported() {
        v.extend(
            imp.eq_presets
                .into_iter()
                .filter(|p| p.name != "Flat")
                .map(|p| {
                    (
                        p.name,
                        Preset {
                            bands: p.bands,
                            preamp: p.preamp,
                        },
                    )
                }),
        );
    }
    v
}

pub fn builtin() -> BTreeMap<String, Preset> {
    builtin_ordered().into_iter().collect()
}

pub fn sound_modes() -> Vec<SoundMode> {
    load_imported().map(|i| i.sound_modes).unwrap_or_default()
}

/// Lowercase alphanumerics only: "Counter-Strike 2" → "counterstrike2", "R & B" → "rb".
pub fn slug(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn load_user() -> BTreeMap<String, Preset> {
    match std::fs::read_to_string(user_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            log::warn!("{}: {e}", user_path().display());
            BTreeMap::new()
        }),
        Err(_) => BTreeMap::new(),
    }
}

/// User presets override built-ins with the same name.
pub fn all() -> BTreeMap<String, Preset> {
    let mut m = builtin();
    m.extend(load_user());
    m
}

/// Find a preset or sound mode by name, ignoring case, spaces and punctuation.
pub fn find(name: &str) -> Option<(String, Preset)> {
    let all = all();
    if let Some(p) = all.get(name) {
        return Some((name.to_string(), p.clone()));
    }
    let want = slug(name);
    if let Some((n, p)) = all.iter().find(|(n, _)| slug(n) == want) {
        return Some((n.clone(), p.clone()));
    }
    let sm = sound_modes().into_iter().find(|m| slug(&m.name) == want)?;
    all.get(&sm.eq_preset)
        .map(|p| (sm.eq_preset.clone(), p.clone()))
}

pub fn save_user(name: &str, p: Preset) -> std::io::Result<PathBuf> {
    let mut m = load_user();
    m.insert(name.to_string(), p);
    let path = user_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&m).unwrap())?;
    Ok(path)
}

// ---- importing Creative's package -------------------------------------------------------

#[derive(Deserialize)]
struct CBand {
    #[serde(rename = "Frequency")]
    frequency: f32,
    #[serde(rename = "Value")]
    value: f32,
}
#[derive(Deserialize)]
struct CEqSettings {
    #[serde(rename = "PreAmp", default)]
    preamp: f32,
    #[serde(rename = "Bands")]
    bands: Vec<CBand>,
}
#[derive(Deserialize)]
struct CEq {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name", default)]
    raw_name: String,
    #[serde(rename = "DisplayName", default)]
    display_name: String,
    #[serde(rename = "Order", default)]
    order: i64,
    #[serde(rename = "Settings")]
    settings: Vec<CEqSettings>,
}
impl CEq {
    fn name(&self) -> String {
        if self.display_name.is_empty() {
            self.raw_name.clone()
        } else {
            self.display_name.clone()
        }
    }
}
#[derive(Deserialize)]
struct CGeq {
    #[serde(rename = "Enable", default)]
    enable: bool,
    #[serde(rename = "PresetId")]
    preset_id: String,
}
#[derive(Deserialize)]
struct CModeSettings {
    #[serde(rename = "GraphicEQ")]
    geq: CGeq,
    #[serde(flatten)]
    rest: serde_json::Map<String, serde_json::Value>,
}
#[derive(Deserialize)]
struct CMode {
    #[serde(rename = "Name", default)]
    raw_name: String,
    #[serde(rename = "DisplayName", default)]
    display_name: String,
    #[serde(rename = "Order", default)]
    order: i64,
    #[serde(rename = "Settings")]
    settings: Vec<CModeSettings>,
}

fn parse_json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    let b = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    serde_json::from_slice(b).ok()
}

/// Build the preset bundle from `(path, contents)` pairs of the package.
fn build(files: Vec<(String, Vec<u8>)>, source: String) -> Result<Imported, String> {
    let mut eqs: Vec<(i64, CEq)> = vec![];
    let mut modes: Vec<(i64, CMode)> = vec![];
    for (path, data) in &files {
        let p = path.replace('\\', "/");
        if p.contains("/Presets/EQ/") && p.ends_with(".json") {
            if let Some(e) = parse_json::<CEq>(data) {
                eqs.push((e.order, e));
            }
        } else if p.contains("/SoundMode/") && p.ends_with(".json") {
            if let Some(m) = parse_json::<CMode>(data) {
                modes.push((m.order, m));
            }
        }
    }
    if eqs.is_empty() {
        return Err("no EQ presets found — is this Creative's ZZ0090 package?".into());
    }
    eqs.sort_by_key(|a| (a.0, a.1.name()));
    let mname = |m: &CMode| {
        if m.display_name.is_empty() {
            m.raw_name.clone()
        } else {
            m.display_name.clone()
        }
    };
    modes.sort_by_key(|a| (a.0, mname(&a.1)));
    let mut by_id = BTreeMap::new();
    let mut eq_presets = vec![];
    for (_, e) in eqs {
        let Some(s) = e.settings.first() else {
            continue;
        };
        let mut bands = s.bands.iter().collect::<Vec<_>>();
        bands.sort_by(|a, b| a.frequency.total_cmp(&b.frequency));
        if bands.len() != EQ_BANDS {
            continue;
        }
        let arr: [f32; EQ_BANDS] = std::array::from_fn(|i| bands[i].value.clamp(-9.0, 9.0));
        by_id.insert(e.id.clone(), e.name());
        eq_presets.push(NamedPreset {
            name: e.name(),
            preamp: s.preamp.clamp(-12.0, 12.0),
            bands: arr,
        });
    }
    let sound_modes = modes
        .into_iter()
        .filter_map(|(_, m)| {
            let name = mname(&m);
            let s = m.settings.into_iter().next()?;
            let eq_preset = by_id.get(&s.geq.preset_id)?.clone();
            let mut host = s.rest;
            host.remove("Type");
            host.remove("SuperXFi");
            Some(SoundMode {
                name,
                eq_enabled: s.geq.enable,
                eq_preset,
                host_effects: serde_json::Value::Object(host),
            })
        })
        .collect();
    Ok(Imported {
        source,
        eq_presets,
        sound_modes,
    })
}

/// Read the package from a `.zip` file or an extracted directory.
pub fn read_package(path: &Path) -> Result<Imported, String> {
    let mut files = vec![];
    if path.is_dir() {
        fn walk(dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, out);
                } else if let Ok(d) = std::fs::read(&p) {
                    out.push((format!("/{}", p.display()), d));
                }
            }
        }
        walk(path, &mut files);
    } else {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        files = unzip(&data)?;
    }
    build(
        files,
        format!("Creative product package ZZ0090 ({})", path.display()),
    )
}

fn unzip(data: &[u8]) -> Result<Vec<(String, Vec<u8>)>, String> {
    let mut z =
        zip::ZipArchive::new(std::io::Cursor::new(data)).map_err(|e| format!("not a zip: {e}"))?;
    let mut out = vec![];
    for i in 0..z.len() {
        let mut f = z.by_index(i).map_err(|e| e.to_string())?;
        if f.is_dir() {
            continue;
        }
        let mut buf = vec![];
        f.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        out.push((format!("/{}", f.name()), buf));
    }
    Ok(out)
}

/// Download Creative's package (plain HTTP, as Creative App does).
pub fn download_package(url: &str) -> Result<Imported, String> {
    let data = http_get(url, 3)?;
    let files = unzip(&data)?;
    build(files, format!("Creative product package ZZ0090 ({url})"))
}

fn http_get(url: &str, redirects: u8) -> Result<Vec<u8>, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or("only http:// URLs are supported")?;
    let (hostport, path) = rest
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((rest, "/".into()));
    let addr = if hostport.contains(':') {
        hostport.to_string()
    } else {
        format!("{hostport}:80")
    };
    let host = hostport.split(':').next().unwrap_or(hostport);
    let mut s =
        std::net::TcpStream::connect(&addr).map_err(|e| format!("connecting to {host}: {e}"))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .ok();
    write!(
        s,
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: x1ctl\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut resp = vec![];
    s.read_to_end(&mut resp)
        .map_err(|e| format!("download: {e}"))?;
    let split = resp
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("bad HTTP response")?;
    let head = String::from_utf8_lossy(&resp[..split]).to_string();
    let body = resp[split + 4..].to_vec();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    if (300..400).contains(&status) && redirects > 0 {
        let loc = head
            .lines()
            .find_map(|l| {
                l.strip_prefix("Location:")
                    .or_else(|| l.strip_prefix("location:"))
            })
            .map(str::trim)
            .ok_or("redirect without Location")?;
        return http_get(loc, redirects - 1);
    }
    if status != 200 {
        return Err(format!("download failed: HTTP {status}"));
    }
    Ok(body)
}

pub fn save_imported(imp: &Imported) -> std::io::Result<PathBuf> {
    let path = imported_path();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(imp).unwrap())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eq_json(id: &str, name: &str, order: i64, vals: [i32; 10]) -> Vec<u8> {
        let hz = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
        let bands: Vec<String> = hz
            .iter()
            .zip(vals)
            .map(|(f, v)| format!("{{\"Frequency\": {f}, \"Value\": {v}}}"))
            .collect();
        // BOM + tabs, like Creative's files
        format!(
            "\u{FEFF}{{\n\t\"Id\": \"{id}\", \"Name\": \"{name}\", \"DisplayName\": \"{name}\", \"Order\": {order},\n\t\"Settings\": [{{\"Type\": \"Headphone\", \"Unit\": \"dB\", \"PreAmp\": 0.0, \"Bands\": [{}]}}]}}",
            bands.join(",")
        )
        .into_bytes()
    }

    #[test]
    fn imports_package_layout() {
        let mode = br#"{"Name":"Effects Off","DisplayName":"Effects Off","Order":5,"Settings":[{"Type":"Headphone",
            "Surround":{"Enable":false,"Level":0.5},"GraphicEQ":{"Enable":false,"PresetId":"A"},"SuperXFi":{"Mode":0}}]}"#;
        let files = vec![
            (
                "/ZZ0090/Presets/EQ/Flat.json".into(),
                eq_json("A", "Flat", 90, [0; 10]),
            ),
            (
                "/ZZ0090/Presets/EQ/Fortnite.json".into(),
                eq_json("B", "Fortnite", 230, [2, 2, -2, -2, 0, 0, 2, -2, 0, 0]),
            ),
            (
                "/ZZ0090/Presets/EQ/Gaming.json".into(),
                eq_json("C", "Gaming", 10, [3, 4, -1, -2, 0, 2, 2, -5, 2, 0]),
            ),
            ("/ZZ0090/SoundMode/EffectsOff.json".into(), mode.to_vec()),
            ("/ZZ0090/version.json".into(), b"{}".to_vec()),
        ];
        let imp = build(files, "test".into()).unwrap();
        let names: Vec<_> = imp.eq_presets.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["Gaming", "Flat", "Fortnite"]); // Creative's order
        assert_eq!(
            imp.eq_presets[2].bands,
            [2.0, 2.0, -2.0, -2.0, 0.0, 0.0, 2.0, -2.0, 0.0, 0.0]
        );
        assert_eq!(imp.sound_modes[0].eq_preset, "Flat");
        assert!(imp.sound_modes[0].host_effects.get("Surround").is_some());
        assert!(build(vec![], "x".into()).is_err());
    }

    #[test]
    fn slug_matching() {
        assert_eq!(slug("Counter-Strike 2"), "counterstrike2");
        assert_eq!(slug("R & B"), "rb");
        assert_eq!(builtin_ordered()[0].0, "Flat");
    }
}
