# omarchy-fans

Fan curves for motherboard PWM fans and Intel Arc A-series GPU fans, with an [Omarchy](https://omarchy.org) bar widget.

- **`omarchy-fand`**: Rust daemon (root, systemd). Reads hwmon temperatures and drives the PWM fan headers, plus an Arc A-series card's fans through its PCODE fan table.
- **`omarchy-fans`**: CLI. The shell plugin uses it too.
- **`qqq.fans`**: Omarchy shell plugin. Bar widget with live RPM and temps, profile buttons, and a curve editor you drag.

Only want the GPU part? [arc-fan](https://github.com/quinprobably-netizen/arc-fan) is the Arc fan controller as a standalone tool.

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

On this B650M D3HP the mainline `it87` driver binds the IT8689E (rev 2) and accepts PWM writes, but the fans don't follow them. Gigabyte's firmware programs extra SmartFan "vectors" in register banks 2 and 3. These keep driving the fans from temperature even when a channel is in manual mode. Calibration detects this: an affected fan is marked **BIOS (ignores PWM)** and left to the firmware.

Fix it with the out-of-tree driver, which disables those vectors and uses MMIO:

```sh
./install.sh --it87-dkms   # pacman dkms + frankcrawford/it87 via DKMS, swaps the module
omarchy-fans calibrate
```

Measured with mainline it87, duty 255 down to 0:
- CPU fan: 1128–1247 RPM, tracking CPU temperature
- System fan 1: flat at ~1149 RPM
- System fan 2: 1175 RPM, then 864 RPM at duty 205 and below

With frankcrawford/it87, the CPU fan (438–1650 RPM) and System fan 2 (0–1580 RPM) follow PWM.

### 3-pin fans

A 3-pin fan has no PWM wire, so on a header in PWM mode it runs at full voltage whatever the duty (System fan 1 here: flat ~1120–1160 RPM even with the DKMS driver). `it87` has no sysfs switch for the header's DC mode, so only the BIOS can change it: set the header to **Voltage** mode (Smart Fan 6, per-header "Fan Control Mode") and reboot. The duty register then sets the output voltage.

You don't need to recalibrate by hand. Every time the daemon starts it **re-tests** each fan that ignored PWM: about 15 s at full duty, then at zero. A fan that now follows PWM gets a full calibration and goes onto its curve. A fan that still ignores PWM stays with the BIOS. The panel marks these fans **ignores PWM · BIOS**, says how to fix them, and has a **Re-test** button per fan (`omarchy-fans retest`).

### Intel Arc A-series GPU fans

Mainline `xe` only reports an Arc card's fan RPM. On DG2 (Alchemist: A380, A580, A750, A770) the daemon drives the fans itself through the GPU's PCODE mailbox (BAR0 MMIO, command `0x7D`). It uses the user fan table that DG2 firmware accepts:

- subcommand 1 sets the number of points (0 means the firmware's stock curve)
- subcommand 2 writes the points two at a time, packed as `temp | speed% << 8`

The protocol was reverse-engineered by [AIVirtuoso/intel-arc-a770-fan-control](https://github.com/AIVirtuoso/intel-arc-a770-fan-control) and verified here on an ASRock A580: 50 % ≈ 1900 rpm, 80 % ≈ 2500, 100 % ≈ 3280. (The Battlemage subcommands from Intel's pending `xe` fan patches do nothing on DG2.)

The mailbox code is the [arc-fan](https://github.com/quinprobably-netizen/arc-fan) crate. Each card gets a worker thread that owns the BAR mapping. The control loop posts the duty it wants and moves on, so a mailbox the driver is holding busy never delays the other fans. The worker always applies the newest duty.

`omarchy-fand.service` conflicts with `arc-fan.service`: starting one stops the other, since both write the same table. The `arc-fan` CLI warns if you use it while the daemon runs.

The card shows up as **GPU fan** (`xe/gpu`, role `gpu`) with its own curves on the GPU and VRAM sensors in every profile. By default it may stop when the card is cool, as the stock firmware does (`omarchy-fans fan gpu --no-stop` keeps it spinning). Each duty is sent as a flat table that still ramps to 100 % between 85 and 95 °C. So if the daemon hangs, the card cools itself. Suspend-to-RAM clears the table, so the daemon re-sends it right after a resume, and every 30 s anyway. Hand to BIOS, daemon exit and `omarchy-fand restore` put the stock curve back. Calibration skips the GPU fan (it slews ~50 rpm/s); its 0 % is 40 % duty, the lowest the fans hold without hunting.

The daemon sends mailbox commands without the `xe` driver's PCODE lock, so it checks the mailbox's READY bit first and retries if it is busy. `arc-pcode` (`target/release/arc-pcode`, root) is a diagnostics tool: `check`, `dg2-set PCT`, `dg2-auto`, `dg2-test`, and read-only register watching.

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
- If none of a fan's sensors exist (say `k10temp` hasn't loaded yet), that fan runs at 100 % and the panel says why. The daemon looks for the missing sensor every 10 s. Calibration refuses to start without a CPU sensor, since it couldn't abort on heat.
- After a resume from suspend, every fan setting is written again. Every tick also checks that each channel is still in manual mode, and takes it back if the firmware reclaimed it.
- A fan commanded to spin that reads 0 RPM is flagged STALLED, and the bar widget turns red.
- On exit, crash, or `systemctl stop`, every channel goes back to the BIOS's automatic mode (`ExecStopPost=omarchy-fand restore`).
- Calibration aborts and restores the curves if the CPU gets warm (80 °C). Fans that are not being calibrated stay on their curves meanwhile.
- **Hand to BIOS** (or `omarchy-fans auto`) gives control back to the motherboard at any time.
- The control socket is limited to the `wheel` group and serves at most 16 clients at once. The status file is world-readable.

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
omarchy-fans calibrate [fan-id ...] # all fans, or just the ones named
omarchy-fans retest [fan-id ...]    # re-check fans that ignored PWM
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
node --test plugin/tests/*.test.js  # panel model tests
```

`tests/sim.rs` runs the real daemon against a fake `/sys/class/hwmon` with simulated fans. The fans have stop/start hysteresis, nonlinear RPM, and empty headers.

## License

MIT
