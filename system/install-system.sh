#!/bin/bash
# Root half of the installer. Run via install.sh (sudo or pkexec).
set -euo pipefail
src="$(cd "$(dirname "$0")/.." && pwd)"
bin="${CARGO_TARGET_DIR:-$src/target}/release"

[[ $EUID -eq 0 ]] || { echo "install-system.sh must run as root" >&2; exit 1; }

install -Dm755 "$bin/omarchy-fand" /usr/local/bin/omarchy-fand
install -Dm755 "$bin/omarchy-fans" /usr/local/bin/omarchy-fans
install -Dm644 "$src/system/omarchy-fand.service" /etc/systemd/system/omarchy-fand.service
install -Dm644 "$src/system/it87.conf" /etc/modprobe.d/omarchy-fans-it87.conf
install -Dm644 "$src/system/modules-load.conf" /etc/modules-load.d/omarchy-fans.conf
install -Dm644 "$src/README.md" /usr/local/share/doc/omarchy-fans/README.md

# Load (or reload with the new option) the fan controller driver.
if lsmod | grep -q '^it87 '; then
  grep -q 1 /sys/module/it87/parameters/ignore_resource_conflict 2>/dev/null || { modprobe -r it87 || true; }
fi
modprobe it87 || echo "warning: modprobe it87 failed; see dmesg | grep it87" >&2

systemctl daemon-reload

fans=$(/usr/local/bin/omarchy-fand detect --run-dir /tmp/omarchy-fand-detect --state-dir /var/lib/omarchy-fans 2>/dev/null \
  | grep -c '"id": "[^"]*/pwm' || true)
rm -rf /tmp/omarchy-fand-detect
if [[ $fans -eq 0 ]]; then
  echo "No controllable PWM fans detected; not enabling omarchy-fand." >&2
  echo "Check: dmesg | grep -i it87 ; ls /sys/class/hwmon/*/pwm*" >&2
  exit 2
fi
echo "Detected $fans PWM channels."
systemctl enable omarchy-fand.service
systemctl restart omarchy-fand.service  # picks up a new binary on reinstall
systemctl --no-pager --lines=5 status omarchy-fand.service || true
