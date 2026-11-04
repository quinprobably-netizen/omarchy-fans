//! omarchy-fand against a fake /sys/class/hwmon with simulated fans.
//!
//! The fake board mirrors this machine: k10temp (Tctl), Intel xe (pkg/vram)
//! and an ITE it8689 with five PWM headers, three of them populated.

use omarchy_fans::daemon::{restore, Daemon, Paths};
use omarchy_fans::fan::DEFAULT_FLOOR_PWM;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Atomic like sysfs: readers never see a truncated attribute.
fn w(path: &Path, value: impl std::fmt::Display) {
    let tmp = path.with_file_name(format!(".{}.{:?}", path.file_name().unwrap().to_str().unwrap(), thread::current().id()));
    fs::write(&tmp, format!("{value}\n")).unwrap();
    fs::rename(&tmp, path).unwrap();
}

fn r(path: &Path) -> i64 {
    fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

/// (stop_pwm, start_pwm, max_rpm, never_stops)
const FANS: [(i64, i64, i64, bool); 3] = [(40, 70, 1800, false), (55, 90, 1200, false), (0, 0, 900, true)];

struct FakeHw {
    root: PathBuf,
    k10: PathBuf,
    xe: PathBuf,
    it: PathBuf,
    spinning: Arc<Mutex<[bool; 3]>>,
    stop: Arc<AtomicBool>,
    sim: Option<thread::JoinHandle<()>>,
}

fn rpm_for(spinning: &mut [bool; 3], i: usize, pwm: i64) -> i64 {
    let (stop, start, max_rpm, never) = FANS[i];
    let pwm = if never { pwm.max(40) } else { pwm }; // hardware minimum duty
    if !never {
        if spinning[i] && pwm < stop {
            spinning[i] = false;
        } else if !spinning[i] && pwm >= start {
            spinning[i] = true;
        }
        if !spinning[i] {
            return 0;
        }
    }
    // Typical fan: RPM rises steeply at low duty and flattens near the top.
    (max_rpm as f64 * (pwm as f64 / 255.0).powf(0.6)) as i64
}

impl FakeHw {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "omarchy-fans-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&root);
        let mk = |d: &str, name: &str, files: &[(&str, String)]| {
            let p = root.join(d);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("name"), format!("{name}\n")).unwrap();
            for (k, v) in files {
                fs::write(p.join(k), format!("{v}\n")).unwrap();
            }
            p
        };
        let s = |v: &str| v.to_string();
        let k10 = mk("hwmon0", "k10temp", &[("temp1_input", s("45000")), ("temp1_label", s("Tctl"))]);
        let xe = mk("hwmon1", "xe", &[
            ("temp2_input", s("60000")),
            ("temp2_label", s("pkg")),
            ("temp3_input", s("70000")),
            ("temp3_label", s("vram")),
            ("fan1_input", s("900")),
        ]);
        let mut files = Vec::new();
        for i in 1..=5 {
            files.push((format!("pwm{i}"), s("128")));
            files.push((format!("pwm{i}_enable"), s("2")));
            files.push((format!("fan{i}_input"), s("0")));
        }
        let files: Vec<(&str, String)> = files.iter().map(|(k, v)| (k.as_str(), v.clone())).collect();
        let it = mk("hwmon2", "it8689", &files);

        let spinning = Arc::new(Mutex::new([true; 3]));
        let stop = Arc::new(AtomicBool::new(false));
        let sim = {
            let (it, spinning, stop) = (it.clone(), spinning.clone(), stop.clone());
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    for i in 0..3 {
                        let n = i + 1;
                        let enable = fs::read_to_string(it.join(format!("pwm{n}_enable"))).ok();
                        let pwm = fs::read_to_string(it.join(format!("pwm{n}"))).ok();
                        let (Some(e), Some(p)) = (enable, pwm) else { continue };
                        let (Ok(e), Ok(p)) = (e.trim().parse::<i64>(), p.trim().parse::<i64>()) else { continue };
                        let p = if e == 1 { p } else { 255 };
                        let rpm = rpm_for(&mut spinning.lock().unwrap(), i, p);
                        w(&it.join(format!("fan{n}_input")), rpm);
                    }
                    // Header 4: a fan the driver cannot steer (as on Gigabyte
                    // boards under mainline it87). RPM wanders, ignores PWM.
                    let wobble = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() / 700 % 7) as i64;
                    w(&it.join("fan4_input"), 1140 + wobble * 15);
                    thread::sleep(Duration::from_millis(2));
                }
            })
        };
        FakeHw { root, k10, xe, it, spinning, stop, sim: Some(sim) }
    }

    fn cpu(&self, c: f64) {
        w(&self.k10.join("temp1_input"), (c * 1000.0) as i64);
    }
    fn gpu(&self, c: f64) {
        w(&self.xe.join("temp2_input"), (c * 1000.0) as i64);
    }
    fn pwm(&self, n: usize) -> i64 {
        r(&self.it.join(format!("pwm{n}")))
    }
    fn enable(&self, n: usize) -> i64 {
        r(&self.it.join(format!("pwm{n}_enable")))
    }
    fn rpm_at(&self, n: usize, pwm: i64) -> i64 {
        let mut s = [true; 3];
        let _ = &self.spinning;
        rpm_for(&mut s, n - 1, pwm)
    }
}

