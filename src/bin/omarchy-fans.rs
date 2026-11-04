//! omarchy-fans: control the omarchy-fand fan-curve daemon.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

const HELP: &str = "omarchy-fans: control the omarchy-fand fan-curve daemon

  omarchy-fans                       one-line summary
  omarchy-fans status [--json|--short]
                                     sensors, fans and active curves
  omarchy-fans profile [name]        silent | balanced | performance | full | custom
  omarchy-fans cycle                 next profile (silent -> balanced -> performance)
  omarchy-fans curve <role> <sensor> <t:p,...>
                                     edit the custom profile, e.g.
                                     omarchy-fans curve cpu cpu 40:10,60:30,75:60,85:100
  omarchy-fans tuning [--ramp-up N] [--ramp-down N] [--hysteresis N]
  omarchy-fans fan <id> [--label L] [--role cpu|case] [--enable|--disable]
                        [--allow-stop|--no-stop]
  omarchy-fans calibrate [fan-id ...]  measure fan RPM/PWM response (~2 min)
  omarchy-fans auto                  hand fans back to the BIOS curve
  omarchy-fans resume                take control again with the active profile
  omarchy-fans rescan                re-detect sensors and fans";

const CYCLE: [&str; 3] = ["silent", "balanced", "performance"];

fn run_dir() -> PathBuf {
    PathBuf::from(std::env::var("OMARCHY_FANS_RUN").unwrap_or_else(|_| "/run/omarchy-fans".into()))
}

fn die(msg: &str) -> ! {
    eprintln!("omarchy-fans: {msg}");
    std::process::exit(1)
}

fn call(req: Value) -> Value {
    let sock = run_dir().join("control.sock");
    let mut s = UnixStream::connect(&sock).unwrap_or_else(|e| match e.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
            die("daemon not running (systemctl status omarchy-fand)")
        }
        std::io::ErrorKind::PermissionDenied => {
            die("permission denied on the control socket (are you in the wheel group?)")
        }
        _ => die(&format!("cannot reach daemon: {e}")),
    });
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    s.write_all(format!("{req}\n").as_bytes()).unwrap_or_else(|e| die(&format!("send failed: {e}")));
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap_or_else(|e| die(&format!("no reply: {e}")));
    let reply: Value = serde_json::from_str(&line).unwrap_or_else(|_| die("garbled reply from daemon"));
    if reply["ok"] != json!(true) {
        die(reply["error"].as_str().unwrap_or("request failed"));
    }
    reply["result"].clone()
}

