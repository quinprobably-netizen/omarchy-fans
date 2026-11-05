//! Built-in fan profiles.
//!
//! Curves map temperature to percent, where 0% is the slowest speed a fan
//! reliably holds (measured by calibration) and 100% is full speed. Once a
//! fan is calibrated the percent is linear in RPM, not PWM duty, so "50%"
//! really is half-way between the fan's quietest and loudest.
//!
//! Tuned for a Ryzen 8000G APU (Tctl, Tjmax 95 °C) plus a discrete GPU that
//! dumps its heat into the case: case fans follow whichever of CPU or GPU
//! demands more airflow.

use crate::curve::Curve;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// role -> sensor id -> curve. A fan's demand is the max over its role's curves.
pub type CurveSet = BTreeMap<String, BTreeMap<String, Curve>>;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Profile {
    pub label: String,
    /// Max speed increase, percent per second.
    pub ramp_up: f64,
    /// Max speed decrease, percent per second. Slow ramp-down is what keeps
    /// fans from audibly hunting after a burst of load.
    pub ramp_down: f64,
    /// Degrees a sensor must fall before fans slow down.
    pub hysteresis: f64,
    pub curves: CurveSet,
}

pub const PROFILE_ORDER: [&str; 5] = ["silent", "balanced", "performance", "full", "custom"];
pub const ROLES: [&str; 3] = ["cpu", "case", "gpu"];

type Spec<'a> = &'a [(&'a str, &'a [(&'a str, &'a [[f64; 2]])])];

fn make(label: &str, ramp_up: f64, ramp_down: f64, hysteresis: f64, spec: Spec) -> Profile {
    let curves = spec
        .iter()
        .map(|(role, sensors)| {
            let inner = sensors.iter().map(|(s, pts)| (s.to_string(), pts.to_vec())).collect();
            (role.to_string(), inner)
        })
        .collect();
    Profile { label: label.into(), ramp_up, ramp_down, hysteresis, curves }
}

pub fn builtin(name: &str) -> Option<Profile> {
    Some(match name {
        "silent" => make("Silent", 12.0, 2.0, 4.0, &[
            ("cpu", &[("cpu", &[[45., 0.], [60., 12.], [70., 30.], [80., 60.], [90., 100.]])]),
            ("case", &[
                ("cpu", &[[50., 0.], [70., 15.], [82., 45.], [90., 100.]]),
                ("gpu", &[[65., 0.], [80., 30.], [90., 65.], [96., 100.]]),
            ]),
            ("gpu", &[
                ("gpu", &[[60., 0.], [70., 15.], [80., 40.], [90., 75.], [96., 100.]]),
                ("vram", &[[85., 0.], [95., 50.], [102., 100.]]),
            ]),
        ]),
        "balanced" => make("Balanced", 20.0, 3.0, 3.0, &[
            ("cpu", &[("cpu", &[[40., 10.], [55., 25.], [65., 40.], [75., 65.], [85., 100.]])]),
            ("case", &[
                ("cpu", &[[45., 10.], [65., 30.], [80., 60.], [88., 100.]]),
                ("gpu", &[[60., 10.], [75., 35.], [85., 65.], [95., 100.]]),
            ]),
            ("gpu", &[
                ("gpu", &[[55., 0.], [65., 20.], [75., 45.], [85., 75.], [93., 100.]]),
                ("vram", &[[80., 0.], [92., 50.], [100., 100.]]),
            ]),
        ]),
        "performance" => make("Performance", 35.0, 5.0, 2.0, &[
            ("cpu", &[("cpu", &[[35., 30.], [50., 45.], [60., 60.], [70., 85.], [80., 100.]])]),
            ("case", &[
                ("cpu", &[[40., 30.], [60., 50.], [75., 80.], [85., 100.]]),
                ("gpu", &[[50., 30.], [70., 55.], [80., 80.], [90., 100.]]),
            ]),
            ("gpu", &[
                ("gpu", &[[45., 15.], [60., 40.], [70., 65.], [80., 90.], [88., 100.]]),
                ("vram", &[[75., 20.], [90., 70.], [98., 100.]]),
            ]),
        ]),
        "full" => make("Full", 100.0, 100.0, 0.0, &[
            ("cpu", &[("cpu", &[[0., 100.]])]),
            ("case", &[("cpu", &[[0., 100.]])]),
            ("gpu", &[("gpu", &[[0., 100.]])]),
        ]),
        _ => return None,
    })
}

pub fn default_custom() -> Profile {
    let mut p = builtin("balanced").unwrap();
    p.label = "Custom".into();
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::curve::{interp, validate};
    use crate::sensor::{critical_for, SENSOR_IDS};

    #[test]
    fn builtin_curves_are_valid_and_reach_full_speed_before_critical() {
        for name in &PROFILE_ORDER[..4] {
            let prof = builtin(name).unwrap();
            for (role, curves) in &prof.curves {
                assert!(ROLES.contains(&role.as_str()));
                for (sensor, pts) in curves {
                    assert!(SENSOR_IDS.contains(&sensor.as_str()), "{name} {sensor}");
                    assert_eq!(&validate(pts).unwrap(), pts, "{name} {role} {sensor}");
                    assert_eq!(interp(pts, critical_for(sensor)), 100.0, "{name} {role} {sensor}");
                }
            }
        }
    }
}