impl Drop for FakeHw {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.sim.take() {
            let _ = h.join();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Rig {
    hw: FakeHw,
    d: Daemon,
    paths: Paths,
}

impl Rig {
    fn new() -> Self {
        let hw = FakeHw::new();
        let paths = Paths {
            sysfs: hw.root.clone(),
            run_dir: hw.root.join("run"),
            state_dir: hw.root.join("state"),
            group: "nonexistent-group".into(),
        };
        fs::create_dir_all(&paths.run_dir).unwrap();
        let d = Daemon::new(paths.clone(), 1.0);
        d.lock().time_scale = 0.02;
        Rig { hw, d, paths }
    }

    /// `n` ticks of 1 s starting at `start`; returns the next start time.
    fn ticks(&self, n: usize, start: f64) -> f64 {
        for k in 0..n {
            self.d.tick(start + k as f64);
        }
        start + n as f64
    }

    fn fan<T>(&self, i: usize, f: impl FnOnce(&omarchy_fans::fan::Fan) -> T) -> T {
        f(&self.d.lock().fans[i])
    }

    fn cmd(&self, req: Value) -> Result<Value, String> {
        self.d.handle(&req)
    }

    fn calibrate(&self) {
        let stop = Arc::new(AtomicBool::new(false));
        let ticker = {
            let (d, stop) = (self.d.clone(), stop.clone());
            let t0 = Instant::now();
            thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    d.tick(10_000.0 + t0.elapsed().as_secs_f64());
                    thread::sleep(Duration::from_millis(5));
                }
            })
        };
        self.cmd(json!({"cmd": "calibrate"})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.d.lock().calibration.is_some() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        stop.store(true, Ordering::Relaxed);
        ticker.join().unwrap();
        assert!(self.d.lock().calibration.is_none(), "calibration did not finish");
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.d.request_stop();
    }
}

// ---- discovery -------------------------------------------------------------

#[test]
fn finds_sensors_and_fans() {
    let rig = Rig::new();
    let core = rig.d.lock();
    let ids: Vec<&str> = core.sensors.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, ["cpu", "gpu", "vram"]);
    assert_eq!(core.sensor("cpu").unwrap().chip, "k10temp");
    assert_eq!(core.sensor("gpu").unwrap().attr, "temp2");
    let fans: Vec<&str> = core.fans.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(fans, (1..=5).map(|i| format!("it8689/pwm{i}")).collect::<Vec<_>>());
    assert_eq!(core.fans[0].cfg.role, "cpu");
    assert_eq!(core.fans[1].cfg.role, "case");
}

// ---- control ----------------------------------------------------------------

#[test]
fn takes_manual_control_and_follows_curve() {
    let rig = Rig::new();
    rig.ticks(3, 1000.0);
    assert_eq!(rig.hw.enable(1), 1);
    // balanced cpu curve at 45 °C: between (40,10) and (55,25) -> 15%
    assert!((rig.fan(0, |f| f.target) - 15.0).abs() < 0.1);
    assert!(rig.hw.pwm(1) >= DEFAULT_FLOOR_PWM);
}

#[test]
fn case_fans_take_max_of_cpu_and_gpu() {
    let rig = Rig::new();
    rig.hw.gpu(85.0);
    rig.ticks(30, 1000.0);
    assert_eq!(rig.fan(1, |f| f.reason.clone()), "gpu");
    assert!((rig.fan(1, |f| f.target) - 65.0).abs() < 0.5);
}

#[test]
fn ramps_up_fast_and_down_slowly() {
    let rig = Rig::new();
    rig.hw.cpu(85.0);
    let t = rig.ticks(40, 1000.0);
    assert!(rig.fan(0, |f| f.percent.unwrap()) > 95.0);
    rig.hw.cpu(40.0);
    let t = rig.ticks(5, t);
    assert!(rig.fan(0, |f| f.percent.unwrap()) > 80.0, "ramp-down is only ~3%/s");
    rig.ticks(120, t);
    assert!(rig.fan(0, |f| f.percent.unwrap()) < 20.0);
}

