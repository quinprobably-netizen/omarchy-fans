//! Fan calibration: measure every fan's RPM/PWM response.
//!
//! All selected fans are measured at once (about two minutes):
//!   1. full speed            -> max RPM; 0 RPM means an empty header
//!   2. step PWM down to 0    -> RPM table and the duty where it stops
//!   3. step up from rest     -> the duty that reliably starts it
//!
//! The result gives each fan a floor (stop point plus margin, so normal
//! operation never stalls it), a kick duty for starting from rest, and the
//! table that makes curve percentages linear in RPM.
//!
//! The control loop keeps reading sensors meanwhile; if anything gets hot
//! the run aborts and fans return to their curves.

use crate::curve::{interp, Point};
use crate::daemon::{CalProgress, Daemon};
use crate::fan::{Calibration, Fan};
use std::collections::BTreeMap;
use std::thread;
use std::time::{Duration, Instant};

const STEPS_DOWN: [i64; 17] = [255, 230, 205, 180, 160, 140, 120, 105, 90, 80, 70, 60, 50, 40, 30, 20, 0];

pub fn start(d: &Daemon, ids: Option<Vec<String>>) -> Result<(), String> {
    // GPU fans slew too slowly (~50 rpm/s) for the sweep and need no table.
    let idxs = begin(d, ids.as_deref(), "calibrate", |f| !f.is_gpu())?;
    let d = d.clone();
    thread::spawn(move || run(&d, &idxs));
    Ok(())
}

/// Re-test fans that calibration found ignoring PWM (a 3-pin fan on a
/// header in PWM mode, or a driver that cannot take control). A short probe
/// (full duty, then zero) checks whether the fan follows PWM now, e.g. after
/// its header was switched to Voltage mode in the BIOS. Fans that do get a
/// full calibration and go onto their curves; the rest stay with the BIOS.
/// The daemon runs this at every start.
pub fn start_retest(d: &Daemon, ids: Option<Vec<String>>) -> Result<(), String> {
    let idxs = begin(d, ids.as_deref(), "retest", |f| f.connected() && !f.responsive())
        .map_err(|e| if e == "no fans to retest" { "no fans that ignore PWM to re-test".to_string() } else { e })?;
    let d = d.clone();
    thread::spawn(move || run_retest(&d, &idxs));
    Ok(())
}

/// Claim the selected, enabled fans for a calibration-style run. Only
/// these fans leave the control loop; the others stay on their curves.
fn begin(d: &Daemon, ids: Option<&[String]>, kind: &str, pick: impl Fn(&Fan) -> bool) -> Result<Vec<usize>, String> {
    let mut core = d.lock();
    if core.calibration.is_some() {
        return Err("calibration already running".into());
    }
    let idxs: Vec<usize> = (0..core.fans.len())
        .filter(|&i| {
            let f = &core.fans[i];
            f.cfg.enabled && pick(f) && ids.is_none_or(|ids| ids.iter().any(|id| *id == f.id || *id == f.pwm))
        })
        .collect();
    if idxs.is_empty() {
        return Err(format!("no fans to {kind}"));
    }
    core.calibration = Some(CalProgress {
        kind: kind.into(),
        step: "starting".into(),
        progress: 0.0,
        fans: idxs.iter().map(|&i| core.fans[i].id.clone()).collect(),
    });
    core.message.clear();
    Ok(idxs)
}

#[derive(Default)]
struct Measured {
    table: BTreeMap<i64, i64>,
    stop_pwm: Option<i64>,
    start_pwm: Option<i64>,
}

impl Measured {
    /// Record one step of the downward sweep. The first duty that reads
    /// 0 RPM is the stop point, including duty 0 itself: a fan that only
    /// stops at 0 still needs a margin above the lowest duty that held it.
    fn record(&mut self, pwm: i64, rpm: i64) {
        self.table.insert(pwm, rpm);
        if rpm == 0 && self.stop_pwm.is_none() && self.table.range(pwm + 1..).any(|(_, &r)| r > 0) {
            self.stop_pwm = Some(pwm);
        }
    }
}

