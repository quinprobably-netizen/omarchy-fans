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
    /// Header 4 ignores PWM (3-pin fan, header in PWM mode). Clearing it is
    /// the BIOS switch to Voltage mode: duty then sets the fan's voltage.
    deaf4: Arc<AtomicBool>,
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
        let deaf4 = Arc::new(AtomicBool::new(true));
        let sim = {
            let (it, spinning, stop, deaf4) = (it.clone(), spinning.clone(), stop.clone(), deaf4.clone());
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
                    let manual = fs::read_to_string(it.join("pwm4_enable")).ok().is_some_and(|e| e.trim() == "1");
                    let duty = fs::read_to_string(it.join("pwm4")).ok().and_then(|p| p.trim().parse::<i64>().ok());
                    let rpm4 = match duty {
                        // Voltage mode: below ~5 V the motor stalls.
                        Some(d) if manual && !deaf4.load(Ordering::Relaxed) => {
                            if d < 100 { 0 } else { (1300.0 * (d as f64 / 255.0).powf(0.8)) as i64 }
                        }
                        _ => 1140 + wobble * 15,
                    };
                    w(&it.join("fan4_input"), rpm4);
                    thread::sleep(Duration::from_millis(2));
                }
            })
        };
        FakeHw { root, k10, xe, it, spinning, deaf4, stop, sim: Some(sim) }
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
    pcode: Option<FakePcode>,
}

/// A DG2 card's BAR0 as a file, with a thread playing PCODE: it logs each
/// FAN_SPEED_CONTROL request as (subcommand, data0) and completes it.
struct FakePcode {
    log: Arc<Mutex<Vec<(u8, u32)>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

const MAILBOX: usize = 0x138124;

impl FakePcode {
    fn new(dev: &Path) -> Self {
        fs::create_dir_all(dev.join("power")).unwrap();
        fs::write(dev.join("device"), "0x56a2\n").unwrap();
        fs::write(dev.join("power/runtime_status"), "active\n").unwrap();
        let bar = fs::File::create(dev.join("resource0")).unwrap();
        bar.set_len(16 << 20).unwrap();
        drop(bar);
        let file = fs::OpenOptions::new().read(true).write(true).open(dev.join("resource0")).unwrap();
        use std::os::fd::AsRawFd;
        let ptr = unsafe {
            libc::mmap(std::ptr::null_mut(), 16 << 20, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, file.as_raw_fd(), 0)
        } as usize;
        assert_ne!(ptr, libc::MAP_FAILED as usize);
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (log, stop) = (log.clone(), stop.clone());
            thread::spawn(move || {
                let reg = |off: usize| (ptr + off) as *mut u32;
                while !stop.load(Ordering::Relaxed) {
                    let mb = unsafe { std::ptr::read_volatile(reg(MAILBOX)) };
                    if mb & (1 << 31) != 0 {
                        let data0 = unsafe { std::ptr::read_volatile(reg(MAILBOX + 4)) };
                        if mb & 0xff == 0x7d {
                            log.lock().unwrap().push((((mb >> 8) & 0xff) as u8, data0));
                        }
                        unsafe { std::ptr::write_volatile(reg(MAILBOX + 4), 0) };
                        // Done, status 0 in the low byte (as the A580 does).
                        unsafe { std::ptr::write_volatile(reg(MAILBOX), 0) };
                    }
                    thread::sleep(Duration::from_micros(20));
                }
                unsafe { libc::munmap(ptr as *mut libc::c_void, 16 << 20) };
            })
        };
        FakePcode { log, stop, thread: Some(thread) }
    }

    fn log(&self) -> Vec<(u8, u32)> {
        self.log.lock().unwrap().clone()
    }

    /// Flat duty (percent) of the last complete table, or None for stock.
    fn last_table(&self) -> Option<Option<u8>> {
        let log = self.log();
        let start = log.iter().rposition(|(sub, _)| *sub == 1)?;
        let (_, count) = log[start];
        if count == 0 {
            return Some(None);
        }
        let first_pair = log.get(start + 1)?;
        assert_eq!(first_pair.0, 2, "points follow the count");
        Some(Some(((first_pair.1 >> 8) & 0xff) as u8))
    }
}

impl Drop for FakePcode {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Rig {
    fn new() -> Self {
        Self::build(false)
    }

    /// With a simulated Arc A580 on the xe PCI driver.
    fn with_gpu() -> Self {
        Self::build(true)
    }

