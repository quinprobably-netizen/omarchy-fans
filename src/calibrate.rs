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
use crate::fan::Calibration;
use std::collections::BTreeMap;
use std::thread;
use std::time::{Duration, Instant};

const STEPS_DOWN: [i64; 17] = [255, 230, 205, 180, 160, 140, 120, 105, 90, 80, 70, 60, 50, 40, 30, 20, 0];

pub fn start(d: &Daemon, ids: Option<Vec<String>>) -> Result<(), String> {
    let idxs: Vec<usize> = {
        let mut core = d.lock();
        if core.calibration.is_some() {
            return Err("calibration already running".into());
        }
        let idxs: Vec<usize> = (0..core.fans.len())
            .filter(|&i| {
                let f = &core.fans[i];
                f.cfg.enabled && ids.as_ref().is_none_or(|ids| ids.iter().any(|id| *id == f.id || *id == f.pwm))
            })
            .collect();
        if idxs.is_empty() {
            return Err("no fans to calibrate".into());
        }
        core.calibration = Some(CalProgress {
            step: "starting".into(),
            progress: 0.0,
            fans: idxs.iter().map(|&i| core.fans[i].id.clone()).collect(),
        });
        core.message.clear();
        idxs
    };
    let d = d.clone();
    thread::spawn(move || run(&d, &idxs));
    Ok(())
}

#[derive(Default)]
struct Measured {
    table: BTreeMap<i64, i64>,
    stop_pwm: Option<i64>,
    start_pwm: Option<i64>,
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
            r.table.insert(pwm, rpm);
            if rpm == 0 && pwm > 0 && r.stop_pwm.is_none() {
                r.stop_pwm = Some(pwm);
            }
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

fn run(d: &Daemon, idxs: &[usize]) {
    let mut results: Vec<Measured> = idxs.iter().map(|_| Measured::default()).collect();
    let outcome = measure(d, idxs, &mut results);
    let mut core = d.lock();
    match &outcome {
        Ok(()) => {
            for (&i, r) in idxs.iter().zip(&results) {
                core.fans[i].cfg.calibration = Some(summarise(r.table.clone(), r.stop_pwm, r.start_pwm));
            }
            core.message = "Calibration complete".into();
        }
        Err(e) => {
            eprintln!("warning: calibration aborted: {e}");
            core.message = format!("Calibration failed: {e}");
        }
    }
    let curve_mode = core.state.mode == "curve";
    for &i in idxs {
        core.fans[i].percent = None;
        core.fans[i].written_pwm = None;
        if curve_mode && core.fans[i].connected() {
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
    fn table_is_forced_monotonic() {
        let cal = summarise([(0, 0), (100, 900), (150, 850), (255, 1500)].into(), Some(0), None);
        assert_eq!(cal.table[2], [150.0, 900.0]);
    }
}
