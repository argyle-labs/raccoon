#!/usr/bin/env bash
# setup-controller-wake.sh - Enable wake-from-suspend for USB game controllers
# (dongle/wired). Bluetooth controllers CANNOT wake the system - use a dongle or
# cable. Works on any Linux distro (Bazzite, CachyOS, ...).
set -euo pipefail

RULE=/etc/udev/rules.d/90-usb-wakeup.rules

echo ">> Installing persistent udev rule ($RULE)..."
sudo tee "$RULE" >/dev/null <<'RULEEOF'
# Arm USB wake for all USB devices and host controllers
ACTION=="add", SUBSYSTEM=="usb", TEST=="power/wakeup", ATTR{power/wakeup}="enabled"
RULEEOF

echo ">> Reloading udev rules..."
sudo udevadm control --reload
sudo udevadm trigger --subsystem-match=usb --action=add || true

echo ">> Arming wake on currently-connected USB devices..."
for f in /sys/bus/usb/devices/*/power/wakeup; do
  echo enabled | sudo tee "$f" >/dev/null 2>&1 || true
done

echo ">> Current USB host-controller wake state:"
for d in /sys/bus/usb/devices/usb*/; do
  printf "   %s -> %s\n" "$(cat "$d/product" 2>/dev/null || echo usb)" "$(cat "$d/power/wakeup" 2>/dev/null)"
done

echo
echo "Done. Test:  systemctl suspend   then press a button on a USB/dongle controller."
