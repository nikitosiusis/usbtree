//! Live per-device activity. Unprivileged: URB-count deltas from sysfs
//! `urbnum`. When usbmon is readable (debugfs text or `/dev/usbmon0` binary):
//! real bytes/s.

use std::collections::HashMap;
#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::io::{self, BufRead, BufReader};
#[cfg(target_os = "linux")]
use std::sync::{Arc, Mutex};

use crate::usb::Device;

#[cfg(target_os = "linux")]
const USBMON: &str = "/sys/kernel/debug/usb/usbmon/0u";
#[cfg(target_os = "linux")]
const USBMON_DEV: &str = "/dev/usbmon0";

/// Why usbmon bytes/s isn't active, so the header can tell the user the
/// concrete fix: load the module, run as root, or address kernel lockdown.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoBytes {
    /// usbmon files missing (`ENOENT`): module not loaded.
    NeedModule,
    /// present but unreadable: not running as root, or root cannot open the
    /// binary monitor for a non-lockdown reason.
    NeedRoot,
    /// present, running as root, but blocked by kernel lockdown/Secure Boot.
    Locked,
}

pub enum Metrics {
    /// URBs/s per device from `/sys/bus/usb/devices/*/urbnum`.
    #[cfg(target_os = "linux")]
    Urb {
        prev: HashMap<String, u64>,
        why: NoBytes,
    },
    /// Bytes/s per (bus, devnum) accumulated by a usbmon reader thread.
    #[cfg(target_os = "linux")]
    UsbMon { bytes: Arc<Mutex<HashMap<(u16, u16), u64>>> },
    /// Synthetic bytes/s for `--demo`, deterministic per device and tick.
    Demo { tick: u64 },
    /// No per-device activity source (macOS/Windows: nothing published
    /// unprivileged — the sole IOKit counter is HID-only, not worth it).
    #[cfg(not(target_os = "linux"))]
    None,
}

impl Metrics {
    #[cfg(target_os = "linux")]
    pub fn new() -> Self {
        match fs::File::open(USBMON) {
            Ok(f) => Metrics::UsbMon {
                bytes: spawn_text_usbmon(f),
            },
            Err(text_err) => binary_usbmon().unwrap_or_else(|bin_err| Metrics::Urb {
                prev: HashMap::new(),
                why: no_bytes_reason(&text_err, &bin_err),
            }),
        }
    }

    // ponytail: macOS/Windows have no unprivileged per-device traffic counter
    // (urbnum is Linux sysfs; IOKit only exposes HID InputReportCount). Show
    // the tree, no rates. Upgrade path: HID-only blink via ioreg if wanted.
    #[cfg(not(target_os = "linux"))]
    pub fn new() -> Self {
        Metrics::None
    }

    pub fn demo() -> Self {
        Metrics::Demo { tick: 0 }
    }

    /// True when rates are real bytes (usbmon), not URB counts.
    pub fn is_bytes(&self) -> bool {
        match self {
            #[cfg(target_os = "linux")]
            Metrics::UsbMon { .. } => true,
            Metrics::Demo { .. } => true,
            _ => false,
        }
    }

    /// True when the header note is an actionable warning, not a neutral source
    /// indicator.
    pub fn is_warning(&self) -> bool {
        match self {
            #[cfg(target_os = "linux")]
            Metrics::Urb {
                why: NoBytes::NeedModule | NoBytes::Locked,
                ..
            } => true,
            _ => false,
        }
    }

