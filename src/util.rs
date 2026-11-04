//! Small filesystem helpers shared by the daemon and CLI.

use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub fn read_text(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

pub fn read_int(path: &Path) -> Option<i64> {
    read_text(path)?.parse().ok()
}

pub fn write_int(path: &Path, value: i64) -> io::Result<()> {
    fs::write(path, value.to_string())
}

/// Write via a temp file + rename so readers never see a half-written file.
pub fn atomic_write(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.tmp{}", std::process::id()));
    fs::write(&tmp, data)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)
}

pub fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    v.max(lo).min(hi)
}

/// Look up a group id in /etc/group without pulling in NSS.
pub fn group_gid(name: &str) -> Option<u32> {
    let text = fs::read_to_string("/etc/group").ok()?;
    text.lines().find_map(|line| {
        let mut parts = line.split(':');
        (parts.next()? == name).then_some(())?;
        parts.nth(1)?.parse().ok()
    })
}

pub fn unix_time() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}
