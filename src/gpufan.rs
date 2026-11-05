//! Intel Arc A-series (DG2) GPU fans, driven through the GPU's PCODE fan
//! table (the `arc-fan` crate). Mainline `xe` only reports their RPM.
//!
//! The daemon treats the card as one more fan channel: each duty becomes a
//! flat user table (with a firmware-side hot ramp as a safety net), and
//! releasing the channel restores the firmware's stock curve.
//!
//! Mailbox transactions can take seconds when the `xe` driver is busy with
//! the mailbox, so each card has a worker thread that owns the BAR mapping.
//! The control loop only posts the duty it wants; the worker applies the
//! newest one, so a stale request can never overwrite a later one.

use arc_fan::card::{is_dg2, Card};
use arc_fan::curve::fixed;
use arc_fan::pcode::Mmio;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::util::read_text;

/// Below about 40% the A580's fans hunt between stopped and ~1400 rpm.
pub const GPU_FLOOR_PWM: i64 = 102;
/// S3 and reboot clear the table without any way to read it back, so the
/// current duty is re-sent this often (and at once after a resume).
const REFRESH: Duration = Duration::from_secs(30);
/// Retry interval while the card is runtime-suspended or a write failed.
const RETRY: Duration = Duration::from_secs(1);
/// How long `release` waits for the stock curve to be written. A busy
/// mailbox costs up to ~1.1 s per transaction, three per table.
const RELEASE_WAIT: Duration = Duration::from_secs(4);

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Want {
    Duty(u8),
    Stock,
}

#[derive(Default)]
struct State {
    /// Newest request; None until the daemon first drives the fan.
    want: Option<Want>,
    /// What the card last accepted, and when.
    applied: Option<(Want, Instant)>,
    /// Re-send even if unchanged (after a resume from suspend).
    resend: bool,
    /// The worker is mid-transaction.
    busy: bool,
    error: Option<String>,
    stop: bool,
}

struct Shared {
    state: Mutex<State>,
    cv: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Nothing left to do: the newest request is on the card.
    fn settled(st: &State) -> bool {
        !st.busy && !st.resend && st.want.is_none_or(|w| st.applied.is_some_and(|(a, _)| a == w))
    }