    /// Header indicator text: the active source, or — when on URB fallback —
    /// the specific fix to unlock bytes/s.
    pub fn header_note(&self) -> &'static str {
        match self {
            #[cfg(target_os = "linux")]
            Metrics::UsbMon { .. } => "◉ usbmon bytes/s",
            Metrics::Demo { .. } => "◉ usbmon bytes/s",
            #[cfg(target_os = "linux")]
            Metrics::Urb { why, .. } => match why {
                NoBytes::NeedModule => "⚠ usbmon not loaded — modprobe usbmon",
                NoBytes::NeedRoot => "◌ urb activity — sudo + modprobe usbmon for bytes/s",
                NoBytes::Locked => "⚠ usbmon blocked by kernel lockdown",
            },
            #[cfg(not(target_os = "linux"))]
            Metrics::None => "◌ activity n/a on this platform",
        }
    }

    /// Per-device rate accumulated since the last call, keyed by sysfs name.
    pub fn sample(&mut self, devices: &[Device]) -> HashMap<String, u64> {
        match self {
            #[cfg(target_os = "linux")]
            Metrics::Urb { prev, .. } => {
                let mut out = HashMap::new();
                let mut cur = HashMap::new();
                for d in devices {
                    let path = format!("/sys/bus/usb/devices/{}/urbnum", d.name);
                    let Some(n) = read_u64(&path) else { continue };
                    let base = prev.get(&d.name).copied().unwrap_or(n);
                    out.insert(d.name.clone(), n.saturating_sub(base));
                    cur.insert(d.name.clone(), n);
                }
                *prev = cur;
                out
            }
            #[cfg(target_os = "linux")]
            Metrics::UsbMon { bytes } => {
                let drained = std::mem::take(&mut *bytes.lock().unwrap());
                devices
                    .iter()
                    .filter_map(|d| {
                        let key = (d.busnum()?, d.devnum as u16);
                        Some((d.name.clone(), *drained.get(&key)?))
                    })
                    .collect()
            }
            Metrics::Demo { tick } => {
                *tick += 1;
                let t = *tick;
                devices
                    .iter()
                    .map(|d| (d.name.clone(), demo_rate(d, t)))
                    .filter(|&(_, r)| r > 0)
                    .collect()
            }
            #[cfg(not(target_os = "linux"))]
            Metrics::None => HashMap::new(),
        }
    }
}

#[cfg(target_os = "linux")]
fn spawn_text_usbmon(f: fs::File) -> Arc<Mutex<HashMap<(u16, u16), u64>>> {
    let bytes: Arc<Mutex<HashMap<(u16, u16), u64>>> = Arc::default();
    let sink = Arc::clone(&bytes);
    std::thread::spawn(move || {
        for line in BufReader::new(f).lines() {
            let Ok(line) = line else { break };
            if let Some((key, len)) = parse_usbmon(&line) {
                *sink.lock().unwrap().entry(key).or_insert(0) += len;
            }
        }
    });
    bytes
}

#[cfg(target_os = "linux")]
fn binary_usbmon() -> io::Result<Metrics> {
    let f = fs::File::open(USBMON_DEV)?;
    Ok(Metrics::UsbMon {
        bytes: spawn_binary_usbmon(f),
    })
}

#[cfg(target_os = "linux")]
fn spawn_binary_usbmon(f: fs::File) -> Arc<Mutex<HashMap<(u16, u16), u64>>> {
    use std::os::fd::AsRawFd;

    let bytes: Arc<Mutex<HashMap<(u16, u16), u64>>> = Arc::default();
    let sink = Arc::clone(&bytes);
    std::thread::spawn(move || {
        let fd = f.as_raw_fd();
        let mut data = vec![0_u8; 65536];
        loop {
            let mut hdr = MonBinHdr::default();
            let mut req = MonBinGet {
                hdr: &mut hdr,
                data: data.as_mut_ptr().cast(),
                alloc: data.len(),
            };
            let rc = unsafe { libc::ioctl(fd, MON_IOCX_GET, &mut req) };
            if rc < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            if let Some((key, len)) = parse_usbmon_bin(&hdr) {
                *sink.lock().unwrap().entry(key).or_insert(0) += len;
            }
        }
    });
    bytes
}

