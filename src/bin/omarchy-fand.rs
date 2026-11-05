//! omarchy-fand: fan-curve daemon (runs as root under systemd).

use omarchy_fans::daemon::{restore, Daemon, Paths, SIGNAL_STOP};
use std::path::PathBuf;
use std::sync::atomic::Ordering;

const USAGE: &str = "usage: omarchy-fand [run|restore|detect] [--sysfs DIR] [--run-dir DIR] [--state-dir DIR] [--interval SECS] [--group NAME]

  run      control fans (default)
  restore  hand all PWM channels back to firmware control
  detect   print detected sensors and fans as JSON, then exit";

extern "C" fn on_signal(_: libc::c_int) {
    SIGNAL_STOP.store(true, Ordering::Relaxed);
}

fn env_or(key: &str, default: &str) -> PathBuf {
    PathBuf::from(std::env::var(key).unwrap_or_else(|_| default.into()))
}

fn main() {
    let mut paths = Paths {
        sysfs: env_or("OMARCHY_FANS_SYSFS", "/sys/class/hwmon"),
        run_dir: env_or("OMARCHY_FANS_RUN", "/run/omarchy-fans"),
        state_dir: env_or("OMARCHY_FANS_STATE", "/var/lib/omarchy-fans"),
        group: "wheel".into(),
        pci_xe: env_or("OMARCHY_FANS_XE", "/sys/bus/pci/drivers/xe"),
    };
    let mut command = "run".to_string();
    let mut interval = 1.0;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| fail(&format!("{a} needs a value")));
        match a.as_str() {
            "--sysfs" => paths.sysfs = val().into(),
            "--run-dir" => paths.run_dir = val().into(),
            "--state-dir" => paths.state_dir = val().into(),
            "--group" => paths.group = val(),
            "--interval" => interval = val().parse().ok().filter(|v: &f64| *v >= 0.2).unwrap_or_else(|| fail("bad --interval")),
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            "--version" => {
                println!("omarchy-fand {}", omarchy_fans::VERSION);
                return;
            }
            "run" | "restore" | "detect" => command = a,
            other => fail(&format!("unknown argument {other}")),
        }
    }

    match command.as_str() {
        "restore" => restore(&paths),
        "detect" => {
            let d = Daemon::new(paths, interval);
            let core = d.lock();
            println!("{}", serde_json::to_string_pretty(&core.status()).unwrap());
        }
        _ => {
            unsafe {
                libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
                libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
            }
            Daemon::new(paths, interval).run();
        }
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("omarchy-fand: {msg}\n{USAGE}");
    std::process::exit(2)
}
