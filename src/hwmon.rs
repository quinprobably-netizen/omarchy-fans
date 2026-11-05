//! Access to /sys/class/hwmon with stable chip names.
//!
//! `hwmonN` numbering changes between boots, so chips are keyed by their
//! `name` attribute, disambiguated as `name#2`, `name#3` (in device-path
//! order) when several chips share a name.

use crate::util::{read_int, read_text, write_int};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Chips whose PWM outputs belong to a GPU running its own fan logic.
const EXCLUDED_PWM_CHIPS: [&str; 6] = ["amdgpu", "nouveau", "nvidia", "radeon", "xe", "i915"];

pub struct Hwmon {
    root: PathBuf,
    chips: BTreeMap<String, PathBuf>,
    devices: BTreeMap<String, PathBuf>,
}

/// A controllable PWM output: chip key, `pwmN`, and matching `fanN` tach if any.
pub struct PwmChannel {
    pub chip: String,
    pub pwm: String,
    pub fan: Option<String>,
}

fn base_name(chip: &str) -> &str {
    chip.split('#').next().unwrap_or(chip)
}

fn numbered(names: &[String], prefix: &str, suffix: &str) -> Vec<(u32, String)> {
    let mut out: Vec<(u32, String)> = names
        .iter()
        .filter_map(|n| {
            let mid = n.strip_prefix(prefix)?.strip_suffix(suffix)?;
            Some((mid.parse().ok()?, format!("{prefix}{mid}")))
        })
        .collect();
    out.sort();
    out
}

fn list(path: &Path) -> Vec<String> {
    fs::read_dir(path)
        .map(|rd| rd.filter_map(|e| e.ok()?.file_name().into_string().ok()).collect())
        .unwrap_or_default()
}

impl Hwmon {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let mut hw = Hwmon { root: root.into(), chips: BTreeMap::new(), devices: BTreeMap::new() };
        hw.rescan();
        hw
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn rescan(&mut self) {
        let mut found: Vec<(String, PathBuf, PathBuf)> = list(&self.root)
            .into_iter()
            .filter_map(|entry| {
                let path = self.root.join(entry);
                let name = read_text(&path.join("name")).filter(|n| !n.is_empty())?;
                let dev = fs::canonicalize(path.join("device")).unwrap_or_default();
                Some((name, dev, path))
            })
            .collect();
        found.sort();
        // A chip seen before keeps its key, so a rescan while running (a
        // driver loaded late) never moves fans or sensors onto another chip.
        let previous: BTreeMap<(PathBuf, PathBuf), String> = self
            .chips
            .iter()
            .filter_map(|(k, p)| Some(((p.clone(), self.devices.get(k)?.clone()), k.clone())))
            .collect();
        self.chips.clear();
        self.devices.clear();
        let mut fresh = Vec::new();
        for (name, dev, path) in found {
            match previous.get(&(path.clone(), dev.clone())) {
                Some(key) if base_name(key) == name => {
                    self.devices.insert(key.clone(), dev);
                    self.chips.insert(key.clone(), path);
                }
                _ => fresh.push((name, dev, path)),
            }
        }
        for (name, dev, path) in fresh {
            let mut key = name.clone();
            let mut n = 2;
            while self.chips.contains_key(&key) {
                key = format!("{name}#{n}");
                n += 1;
            }
            self.devices.insert(key.clone(), dev);
            self.chips.insert(key, path);
        }
    }

    /// Chip key for the hwmon belonging to a device (e.g. a GPU's PCI dir);
    /// falls back to the only chip with that name.
    pub fn chip_for_device(&self, dev: &Path, name: &str) -> Option<String> {
        let dev = fs::canonicalize(dev).unwrap_or_else(|_| dev.to_path_buf());
        self.devices
            .iter()
            .find(|(_, d)| **d == dev)
            .map(|(k, _)| k.clone())
            .or_else(|| {
                let same: Vec<&String> = self.chips.keys().filter(|k| base_name(k) == name).collect();
                (same.len() == 1).then(|| same[0].clone())
            })
    }

    pub fn attr(&self, chip: &str, attr: &str) -> Option<PathBuf> {
        self.chips.get(chip).map(|p| p.join(attr))
    }

    pub fn read(&self, chip: &str, attr: &str) -> Option<i64> {
        read_int(&self.attr(chip, attr)?)
    }

    pub fn write(&self, chip: &str, attr: &str, value: i64) -> io::Result<()> {
        let path = self
            .attr(chip, attr)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("no chip {chip}")))?;
        write_int(&path, value)
    }

    /// Find `(chip, "tempN")` on a chip named `chip_name`, matching `label`
    /// if given, else the first temperature input.
    pub fn find_temp(&self, chip_name: &str, label: Option<&str>) -> Option<(String, String)> {
        for (chip, path) in &self.chips {
            if base_name(chip) != chip_name {
                continue;
            }
            for (_, t) in numbered(&list(path), "temp", "_input") {
                match label {
                    None => return Some((chip.clone(), t)),
                    Some(l) if read_text(&path.join(format!("{t}_label"))).as_deref() == Some(l) => {
                        return Some((chip.clone(), t));
                    }
                    _ => {}
                }
            }
        }
        None
    }

    pub fn pwm_channels(&self) -> Vec<PwmChannel> {
        let mut out = Vec::new();
        for (chip, path) in &self.chips {
            if EXCLUDED_PWM_CHIPS.contains(&base_name(chip)) {
                continue;
            }
            let names = list(path);
            for (n, pwm) in numbered(&names, "pwm", "") {
                if !names.contains(&format!("{pwm}_enable")) {
                    continue;
                }
                let fan = format!("fan{n}");
                let has_tach = names.contains(&format!("{fan}_input"));
                out.push(PwmChannel { chip: chip.clone(), pwm, fan: has_tach.then_some(fan) });
            }
        }
        out
    }
}
