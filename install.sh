#!/bin/bash
# omarchy-fans installer: builds the Rust binaries, installs the daemon
# (needs root once), and adds the Fans widget to the Omarchy bar.
#
#   ./install.sh            build + install everything
#   ./install.sh --plugin   only (re)install the shell plugin
#   ./install.sh --it87-dkms  Gigabyte boards: install frankcrawford/it87 (fan control fix)
#   ./install.sh --uninstall
set -euo pipefail
src="$(cd "$(dirname "$0")" && pwd)"
plugin_dir="$HOME/.config/omarchy/plugins/qqq.fans"

as_root() {
  if [[ $EUID -eq 0 ]]; then "$@"
  elif [[ -t 0 ]]; then sudo "$@"
  else pkexec "$@"
  fi
}

install_plugin() {
  [[ -L $plugin_dir ]] && rm "$plugin_dir"
  mkdir -p "$plugin_dir"
  cp "$src"/plugin/{manifest.json,Panel.qml,CurveEditor.qml,Model.js} "$plugin_dir/"
  omarchy plugin validate "$plugin_dir"
  if ! omarchy plugin list --json | jq -e '.[] | select(.id == "qqq.fans" and .enabled)' >/dev/null; then
    omarchy plugin enable qqq.fans --section right
  fi
  echo "Fans widget installed in the bar."
}

case "${1:-}" in
  "") ;;
  -h|--help) sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
  --plugin) install_plugin; exit 0 ;;
  --it87-dkms)
    as_root "$src/system/install-it87-dkms.sh"
    echo "Now recalibrate: omarchy-fans calibrate"
    exit 0 ;;
  --uninstall)
    as_root "$src/system/uninstall-system.sh"
    omarchy plugin disable qqq.fans 2>/dev/null || true
    rm -rf "$plugin_dir"
    exit 0 ;;
  *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
esac

cargo build --release --manifest-path "$src/Cargo.toml"
as_root "$src/system/install-system.sh"
install_plugin
cat <<MSG

Done. Next: open the Fans widget and press "Calibrate fans" (about 2 minutes;
fans briefly stop), or run: omarchy-fans calibrate
MSG