/// The status file is world-readable, so reading never needs the socket.
fn read_status() -> Value {
    std::fs::read(run_dir().join("status.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| call(json!({"cmd": "status"})))
}

fn arr(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or(&[])
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

fn summary(st: &Value) -> String {
    let temps: Vec<String> = arr(&st["sensors"])
        .iter()
        .filter(|s| matches!(s["id"].as_str(), Some("cpu" | "gpu")))
        .filter_map(|s| Some(format!("{} {:.0}°", s["label"].as_str()?, num(&s["temp"])?)))
        .collect();
    let rpms: Vec<String> = arr(&st["fans"])
        .iter()
        .filter(|f| f["connected"] == json!(true))
        .filter_map(|f| f["rpm"].as_i64().map(|r| r.to_string()))
        .collect();
    let mode = if !st["calibration"].is_null() {
        format!("calibrating {:.0}%", num(&st["calibration"]["progress"]).unwrap_or(0.0) * 100.0)
    } else if st["mode"] == "auto" {
        "BIOS".into()
    } else {
        st["profile"].as_str().unwrap_or("?").into()
    };
    format!("{mode} | {} | rpm {}", temps.join(" "), rpms.join(" "))
}

fn show(st: &Value) {
    let s = |v: &Value| v.as_str().unwrap_or("").to_string();
    println!("mode {}   profile {}   daemon {}", s(&st["mode"]), s(&st["profile"]), s(&st["version"]));
    if !s(&st["message"]).is_empty() {
        println!("  {}", s(&st["message"]));
    }
    if !st["calibration"].is_null() {
        let c = &st["calibration"];
        println!("  calibrating: {} ({:.0}%)", s(&c["step"]), num(&c["progress"]).unwrap_or(0.0) * 100.0);
    }
    println!("\nsensors");
    for x in arr(&st["sensors"]) {
        let t = num(&x["temp"]).map_or("    —".into(), |t| format!("{t:5.1}°C"));
        let failed = if x["ok"] == json!(true) { "" } else { "  FAILED" };
        println!(
            "  {:<6} {:<6} {t}  held {:5.1}  [{}]{failed}",
            s(&x["id"]),
            s(&x["label"]),
            num(&x["held"]).unwrap_or(0.0),
            s(&x["source"])
        );
    }
    println!("\nfans");
    for f in arr(&st["fans"]) {
        let mut flags = Vec::new();
        if f["connected"] != json!(true) { flags.push("not connected") }
        if f["enabled"] != json!(true) { flags.push("disabled") }
        if f["calibrated"] != json!(true) && f["connected"] == json!(true) { flags.push("uncalibrated") }
        if f["stalled"] == json!(true) { flags.push("STALLED") }
        let pct = num(&f["percent"]).map_or("    —".into(), |p| format!("{p:5.1}%"));
        let pwm = f["pwm"].as_i64().map_or("—".into(), |p| p.to_string());
        let rpm = f["rpm"].as_i64().map_or("—".into(), |r| format!("{r} rpm"));
        println!(
            "  {:<14} {:<14} {:<5} {pct} pwm {pwm:>3}  {rpm:>9}  {}  {}",
            s(&f["id"]),
            s(&f["label"]),
            s(&f["role"]),
            s(&f["reason"]),
            flags.join(" ")
        );
    }
    println!("\ncurves (active profile)");
    if let Some(roles) = st["curves"].as_object() {
        for (role, curves) in roles {
            for (sensor, pts) in curves.as_object().into_iter().flatten() {
                let pts: Vec<String> = arr(pts)
                    .iter()
                    .map(|p| format!("{}°:{}%", num(&p[0]).unwrap_or(0.0), num(&p[1]).unwrap_or(0.0)))
                    .collect();
                println!("  {role:<5} <- {sensor:<5} {}", pts.join("  "));
            }
        }
    }
    let t = &st["tuning"];
    println!(
        "\nramp up {}%/s  ramp down {}%/s  hysteresis {}°C",
        num(&t["ramp_up"]).unwrap_or(0.0),
        num(&t["ramp_down"]).unwrap_or(0.0),
        num(&t["hysteresis"]).unwrap_or(0.0)
    );
}

fn parse_points(text: &str) -> Value {
    let pts: Vec<[f64; 2]> = text
        .split(',')
        .map(|part| {
            let (t, p) = part.split_once(':').unwrap_or_else(|| die(&format!("bad point {part:?}; use temp:percent")));
            match (t.trim().parse(), p.trim().parse()) {
                (Ok(t), Ok(p)) => [t, p],
                _ => die(&format!("bad point {part:?}; use temp:percent")),
            }
        })
        .collect();
    json!(pts)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        println!("{}", summary(&read_status()));
        return;
    };
    let rest = &args[1..];
    let has = |flag: &str| rest.iter().any(|a| a == flag);
    match cmd.as_str() {
        "-h" | "--help" | "help" => println!("{HELP}"),
        "status" => {
            let st = read_status();
            if has("--json") {
                println!("{}", serde_json::to_string_pretty(&st).unwrap());
            } else if has("--short") {
                println!("{}", summary(&st));
            } else {
                show(&st);
            }
        }
        "profile" => match rest.first() {
            None => println!("{}", read_status()["profile"].as_str().unwrap_or("")),
            Some(name) => {
                call(json!({"cmd": "profile", "name": name}));
            }
        },
        "cycle" => {
            let st = read_status();
            let cur = st["profile"].as_str().unwrap_or("");
            let next = CYCLE.iter().position(|p| *p == cur).map_or(CYCLE[0], |i| CYCLE[(i + 1) % CYCLE.len()]);
            call(json!({"cmd": "profile", "name": next}));
            println!("{next}");
        }
        "curve" => {
            let [role, sensor, pts] = rest else { die("usage: omarchy-fans curve <cpu|case> <sensor> <temp:pct,...>") };
            let res = call(json!({"cmd": "curve", "role": role, "sensor": sensor, "points": parse_points(pts)}));
            println!("{}", res["curves"]);
        }
        "tuning" => {
            let mut req = json!({"cmd": "tuning"});
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                let key = match a.as_str() {
                    "--ramp-up" => "ramp_up",
                    "--ramp-down" => "ramp_down",
                    "--hysteresis" => "hysteresis",
                    _ => die(&format!("unknown option {a}")),
                };
                let v: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| die(&format!("{a} needs a number")));
                req[key] = json!(v);
            }
            println!("{}", call(req));
        }
        "fan" => {
            let Some(id) = rest.first() else { die("usage: omarchy-fans fan <id> [options]") };
            let mut req = json!({"cmd": "fan", "id": id});
            let mut it = rest[1..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--label" => req["label"] = json!(it.next().unwrap_or_else(|| die("--label needs a value"))),
                    "--role" => req["role"] = json!(it.next().unwrap_or_else(|| die("--role needs a value"))),
                    "--enable" | "--disable" => req["enabled"] = json!(a == "--enable"),
                    "--allow-stop" | "--no-stop" => req["allow_stop"] = json!(a == "--allow-stop"),
                    _ => die(&format!("unknown option {a}")),
                }
            }
            println!("{}", call(req));
        }
        "calibrate" => {
            let fans = if rest.is_empty() { Value::Null } else { json!(rest) };
            call(json!({"cmd": "calibrate", "fans": fans}));
            println!("calibration started (~2 min); follow with: omarchy-fans status");
        }
        "auto" => {
            call(json!({"cmd": "mode", "mode": "auto"}));
        }
        "resume" => {
            call(json!({"cmd": "mode", "mode": "curve"}));
        }
        "rescan" => println!("{}", call(json!({"cmd": "rescan"}))),
        other => die(&format!("unknown command {other:?} (see omarchy-fans --help)")),
    }
}