#[test]
fn smoothing_ignores_short_spike() {
    let rig = Rig::new();
    let t = rig.ticks(5, 1000.0);
    let before = rig.fan(0, |f| f.target);
    rig.hw.cpu(80.0);
    let t = rig.ticks(1, t);
    rig.hw.cpu(45.0);
    rig.ticks(1, t);
    assert!(rig.fan(0, |f| f.target) - before < 15.0, "a 1 s Tctl spike must not slam the fan");
}

#[test]
fn hysteresis_holds_on_small_drop() {
    let rig = Rig::new();
    rig.hw.cpu(70.0);
    let t = rig.ticks(60, 1000.0);
    let held = rig.d.lock().sensor("cpu").unwrap().held.unwrap();
    rig.hw.cpu(68.0); // 2 °C drop < 3 °C hysteresis
    rig.ticks(60, t);
    assert!((rig.d.lock().sensor("cpu").unwrap().held.unwrap() - held).abs() < 0.2);
}

#[test]
fn critical_forces_full_speed_immediately() {
    let rig = Rig::new();
    let t = rig.ticks(5, 1000.0);
    rig.hw.gpu(99.0);
    rig.ticks(1, t);
    for n in 1..=3 {
        assert_eq!(rig.hw.pwm(n), 255, "fan {n}");
    }
}

#[test]
fn lost_sensor_fails_safe() {
    let rig = Rig::new();
    let t = rig.ticks(5, 1000.0);
    fs::remove_file(rig.hw.k10.join("temp1_input")).unwrap();
    rig.ticks(4, t);
    assert_eq!(rig.hw.pwm(1), 255);
    assert!(rig.fan(0, |f| f.reason.contains("lost")));
}

#[test]
fn auto_mode_releases_to_firmware() {
    let rig = Rig::new();
    let t = rig.ticks(3, 1000.0);
    rig.cmd(json!({"cmd": "mode", "mode": "auto"})).unwrap();
    assert_eq!(rig.hw.enable(1), 2);
    let t = rig.ticks(3, t);
    assert_eq!(rig.hw.enable(1), 2, "auto mode must not re-take control");
    rig.cmd(json!({"cmd": "mode", "mode": "curve"})).unwrap();
    rig.ticks(1, t);
    assert_eq!(rig.hw.enable(1), 1);
}

#[test]
fn release_and_restore_hand_back_original_mode() {
    let rig = Rig::new();
    let t = rig.ticks(3, 1000.0);
    rig.d.lock().release_all();
    for n in 1..=3 {
        assert_eq!(rig.hw.enable(n), 2);
    }
    rig.ticks(3, t);
    assert_eq!(rig.hw.enable(1), 1);
    restore(&rig.paths); // as systemd ExecStopPost would, after a crash
    for n in 1..=5 {
        assert_eq!(rig.hw.enable(n), 2);
    }
}

// ---- commands -------------------------------------------------------------

#[test]
fn profile_switch_persists() {
    let rig = Rig::new();
    rig.cmd(json!({"cmd": "profile", "name": "silent"})).unwrap();
    let saved: Value = serde_json::from_slice(&fs::read(rig.paths.state_file()).unwrap()).unwrap();
    assert_eq!(saved["profile"], "silent");
    assert!(rig.cmd(json!({"cmd": "profile", "name": "turbo"})).is_err());
}

#[test]
fn curve_edit_forks_active_profile_into_custom() {
    let rig = Rig::new();
    rig.cmd(json!({"cmd": "profile", "name": "performance"})).unwrap();
    rig.cmd(json!({"cmd": "curve", "role": "cpu", "sensor": "cpu", "points": [[30, 20], [90, 100]]})).unwrap();
    let core = rig.d.lock();
    assert_eq!(core.state.profile, "custom");
    assert_eq!(core.state.custom.curves["cpu"]["cpu"], vec![[30.0, 20.0], [90.0, 100.0]]);
    let perf = omarchy_fans::profiles::builtin("performance").unwrap();
    assert_eq!(core.state.custom.curves["case"], perf.curves["case"]);
    drop(core);
    assert!(rig.cmd(json!({"cmd": "curve", "role": "cpu", "sensor": "cpu", "points": [[30, 50], [90, 10]]})).is_err());
}

#[test]
fn fan_settings_persist() {
    let rig = Rig::new();
    rig.cmd(json!({"cmd": "fan", "id": "pwm2", "label": "Front intake", "role": "case"})).unwrap();
    let d2 = Daemon::new(rig.paths.clone(), 1.0);
    assert_eq!(d2.lock().fans[1].cfg.label, "Front intake");
}

