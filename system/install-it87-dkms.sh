#!/bin/bash
# Root half of `install.sh --it87-dkms`.
#
# Installs the out-of-tree frankcrawford/it87 driver via DKMS. On Gigabyte
# boards with the IT8689E, mainline it87 writes PWM registers the fans
# ignore: the firmware's extra SmartFan "vectors" keep driving the fans from
# temperature. The out-of-tree driver disables those vectors (and uses MMIO)
# so manual PWM actually works. DKMS rebuilds it for every new kernel.
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "must run as root" >&2; exit 1; }

pacman -S --needed --noconfirm dkms git
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
git clone --depth 1 https://github.com/frankcrawford/it87 "$work/it87"
(cd "$work/it87" && ./dkms-install.sh)

# Swap the running driver for the DKMS one; the daemon hands fans to the
# BIOS while it is stopped.
systemctl stop omarchy-fand.service || true
modprobe -r it87 || true
depmod -a
modprobe it87
echo "it87 now loaded from: $(modinfo -F filename it87)"
dmesg | grep -i it87 | tail -3 || true
systemctl start omarchy-fand.service
