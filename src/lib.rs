//! Control library for the Creative Sound Blaster X1 (USB `041e:3286`).
//!
//! The X1 exposes a CDC ACM control channel (`/dev/ttyACM*`). After a text-mode
//! challenge/response unlock and `SW_MODE1`, it speaks Creative's binary "CDC raw"
//! protocol. What the X1 actually implements on board: Super X-Fi on/off (with a separate
//! EQ slot for each state), the 10-band graphic EQ with preamp, headphone/mic level and
//! mute, and the EnableUAC2 feature flag. Everything else in Creative App (Acoustic Engine,
//! CrystalVoice, Scout Mode, speaker config…) runs on the Windows host, not the device.

pub mod auth;
pub mod device;
pub mod error;
pub mod presets;
pub mod proto;
pub mod serial;

pub use device::{Device, Notice, State};
pub use error::{Error, Result};

/// D-Bus coordinates of `x1d`.
pub const DBUS_NAME: &str = "io.github.x1ctl";
pub const DBUS_PATH: &str = "/io/github/x1ctl/Device";
pub const DBUS_IFACE: &str = "io.github.x1ctl.Device";