/// Plausible traffic per class: steady audio, bursty storage, trickling HID.
fn demo_rate(d: &Device, t: u64) -> u64 {
    let phase: u64 = d.name.bytes().map(u64::from).sum();
    let wave = (((t + phase) as f64) * 0.9).sin() * 0.5 + 0.5; // 0..1
    let base = match d.effective_class() {
        0x01 => 12_000_000.0,
        0x0e => 24_000_000.0,
        0x08 => {
            if (t + phase) % 11 < 5 {
                280_000_000.0
            } else {
                400_000.0
            }
        }
        0x03 if (t + phase).is_multiple_of(3) => 1_800.0,
        _ => 0.0,
    };
    (base * (0.6 + 0.4 * wave)) as u64
}

#[cfg(target_os = "linux")]
fn read_u64(path: &str) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(target_os = "linux")]
fn no_bytes_reason(text_err: &io::Error, bin_err: &io::Error) -> NoBytes {
    no_bytes_reason_for(
        effective_uid_is_root(),
        kernel_lockdown_enabled(),
        text_err.kind(),
        bin_err.kind(),
    )
}

#[cfg(target_os = "linux")]
fn no_bytes_reason_for(
    is_root: bool,
    lockdown: bool,
    text_kind: io::ErrorKind,
    bin_kind: io::ErrorKind,
) -> NoBytes {
    if lockdown && is_root && text_kind == io::ErrorKind::PermissionDenied {
        NoBytes::Locked
    } else if bin_kind == io::ErrorKind::NotFound || text_kind == io::ErrorKind::NotFound {
        NoBytes::NeedModule
    } else {
        NoBytes::NeedRoot
    }
}

#[cfg(target_os = "linux")]
fn effective_uid_is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}

#[cfg(target_os = "linux")]
fn kernel_lockdown_enabled() -> bool {
    fs::read_to_string("/sys/kernel/security/lockdown")
        .ok()
        .is_some_and(|s| lockdown_on(&s))
}

/// The bracket check, split out so it's testable without the file. The active
/// mode is the bracketed word: `[none] integrity confidentiality` = off.
#[cfg(any(target_os = "linux", test))]
fn lockdown_on(mode: &str) -> bool {
    mode.split_whitespace()
        .find(|part| part.starts_with('[') && part.ends_with(']'))
        .is_some_and(|part| part != "[none]")
}

/// Parse one usbmon text line, e.g.
/// `ffff9c.. 3003687252 C Ii:1:002:1 0:8 8 = 1f00..` -> ((bus, dev), bytes).
/// Counts completed IN and submitted OUT transfers (usbtop's method).
// ponytail: control transfers with inline setup ('s' status word) are
// skipped — a few bytes each, not worth the extra field shuffling
#[cfg(any(target_os = "linux", test))]
fn parse_usbmon(line: &str) -> Option<((u16, u16), u64)> {
    let mut f = line.split_whitespace();
    let (_tag, _ts) = (f.next()?, f.next()?);
    let event = f.next()?;
    let addr = f.next()?;
    if f.next()? == "s" {
        return None;
    }
    let len: u64 = f.next()?.parse().ok()?;
    let mut a = addr.split(':');
    let dir = a.next()?.chars().nth(1)?; // "Ii" -> 'i', "Bo" -> 'o'
    let bus: u16 = a.next()?.parse().ok()?;
    let dev: u16 = a.next()?.parse().ok()?;
    match (event, dir) {
        ("C", 'i') | ("S", 'o') => Some(((bus, dev), len)),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Default)]
