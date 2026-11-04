# omarchy-fans

Fan curves for motherboard PWM fans, with an Omarchy bar widget.

- **`omarchy-fand`**: Rust daemon (root, systemd). Reads hwmon temperatures and drives the PWM fan headers.
- **`omarchy-fans`**: CLI. The shell plugin uses it too.
- **`qqq.fans`**: Omarchy shell plugin. Bar widget with live RPM and temps, profile buttons, and a curve editor you drag.

Built for a Gigabyte B650M D3HP (ITE IT8689E, in-kernel `it87` driver) with a Ryzen 8000G and an Arc A580. It works with any board whose fan chip exposes `pwmN`/`pwmN_enable` in hwmon.

## Install

```sh
./install.sh            # cargo build --release, install daemon (sudo once), add bar widget
omarchy-fans calibrate  # ~2 min; or the "Calibrate fans" button in the panel
```

`./install.sh --plugin` reinstalls only the widget. `./install.sh --uninstall` removes everything and hands the fans back to the BIOS.

The installer:

- loads `it87` with `ignore_resource_conflict=1`, which Gigabyte boards need (`/etc/modprobe.d/omarchy-fans-it87.conf`)
- loads `it87` at boot
- enables `omarchy-fand.service`

It does not enable the service if no PWM fans are detected.

### Gigabyte boards: fans that ignore PWM

On this B650M D3HP the mainline `it87` driver binds the IT8689E (rev 2) and accepts PWM writes, but the fans don't follow them. Gigabyte's firmware programs extra SmartFan "vectors" in register banks 2 and 3. These keep driving the fans from temperature even when a channel is in manual mode. Calibration detects this: an affected fan is marked **BIOS (driver can't steer)** and left to the firmware.

Fix it with the out-of-tree driver, which disables those vectors and uses MMIO:

```sh
./install.sh --it87-dkms   # pacman dkms + frankcrawford/it87 via DKMS, swaps the module
omarchy-fans calibrate
```

Measured with mainline it87, duty 255 down to 0:
- CPU fan: 1128–1247 RPM, tracking CPU temperature
- System fan 1: flat at ~1149 RPM
- System fan 2: 1175 RPM, then 864 RPM at duty 205 and below

## How the curves are optimised

Each tick (1 s), the daemon processes each temperature in this order:

1. **Smoothing.** An exponential moving average with τ = 4 s for CPU Tctl and 3 s for the GPU. Ryzen Tctl jumps 10 °C or more for a fraction of a second; smoothing stops the fans from chasing those spikes.
2. **Hysteresis.** Fans only slow down after the temperature has dropped 2–4 °C below its peak (depends on the profile). This stops fans hunting around a curve point.
3. **Curves.** A fan's speed is the max over its role's curves:
   - CPU fan: CPU temp.
   - Case fans: CPU temp *or* GPU temp, whichever demands more. The Arc card dumps its heat into the case.
4. **Ramp limits.** Fast up (12–35 %/s), slow down (2–5 %/s).
5. **Calibrated output.** Calibration measures each fan's RPM-vs-PWM response, the duty where it stops, and the duty that restarts it. As a result:
   - 0 % means the slowest speed the fan *reliably* holds: the stop point plus a margin.
   - Percent is linear in **RPM**, not PWM duty. A fan's RPM rises steeply at low duty, so uncalibrated 30 % duty can already be 60 % RPM.
   - A fan starting from rest gets a 2 s kick at its measured start duty.
   - Empty headers are detected and left to the BIOS.

Profiles: **Silent**, **Balanced** (default), **Performance**, **Full**, and **Custom**. Dragging any curve point in the panel copies the active profile into Custom and edits that copy.

## Safety

- If any sensor reaches its critical temperature, every fan goes to 100 % at once, with no ramp. Thresholds: CPU 90 °C, GPU 97 °C, VRAM 102 °C, NVMe 75 °C.
- If a sensor stops reading, every fan that depends on it goes to 100 %.
- A fan commanded to spin that reads 0 RPM is flagged STALLED, and the bar widget turns red.
- On exit, crash, or `systemctl stop`, every channel goes back to the BIOS's automatic mode (`ExecStopPost=omarchy-fand restore`).
- Calibration aborts and restores the curves if the CPU gets warm (80 °C).
- **Hand to BIOS** (or `omarchy-fans auto`) gives control back to the motherboard at any time.
- The control socket is limited to the `wheel` group. The status file is world-readable.

## CLI

```
omarchy-fans                         one-line summary
omarchy-fans status [--json]
omarchy-fans profile silent|balanced|performance|full|custom
omarchy-fans cycle                   (also: right-click the bar widget)
omarchy-fans curve cpu cpu 40:10,60:30,75:60,85:100
omarchy-fans curve case gpu 60:10,80:40,92:100
omarchy-fans tuning --ramp-down 2 --hysteresis 4
omarchy-fans fan pwm2 --label "Front intake" --role case
omarchy-fans fan pwm3 --allow-stop   # let a case fan stop when the curve is at 0 %
omarchy-fans calibrate
omarchy-fans auto | resume
```

Shell IPC: `omarchy-shell qqq.fans toggle`, `omarchy-shell qqq.fans cycle`, `omarchy-shell qqq.fans profile silent`.

Widget settings (`~/.config/omarchy/shell.json`, `qqq.fans` entry): `barMode` is `RPM`, `Temp` or `Icon`.

## Files

| Path | What |
|---|---|
| `/var/lib/omarchy-fans/state.json` | profile, custom curves, fan labels/roles, calibration |
| `/run/omarchy-fans/status.json` | live status, rewritten every second |
| `/run/omarchy-fans/control.sock` | JSON-line control socket |

## Development

```sh
cargo test                     # unit + simulated-hwmon integration tests
node --test plugin/tests/      # panel model tests
```

`tests/sim.rs` runs the real daemon against a fake `/sys/class/hwmon` with simulated fans. The fans have stop/start hysteresis, nonlinear RPM, and empty headers.
