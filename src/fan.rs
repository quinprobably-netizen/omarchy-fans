//! PWM fan channels: persistent config, calibration data, runtime state.

use crate::curve::{interp, Point};
use crate::gpufan::{GpuFan, GPU_FLOOR_PWM};
use crate::util::clamp;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// PWM floor used until a fan has been calibrated.
pub const DEFAULT_FLOOR_PWM: i64 = 70;

/// Measured response of one fan, produced by calibration.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Calibration {
    /// Unix seconds.
    pub at: u64,
    pub max_rpm: i64,
    pub connected: bool,
    /// `[pwm, rpm]`, ascending PWM, RPM forced monotonic.
    pub table: Vec<Point>,
    /// Highest PWM at which the fan stopped while ramping down.
    pub stop_pwm: Option<i64>,
    /// Duty that reliably starts the fan from rest (with margin).
    pub start_pwm: Option<i64>,
    /// Lowest duty used in normal operation (stop point plus margin).
    pub floor_pwm: Option<i64>,
    pub min_rpm: Option<f64>,
    /// RPM actually follows PWM. False when the driver's writes have no
    /// effect (e.g. Gigabyte boards where mainline it87 cannot take control).
    #[serde(default = "yes")]
    pub responsive: bool,
}

fn yes() -> bool {
    true
}

impl Default for Calibration {
    fn default() -> Self {
        Calibration {
            at: 0,
            max_rpm: 0,
            connected: false,
            table: Vec::new(),
            stop_pwm: None,
            start_pwm: None,
            floor_pwm: None,
            min_rpm: None,
            responsive: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct FanConfig {
    pub label: String,
    pub role: String,
    pub enabled: bool,
    /// Let the fan stop entirely when its curve asks for 0%.
    pub allow_stop: bool,
    pub calibration: Option<Calibration>,
}

impl Default for FanConfig {
    fn default() -> Self {
        FanConfig { label: String::new(), role: "case".into(), enabled: true, allow_stop: false, calibration: None }
    }
}

pub struct Fan {
    pub id: String,
    pub chip: String,
    pub pwm: String,
    pub rpm_attr: Option<String>,
    pub cfg: FanConfig,
    /// `pwmN_enable` before we took over; restored on release.
    pub original_enable: Option<i64>,
    /// Commanded percent after ramp limiting; None until first control tick.
    pub percent: Option<f64>,
    pub target: f64,
    pub written_pwm: Option<i64>,
    pub rpm: Option<i64>,
    pub kick_until: f64,
    pub stall_since: Option<f64>,
    pub stalled: bool,
    /// Tach has reported RPM > 0 since startup (tells empty headers apart).
    pub seen_spinning: bool,
    pub reason: String,
    /// Set for a GPU fan driven through PCODE instead of hwmon PWM.
    pub gpu: Option<GpuFan>,
}

impl Fan {
    pub fn new(chip: String, pwm: String, rpm_attr: Option<String>, cfg: FanConfig) -> Self {
        Fan {
            id: format!("{chip}/{pwm}"),
            chip,
            pwm,
            rpm_attr,
            cfg,
            original_enable: None,
            percent: None,
            target: 0.0,
            written_pwm: None,
            rpm: None,
            kick_until: 0.0,
            stall_since: None,
            stalled: false,
            seen_spinning: false,
            reason: String::new(),
            gpu: None,
        }
    }

    /// The fan(s) of an Arc A-series card. `chip` is its hwmon chip (for
    /// RPM), if found.
    pub fn new_gpu(gpu: GpuFan, chip: Option<String>, cfg: FanConfig) -> Self {
        let has_tach = chip.is_some();
        let mut f = Fan::new(chip.unwrap_or_else(|| format!("xe-{}", gpu.address())), "gpu".into(), has_tach.then(|| "fan1".into()), cfg);
        f.gpu = Some(gpu);
        f
    }

    pub fn is_gpu(&self) -> bool {
        self.gpu.is_some()
    }

    pub fn connected(&self) -> bool {
        self.cfg.calibration.as_ref().is_none_or(|c| c.connected)
    }

    /// Calibration found that PWM writes do not change this fan's speed.
    pub fn responsive(&self) -> bool {
        self.cfg.calibration.as_ref().is_none_or(|c| c.responsive)
    }

    /// Present and actually steerable: the only fans the control loop drives.
    pub fn controllable(&self) -> bool {
        self.connected() && self.responsive()
    }

    pub fn calibrated(&self) -> bool {
        self.cfg.calibration.as_ref().is_some_and(|c| !c.table.is_empty())
    }

    pub fn floor_pwm(&self) -> i64 {
        let default = if self.is_gpu() { GPU_FLOOR_PWM } else { DEFAULT_FLOOR_PWM };
        self.cfg.calibration.as_ref().and_then(|c| c.floor_pwm).unwrap_or(default)
    }

    pub fn start_pwm(&self) -> i64 {
        self.cfg
            .calibration
            .as_ref()
            .and_then(|c| c.start_pwm)
            .unwrap_or_else(|| if self.is_gpu() { self.floor_pwm() } else { self.floor_pwm().max(100) })
    }

    /// Map 0-100% onto PWM. Calibrated fans are linear in RPM between their
    /// floor speed and full speed; uncalibrated fans are linear in duty.
    pub fn pwm_for(&self, percent: f64) -> i64 {
        let percent = clamp(percent, 0.0, 100.0);
        let floor = self.floor_pwm() as f64;
        if let Some(cal) = self.cfg.calibration.as_ref().filter(|c| !c.table.is_empty()) {
            let mut pts: Vec<Point> = vec![[floor, interp(&cal.table, floor)]];
            pts.extend(cal.table.iter().filter(|p| p[0] > floor));
            let (lo, hi) = (pts[0][1], pts[pts.len() - 1][1]);
            if pts.len() >= 2 && hi > lo {
                let want = lo + (hi - lo) * percent / 100.0;
                for w in pts.windows(2) {
                    let ([p0, r0], [p1, r1]) = (w[0], w[1]);
                    if want <= r0 {
                        return p0.round() as i64;
                    }
                    if want <= r1 {
                        return (p0 + (p1 - p0) * (want - r0) / (r1 - r0)).round() as i64;
                    }
                }
                return 255;
            }
        }
        (floor + (255.0 - floor) * percent / 100.0).round() as i64
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "kind": if self.is_gpu() { "gpu" } else { "hwmon" },
            "label": self.cfg.label,
            "role": self.cfg.role,
            "enabled": self.cfg.enabled,
            "connected": self.connected(),
            "responsive": self.responsive(),
            "calibrated": self.calibrated(),
            "allow_stop": self.cfg.allow_stop,
            "rpm": self.rpm,
            "max_rpm": self.cfg.calibration.as_ref().map(|c| c.max_rpm),
            "pwm": self.written_pwm,
            "percent": self.percent.map(|p| (p * 10.0).round() / 10.0),
            "target": (self.target * 10.0).round() / 10.0,
            "stalled": self.stalled,
            "seen_spinning": self.seen_spinning,
            "reason": self.reason,
        })
    }
}
