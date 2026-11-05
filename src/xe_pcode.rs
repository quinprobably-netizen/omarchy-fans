//! Intel Xe GPU PCODE mailbox over BAR0 MMIO, for the fan-speed-control
//! (FSC) commands that mainline `xe` does not expose.
//!
//! Protocol (drivers/gpu/drm/xe/xe_pcode.c, xe_pcode_api.h):
//!   1. PCODE_MAILBOX must not have READY set (else busy)
//!   2. write DATA0, DATA1
//!   3. write MAILBOX = READY | param2<<16 | param1<<8 | command
//!   4. poll until READY clears; low byte of MAILBOX is the error code
//!   5. read DATA0/DATA1 back
//!
//! The xe driver serialises its own mailbox use with a mutex we cannot
//! take; the READY check makes a collision fail with "busy" rather than
//! interleave, and every caller retries.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

pub const PCODE_MAILBOX: usize = 0x138124;
pub const PCODE_DATA0: usize = 0x138128;
pub const PCODE_DATA1: usize = 0x13812C;
const PCODE_READY: u32 = 1 << 31;

pub const FAN_SPEED_CONTROL: u8 = 0x7D;
pub const FSC_WRITE_NUM_FAN_CONTROL_POINTS: u8 = 0x0;
pub const FSC_WRITE_FAN_TABLE: u8 = 0x1;
pub const FSC_READ_MAX_FAN_RPS: u8 = 0x3;
pub const FSC_READ_NUM_FANS: u8 = 0x4;
pub const FSC_READ_STOCK_FAN_CONTROL_POINTS: u8 = 0x5;
pub const FSC_READ_USER_FAN_CONTROL_POINTS: u8 = 0x6;
pub const FSC_READ_FAN_TABLE: u8 = 0x7;
pub const FSC_READ_FAN_MIN_PWM: u8 = 0x8;

/// `FCP_INDEX(type, point)`: table type in bit 31, point index below.
pub fn fcp_index(user: bool, point: u32) -> u32 {
    (u32::from(user) << 31) | (point & 0x7fff_ffff)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcodeError {
    Busy,
    Timeout,
    /// Non-zero status byte from PCODE.
    Status(u8),
    /// MMIO reads all-ones: device suspended or gone.
    DeviceAsleep,
}

impl std::fmt::Display for PcodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PcodeError::Busy => write!(f, "mailbox busy"),
            PcodeError::Timeout => write!(f, "mailbox timeout"),
            PcodeError::DeviceAsleep => write!(f, "GPU MMIO reads all-ones (suspended?)"),
            PcodeError::Status(s) => write!(
                f,
                "pcode error {s:#x} ({})",
                match s {
                    0x1 => "illegal command",
                    0x2 => "timed out",
                    0x3 => "illegal data",
                    0x4 => "illegal subcommand",
                    0x6 => "locked",
                    0x10 => "GT ratio out of range",
                    0x11 => "rejected",
                    _ => "unknown",
                }
            ),
        }
    }
}

pub struct Mmio {
    ptr: *mut u8,
    len: usize,
}

// The mapping is plain device memory; access is serialised by callers.
unsafe impl Send for Mmio {}

