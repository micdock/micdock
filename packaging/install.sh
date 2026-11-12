#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
# Install MicDock from the release archive (run it from the unpacked folder).
#
#   ./install.sh            for your user, into ~/.local (no root needed, except for the udev rule)
#   sudo ./install.sh --system   for everyone, into /usr/local, including the udev rule
#   ./install.sh --remove   (or sudo ./install.sh --system --remove) to uninstall
set -eu

here=$(cd "$(dirname "$0")" && pwd)
prefix="$HOME/.local"
system=0
remove=0
for a in "$@"; do
    case "$a" in
        --system) prefix=/usr/local; system=1 ;;
        --remove) remove=1 ;;
        -h|--help) sed -n '3,8p' "$0"; exit 0 ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done

bins="micdock rodego2 ugg2wav micdock-firmware"
apps="$prefix/share/applications"
rule=/etc/udev/rules.d/70-rode.rules

if [ "$remove" = 1 ]; then
    for b in $bins; do rm -f "$prefix/bin/$b"; done
    rm -rf "$prefix/share/micdock"
    rm -f "$apps/micdock.desktop"
    if [ "$system" = 1 ]; then rm -f "$rule"; fi
    echo "MicDock removed from $prefix."
    exit 0
fi

mkdir -p "$prefix/bin" "$prefix/share/micdock" "$apps"
for b in $bins; do install -m 755 "$here/bin/$b" "$prefix/bin/$b"; done
rm -rf "$prefix/share/micdock/docs"
cp -r "$here/share/micdock/docs" "$prefix/share/micdock/docs"
sed "s|^Exec=.*|Exec=$prefix/bin/micdock|; s|^TryExec=.*|TryExec=$prefix/bin/micdock|" \
    "$here/share/applications/micdock.desktop" > "$apps/micdock.desktop"
command -v update-desktop-database >/dev/null && update-desktop-database "$apps" 2>/dev/null || true

if [ "$system" = 1 ]; then
    install -m 644 "$here/70-rode.rules" "$rule"
    udevadm control --reload 2>/dev/null || true
    udevadm trigger 2>/dev/null || true
    echo "MicDock installed into $prefix, with the udev rule."
else
    echo "MicDock installed into $prefix."
    if [ ! -e "$rule" ]; then
        echo
        echo "To let MicDock talk to the units, install the udev rule once:"
        echo "  sudo cp '$here/70-rode.rules' /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger"
    fi
    case ":$PATH:" in *":$prefix/bin:"*) ;; *) echo "Note: $prefix/bin is not in your PATH (the app-menu entry works anyway)." ;; esac
fi