    fn wait_until(&self, timeout: Duration, done: impl Fn(&State) -> bool) -> MutexGuard<'_, State> {
        let end = Instant::now() + timeout;
        let mut st = self.lock();
        while !done(&st) {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            st = self.cv.wait_timeout(st, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        st
    }
}

pub struct GpuFan {
    pub pci: PathBuf,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl GpuFan {
    /// No thread starts until the fan is first driven or released.
    pub fn new(pci: PathBuf) -> Self {
        GpuFan { pci, shared: Arc::new(Shared { state: Mutex::default(), cv: Condvar::new() }), worker: None }
    }

    /// PCI address, e.g. 0000:03:00.0.
    pub fn address(&self) -> String {
        self.pci.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    fn post(&mut self, want: Want) {
        if self.worker.is_none() {
            let (pci, shared) = (self.pci.clone(), self.shared.clone());
            self.worker = Some(std::thread::spawn(move || work(&pci, &shared)));
        }
        let mut st = self.shared.lock();
        if st.want != Some(want) {
            st.want = Some(want);
            self.shared.cv.notify_all();
        }
    }

    /// Run the fan at `pct` percent. Returns at once; an error is the last
    /// failed write (the worker keeps retrying).
    pub fn set(&mut self, pct: u8) -> io::Result<()> {
        self.post(Want::Duty(pct));
        match &self.shared.lock().error {
            Some(e) => Err(io::Error::other(e.clone())),
            None => Ok(()),
        }
    }

    /// Back to the firmware's stock curve. Waits until the card has it.
    pub fn release(&mut self) -> io::Result<()> {
        if self.worker.is_none() && self.shared.lock().want.is_none() {
            return Ok(()); // never driven: the card has its stock curve
        }
        self.post(Want::Stock);
        let st = self.shared.wait_until(RELEASE_WAIT, Shared::settled);
        if Shared::settled(&st) {
            return Ok(());
        }
        Err(io::Error::other(st.error.clone().unwrap_or_else(|| "timed out restoring the stock curve".into())))
    }

    /// The card may have lost its table (resume from suspend): send it again.
    pub fn resend(&self) {
        let mut st = self.shared.lock();
        // Stock is what the card falls back to anyway: no need to wake it.
        if matches!(st.want, Some(Want::Duty(_))) {
            st.resend = true;
            self.shared.cv.notify_all();
        }
    }

    /// Wait until the newest request is on the card. For tests.
    pub fn settle(&self, timeout: Duration) -> bool {
        Shared::settled(&self.shared.wait_until(timeout, Shared::settled))
    }
}

impl Drop for GpuFan {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.cv.notify_all();
        if let Some(t) = self.worker.take() {
            let _ = t.join();
        }
    }
}

fn awake(pci: &Path) -> bool {
    // A runtime-suspended card reads all-ones; the table survives D3hot,
    // so a duty just waits until the card is back.
    read_text(&pci.join("power/runtime_status")).is_none_or(|s| s == "active")
}

fn table(want: Want) -> Vec<(u8, u8)> {
    match want {
        Want::Duty(pct) => fixed(pct),
        Want::Stock => Vec::new(),
    }
}

fn commit(mmio: &mut Option<Mmio>, pci: &Path, points: &[(u8, u8)]) -> Result<(), String> {
    if mmio.is_none() {
        *mmio = Some(Mmio::open(&pci.join("resource0")).map_err(|e| format!("map BAR0: {e}"))?);
    }
    let r = mmio.as_ref().expect("mapped above").commit(points);
    if r.is_err() {
        *mmio = None; // remap next time, in case the device went away
    }
    r
}

/// Worker: apply the newest request, refresh it every `REFRESH`, retry
/// failures every `RETRY`.
fn work(pci: &Path, sh: &Shared) {
    let mut mmio = None;
    let mut st = sh.lock();
    loop {
        if st.stop {
            return;
        }
        let Some(want) = st.want else {
            st = sh.cv.wait(st).unwrap_or_else(|e| e.into_inner());
            continue;
        };
        let wait = match st.applied {
            Some((a, at)) if a == want && !st.resend => match want {
                Want::Duty(_) => REFRESH.saturating_sub(at.elapsed()),
                Want::Stock => Duration::MAX, // the firmware default needs no refresh
            },
            _ => Duration::ZERO,
        };
        if !wait.is_zero() {
            st = if wait == Duration::MAX {
                sh.cv.wait(st).unwrap_or_else(|e| e.into_inner())
            } else {
                sh.cv.wait_timeout(st, wait).unwrap_or_else(|e| e.into_inner()).0
            };
            continue;
        }
        (st.busy, st.resend) = (true, false);
        drop(st);
        let result = match want {
            // Restoring stock wakes the card, so it is never left on our table.
            Want::Stock => Card { dir: pci.to_path_buf() }
                .awake(|| commit(&mut mmio, pci, &[]))
                .unwrap_or_else(|e| Err(format!("runtime PM: {e}"))),
            // Suspended: not a failure; try again once it is back.
            Want::Duty(_) if !awake(pci) => Err(String::new()),
            Want::Duty(_) => commit(&mut mmio, pci, &table(want)),
        };
        st = sh.lock();
        st.busy = false;
        let ok = result.is_ok();
        match result {
            Ok(()) => (st.applied, st.error) = (Some((want, Instant::now())), None),
            Err(e) => {
                st.applied = None;
                st.error = (!e.is_empty()).then_some(e);
            }
        }
        sh.cv.notify_all();
        if !ok && !st.stop && st.want == Some(want) {
            st = sh.cv.wait_timeout(st, RETRY).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// Percent for a 0..255 duty.
pub fn pct_for_pwm(pwm: i64) -> u8 {
    ((pwm.clamp(0, 255) * 100 + 127) / 255) as u8
}

/// Restore the stock curve on every DG2 card (systemd ExecStopPost), waking
/// a runtime-suspended card to do it.
pub fn restore_all(pci_root: &Path) {
    for pci in discover(pci_root) {
        let mut mmio = None;
        let r = Card { dir: pci.clone() }
            .awake(|| commit(&mut mmio, &pci, &[]))
            .unwrap_or_else(|e| Err(format!("runtime PM: {e}")));
        if let Err(e) = r {
            eprintln!("warning: GPU fan {}: {e}", pci.display());
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

    #[test]
    fn flat_table_keeps_the_hot_ramp() {
        assert_eq!(table(Want::Duty(42)), vec![(0, 42), (85, 42), (95, 100), (96, 100)]);
        assert!(table(Want::Stock).is_empty());
    }
}
