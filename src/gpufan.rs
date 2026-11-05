//! Intel Arc A-series (DG2) GPU fans, driven through the GPU's PCODE fan
//! table (see `xe_pcode`). Mainline `xe` only reports their RPM.
//!
//! The daemon treats the card as one more fan channel: each duty becomes a
//! flat user table (with a firmware-side hot ramp as a safety net), and
//! releasing the channel restores the firmware's stock curve.

use crate::util::read_text;
use crate::xe_pcode::{dg2_flat_table, is_dg2, Mmio};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Below about 40% the A580's fans hunt between stopped and ~1400 rpm.
pub const GPU_FLOOR_PWM: i64 = 102;
/// S3 and reboot clear the table without any way to read it back, so the
/// current duty is re-sent this often.
const REFRESH: Duration = Duration::from_secs(30);

/// DG2 cards bound to `xe`: PCI device directories under `pci_root`
/// (normally /sys/bus/pci/drivers/xe).
pub fn discover(pci_root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(pci_root)
        .map(|rd| rd.flatten().map(|e| e.path()).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.join("resource0").exists())
        .filter(|p| {
            read_text(&p.join("device"))
                .and_then(|d| u16::from_str_radix(d.trim_start_matches("0x"), 16).ok())
                .is_some_and(is_dg2)
        })
        .collect();
    out.sort();
    out
}

pub struct GpuFan {
    pub pci: PathBuf,
    mmio: Option<Mmio>,
    sent: Option<(u8, Instant)>,
}

impl GpuFan {
    pub fn new(pci: PathBuf) -> Self {
        GpuFan { pci, mmio: None, sent: None }
    }

    /// PCI address, e.g. 0000:03:00.0.
    pub fn address(&self) -> String {
        self.pci.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    fn awake(&self) -> bool {
        // A runtime-suspended card reads all-ones; the table survives D3hot,
        // so just try again once it is back.
        read_text(&self.pci.join("power/runtime_status")).is_none_or(|s| s == "active")
    }

    fn commit(&mut self, table: &[(u8, u8)]) -> io::Result<()> {
        if self.mmio.is_none() {
            self.mmio = Some(Mmio::open(&self.pci.join("resource0"))?);
        }
        let mmio = self.mmio.as_ref().expect("mapped above");
        mmio.dg2_commit(table).map_err(io::Error::other)
    }

    /// Run the fan at `pct` percent. Unchanged duty is only re-sent every
    /// `REFRESH`.
    pub fn set(&mut self, pct: u8) -> io::Result<()> {
        if self.sent.is_some_and(|(p, at)| p == pct && at.elapsed() < REFRESH) {
            return Ok(());
        }
        if !self.awake() {
            self.sent = None;
            return Ok(());
        }
        self.commit(&dg2_flat_table(pct))?;
        self.sent = Some((pct, Instant::now()));
        Ok(())
    }

    /// Back to the firmware's stock curve.
    pub fn release(&mut self) -> io::Result<()> {
        self.sent = None;
        self.commit(&[])
    }
}

/// Percent for a 0..255 duty.
pub fn pct_for_pwm(pwm: i64) -> u8 {
    ((pwm.clamp(0, 255) * 100 + 127) / 255) as u8
}

/// Restore the stock curve on every DG2 card (systemd ExecStopPost).
pub fn restore_all(pci_root: &Path) {
    for pci in discover(pci_root) {
        let mut g = GpuFan::new(pci);
        if let Err(e) = g.release() {
            eprintln!("warning: GPU fan {}: {e}", g.address());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duty_to_percent() {
        assert_eq!(pct_for_pwm(0), 0);
        assert_eq!(pct_for_pwm(255), 100);
        assert_eq!(pct_for_pwm(GPU_FLOOR_PWM), 40);
        assert_eq!(pct_for_pwm(300), 100);
    }
}
