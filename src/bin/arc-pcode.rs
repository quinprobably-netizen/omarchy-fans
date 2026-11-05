//! arc-pcode: diagnostics for an Intel Arc A-series (DG2) card's PCODE fan
//! table. Root only.
//!
//!   check              read-only: temps, tach, PCODE init status
//!   dg2-set PCT        flat fan duty (with a 95 °C -> 100% safety ramp)
//!   dg2-auto           back to the firmware's stock curve
//!   dg2-test           stock -> 50% -> 80% -> 40% -> stock, logging RPM
//!   peek|watch|watchr|scan|corr   read-only register inspection

use omarchy_fans::xe_pcode::*;
use std::path::PathBuf;

fn find_gpu() -> Option<PathBuf> {
    for e in std::fs::read_dir("/sys/bus/pci/drivers/xe").ok()?.flatten() {
        let p = e.path();
        if p.join("resource0").exists() && p.join("vendor").exists() {
            return Some(p);
        }
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dev = std::env::var("ARC_PCI").map(PathBuf::from).ok().or_else(find_gpu).unwrap_or_else(|| {
        eprintln!("no xe GPU found");
        std::process::exit(1)
    });
    let status = std::fs::read_to_string(dev.join("power/runtime_status")).unwrap_or_default();
    if status.trim() != "active" {
        eprintln!("GPU runtime status is {:?}, not active; refusing", status.trim());
        std::process::exit(1);
    }
    let mmio = Mmio::open(&dev.join("resource0")).unwrap_or_else(|e| {
        eprintln!("mmap {}: {e}", dev.join("resource0").display());
        std::process::exit(1)
    });
    println!("device {}  mailbox={:#010x}", dev.display(), mmio.read32(PCODE_MAILBOX));
    match args.first().map(String::as_str).unwrap_or("check") {
        "check" => {
            // Registers the xe hwmon reads on DG2, then DGFX_PCODE_STATUS /
            // GET_INIT_STATUS (0x7E/0), the read xe_pcode_ready() does at boot.
            for (name, off) in [("PCU_CR_PACKAGE_TEMPERATURE", 0x145978usize), ("BMG_VRAM_TEMPERATURE", 0x1382c0), ("FAN_1_SPEED (tach)", 0x138140)] {
                println!("  {name:<28} {off:#08x} = {:#010x}", mmio.read32(off));
            }
            show("PCODE init status (want 1)", mmio.mailbox_retry(0x7E, 0, 0, 0, 0));
            println!("  fan {} rpm", hwmon_rpm(&dev).map_or("?".into(), |r| r.to_string()));
        }
        "dg2-auto" => report(mmio.dg2_commit(&[])),
        "dg2-set" => {
            let pct: u8 = args.get(1).and_then(|a| a.parse().ok()).filter(|p| *p <= 100).unwrap_or_else(|| {
                eprintln!("usage: arc-pcode dg2-set PCT (0..100)");
                std::process::exit(2)
            });
            report(mmio.dg2_commit(&dg2_flat_table(pct)));
        }
        "dg2-test" => dg2_test(&mmio, &dev),
        "peek" => {
            for a in &args[1..] {
                let off = usize::from_str_radix(a.trim_start_matches("0x"), 16).expect("hex offset");
                println!("  {off:#08x} = {:#010x}", mmio.read32(off));
            }
        }
        "watch" | "watchr" => {
            // watch <secs> [hex offset...]: RPM from the tach counter plus registers.
            let secs: u64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(60);
            let mut offs: Vec<usize> = args.iter().skip(2).map(|a| usize::from_str_radix(a.trim_start_matches("0x"), 16).expect("hex")).collect();
            if offs.is_empty() {
                offs = (0x138140..0x138180).step_by(4).collect();
            }
            let mut prev = mmio.read32(0x138140);
            println!("    t    rpm  °C  {}", offs.iter().map(|o| format!("{o:>10x}")).collect::<Vec<_>>().join(" "));
            for t in 0..secs * 2 {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let tach = mmio.read32(0x138140);
                let rpm = (tach.wrapping_sub(prev) as f64 / 2.0) * 120.0;
                prev = tach;
                println!("{:>5.1} {rpm:>6.0} {:>3}  {}", t as f64 / 2.0, mmio.read32(0x145978) & 0xff, offs.iter().map(|&o| format!("{:>10x}", mmio.read32(o))).collect::<Vec<_>>().join(" "));
            }
        }
        "scan" => scan(&mmio),
        "corr" => {
            let secs: f64 = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(180.0);
            let hex = |i: usize, d: usize| args.get(i).and_then(|a| usize::from_str_radix(a.trim_start_matches("0x"), 16).ok()).unwrap_or(d);
            corr(&mmio, secs, hex(2, 0x138000), hex(3, 0x13a000));
        }
        other => {
            eprintln!("unknown command {other:?} (check, dg2-set, dg2-auto, dg2-test, peek, watch, scan, corr)");
            std::process::exit(2)
        }
    }
}

fn show(name: &str, r: Result<(u32, u32), PcodeError>) -> Option<u32> {
    match r {
        Ok((d0, d1)) => {
            println!("  {name:<34} data0={d0:#010x} ({d0})  data1={d1:#x}");
            Some(d0)
        }
        Err(e) => {
            println!("  {name:<34} {e}");
            None
        }
    }
}




/// Read-only: sample the fan register block while the fan cycles and print
/// every register that changes, next to the measured RPM.
fn scan(m: &Mmio) {
    let offs: Vec<usize> = (0x138100..0x138200).step_by(4).collect();
    let mut first: Vec<u32> = offs.iter().map(|&o| m.read32(o)).collect();
    let mut rows = Vec::new();
    let mut prev_tach = m.read32(0x138140);
    for i in 0..90 {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let vals: Vec<u32> = offs.iter().map(|&o| m.read32(o)).collect();
        let tach = vals[(0x138140 - 0x138100) / 4];
        let rpm = (tach.wrapping_sub(prev_tach) as f64 / 2.0) * 120.0;
        prev_tach = tach;
        rows.push((i, rpm, m.read32(0x145978) & 0xff, vals));
    }
    let changing: Vec<usize> = (0..offs.len()).filter(|&k| rows.iter().any(|r| r.3[k] != first[k])).collect();
    first.clear();
    print!("  t    rpm  °C");
    for &k in &changing { print!(" {:>10x}", offs[k]); }
    println!();
    for (i, rpm, t, v) in &rows {
        print!("{:>4} {:>5.0} {:>3}", i, rpm, t);
        for &k in &changing { print!(" {:>10x}", v[k]); }
        println!();
    }
    println!("static registers:");
    for (k, &o) in offs.iter().enumerate() {
        if !changing.contains(&k) && rows[0].3[k] != 0 { println!("  {o:#x} = {:#010x}", rows[0].3[k]); }
    }
}

/// Read-only: find registers whose value separates "fan spinning" from
/// "fan stopped" while the firmware cycles the fan.
fn corr(m: &Mmio, secs: f64, lo: usize, hi: usize) {
    let offs: Vec<usize> = (lo..hi).step_by(4).filter(|&o| o != PCODE_MAILBOX && o != PCODE_DATA0 && o != PCODE_DATA1).collect();
    let mut samples: Vec<(f64, Vec<u32>)> = Vec::new();
    let mut prev = m.read32(0x138140);
    let t0 = std::time::Instant::now();
    let mut last = t0;
    while t0.elapsed().as_secs_f64() < secs {
        std::thread::sleep(std::time::Duration::from_millis(500));
        let tach = m.read32(0x138140);
        let dt = last.elapsed().as_secs_f64();
        last = std::time::Instant::now();
        let rpm = (tach.wrapping_sub(prev) as f64 / 2.0) * 60.0 / dt;
        prev = tach;
        samples.push((rpm, offs.iter().map(|&o| m.read32(o)).collect()));
    }
    let on: Vec<&Vec<u32>> = samples.iter().filter(|s| s.0 > 300.0).map(|s| &s.1).collect();
    let off: Vec<&Vec<u32>> = samples.iter().filter(|s| s.0 == 0.0).map(|s| &s.1).collect();
    println!("samples {} (spinning {}, stopped {})", samples.len(), on.len(), off.len());
    if on.is_empty() || off.is_empty() {
        println!("fan did not both spin and stop; rerun longer");
        return;
    }
    let mut cands = Vec::new();
    for k in 0..offs.len() {
        let mean = |v: &[&Vec<u32>]| v.iter().map(|s| s[k] as f64).sum::<f64>() / v.len() as f64;
        let (mon, moff) = (mean(&on), mean(&off));
        let on_min = on.iter().map(|s| s[k]).min().unwrap();
        let on_max = on.iter().map(|s| s[k]).max().unwrap();
        let off_min = off.iter().map(|s| s[k]).min().unwrap();
        let off_max = off.iter().map(|s| s[k]).max().unwrap();
        let disjoint = on_min > off_max || off_min > on_max;
        if disjoint || (mon - moff).abs() > 0.0 && (on_max - on_min < 0x100 || off_max - off_min < 0x100) && mon != moff {
            cands.push((disjoint, offs[k], on_min, on_max, off_min, off_max));
        }
    }
    cands.sort_by_key(|c| !c.0);
    for (d, o, a, b, c, e) in cands.iter().take(60) {
        println!("{} {o:#08x}  spinning {a:#x}..{b:#x}   stopped {c:#x}..{e:#x}", if *d { "DISJOINT" } else { "        " });
    }
    // Time series of disjoint registers next to rpm.
    let dis: Vec<usize> = cands.iter().filter(|c| c.0).map(|c| offs.iter().position(|&o| o == c.1).unwrap()).take(8).collect();
    for (i, (rpm, v)) in samples.iter().enumerate() {
        if i % 2 == 0 {
            println!("{i:>4} {rpm:>6.0}  {}", dis.iter().map(|&k| format!("{:>8x}", v[k])).collect::<Vec<_>>().join(" "));
        }
    }
}

fn report(r: Result<(), String>) {
    match r {
        Ok(()) => println!("  ok"),
        Err(e) => {
            println!("  failed: {e}");
            std::process::exit(1)
        }
    }
}

fn hwmon_rpm(dev: &std::path::Path) -> Option<i64> {
    let hw = std::fs::read_dir(dev.join("hwmon")).ok()?.flatten().next()?.path();
    std::fs::read_to_string(hw.join("fan1_input")).ok()?.trim().parse().ok()
}

/// Live check: stock -> 50% -> 80% -> 40% -> stock, logging RPM.
fn dg2_test(m: &Mmio, dev: &std::path::Path) {
    let ctl = dev.join("power/control");
    let saved = std::fs::read_to_string(&ctl).unwrap_or_else(|_| "auto".into());
    let _ = std::fs::write(&ctl, "on");
    let phase = |name: &str, table: Vec<(u8, u8)>, secs: u64| {
        let r = m.dg2_commit(&table);
        println!("== {name}: {}", r.as_ref().map_or_else(|e| e.clone(), |_| "ok".into()));
        let _ = hwmon_rpm(dev); // prime the driver's delta
        for t in 0..secs {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if t % 2 == 1 {
                println!("  {:>3}s  {:>5} rpm  {}°C", t + 1, hwmon_rpm(dev).unwrap_or(-1), m.read32(0x145978) & 0xff);
            }
        }
    };
    phase("stock (count 0)", vec![], 6);
    phase("flat 50%", dg2_flat_table(50), 24);
    phase("flat 80%", dg2_flat_table(80), 24);
    phase("flat 40%", dg2_flat_table(40), 20);
    phase("stock (count 0)", vec![], 10);
    let _ = std::fs::write(&ctl, saved.trim());
}
