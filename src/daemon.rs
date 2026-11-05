//! The fan-control daemon: control loop, command handling, socket server.
//!
//! Per tick:
//!
//! ```text
//! raw temp -> EMA smoothing -> hysteresis hold -> curve (max over sensors)
//!          -> ramp limiter -> calibrated %->PWM (RPM-linear)
//!          -> spin-up kick when starting from rest -> sysfs write
//! ```
//!
//! Safety: a sensor at its critical temperature forces every fan to 100%;
//! a configured sensor that stops reading forces its fans to 100%; on exit
//! (and from `omarchy-fand restore`, run by systemd `ExecStopPost`) every
//! PWM channel goes back to the firmware's automatic mode.

use crate::calibrate;
use crate::curve::{self, interp, Point};
use crate::fan::{Fan, FanConfig};
use crate::gpufan::{self, GpuFan};
use crate::hwmon::Hwmon;
use crate::profiles::{self, Profile, PROFILE_ORDER, ROLES};
use crate::sensor::{self, Sensor, SENSOR_IDS};
use crate::util::{atomic_write, clamp, group_gid, unix_time};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub const KICK_SECONDS: f64 = 2.0;
/// Status line for fans whose speed ignores PWM, naming them.
pub fn unresponsive_message(fans: &[Fan]) -> Option<String> {
    let names: Vec<&str> = fans.iter().filter(|f| f.connected() && !f.responsive()).map(|f| f.cfg.label.as_str()).collect();
    if names.is_empty() {
        return None;
    }
    let (list, verb) = if names.len() == 1 { (names[0].to_string(), "ignores") } else { (names.join(", "), "ignore") };
    Some(format!(
        "{list} {verb} PWM and stay{s} on BIOS control: a 3-pin fan needs its header in Voltage mode in the BIOS \
         (Gigabyte with mainline it87: also install the DKMS driver, see README). Re-tested at every daemon start.",
        s = if names.len() == 1 { "s" } else { "" }
    ))
}

pub const STALL_SECONDS: f64 = 6.0;
/// While a sensor the active curves use is missing (driver not loaded yet),
/// look for it this often.
pub const SENSOR_RESCAN_SECONDS: f64 = 10.0;
/// Socket clients served at once; more get a "busy" reply. Each one is a
/// thread that may wait up to 5 s for its request line.
pub const MAX_CLIENTS: usize = 16;

/// Set from the SIGTERM/SIGINT handler in the daemon binary.
pub static SIGNAL_STOP: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Debug)]
pub struct Paths {
    pub sysfs: PathBuf,
    pub run_dir: PathBuf,
    pub state_dir: PathBuf,
    /// Group allowed to use the control socket.
    pub group: String,
    /// PCI driver directory scanned for Arc A-series cards
    /// (/sys/bus/pci/drivers/xe).
    pub pci_xe: PathBuf,
}