struct MonBinHdr {
    id: u64,
    type_: u8,
    xfer_type: u8,
    epnum: u8,
    devnum: u8,
    busnum: u16,
    flag_setup: i8,
    flag_data: i8,
    ts_sec: i64,
    ts_usec: i32,
    status: i32,
    length: u32,
    len_cap: u32,
    setup: [u8; 8],
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct MonBinGet {
    hdr: *mut MonBinHdr,
    data: *mut libc::c_void,
    alloc: libc::size_t,
}

#[cfg(target_os = "linux")]
const MON_IOC_MAGIC: u8 = 0x92;
#[cfg(target_os = "linux")]
const MON_IOCX_GET: libc::c_ulong = iow::<MonBinGet>(MON_IOC_MAGIC, 6);

#[cfg(target_os = "linux")]
const fn iow<T>(type_: u8, nr: u8) -> libc::c_ulong {
    ioc::<T>(1, type_, nr)
}

#[cfg(target_os = "linux")]
const fn ioc<T>(dir: u8, type_: u8, nr: u8) -> libc::c_ulong {
    ((dir as libc::c_ulong) << 30)
        | ((std::mem::size_of::<T>() as libc::c_ulong) << 16)
        | ((type_ as libc::c_ulong) << 8)
        | nr as libc::c_ulong
}

#[cfg(target_os = "linux")]
fn parse_usbmon_bin(h: &MonBinHdr) -> Option<((u16, u16), u64)> {
    let event = h.type_ as char;
    let dir_in = h.epnum & 0x80 != 0;
    match (event, dir_in) {
        ('C', true) | ('S', false) if h.length > 0 => {
            Some(((h.busnum, h.devnum as u16), h.length as u64))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usbmon_parse() {
        // completed interrupt IN: counted
        let l = "ffff9c1a 3003687252 C Ii:1:002:1 0:8 8 = 1f000000";
        assert_eq!(parse_usbmon(l), Some(((1, 2), 8)));
        // submitted bulk OUT: counted
        let l = "ffff9c1a 3003687252 S Bo:2:005:2 -115 512 = aa";
        assert_eq!(parse_usbmon(l), Some(((2, 5), 512)));
        // submitted IN (no data moved yet): not counted
        let l = "ffff9c1a 3003687252 S Ii:1:002:1 -115:8 8 <";
        assert_eq!(parse_usbmon(l), None);
        // control setup: skipped
        let l = "ffff9c1a 3003687252 S Co:1:001:0 s 23 01 0010 0002 0000 0";
        assert_eq!(parse_usbmon(l), None);
        assert_eq!(parse_usbmon("garbage"), None);
    }

    #[test]
    fn lockdown_bracket() {
        assert!(!lockdown_on("[none] integrity confidentiality\n"));
        assert!(lockdown_on("none [integrity] confidentiality\n"));
        assert!(lockdown_on("none integrity [confidentiality]\n"));
        assert!(!lockdown_on("none integrity confidentiality\n"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn root_warning_distinguishes_lockdown_and_unloaded_usbmon() {
        assert_eq!(
            no_bytes_reason_for(
                true,
                true,
                io::ErrorKind::PermissionDenied,
                io::ErrorKind::NotFound,
            ),
            NoBytes::Locked
        );
        assert_eq!(
            no_bytes_reason_for(false, false, io::ErrorKind::NotFound, io::ErrorKind::NotFound),
            NoBytes::NeedModule
        );
        assert_eq!(
            no_bytes_reason_for(true, false, io::ErrorKind::NotFound, io::ErrorKind::NotFound),
            NoBytes::NeedModule
        );
        assert_eq!(
            no_bytes_reason_for(
                false,
                false,
                io::ErrorKind::PermissionDenied,
                io::ErrorKind::PermissionDenied,
            ),
            NoBytes::NeedRoot
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn usbmon_binary_parse() {
        let mut h = MonBinHdr {
            type_: b'C',
            epnum: 0x81,
            devnum: 2,
            busnum: 1,
            length: 64,
            ..Default::default()
        };
        assert_eq!(parse_usbmon_bin(&h), Some(((1, 2), 64)));

        h.type_ = b'S';
        h.epnum = 0x02;
        h.devnum = 5;
        h.busnum = 3;
        h.length = 512;
        assert_eq!(parse_usbmon_bin(&h), Some(((3, 5), 512)));

        h.type_ = b'S';
        h.epnum = 0x81;
        assert_eq!(parse_usbmon_bin(&h), None);

        h.type_ = b'C';
        h.epnum = 0x02;
        assert_eq!(parse_usbmon_bin(&h), None);
    }
}