#[test]
fn socket_roundtrip() {
    let rig = Rig::new();
    let d = rig.d.clone();
    thread::spawn(move || d.serve());
    let sock = rig.paths.socket();
    for _ in 0..200 {
        if sock.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let mut s = UnixStream::connect(&sock).unwrap();
    s.write_all(b"{\"cmd\":\"profile\",\"name\":\"silent\"}\n").unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(rig.d.lock().state.profile, "silent");
    let status: Value = serde_json::from_slice(&fs::read(rig.paths.status_file()).unwrap()).unwrap();
    assert_eq!(status["profile"], "silent");
    assert_eq!(status["fans"].as_array().unwrap().len(), 5);
}

// ---- calibration ----------------------------------------------------------

#[test]
fn calibration_measures_fans() {
    let rig = Rig::new();
    rig.calibrate();
    let core = rig.d.lock();
    let f = &core.fans;
    assert!(f[0].connected() && f[1].connected() && f[2].connected());
    assert!(!f[4].connected(), "empty header must be detected");
    assert!(f[3].connected() && !f[3].responsive(), "PWM-deaf fan must be flagged");
    assert!(f[0].responsive() && f[1].responsive() && f[2].responsive());
    assert!(core.message.contains("ignore PWM"), "{}", core.message);
    let c1 = f[0].cfg.calibration.as_ref().unwrap();
    // fan 1 stops below 40 and restarts at 70: floor above stop, kick above start
    assert_eq!(c1.stop_pwm, Some(30));
    assert!(f[0].floor_pwm() > 40);
    assert!(f[0].start_pwm() >= 70);
    assert!(f[1].start_pwm() >= 90);
    assert_eq!(f[2].cfg.calibration.as_ref().unwrap().stop_pwm, None, "fan 3 never stops");
    assert!((c1.max_rpm - 1800).abs() <= 5);
    let saved: Value = serde_json::from_slice(&fs::read(rig.paths.state_file()).unwrap()).unwrap();
    assert!(!saved["fans"]["it8689/pwm1"]["calibration"]["table"].as_array().unwrap().is_empty());
}

#[test]
fn calibrated_mapping_is_rpm_linear() {
    let rig = Rig::new();
    rig.calibrate();
    let core = rig.d.lock();
    let f1 = &core.fans[0];
    let c = f1.cfg.calibration.as_ref().unwrap();
    let (lo, hi) = (c.min_rpm.unwrap(), c.max_rpm as f64);
    let mid = rig.hw.rpm_at(1, f1.pwm_for(50.0)) as f64;
    assert!((mid - (lo + hi) / 2.0).abs() < (hi - lo) * 0.06, "mid {mid} lo {lo} hi {hi}");
    assert_eq!(f1.pwm_for(100.0), 255);
    assert_eq!(f1.pwm_for(0.0), f1.floor_pwm());
}

#[test]
fn disconnected_fans_left_to_firmware() {
    let rig = Rig::new();
    rig.calibrate();
    rig.ticks(3, 50_000.0);
    assert_eq!(rig.hw.enable(5), 2, "empty header back to firmware");
    assert_eq!(rig.hw.enable(4), 2, "uncontrollable fan back to firmware");
    assert_eq!(rig.hw.enable(1), 1);
    assert!(rig.fan(3, |f| f.reason.contains("not controllable")));
}

#[test]
fn kicks_fan_from_rest() {
    let rig = Rig::new();
    rig.calibrate();
    rig.d.lock().fans[0].cfg.allow_stop = true;
    rig.hw.cpu(30.0);
    rig.cmd(json!({"cmd": "profile", "name": "silent"})).unwrap();
    let t = rig.ticks(200, 60_000.0);
    assert_eq!(rig.hw.pwm(1), 0, "silent + allow_stop rests the fan when cold");
    rig.hw.cpu(65.0);
    rig.ticks(6, t);
    assert!(rig.hw.pwm(1) >= rig.fan(0, |f| f.start_pwm()));
}

#[test]
fn calibration_aborts_when_hot() {
    let rig = Rig::new();
    rig.hw.cpu(86.0);
    rig.calibrate();
    assert!(rig.d.lock().message.contains("failed"));
    assert!(!rig.fan(0, |f| f.calibrated()));
}

#[test]
fn empty_headers_never_flag_stall() {
    let rig = Rig::new();
    rig.ticks(20, 1000.0);
    assert!(rig.fan(0, |f| f.seen_spinning));
    assert!(!rig.fan(4, |f| f.seen_spinning || f.stalled), "empty header pwm5 must not report a stall");
}
