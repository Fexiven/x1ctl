//! Minimal raw tty access (no libudev): discover the X1's CDC ACM port via sysfs and
//! open it in raw mode with a short read timeout.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};

pub const VID: u16 = 0x041E;
pub const PID_X1: u16 = 0x3286;

fn read_hex(p: &Path) -> Option<u16> {
    u16::from_str_radix(fs::read_to_string(p).ok()?.trim(), 16).ok()
}

/// Find `/dev/ttyACM*` belonging to the X1 by walking up from the tty's sysfs device to the
/// USB device node that carries `idVendor`/`idProduct`.
pub fn find_x1() -> Result<PathBuf> {
    find_by_ids(VID, PID_X1)
}

pub fn find_by_ids(vid: u16, pid: u16) -> Result<PathBuf> {
    let Ok(rd) = fs::read_dir("/sys/class/tty") else {
        return Err(Error::NotFound);
    };
    let mut names: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("ttyACM"))
        .collect();
    names.sort();
    for name in names {
        let Ok(mut dev) = fs::canonicalize(format!("/sys/class/tty/{name}/device")) else {
            continue;
        };
        for _ in 0..4 {
            if let (Some(v), Some(p)) = (
                read_hex(&dev.join("idVendor")),
                read_hex(&dev.join("idProduct")),
            ) {
                if v == vid && p == pid {
                    return Ok(PathBuf::from(format!("/dev/{name}")));
                }
                break;
            }
            if !dev.pop() {
                break;
            }
        }
    }
    Err(Error::NotFound)
}

/// USB descriptor details of the device behind a tty, from sysfs.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct UsbInfo {
    pub manufacturer: String,
    pub product: String,
    pub bcd_device: String,
    pub usb_version: String,
    pub speed_mbps: String,
    pub bus_path: String,
}

pub fn usb_info(tty: &Path) -> Option<UsbInfo> {
    let name = fs::canonicalize(tty)
        .ok()?
        .file_name()?
        .to_string_lossy()
        .into_owned();
    let mut dev = fs::canonicalize(format!("/sys/class/tty/{name}/device")).ok()?;
    for _ in 0..4 {
        if dev.join("idVendor").exists() {
            let r = |f: &str| {
                fs::read_to_string(dev.join(f))
                    .map(|s| s.trim().to_string())
                    .unwrap_or_default()
            };
            return Some(UsbInfo {
                manufacturer: r("manufacturer"),
                product: r("product"),
                bcd_device: r("bcdDevice"),
                usb_version: r("version"),
                speed_mbps: r("speed"),
                bus_path: format!("{}-{}", r("busnum"), r("devpath")),
            });
        }
        if !dev.pop() {
            break;
        }
    }
    None
}

pub struct Port {
    f: File,
    path: PathBuf,
}

impl Port {
    pub fn open(path: &Path) -> Result<Self> {
        let f = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY | libc::O_CLOEXEC)
            .open(path)?;
        let fd = f.as_raw_fd();
        // Exclusive access: the session is stateful, a second opener would corrupt it.
        if unsafe { libc::ioctl(fd, libc::TIOCEXCL) } != 0 {
            log::warn!("TIOCEXCL failed: {}", std::io::Error::last_os_error());
        }
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut t) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // cfmakeraw() by hand (keeps the binary linkable fully statically);
            // baud is nominal on USB CDC but set it anyway.
            t.c_iflag &= !(libc::IGNBRK
                | libc::BRKINT
                | libc::PARMRK
                | libc::ISTRIP
                | libc::INLCR
                | libc::IGNCR
                | libc::ICRNL
                | libc::IXON
                | libc::IXOFF);
            t.c_oflag &= !libc::OPOST;
            t.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG | libc::IEXTEN);
            t.c_cflag &= !(libc::CSIZE | libc::PARENB | libc::CBAUD | libc::CRTSCTS);
            t.c_cflag |= libc::CS8 | libc::B115200 | libc::CLOCAL | libc::CREAD;
            t.c_cc[libc::VMIN] = 0;
            t.c_cc[libc::VTIME] = 1; // read() returns after ≤100 ms without data
            if libc::tcsetattr(fd, libc::TCSANOW, &t) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            libc::tcflush(fd, libc::TCIOFLUSH);
        }
        Ok(Self {
            f,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn try_clone(&self) -> Result<Self> {
        Ok(Self {
            f: self.f.try_clone()?,
            path: self.path.clone(),
        })
    }

    /// True once the tty has been hung up — i.e. the USB device was unplugged or reset.
    /// cdc_acm then returns EOF (0 bytes) from read() forever instead of an error, so this
    /// has to be checked explicitly. Also true if the device node is gone.
    pub fn hung_up(&self) -> bool {
        let mut pfd = libc::pollfd { fd: self.f.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, 0) };
        if r < 0 {
            return true;
        }
        if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return true;
        }
        !self.path.exists()
    }

    /// Read whatever arrives within ~100 ms. `Ok(0)` = nothing yet.
    /// A vanished USB device shows up as `EIO`/`ENODEV` → `Error::Disconnected`.
    pub fn read_some(&mut self, buf: &mut [u8]) -> Result<usize> {
        match self.f.read(buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(0),
            Err(e)
                if matches!(
                    e.raw_os_error(),
                    Some(libc::EIO | libc::ENODEV | libc::ENXIO)
                ) =>
            {
                Err(Error::Disconnected)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub fn write_all(&mut self, data: &[u8]) -> Result<()> {
        self.f.write_all(data).map_err(|e| match e.raw_os_error() {
            Some(libc::EIO | libc::ENODEV | libc::ENXIO) => Error::Disconnected,
            _ => e.into(),
        })?;
        Ok(())
    }

    /// Collect bytes until `pred(buf)` holds or `timeout` passes.
    pub fn read_until(
        &mut self,
        timeout: Duration,
        mut pred: impl FnMut(&[u8]) -> bool,
    ) -> Result<Vec<u8>> {
        let deadline = std::time::Instant::now() + timeout;
        let mut out = Vec::new();
        let mut tmp = [0u8; 512];
        while std::time::Instant::now() < deadline {
            let n = self.read_some(&mut tmp)?;
            out.extend_from_slice(&tmp[..n]);
            if pred(&out) {
                break;
            }
        }
        Ok(out)
    }

    pub fn drain(&mut self) -> Result<Vec<u8>> {
        self.read_until(Duration::from_millis(150), |_| false)
    }
}
