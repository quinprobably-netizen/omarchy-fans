#!/bin/bash
# Stops the daemon (fans go back to the BIOS) and removes system files.
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "must run as root" >&2; exit 1; }
systemctl disable --now omarchy-fand.service 2>/dev/null || true
/usr/local/bin/omarchy-fand restore 2>/dev/null || true
rm -f /usr/local/bin/omarchy-fand /usr/local/bin/omarchy-fans \
  /etc/systemd/system/omarchy-fand.service \
  /etc/modprobe.d/omarchy-fans-it87.conf /etc/modules-load.d/omarchy-fans.conf
rm -rf /usr/local/share/doc/omarchy-fans
systemctl daemon-reload
echo "Removed. Calibration and settings remain in /var/lib/omarchy-fans (delete by hand if wanted)."