impl Mmio {
    /// Map a PCI BAR, e.g. /sys/bus/pci/devices/0000:03:00.0/resource0.
    pub fn open(resource: &Path) -> io::Result<Mmio> {
        let file = OpenOptions::new().read(true).write(true).open(resource)?;
        let len = file.metadata()?.len() as usize;
        if len < PCODE_DATA1 + 4 {
            return Err(io::Error::other("BAR too small for PCODE registers"));
        }
        // SAFETY: mapping a sysfs PCI resource file; checked length above.
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, file.as_raw_fd(), 0)
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(Mmio { ptr: ptr.cast(), len })
    }

    pub fn read32(&self, off: usize) -> u32 {
        assert!(off + 4 <= self.len && off.is_multiple_of(4));
        // SAFETY: in-bounds, aligned, device memory.
        unsafe { std::ptr::read_volatile(self.ptr.add(off).cast::<u32>()) }
    }

    fn write32(&self, off: usize, v: u32) {
        assert!(off + 4 <= self.len && off.is_multiple_of(4));
        // SAFETY: in-bounds, aligned, device memory.
        unsafe { std::ptr::write_volatile(self.ptr.add(off).cast::<u32>(), v) }
    }

    fn wait_idle(&self, timeout: Duration) -> Result<u32, PcodeError> {
        let end = Instant::now() + timeout;
        loop {
            let mb = self.read32(PCODE_MAILBOX);
            if mb == u32::MAX {
                return Err(PcodeError::DeviceAsleep);
            }
            if mb & PCODE_READY == 0 {
                return Ok(mb);
            }
            if Instant::now() >= end {
                return Err(PcodeError::Timeout);
            }
            thread::sleep(Duration::from_micros(50));
        }
    }

    /// One mailbox transaction. Returns (DATA0, DATA1) after completion.
    pub fn mailbox(&self, cmd: u8, param1: u8, param2: u8, data0: u32, data1: u32) -> Result<(u32, u32), PcodeError> {
        // The driver may be mid-transaction: give it a moment, then fail.
        match self.wait_idle(Duration::from_millis(20)) {
            Err(PcodeError::Timeout) => return Err(PcodeError::Busy),
            r => r?,
        };
        self.write32(PCODE_DATA0, data0);
        self.write32(PCODE_DATA1, data1);
        self.write32(PCODE_MAILBOX, PCODE_READY | (u32::from(param2) << 16) | (u32::from(param1) << 8) | u32::from(cmd));
        let mb = self.wait_idle(Duration::from_millis(100))?;
        let out = (self.read32(PCODE_DATA0), self.read32(PCODE_DATA1));
        match (mb & 0xff) as u8 {
            0 => Ok(out),
            s => Err(PcodeError::Status(s)),
        }
    }

    /// `mailbox` with retries on a busy mailbox.
    pub fn mailbox_retry(&self, cmd: u8, param1: u8, param2: u8, data0: u32, data1: u32) -> Result<(u32, u32), PcodeError> {
        let mut last = PcodeError::Busy;
        for _ in 0..20 {
            match self.mailbox(cmd, param1, param2, data0, data1) {
                Err(PcodeError::Busy) => {
                    last = PcodeError::Busy;
                    thread::sleep(Duration::from_millis(5));
                }
                r => return r,
            }
        }
        Err(last)
    }

    pub fn fsc_read(&self, sub: u8, fan: u8, data0: u32) -> Result<(u32, u32), PcodeError> {
        self.mailbox_retry(FAN_SPEED_CONTROL, sub, fan, data0, 0)
    }

    pub fn fsc_write(&self, sub: u8, fan: u8, data0: u32) -> Result<(u32, u32), PcodeError> {
        self.mailbox_retry(FAN_SPEED_CONTROL, sub, fan, data0, 0)
    }
}