impl Paths {
    pub fn state_file(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }
    pub fn status_file(&self) -> PathBuf {
        self.run_dir.join("status.json")
    }
    pub fn socket(&self) -> PathBuf {
        self.run_dir.join("control.sock")
    }
    pub fn originals_file(&self) -> PathBuf {
        self.run_dir.join("original.json")
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SensorOverride {
    pub chip: String,
    pub input: String,
    pub label: Option<String>,
}

/// Everything persisted in /var/lib/omarchy-fans/state.json.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct State {
    pub version: u32,
    pub profile: String,
    /// "curve" (we drive the fans) or "auto" (firmware drives them).
    pub mode: String,
    pub fans: BTreeMap<String, FanConfig>,
    pub custom: Profile,
    pub sensors: BTreeMap<String, SensorOverride>,
}

impl Default for State {
    fn default() -> Self {
        State {
            version: 1,
            profile: "balanced".into(),
            mode: "curve".into(),
            fans: BTreeMap::new(),
            custom: profiles::default_custom(),
            sensors: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CalProgress {
    /// "calibrate" or "retest".
    pub kind: String,
    pub step: String,
    pub progress: f64,
    pub fans: Vec<String>,
}

pub struct Core {
    pub hw: Hwmon,
    pub paths: Paths,
    pub state: State,
    pub sensors: Vec<Sensor>,
    pub fans: Vec<Fan>,
    pub calibration: Option<CalProgress>,
    pub message: String,
    pub interval: f64,
    /// Tests shrink calibration waits with this.
    pub time_scale: f64,
    last_tick: Option<f64>,
    last_sensor_scan: f64,
    /// CLOCK_BOOTTIME - CLOCK_MONOTONIC at the last tick; it grows by the
    /// time spent suspended.
    sleep_offset: Option<f64>,
    /// Fans in failsafe for lack of a sensor, for the status message.
    alert: String,
}

/// What a fan's curves ask for.
pub struct Demand {
    pub percent: f64,
    /// Sensor that set the percent, or why the fan is in failsafe.
    pub reason: String,
    /// Failsafe: go to `percent` at once, no ramp.
    pub emergency: bool,
}

type CmdResult = Result<Value, String>;

impl Core {
    fn new(paths: Paths, interval: f64) -> Self {
        let state = Self::load_state(&paths);
        let mut core = Core {
            hw: Hwmon::new(&paths.sysfs),
            paths,
            state,
            sensors: Vec::new(),
            fans: Vec::new(),
            calibration: None,
            message: String::new(),
            interval,
            time_scale: 1.0,
            last_tick: None,
            last_sensor_scan: 0.0,
            sleep_offset: None,
            alert: String::new(),
        };
        core.setup_hardware();
        core
    }

    fn load_state(paths: &Paths) -> State {
        let mut state: State = fs::read(paths.state_file())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        state.custom.label = "Custom".into();
        // Custom profiles saved before GPU fan support get the default GPU curve.
        if !state.custom.curves.contains_key("gpu")
            && let Some(c) = profiles::default_custom().curves.get("gpu")
        {
            state.custom.curves.insert("gpu".into(), c.clone());
        }
        if !PROFILE_ORDER.contains(&state.profile.as_str()) {
            state.profile = "balanced".into();
        }
        if state.mode != "auto" {
            state.mode = "curve".into();
        }
        state
    }

    pub fn save_state(&mut self) {
        self.state.fans = self.fans.iter().map(|f| (f.id.clone(), f.cfg.clone())).collect();
        let res = fs::create_dir_all(&self.paths.state_dir).and_then(|_| {
            let data = serde_json::to_vec_pretty(&self.state).expect("state serialises");
            atomic_write(&self.paths.state_file(), &data, 0o644)
        });
        if let Err(e) = res {
            eprintln!("warning: could not save state: {e}");
        }
    }

    pub fn profile(&self, name: &str) -> Profile {
        if name == "custom" { self.state.custom.clone() } else { profiles::builtin(name).unwrap_or_else(profiles::default_custom) }
    }

    pub fn active_profile(&self) -> Profile {
        self.profile(&self.state.profile)
    }

    pub fn setup_hardware(&mut self) {
        self.hw.rescan();
        self.sensors.clear();
        self.add_missing_sensors();
        self.fans = self
            .hw
            .pwm_channels()
            .into_iter()
            .map(|ch| {
                let id = format!("{}/{}", ch.chip, ch.pwm);
                let cfg = self.state.fans.get(&id).cloned().unwrap_or_else(|| {
                    // pwm1 is the CPU header on practically every ITE/Nuvoton board.
                    let n: i64 = ch.pwm[3..].parse().unwrap_or(0);
                    FanConfig {
                        label: if n == 1 { "CPU fan".into() } else { format!("System fan {}", n - 1) },
                        role: if n == 1 { "cpu".into() } else { "case".into() },
                        ..FanConfig::default()
                    }
                });
                Fan::new(ch.chip, ch.pwm, ch.fan, cfg)
            })
            .collect();
        for pci in gpufan::discover(&self.paths.pci_xe) {
            let chip = self.hw.chip_for_device(&pci, "xe");
            let gpu = GpuFan::new(pci);
            let probe = Fan::new_gpu(GpuFan::new(gpu.pci.clone()), chip.clone(), FanConfig::default());
            let cfg = self.state.fans.get(&probe.id).cloned().unwrap_or_else(|| FanConfig {
                label: "GPU fan".into(),
                role: "gpu".into(),
                // Like the stock firmware curve: silent when the card is cool.
                allow_stop: true,
                ..FanConfig::default()
            });
            self.fans.push(Fan::new_gpu(gpu, chip, cfg));
        }
    }

    /// Find sensors not yet known. Existing ones keep their smoothing state.
    fn add_missing_sensors(&mut self) {
        for id in SENSOR_IDS {
            if self.sensor(id).is_some() {
                continue;
            }
            if let Some(o) = self.state.sensors.get(id)
                && self.hw.read(&o.chip, &format!("{}_input", o.input)).is_some()
            {
                let label = o.label.clone().unwrap_or_else(|| sensor::label_for(id).into());
                self.sensors.push(Sensor::new(id, o.chip.clone(), o.input.clone(), label));
                continue;
            }
            if let Some((chip, attr)) = sensor::candidates(id).iter().find_map(|(c, l)| self.hw.find_temp(c, *l)) {
                self.sensors.push(Sensor::new(id, chip, attr, sensor::label_for(id).into()));
            }
        }
        self.sensors.sort_by_key(|s| SENSOR_IDS.iter().position(|id| *id == s.id));
    }

    /// A sensor that the curves of a driven fan use is not present.
    fn curve_sensor_missing(&self, prof: &Profile) -> bool {
        self.fans
            .iter()
            .filter(|f| f.cfg.enabled && f.controllable())
            .filter_map(|f| prof.curves.get(&f.cfg.role).or_else(|| prof.curves.get("case")))
            .flat_map(|c| c.keys())
            .any(|sid| self.sensor(sid).is_none())
    }

    pub fn sensor(&self, id: &str) -> Option<&Sensor> {
        self.sensors.iter().find(|s| s.id == id)
    }

    // ---- PWM ownership --------------------------------------------------

    /// Write a PWM value, switching the channel to manual mode first.
    pub fn set_pwm(&mut self, idx: usize, value: i64) -> std::io::Result<()> {
        let value = value.clamp(0, 255);
        let hw = &self.hw;
        let fan = &mut self.fans[idx];
        if let Some(g) = fan.gpu.as_mut() {
            g.set(gpufan::pct_for_pwm(value))?;
            fan.written_pwm = Some(value);
            return Ok(());
        }
        let enable = format!("{}_enable", fan.pwm);
        // Re-check the mode even when the duty is unchanged: firmware can
        // take a channel back (resume from S3) without anything else changing.
        if fan.written_pwm == Some(value) && hw.read(&fan.chip, &enable) == Some(1) {
            return Ok(());
        }
        let mut new_original = false;
        if fan.original_enable.is_none() {
            // Never hand back "manual": that would leave the fan frozen.
            fan.original_enable = Some(match hw.read(&fan.chip, &enable) {
                Some(1) | None => 2,
                Some(m) => m,
            });
            new_original = true;
        }
        if hw.read(&fan.chip, &enable) != Some(1) {
            hw.write(&fan.chip, &enable, 1)?;
        }
        hw.write(&fan.chip, &fan.pwm, value)?;
        fan.written_pwm = Some(value);
        if new_original {
            self.save_originals();
        }
        Ok(())
    }

    pub fn release(&mut self, idx: usize) {
        let hw = &self.hw;
        let fan = &mut self.fans[idx];
        if let Some(g) = fan.gpu.as_mut() {
            // Not guarded by written_pwm: a duty the worker is still trying
            // to send must be cancelled too. A never-driven card is a no-op.
            if let Err(e) = g.release() {
                eprintln!("error: could not release {}: {e}", fan.id);
            }
            fan.written_pwm = None;
            fan.percent = None;
            return;
        }
        let enable = format!("{}_enable", fan.pwm);
        let mode = fan.original_enable.unwrap_or(2);
        if hw.read(&fan.chip, &enable) == Some(1) && hw.write(&fan.chip, &enable, mode).is_err() {
            // Firmware mode rejected: full speed is the only safe fallback.
            if hw.write(&fan.chip, &fan.pwm, 255).is_err() {
                eprintln!("error: could not release {}", fan.id);
            }
        }
        fan.written_pwm = None;
        fan.percent = None;
    }

    pub fn release_all(&mut self) {
        for i in 0..self.fans.len() {
            self.release(i);
        }
    }

    fn save_originals(&self) {
        let map: BTreeMap<&str, i64> =
            self.fans.iter().filter_map(|f| Some((f.id.as_str(), f.original_enable?))).collect();
        let _ = fs::create_dir_all(&self.paths.run_dir).and_then(|_| {
            atomic_write(&self.paths.originals_file(), &serde_json::to_vec(&map).unwrap(), 0o644)
        });
    }

    // ---- control loop ---------------------------------------------------

    /// The machine was suspended: the Super I/O and the GPU may have dropped
    /// what we wrote, so write everything again.
    pub fn on_resume(&mut self) {
        for fan in &mut self.fans {
            match &fan.gpu {
                Some(g) => g.resend(),
                None => fan.written_pwm = None,
            }
        }
    }

    fn check_resume(&mut self) {
        let offset = sleep_offset();
        if let Some(prev) = self.sleep_offset.replace(offset)
            && offset - prev > 1.0
        {
            eprintln!("resumed after {:.0} s suspended: re-applying fan settings", offset - prev);
            self.on_resume();
        }
    }

    pub fn tick(&mut self, now: f64) {
        self.check_resume();
        let dt = self.last_tick.map_or(self.interval, |l| clamp(now - l, 0.05, 10.0));
        self.last_tick = Some(now);
        let prof = self.active_profile();
        if now - self.last_sensor_scan >= SENSOR_RESCAN_SECONDS && self.curve_sensor_missing(&prof) {
            self.last_sensor_scan = now;
            self.hw.rescan();
            let before = self.sensors.len();
            self.add_missing_sensors();
            if self.sensors.len() > before {
                eprintln!("found sensors: {}", self.sensors.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", "));
            }
        }
        for s in &mut self.sensors {
            s.update(&self.hw, dt, prof.hysteresis);
        }
        let critical: Option<String> = self.sensors.iter().find(|s| s.is_critical()).map(|s| s.id.clone());
        for fan in &mut self.fans {
            fan.rpm = fan.rpm_attr.as_ref().and_then(|a| self.hw.read(&fan.chip, &format!("{a}_input")));
            fan.seen_spinning |= fan.rpm.is_some_and(|r| r > 0);
        }
        if self.state.mode != "curve" {
            return;
        }
        // Fans being calibrated belong to the calibration thread; every
        // other fan stays on its curve meanwhile.
        let busy: Vec<String> = self.calibration.as_ref().map(|c| c.fans.clone()).unwrap_or_default();
        let mut alerts = Vec::new();
        for i in 0..self.fans.len() {
            if busy.contains(&self.fans[i].id) {
                continue;
            }
            if !self.fans[i].cfg.enabled || !self.fans[i].controllable() {
                let f = &mut self.fans[i];
                (f.stalled, f.stall_since, f.target) = (false, None, 0.0);
                f.reason = if !f.cfg.enabled {
                    "disabled".into()
                } else if !f.connected() {
                    "no fan".into()
                } else {
                    "not controllable: ignores PWM (BIOS)".into()
                };
                continue;
            }
            if let Err(e) = self.control_fan(i, &prof, critical.as_deref(), dt, now) {
                self.fans[i].reason = format!("write failed: {e}");
                eprintln!("warning: {}: {e}", self.fans[i].id);
            }
            let f = &self.fans[i];
            if f.reason.starts_with("no ") && f.reason.ends_with(" sensor") {
                alerts.push(format!("{}: {}", f.cfg.label, f.reason));
            }
        }
        self.alert = if alerts.is_empty() {
            String::new()
        } else {
            format!("Full speed, {} (driver not loaded?). Rechecked every {SENSOR_RESCAN_SECONDS:.0} s.", alerts.join(", "))
        };
    }

    /// Percent the profile demands for a fan, and which sensor drove it.
    ///
    /// A curve whose sensor this machine lacks (no NVMe, no GPU) is unused,
    /// but if none of the fan's curves has a sensor the fan has nothing to
    /// follow, e.g. k10temp not loaded yet: full speed until one appears.
    pub fn fan_target(&self, fan: &Fan, prof: &Profile) -> Demand {
        let curves = prof.curves.get(&fan.cfg.role).or_else(|| prof.curves.get("case"));
        let mut best = Demand { percent: 0.0, reason: String::new(), emergency: false };
        let mut found = false;
        for (sid, points) in curves.into_iter().flatten() {
            let Some(s) = self.sensor(sid) else { continue };
            found = true;
            if !s.ok {
                return Demand { percent: 100.0, reason: format!("{sid} sensor lost"), emergency: true };
            }
            let value = interp(points, s.held.unwrap_or(0.0));
            if value >= best.percent {
                (best.percent, best.reason) = (value, sid.clone());
            }
        }
        match curves.filter(|c| !found && !c.is_empty()) {
            Some(c) => {
                let ids: Vec<&str> = c.keys().map(String::as_str).collect();
                Demand { percent: 100.0, reason: format!("no {} sensor", ids.join("/")), emergency: true }
            }
            None => best,
        }
    }

    fn control_fan(&mut self, i: usize, prof: &Profile, critical: Option<&str>, dt: f64, now: f64) -> std::io::Result<()> {
        let Demand { percent: mut target, mut reason, mut emergency } = self.fan_target(&self.fans[i], prof);
        if let Some(c) = critical {
            (target, reason, emergency) = (100.0, format!("{c} critical"), true);
        }
        let fan = &mut self.fans[i];
        fan.target = target;
        fan.reason = reason;
        let percent = match fan.percent {
            None => target, // first tick: jump straight onto the curve
            Some(_) if emergency => 100.0,
            Some(p) if target > p => target.min(p + prof.ramp_up * dt),
            Some(p) => target.max(p - prof.ramp_down * dt),
        };
        fan.percent = Some(percent);

        let pwm = if fan.cfg.allow_stop && target <= 0.0 && percent <= 0.0 {
            0
        } else {
            let mut pwm = fan.pwm_for(percent);
            // Spin-up kick: a fan at rest may not start at its floor duty.
            let resting = fan.written_pwm == Some(0) || (fan.rpm_attr.is_some() && fan.rpm == Some(0));
            if resting && pwm < fan.start_pwm() && fan.kick_until <= now {
                fan.kick_until = now + KICK_SECONDS;
            }
            if fan.kick_until > now {
                pwm = pwm.max(fan.start_pwm());
            }
            pwm
        };

        // Stall detection: commanded to spin, tach says 0. Only for fans known
        // to have a motor, so empty headers on an uncalibrated board stay quiet.
        let has_motor = fan.seen_spinning || fan.calibrated();
        if has_motor && pwm >= fan.start_pwm() && fan.rpm == Some(0) {
            let since = *fan.stall_since.get_or_insert(now);
            fan.stalled = now - since >= STALL_SECONDS;
        } else {
            fan.stall_since = None;
            fan.stalled = false;
        }
        self.set_pwm(i, pwm)
    }

    // ---- commands -------------------------------------------------------

    pub fn status(&self) -> Value {
        let prof = self.active_profile();
        json!({
            "version": crate::VERSION,
            "time": unix_time(),
            "mode": self.state.mode,
            "profile": self.state.profile,
            "profiles": PROFILE_ORDER.iter().map(|p| json!({"id": p, "label": self.profile(p).label})).collect::<Vec<_>>(),
            "curves": prof.curves,
            "tuning": {"ramp_up": prof.ramp_up, "ramp_down": prof.ramp_down, "hysteresis": prof.hysteresis},
            "sensors": self.sensors.iter().map(Sensor::to_json).collect::<Vec<_>>(),
            "fans": self.fans.iter().map(Fan::to_json).collect::<Vec<_>>(),
            "calibration": self.calibration,
            "message": self.status_message(),
        })
    }

    /// Failsafe alert first, then the latest note.
    fn status_message(&self) -> String {
        [self.alert.as_str(), self.message.as_str()].into_iter().filter(|m| !m.is_empty()).collect::<Vec<_>>().join(" ")
    }

    pub fn write_status(&self) {
        let data = serde_json::to_vec(&self.status()).expect("status serialises");
        if let Err(e) = atomic_write(&self.paths.status_file(), &data, 0o644) {
            eprintln!("warning: status write failed: {e}");
        }
    }

    fn handle(&mut self, req: &Value) -> CmdResult {
        let s = |k: &str| req.get(k).and_then(Value::as_str);
        match s("cmd").unwrap_or("") {
            "status" => Ok(self.status()),
            "profile" => {
                let name = s("name").unwrap_or("");
                if !PROFILE_ORDER.contains(&name) {
                    return Err(format!("unknown profile {name:?}"));
                }
                self.state.profile = name.into();
                self.state.mode = "curve".into();
                self.save_state();
                Ok(json!({"profile": name}))
            }
            "mode" => {
                let mode = s("mode").unwrap_or("");
                if mode != "curve" && mode != "auto" {
                    return Err("mode is 'curve' or 'auto'".into());
                }
                self.state.mode = mode.into();
                if mode == "auto" {
                    self.release_all();
                }
                self.save_state();
                Ok(json!({"mode": mode}))
            }
            "curve" => {
                let (role, sensor) = (s("role").unwrap_or(""), s("sensor").unwrap_or(""));
                if !ROLES.contains(&role) {
                    return Err(format!("role must be one of {ROLES:?}"));
                }
                if !SENSOR_IDS.contains(&sensor) {
                    return Err(format!("unknown sensor {sensor:?}"));
                }
                let points: Vec<Point> = serde_json::from_value(req.get("points").cloned().unwrap_or_default())
                    .map_err(|_| "points must be [[temp, percent], ...]".to_string())?;
                let points = curve::validate(&points)?;
                if self.state.profile != "custom" && req.get("fork").and_then(Value::as_bool).unwrap_or(true) {
                    // Editing starts from whatever profile is active.
                    self.state.custom = self.active_profile();
                    self.state.custom.label = "Custom".into();
                }
                self.state.custom.curves.entry(role.into()).or_default().insert(sensor.into(), points);
                self.state.profile = "custom".into();
                self.state.mode = "curve".into();
                self.save_state();
                Ok(json!({"profile": "custom", "curves": self.state.custom.curves}))
            }
            "curve-remove" => {
                let (role, sensor) = (s("role").unwrap_or(""), s("sensor").unwrap_or(""));
                if let Some(curves) = self.state.custom.curves.get_mut(role)
                    && curves.len() > 1 && curves.remove(sensor).is_some() {
                        self.save_state();
                    }
                Ok(json!({"curves": self.state.custom.curves}))
            }
            "tuning" => {
                let num = |k: &str| req.get(k).and_then(Value::as_f64);
                let c = &mut self.state.custom;
                if let Some(v) = num("ramp_up") { c.ramp_up = clamp(v, 1.0, 100.0) }
                if let Some(v) = num("ramp_down") { c.ramp_down = clamp(v, 0.5, 100.0) }
                if let Some(v) = num("hysteresis") { c.hysteresis = clamp(v, 0.0, 10.0) }
                let out = json!({"ramp_up": c.ramp_up, "ramp_down": c.ramp_down, "hysteresis": c.hysteresis});
                self.save_state();
                Ok(out)
            }
            "fan" => {
                let id = s("id").unwrap_or("");
                let i = self
                    .fans
                    .iter()
                    .position(|f| f.id == id || f.pwm == id)
                    .ok_or_else(|| format!("unknown fan {id:?}"))?;
                if let Some(label) = s("label").filter(|l| !l.is_empty()) {
                    self.fans[i].cfg.label = label.chars().take(32).collect();
                }
                if let Some(role) = s("role") {
                    if !ROLES.contains(&role) {
                        return Err(format!("role must be one of {ROLES:?}"));
                    }
                    self.fans[i].cfg.role = role.into();
                }
                if let Some(en) = req.get("enabled").and_then(Value::as_bool) {
                    self.fans[i].cfg.enabled = en;
                    if !en {
                        self.release(i);
                    }
                }
                if let Some(stop) = req.get("allow_stop").and_then(Value::as_bool) {
                    self.fans[i].cfg.allow_stop = stop;
                }
                self.save_state();
                Ok(self.fans[i].to_json())
            }
            "rescan" => {
                if self.calibration.is_some() {
                    return Err("calibration running".into());
                }
                self.release_all();
                self.save_state();
                self.setup_hardware();
                Ok(json!({
                    "fans": self.fans.iter().map(|f| &f.id).collect::<Vec<_>>(),
                    "sensors": self.sensors.iter().map(|s| &s.id).collect::<Vec<_>>(),
                }))
            }
            other => Err(format!("unknown command {other:?}")),
        }
    }
}

/// Cheap, cloneable handle on the shared daemon state.
#[derive(Clone)]
pub struct Daemon {
    core: Arc<Mutex<Core>>,
    stop: Arc<AtomicBool>,
    epoch: Instant,
    clients: Arc<AtomicUsize>,
}

/// One socket client slot, given back when the client thread ends (even by
/// panic).
struct ClientSlot(Arc<AtomicUsize>);

impl Drop for ClientSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Daemon {
    pub fn new(paths: Paths, interval: f64) -> Self {
        Daemon {
            core: Arc::new(Mutex::new(Core::new(paths, interval))),
            stop: Arc::new(AtomicBool::new(false)),
            epoch: Instant::now(),
            clients: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Lock the core, recovering from a poisoned mutex: a panicked client
    /// thread must never stop the control loop from running.
    pub fn lock(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || SIGNAL_STOP.load(Ordering::Relaxed)
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Seconds since daemon start; the control loop's clock.
    pub fn now(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64()
    }

    pub fn tick(&self, now: f64) {
        self.lock().tick(now);
    }

    pub fn handle(&self, req: &Value) -> CmdResult {
        if req.get("cmd").and_then(Value::as_str) == Some("calibrate") {
            let ids: Option<Vec<String>> = req.get("fans").and_then(|v| serde_json::from_value(v.clone()).ok());
            calibrate::start(self, ids)?;
            return Ok(json!({"calibrating": true}));
        }
        if req.get("cmd").and_then(Value::as_str) == Some("retest") {
            let ids: Option<Vec<String>> = req.get("fans").and_then(|v| serde_json::from_value(v.clone()).ok());
            calibrate::start_retest(self, ids)?;
            return Ok(json!({"retesting": true}));
        }
        self.lock().handle(req)
    }

    pub fn serve(&self) -> std::io::Result<()> {
        let paths = self.lock().paths.clone();
        let sock = paths.socket();
        let _ = fs::remove_file(&sock);
        let listener = UnixListener::bind(&sock)?;
        match group_gid(&paths.group) {
            Some(gid) if std::os::unix::fs::chown(&sock, None, Some(gid)).is_ok() => {
                fs::set_permissions(&sock, fs::Permissions::from_mode(0o660))?
            }
            _ => fs::set_permissions(&sock, fs::Permissions::from_mode(0o600))?,
        }
        while !self.stopping() {
            // Wait with a timeout rather than block in accept(), so a stop
            // is noticed without another connection arriving.
            let mut pfd = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd for the duration of the call.
            if unsafe { libc::poll(&mut pfd, 1, 250) } <= 0 {
                continue; // timeout or EINTR
            }
            let Ok((mut conn, _)) = listener.accept() else { continue };
            if self.clients.fetch_add(1, Ordering::Relaxed) >= MAX_CLIENTS {
                self.clients.fetch_sub(1, Ordering::Relaxed);
                let _ = conn.set_write_timeout(Some(Duration::from_millis(200)));
                let _ = conn.write_all(b"{\"ok\":false,\"error\":\"daemon busy: too many clients\"}\n");
                continue;
            }
            let slot = ClientSlot(self.clients.clone());
            let d = self.clone();
            std::thread::spawn(move || {
                let _slot = slot;
                d.client(conn);
            });
        }
        Ok(())
    }

    fn client(&self, mut conn: UnixStream) {
        let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = conn.set_write_timeout(Some(Duration::from_secs(5)));
        let mut line = String::new();
        let read = conn.try_clone().and_then(|c| BufReader::new(c).take(65536).read_line(&mut line));
        let reply = match read {
            Ok(_) => match serde_json::from_str::<Value>(&line) {
                Ok(req) => match self.handle(&req) {
                    Ok(result) => json!({"ok": true, "result": result}),
                    Err(e) => json!({"ok": false, "error": e}),
                },
                Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
            },
            Err(e) => json!({"ok": false, "error": format!("read failed: {e}")}),
        };
        self.lock().write_status();
        let _ = conn.write_all(format!("{reply}\n").as_bytes());
    }

    /// A BIOS change (header to Voltage mode) or a driver swap only happens
    /// across a reboot: re-test PWM-deaf fans once per daemon start. Returns
    /// false while that has to wait for the CPU sensor (k10temp loading late).
    pub fn startup_retest(&self) -> bool {
        {
            let core = self.lock();
            if core.state.mode != "curve" || !core.fans.iter().any(|f| f.cfg.enabled && f.connected() && !f.responsive()) {
                return true;
            }
            if core.sensor("cpu").is_none() {
                return false;
            }
        }
        match calibrate::start_retest(self, None) {
            Ok(()) => eprintln!("re-testing fans that ignored PWM"),
            Err(e) => eprintln!("warning: re-test: {e}"),
        }
        true
    }

    pub fn run(&self) {
        {
            let mut core = self.lock();
            let run_dir = core.paths.run_dir.clone();
            let _ = fs::create_dir_all(&run_dir);
            let _ = fs::set_permissions(&run_dir, fs::Permissions::from_mode(0o755));
            if core.fans.is_empty() {
                core.message = "No controllable fans found (is the it87 module loaded?)".into();
            } else if let Some(msg) = unresponsive_message(&core.fans) {
                core.message = msg;
            } else if !core.fans.iter().any(Fan::calibrated) {
                core.message = "Fans not calibrated yet: run Calibrate for quieter, RPM-accurate curves".into();
            }
            eprintln!(
                "omarchy-fand {} running: profile={} mode={} fans=[{}] sensors=[{}]",
                crate::VERSION,
                core.state.profile,
                core.state.mode,
                core.fans.iter().map(|f| f.id.as_str()).collect::<Vec<_>>().join(", "),
                core.sensors.iter().map(|s| format!("{}={}/{}", s.id, s.chip, s.attr)).collect::<Vec<_>>().join(", "),
            );
        }
        // A BIOS change (header to Voltage mode) or a driver swap only
        // happens across a reboot: check PWM-deaf fans again on every start.
        let mut retest_pending = !self.startup_retest();
        let d = self.clone();
        std::thread::spawn(move || {
            if let Err(e) = d.serve() {
                eprintln!("error: control socket: {e}");
            }
        });
        while !self.stopping() {
            let start = self.now();
            {
                let mut core = self.lock();
                core.tick(start);
                core.write_status();
            }
            if retest_pending {
                retest_pending = !self.startup_retest();
            }
            let interval = self.lock().interval;
            // Sleep in short slices so SIGTERM is handled promptly.
            while !self.stopping() && self.now() - start < interval {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let mut core = self.lock();
        core.release_all();
        let _ = fs::remove_file(core.paths.socket());
        eprintln!("released fans to firmware control");
    }
}

/// Seconds the machine has spent suspended since boot.
fn sleep_offset() -> f64 {
    let secs = |clock| {
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: valid clock id and out-pointer.
        unsafe { libc::clock_gettime(clock, &mut ts) };
        ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
    };
    secs(libc::CLOCK_BOOTTIME) - secs(libc::CLOCK_MONOTONIC)
}

/// Hand every PWM channel back to firmware control. Safe to run any time;
/// systemd runs it after the daemon exits, however it exited.
pub fn restore(paths: &Paths) {
    gpufan::restore_all(&paths.pci_xe);
    let hw = Hwmon::new(&paths.sysfs);
    let originals: BTreeMap<String, i64> = fs::read(paths.originals_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    for ch in hw.pwm_channels() {
        let enable = format!("{}_enable", ch.pwm);
        if hw.read(&ch.chip, &enable) != Some(1) {
            continue;
        }
        let mode = originals.get(&format!("{}/{}", ch.chip, ch.pwm)).copied().unwrap_or(2);
        if hw.write(&ch.chip, &enable, mode).is_err() {
            let _ = hw.write(&ch.chip, &ch.pwm, 255);
        }
    }
}
