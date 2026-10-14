use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    NotFound,
    Timeout(&'static str),
    Auth(String),
    NotReady,
    /// Device answered with a non-success `02` ack: (opcode, status).
    Nak(u8, u8),
    Protocol(String),
    /// Refused locally (unsafe opcode, value out of range, guarded command).
    Refused(String),
    Disconnected,
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub fn status_name(s: u8) -> &'static str {
    match s {
        0x00 => "success",
        0x01 => "data pending",
        0x02 => "conditional success",
        0x80 => "general failure",
        0x81 => "not supported",
        0x82 => "temporarily unsupported",
        0x83 => "invalid parameter",
        0x84 => "invalid length",
        0x85 => "device booting up",
        _ => "unknown status",
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::NotFound => write!(f, "Sound Blaster X1 (041e:3286) not found"),
            Error::Timeout(what) => write!(f, "timed out waiting for {what}"),
            Error::Auth(m) => write!(f, "authentication failed: {m}"),
            Error::NotReady => write!(f, "device still booting (NotYet); try again"),
            Error::Nak(op, s) => {
                write!(
                    f,
                    "device rejected opcode 0x{op:02X}: {} (0x{s:02X})",
                    status_name(*s)
                )
            }
            Error::Protocol(m) => write!(f, "protocol error: {m}"),
            Error::Refused(m) => write!(f, "refused: {m}"),
            Error::Disconnected => write!(f, "device disconnected"),
        }
    }
}

impl std::error::Error for Error {}
