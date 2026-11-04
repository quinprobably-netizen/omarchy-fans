//! Temperature sensors: smoothing, hysteresis and failure tracking.

use crate::hwmon::Hwmon;
use serde_json::{json, Value};

/// Logical sensors, in display order, with candidate `(chip, label)` sources.
/// The first candidate present on the machine wins.
pub const SENSOR_IDS: [&str; 4] = ["cpu", "gpu", "vram", "nvme"];

pub fn candidates(id: &str) -> &'static [(&'static str, Option<&'static str>)] {
    match id {
        "cpu" => &[("k10temp", Some("Tctl")), ("k10temp", None), ("coretemp", Some("Package id 0")), ("zenpower", Some("Tdie"))],
        "gpu" => &[("xe", Some("pkg")), ("amdgpu", Some("junction")), ("amdgpu", Some("edge")), ("nouveau", None)],
        "vram" => &[("xe", Some("vram")), ("amdgpu", Some("mem"))],
        "nvme" => &[("nvme", Some("Composite"))],
        _ => &[],
    }
}

pub fn label_for(id: &str) -> &'static str {
    match id {
        "cpu" => "CPU",
        "gpu" => "GPU",
        "vram" => "VRAM",
        "nvme" => "NVMe",
        _ => "Sensor",
    }
}

/// Smoothing time constant in seconds. Ryzen Tctl jumps 10+ °C for
/// sub-second bursts; smoothing keeps fans from chasing those spikes.
fn tau_for(id: &str) -> f64 {
    match id {
        "cpu" => 4.0,
        "gpu" => 3.0,
        "vram" => 5.0,
        "nvme" => 8.0,
        _ => 3.0,
    }
}

/// At or above this raw reading every fan goes to 100% immediately.
pub fn critical_for(id: &str) -> f64 {
    match id {
        "cpu" => 90.0,
        "gpu" => 97.0,
        "vram" => 102.0,
        "nvme" => 75.0,
        _ => 95.0,
    }
}

pub struct Sensor {
    pub id: String,
    pub chip: String,
    pub attr: String,
    pub label: String,
    pub tau: f64,
    pub critical: f64,
    pub raw: Option<f64>,
    pub smooth: Option<f64>,
    pub held: Option<f64>,
    pub ok: bool,
    failures: u32,
}

impl Sensor {
    pub fn new(id: &str, chip: String, attr: String, label: String) -> Self {
        Sensor {
            id: id.into(),
            chip,
            attr,
            label,
            tau: tau_for(id),
            critical: critical_for(id),
            raw: None,
            smooth: None,
            held: None,
            ok: false,
            failures: 0,
        }
    }

    pub fn update(&mut self, hw: &Hwmon, dt: f64, hysteresis: f64) {
        let value = hw.read(&self.chip, &format!("{}_input", self.attr));
        // Drivers sometimes report absurd values while initialising.
        let Some(v) = value.filter(|v| (-20_000..=150_000).contains(v)) else {
            self.failures += 1;
            // Ride out a couple of transient failures before failsafe.
            self.ok = self.failures < 3 && self.smooth.is_some();
            return;
        };
        self.failures = 0;
        self.ok = true;
        let raw = v as f64 / 1000.0;
        self.raw = Some(raw);
        let smooth = match self.smooth {
            None => raw,
            Some(s) => {
                let alpha = if self.tau > 0.0 { 1.0 - (-dt / self.tau).exp() } else { 1.0 };
                s + alpha * (raw - s)
            }
        };
        self.smooth = Some(smooth);
        // Hysteresis hold: follow rising temps at once, but only let the held
        // value fall once the smoothed temp is `hysteresis` below it.
        let held = self.held.unwrap_or(smooth);
        self.held = Some(if smooth > held {
            smooth
        } else if held - smooth > hysteresis {
            smooth + hysteresis
        } else {
            held
        });
    }

    pub fn is_critical(&self) -> bool {
        self.raw.is_some_and(|r| r >= self.critical)
    }

    pub fn to_json(&self) -> Value {
        let r = |v: Option<f64>| v.map(|v| (v * 10.0).round() / 10.0);
        json!({
            "id": self.id,
            "label": self.label,
            "source": format!("{}/{}", self.chip, self.attr),
            "temp": r(self.raw),
            "smooth": r(self.smooth),
            "held": r(self.held),
            "critical": self.critical,
            "ok": self.ok,
        })
    }
}