    fn build(gpu: bool) -> Self {
        let hw = FakeHw::new();
        let paths = Paths {
            sysfs: hw.root.clone(),
            run_dir: hw.root.join("run"),
            state_dir: hw.root.join("state"),
            group: "nonexistent-group".into(),
            pci_xe: hw.root.join("pci-xe"),
        };
        fs::create_dir_all(&paths.run_dir).unwrap();
        let pcode = gpu.then(|| FakePcode::new(&paths.pci_xe.join("0000:03:00.0")));
        let d = Daemon::new(paths.clone(), 1.0);
        d.lock().time_scale = 0.02;
        Rig { hw, d, paths, pcode }
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
        self.run_cal(json!({"cmd": "calibrate"}));
    }

    fn retest(&self) {
        self.run_cal(json!({"cmd": "retest"}));
    }

    /// Run a calibration-style command to completion with the loop ticking.
    fn run_cal(&self, req: Value) {
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
        self.cmd(req).unwrap();
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
    assert!(core.message.contains("System fan 3 ignores PWM"), "{}", core.message);
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
    // The first tick that leaves rest must kick at the start duty, whatever
    // the fan settles at once the kick expires.
    let first_spin = (0..10).find_map(|k| {
        rig.d.tick(t + k as f64);
        let p = rig.hw.pwm(1);
        (p > 0).then_some(p)
    });
    assert!(first_spin.unwrap() >= rig.fan(0, |f| f.start_pwm()), "{first_spin:?}");
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

// ---- fans that ignore PWM -------------------------------------------------

#[test]
fn retest_takes_control_after_switch_to_voltage_mode() {
    let rig = Rig::new();
    rig.calibrate();
    assert!(!rig.fan(3, |f| f.responsive()));
    rig.hw.deaf4.store(false, Ordering::Relaxed); // BIOS: header 4 -> Voltage
    rig.retest();
    assert!(rig.fan(3, |f| f.controllable() && f.calibrated()), "{}", rig.d.lock().message);
    assert!(rig.d.lock().message.contains("System fan 3 follows PWM now"), "{}", rig.d.lock().message);
    // Voltage-mode fan stalls below duty 100: floor must sit above that.
    assert!(rig.fan(3, |f| f.floor_pwm()) > 100);
    rig.ticks(3, 70_000.0);
    assert_eq!(rig.hw.enable(4), 1, "now driven by the curve");
    assert!(rig.hw.pwm(4) >= rig.fan(3, |f| f.floor_pwm()));
    let saved: Value = serde_json::from_slice(&fs::read(rig.paths.state_file()).unwrap()).unwrap();
    assert_eq!(saved["fans"]["it8689/pwm4"]["calibration"]["responsive"], true);
}

#[test]
fn retest_leaves_still_deaf_fan_with_bios() {
    let rig = Rig::new();
    rig.calibrate();
    rig.retest();
    assert!(!rig.fan(3, |f| f.responsive()));
    assert!(rig.d.lock().message.contains("System fan 3 ignores PWM"), "{}", rig.d.lock().message);
    rig.ticks(3, 70_000.0);
    assert_eq!(rig.hw.enable(4), 2, "handed back to the BIOS");
}

#[test]
fn retest_needs_a_deaf_fan() {
    let rig = Rig::new();
    let err = rig.cmd(json!({"cmd": "retest"})).unwrap_err();
    assert!(err.contains("no fans that ignore PWM"), "{err}");
    assert!(rig.d.lock().calibration.is_none());
}

#[test]
fn daemon_start_retests_deaf_fans() {
    let rig = Rig::new();
    rig.calibrate();
    rig.hw.deaf4.store(false, Ordering::Relaxed);
    // Reboot: a fresh daemon with the saved calibration.
    rig.d.request_stop();
    let d2 = Daemon::new(rig.paths.clone(), 0.05);
    d2.lock().time_scale = 0.02;
    let runner = {
        let d2 = d2.clone();
        thread::spawn(move || d2.run())
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while !d2.lock().fans[3].controllable() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    while d2.lock().calibration.is_some() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(d2.lock().fans[3].controllable() && d2.lock().fans[3].calibrated());
    d2.request_stop();
    runner.join().unwrap();
}

#[test]
fn other_fans_stay_on_curve_while_one_calibrates() {
    let rig = Rig::new();
    rig.hw.cpu(75.0);
    rig.cmd(json!({"cmd": "calibrate", "fans": ["pwm3"]})).unwrap();
    rig.ticks(40, 1000.0);
    // balanced cpu curve at 75 °C -> 65 %
    assert!(rig.fan(0, |f| f.percent.is_some_and(|p| p > 60.0)), "CPU fan must keep following its curve");
    assert!(rig.fan(2, |f| f.percent.is_none()), "fan under calibration is left alone");
    let deadline = Instant::now() + Duration::from_secs(30);
    while rig.d.lock().calibration.is_some() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(rig.fan(2, |f| f.calibrated()));
    assert!(!rig.fan(0, |f| f.calibrated()), "only the selected fan is calibrated");
}

// ---- Arc A580 GPU fan -------------------------------------------------------

#[test]
fn gpu_fan_is_discovered() {
    let rig = Rig::with_gpu();
    let core = rig.d.lock();
    let f = core.fans.last().unwrap();
    assert_eq!(f.id, "xe/gpu", "uses the xe hwmon chip for RPM");
    assert_eq!((f.cfg.label.as_str(), f.cfg.role.as_str()), ("GPU fan", "gpu"));
    assert!(f.is_gpu() && f.cfg.allow_stop);
    assert_eq!(core.status()["fans"].as_array().unwrap().last().unwrap()["kind"], "gpu");
}

#[test]
fn gpu_fan_follows_gpu_curve() {
    let rig = Rig::with_gpu();
    rig.hw.gpu(85.0);
    rig.ticks(40, 1000.0);
    // balanced gpu curve at 85 °C -> 75 %, mapped above the 40 % floor
    let pct = rig.pcode.as_ref().unwrap().last_table().unwrap().unwrap();
    assert!((80..=88).contains(&pct), "{pct}");
    let log = rig.pcode.as_ref().unwrap().log();
    let start = log.iter().rposition(|(s, _)| *s == 1).unwrap();
    assert_eq!(log[start], (1, 4), "flat table with a hot ramp: 4 points");
    assert_eq!(log[start + 2].1 & 0xffff, 95 | (100 << 8), "firmware ramps to 100% at 95 °C");
}

#[test]
fn cool_gpu_lets_fan_stop() {
    let rig = Rig::with_gpu();
    rig.hw.gpu(40.0);
    rig.ticks(5, 1000.0);
    assert_eq!(rig.pcode.as_ref().unwrap().last_table(), Some(Some(0)));
}

#[test]
fn gpu_critical_forces_full_speed() {
    let rig = Rig::with_gpu();
    let t = rig.ticks(5, 1000.0);
    rig.hw.gpu(99.0);
    rig.ticks(1, t);
    assert_eq!(rig.pcode.as_ref().unwrap().last_table(), Some(Some(100)));
}

#[test]
fn gpu_fan_handed_back_to_stock_curve() {
    let rig = Rig::with_gpu();
    rig.hw.gpu(80.0);
    let t = rig.ticks(3, 1000.0);
    assert!(matches!(rig.pcode.as_ref().unwrap().last_table(), Some(Some(_))));
    rig.cmd(json!({"cmd": "mode", "mode": "auto"})).unwrap();
    assert_eq!(rig.pcode.as_ref().unwrap().last_table(), Some(None), "count 0 = stock curve");
    rig.ticks(3, t);
    assert_eq!(rig.pcode.as_ref().unwrap().last_table(), Some(None), "auto mode leaves it alone");
}

#[test]
fn restore_resets_gpu_to_stock() {
    let rig = Rig::with_gpu();
    rig.hw.gpu(80.0);
    rig.ticks(3, 1000.0);
    restore(&rig.paths);
    assert_eq!(rig.pcode.as_ref().unwrap().last_table(), Some(None));
}

#[test]
fn calibration_skips_gpu_fan() {
    let rig = Rig::with_gpu();
    rig.cmd(json!({"cmd": "calibrate"})).unwrap();
    let fans = rig.d.lock().calibration.as_ref().unwrap().fans.clone();
    assert!(!fans.iter().any(|f| f == "xe/gpu"), "{fans:?}");
    rig.d.lock().calibration = None;
}

#[test]
fn gpu_curve_is_editable() {
    let rig = Rig::with_gpu();
    rig.cmd(json!({"cmd": "curve", "role": "gpu", "sensor": "gpu", "points": [[40, 50], [90, 100]]})).unwrap();
    rig.hw.gpu(40.0);
    rig.ticks(3, 1000.0);
    let pct = rig.pcode.as_ref().unwrap().last_table().unwrap().unwrap();
    assert!((68..=72).contains(&pct), "50% of the 40..100% range is 70%, got {pct}");
}