fn set_all(d: &Daemon, idxs: &[usize], pwm: i64, step: &str) -> Result<(), String> {
    let mut core = d.lock();
    if let Some(c) = core.calibration.as_mut() {
        c.step = step.into();
    }
    for &i in idxs {
        core.set_pwm(i, pwm).map_err(|e| format!("{}: {e}", core.fans[i].id))?;
    }
    Ok(())
}

/// Sleep (scaled for tests), publishing progress and watching temperatures.
fn wait(d: &Daemon, seconds: f64, progress: f64) -> Result<(), String> {
    let scale = d.lock().time_scale;
    let end = Instant::now() + Duration::from_secs_f64(seconds * scale);
    loop {
        if d.stopping() {
            return Err("daemon stopping".into());
        }
        {
            let mut core = d.lock();
            if let Some(c) = core.calibration.as_mut() {
                c.progress = progress;
            }
            if let Some(s) = core.sensors.iter().find(|s| s.raw.is_some_and(|r| r >= (s.critical - 10.0).min(85.0))) {
                return Err(format!("{} too hot ({:.0}°C)", s.label, s.raw.unwrap_or(0.0)));
            }
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        thread::sleep(left.min(Duration::from_millis(250)));
    }
}

/// Read tachometers. A failed read is "unknown", not "stopped", so retry.
fn rpms(d: &Daemon, idxs: &[usize]) -> Vec<i64> {
    idxs.iter()
        .map(|&i| {
            for _ in 0..3 {
                let (value, scale) = {
                    let core = d.lock();
                    let f = &core.fans[i];
                    let v = match &f.rpm_attr {
                        Some(a) => core.hw.read(&f.chip, &format!("{a}_input")),
                        None => Some(0),
                    };
                    (v, core.time_scale)
                };
                if let Some(v) = value {
                    return v;
                }
                thread::sleep(Duration::from_secs_f64(0.05 * scale));
            }
            0
        })
        .collect()
}

fn measure(d: &Daemon, idxs: &[usize], results: &mut [Measured]) -> Result<(), String> {
    let total = (1 + STEPS_DOWN.len() + 24) as f64;
    let mut n = 0.0;
    set_all(d, idxs, 255, "full speed")?;
    wait(d, 6.0, n / total)?;
    n += 1.0;
    for pwm in STEPS_DOWN {
        set_all(d, idxs, pwm, &format!("measuring PWM {pwm}"))?;
        wait(d, if pwm < 100 { 4.0 } else { 3.0 }, n / total)?;
        n += 1.0;
        for (r, rpm) in results.iter_mut().zip(rpms(d, idxs)) {
            r.record(pwm, rpm);
        }
    }
    // Start test: fans that stopped at 0 and spin at 255 are at rest now.
    let mut pending: Vec<usize> = (0..idxs.len())
        .filter(|&k| results[k].table.get(&0) == Some(&0) && results[k].table.get(&255).is_some_and(|&r| r > 0))
        .collect();
    wait(d, 4.0, n / total)?;
    for pwm in (20..=255).step_by(10) {
        if pending.is_empty() {
            break;
        }
        let sel: Vec<usize> = pending.iter().map(|&k| idxs[k]).collect();
        set_all(d, &sel, pwm, "finding start duty")?;
        wait(d, 2.5, n / total)?;
        n += 1.0;
        let now = rpms(d, &sel);
        let mut still = Vec::new();
        for (&k, rpm) in pending.iter().zip(now) {
            if rpm > 0 {
                results[k].start_pwm = Some(pwm);
            } else {
                still.push(k);
            }
        }
        pending = still;
    }
    Ok(())
}

/// Speed must change by this share between full duty and zero for a fan
/// to count as following PWM (PWM-deaf fans drift by a few percent).
const FOLLOWS_PWM: f64 = 0.15;

pub fn follows_pwm(full_rpm: i64, zero_rpm: i64) -> bool {
    full_rpm > 0 && (full_rpm - zero_rpm) as f64 >= FOLLOWS_PWM * full_rpm as f64
}

/// Full duty, then zero: which of these fans follow PWM?
fn probe(d: &Daemon, idxs: &[usize]) -> Result<Vec<usize>, String> {
    set_all(d, idxs, 255, "re-test: full speed")?;
    wait(d, 6.0, 0.05)?;
    let full = rpms(d, idxs);
    set_all(d, idxs, 0, "re-test: zero duty")?;
    wait(d, 8.0, 0.1)?;
    let zero = rpms(d, idxs);
    Ok(idxs.iter().zip(full.iter().zip(&zero)).filter(|(_, (f, z))| follows_pwm(**f, **z)).map(|(&i, _)| i).collect())
}

fn run(d: &Daemon, idxs: &[usize]) {
    let mut results: Vec<Measured> = idxs.iter().map(|_| Measured::default()).collect();
    let outcome = measure(d, idxs, &mut results).map(|()| idxs.iter().copied().zip(results).collect());
    finish(d, idxs, outcome, "Calibration", |_| "Calibration complete.".into());
}

fn run_retest(d: &Daemon, idxs: &[usize]) {
    let outcome = probe(d, idxs).and_then(|live| {
        let mut results: Vec<Measured> = live.iter().map(|_| Measured::default()).collect();
        if !live.is_empty() {
            measure(d, &live, &mut results)?;
        }
        Ok(live.into_iter().zip(results).collect::<Vec<_>>())
    });
    finish(d, idxs, outcome, "Re-test", |names| match names.len() {
        0 => String::new(),
        1 => format!("{} follows PWM now: calibrated and on its curve.", names[0]),
        _ => format!("{} follow PWM now: calibrated and on their curves.", names.join(", ")),
    });
}

/// Store measurements, report, and hand the fans back to the control loop
/// (or the BIOS, for fans we cannot steer).
fn finish(
    d: &Daemon,
    idxs: &[usize],
    outcome: Result<Vec<(usize, Measured)>, String>,
    what: &str,
    done: impl FnOnce(&[String]) -> String,
) {
    let mut core = d.lock();
    match outcome {
        Ok(measured) => {
            let names: Vec<String> = measured.iter().map(|(i, _)| core.fans[*i].cfg.label.clone()).collect();
            for (i, r) in measured {
                core.fans[i].cfg.calibration = Some(summarise(r.table, r.stop_pwm, r.start_pwm));
            }
            let msgs: Vec<String> = [done(&names)].into_iter().chain(crate::daemon::unresponsive_message(&core.fans)).filter(|m| !m.is_empty()).collect();
            core.message = msgs.join(" ");
        }
        Err(e) => {
            eprintln!("warning: {} aborted: {e}", what.to_lowercase());
            core.message = format!("{what} failed: {e}");
        }
    }
    let curve_mode = core.state.mode == "curve";
    for &i in idxs {
        core.fans[i].percent = None;
        core.fans[i].written_pwm = None;
        if curve_mode && core.fans[i].controllable() {
            // Full speed until the next tick puts it back on its curve.
            let _ = core.set_pwm(i, 255);
        } else {
            core.release(i);
        }
    }
    core.calibration = None;
    core.save_state();
}

/// Turn raw measurements into a fan's calibration record.
pub fn summarise(table: BTreeMap<i64, i64>, stop_pwm: Option<i64>, start_pwm: Option<i64>) -> Calibration {
    let max_rpm = table.values().copied().max().unwrap_or(0);
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut cal = Calibration { at, max_rpm, connected: max_rpm > 0, ..Default::default() };
    if max_rpm == 0 {
        return cal;
    }
    // Monotonic RPM so the %->PWM inversion is well defined.
    let mut best = 0;
    let mono: Vec<Point> = table
        .iter()
        .map(|(&p, &rpm)| {
            best = best.max(rpm);
            [p as f64, best as f64]
        })
        .collect();
    // Does speed follow duty at all? Compare full duty with the slowest
    // spinning duty at or below half. Real fans differ by well over 2x;
    // under 15% means the writes are not reaching the fan.
    let slowest = table.iter().filter(|(p, rpm)| **p <= 128 && **rpm > 0).map(|(_, r)| *r).min();
    let full = table.get(&255).copied().unwrap_or(max_rpm);
    if stop_pwm.is_none() && slowest.is_some_and(|s| ((full - s) as f64) < 0.15 * full as f64) {
        cal.responsive = false;
        cal.table = table.iter().map(|(&p, &r)| [p as f64, r as f64]).collect();
        return cal;
    }
    let lowest_spin = table.iter().find(|(_, rpm)| **rpm > 0).map(|(p, _)| *p).unwrap_or(0);
    // Margin above the lowest duty that held rotation, so wear, dust and
    // temperature drift never stall the fan in normal operation.
    let floor = if stop_pwm.is_some() { (lowest_spin + 15).min(255) } else { lowest_spin };
    let start = match start_pwm {
        Some(s) => (s + 10).max(floor).min(255),
        None => floor,
    };
    cal.min_rpm = Some(interp(&mono, floor as f64));
    cal.table = mono;
    cal.stop_pwm = stop_pwm;
    cal.start_pwm = Some(start);
    cal.floor_pwm = Some(floor);
    cal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarise_empty_header() {
        let cal = summarise([(255, 0), (0, 0)].into(), None, None);
        assert!(!cal.connected);
        assert!(cal.table.is_empty());
    }

    #[test]
    fn summarise_floor_and_start() {
        let table = [(0, 0), (30, 0), (40, 500), (60, 800), (255, 1800)].into();
        let cal = summarise(table, Some(30), Some(70));
        assert_eq!(cal.floor_pwm, Some(55));
        assert_eq!(cal.start_pwm, Some(80));
        assert_eq!(cal.max_rpm, 1800);
        assert_eq!(cal.min_rpm, Some(725.0));
    }

    #[test]
    fn summarise_never_stopping_fan() {
        let cal = summarise([(0, 300), (128, 900), (255, 1200)].into(), None, None);
        assert_eq!(cal.floor_pwm, Some(0));
        assert_eq!(cal.start_pwm, Some(0));
    }

    #[test]
    fn flat_response_is_unresponsive() {
        // Measured on the B650M D3HP with mainline it87: duty does nothing.
        let flat = [(0, 1240), (20, 1245), (50, 1132), (90, 1163), (105, 1245), (160, 1130), (205, 1169), (255, 1243)];
        let cal = summarise(flat.into(), None, None);
        assert!(cal.connected && !cal.responsive);
        let flat2 = [(0, 1149), (50, 1147), (128, 1151), (255, 1147)];
        assert!(!summarise(flat2.into(), None, None).responsive);
        // A never-stopping fan with a real (if limited) range stays responsive.
        let real = [(0, 300), (128, 900), (255, 1200)];
        assert!(summarise(real.into(), None, None).responsive);
    }

    #[test]
    fn sweep_records_stop_point_including_zero() {
        let sweep = |pts: &[(i64, i64)]| {
            let mut m = Measured::default();
            pts.iter().for_each(|&(p, r)| m.record(p, r));
            m.stop_pwm
        };
        assert_eq!(sweep(&[(255, 1436), (40, 333), (20, 202), (0, 0)]), Some(0));
        assert_eq!(sweep(&[(255, 1800), (40, 500), (30, 0), (20, 0), (0, 0)]), Some(30));
        assert_eq!(sweep(&[(255, 1200), (0, 300)]), None, "never stops");
        assert_eq!(sweep(&[(255, 0), (0, 0)]), None, "empty header has no stop point");
    }

    #[test]
    fn fan_that_only_stops_at_zero_still_gets_a_margin() {
        // System fan 2 on the B650M D3HP (frankcrawford it87): spins at duty 20.
        let t = [(0, 0), (20, 202), (30, 254), (40, 333), (90, 753), (255, 1436)];
        let cal = summarise(t.into(), Some(0), Some(40));
        assert_eq!(cal.stop_pwm, Some(0));
        assert_eq!(cal.floor_pwm, Some(35));
    }

    #[test]
    fn retest_probe_threshold() {
        // System fan 1 on the B650M D3HP (3-pin, header in PWM mode).
        assert!(!follows_pwm(1159, 1123));
        assert!(follows_pwm(1300, 0), "voltage-mode fan stops at duty 0");
        assert!(follows_pwm(1300, 600), "voltage-mode fan with a minimum voltage");
        assert!(!follows_pwm(0, 0), "empty header");
    }

    #[test]
    fn table_is_forced_monotonic() {
        let cal = summarise([(0, 0), (100, 900), (150, 850), (255, 1500)].into(), Some(0), None);
        assert_eq!(cal.table[2], [150.0, 900.0]);
    }
}