impl Drop for Mmio {
    fn drop(&mut self) {
        // SAFETY: unmapping our own mapping.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

// ---- DG2 (Arc A-series) fan table ------------------------------------
//
// DG2 firmware takes a user fan curve on the same command with its own
// subcommands (reverse engineered by AIVirtuoso/intel-arc-a770-fan-control,
// verified here on an ASRock A580):
//   sub 1: number of points, 1..=10; 0 = back to the firmware's stock curve.
//          Must come first: pairs written before it are never armed.
//   sub 2: one call per pair of points, in order:
//          DATA0 = pt[i] | pt[i+1] << 16, pt = temp_c | speed_pct << 8.
// The firmware interpolates on package temperature. Nothing reads back
// (every FSC read returns 0), and the table is lost on S3 and reboot.

pub const DG2_FSC_NUM_POINTS: u8 = 0x1;
pub const DG2_FSC_POINT_PAIR: u8 = 0x2;
pub const DG2_MAX_POINTS: usize = 10;

/// PCI device IDs of DG2 (Alchemist) desktop and mobile parts.
pub fn is_dg2(device_id: u16) -> bool {
    matches!(device_id, 0x4f80..=0x4f88 | 0x5690..=0x56c2)
}

pub fn dg2_validate(points: &[(u8, u8)]) -> Result<(), String> {
    if points.len() > DG2_MAX_POINTS {
        return Err(format!("at most {DG2_MAX_POINTS} points"));
    }
    if points.iter().any(|&(_, s)| s > 100) {
        return Err("speed is a percentage (0..=100)".into());
    }
    if points.windows(2).any(|w| w[1].0 < w[0].0 || w[1].1 < w[0].1) {
        return Err("temperatures and speeds must not decrease".into());
    }
    Ok(())
}

/// Pairs as DG2 wants them for subcommand 2.
pub fn dg2_pairs(points: &[(u8, u8)]) -> Vec<u32> {
    points
        .chunks(2)
        .map(|c| encode_point(c[0].0, c[0].1) | c.get(1).map_or(0, |&(t, s)| encode_point(t, s) << 16))
        .collect()
}

/// Flat duty with a firmware-side safety net: if nothing refreshes the
/// table (daemon died), the card still ramps to 100% when hot.
pub fn dg2_flat_table(pct: u8) -> Vec<(u8, u8)> {
    let pct = pct.min(100);
    vec![(0, pct), (85, pct), (95, 100), (96, 100)]
}

impl Mmio {
    /// Install a DG2 user fan table; an empty table restores the stock curve.
    pub fn dg2_commit(&self, points: &[(u8, u8)]) -> Result<(), String> {
        dg2_validate(points)?;
        self.fsc_write(DG2_FSC_NUM_POINTS, 0, points.len() as u32).map_err(|e| format!("point count: {e}"))?;
        for (i, d) in dg2_pairs(points).into_iter().enumerate() {
            self.fsc_write(DG2_FSC_POINT_PAIR, 0, d).map_err(|e| format!("points {}..: {e}", i * 2))?;
        }
        Ok(())
    }
}

/// Fan control point as PCODE stores it: temp in bits 7:0 (°C), speed in
/// bits 15:8 (percent).
pub fn decode_point(raw: u32) -> (u8, u8) {
    ((raw & 0xff) as u8, ((raw >> 8) & 0xff) as u8)
}

pub fn encode_point(temp: u8, speed: u8) -> u32 {
    u32::from(temp) | (u32::from(speed.min(100)) << 8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dg2_pairs_pack_two_points_per_word() {
        let pts = [(40, 35), (60, 50), (80, 100)];
        assert_eq!(dg2_pairs(&pts), vec![(40 | 35 << 8) | ((60 | 50 << 8) << 16), 80 | 100 << 8]);
        assert!(dg2_validate(&pts).is_ok());
        assert!(dg2_validate(&[(60, 50), (40, 60)]).is_err(), "temps must not decrease");
        assert!(dg2_validate(&[(40, 60), (60, 50)]).is_err(), "speeds must not decrease");
        assert!(dg2_validate(&[(0, 0); 11]).is_err());
        assert!(dg2_validate(&dg2_flat_table(42)).is_ok());
        assert!(is_dg2(0x56a2) && is_dg2(0x56a0) && !is_dg2(0xe20b));
    }

    #[test]
    fn point_encoding_matches_driver_layout() {
        assert_eq!(encode_point(60, 35), 60 | (35 << 8));
        assert_eq!(decode_point(encode_point(85, 100)), (85, 100));
        assert_eq!(encode_point(40, 150) >> 8, 100, "speed is a percentage");
        assert_eq!(fcp_index(true, 3), 0x8000_0003);
        assert_eq!(fcp_index(false, 3), 3);
    }
}
