#!/bin/sh
# OpenWave installer — works on Arch, Debian/Ubuntu, Fedora, openSUSE, Void.
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/rikkichy/openwave/main/install.sh | sh
#   ./install.sh                  (from a checkout)
#   PREFIX=/usr ./install.sh      (default is /usr/local)
set -eu

REPO="https://github.com/rikkichy/openwave.git"
PREFIX="${PREFIX:-/usr/local}"

msg()  { printf '\033[1;34m::\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33mwarn:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

# --- privilege escalation -----------------------------------------------------
if [ "$(id -u)" -eq 0 ]; then
    SUDO=""
elif command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
elif command -v doas >/dev/null 2>&1; then
    SUDO="doas"
elif command -v pkexec >/dev/null 2>&1; then
    SUDO="pkexec"
else
    die "need root, sudo, doas, or pkexec"
fi

# --- package manager detection ------------------------------------------------
if command -v pacman >/dev/null 2>&1; then
    PM=pacman
    PKGS="python python-gobject gtk4 libadwaita libusb pipewire alsa-utils libpulse"
    INSTALL_CMD="pacman -S --needed --noconfirm $PKGS"
elif command -v apt-get >/dev/null 2>&1; then
    PM=apt
    PKGS="python3 python3-gi gir1.2-gtk-4.0 gir1.2-adw-1 libadwaita-1-0 libusb-1.0-0 pipewire alsa-utils pulseaudio-utils"
    INSTALL_CMD="apt-get update && apt-get install -y $PKGS"
elif command -v dnf >/dev/null 2>&1; then
    PM=dnf
    PKGS="python3 python3-gobject gtk4 libadwaita libusb1 pipewire alsa-utils pulseaudio-utils"
    INSTALL_CMD="dnf install -y $PKGS"
elif command -v zypper >/dev/null 2>&1; then
    PM=zypper
    PKGS="python3 python3-gobject gtk4 libadwaita-1-0 typelib-1_0-Adw-1 libusb-1_0-0 pipewire alsa-utils pulseaudio-utils"
    INSTALL_CMD="zypper install -y $PKGS"
elif command -v xbps-install >/dev/null 2>&1; then
    PM=xbps
    PKGS="python3-gobject gtk4 libadwaita libusb pipewire alsa-utils pulseaudio-utils"
    INSTALL_CMD="xbps-install -Sy $PKGS"
else
    die "no supported package manager (pacman / apt / dnf / zypper / xbps)"
fi

msg "package manager: $PM"
msg "installing: $PKGS"
$SUDO sh -c "$INSTALL_CMD"

# --- fetch source -------------------------------------------------------------
if [ -f Makefile ] && [ -d wavexlr ] && [ -f wavexlr.desktop ]; then
    SRC="$PWD"
    msg "using checkout: $SRC"
else
    command -v git >/dev/null 2>&1 || die "git not found"
    SRC="$(mktemp -d)/openwave"
    msg "cloning $REPO"
    git clone --depth 1 "$REPO" "$SRC"
fi

# --- install ------------------------------------------------------------------
msg "installing to $PREFIX"
$SUDO make -C "$SRC" install PREFIX="$PREFIX"

# refresh desktop database when possible (failures are harmless)
if command -v update-desktop-database >/dev/null 2>&1; then
    $SUDO update-desktop-database -q "$PREFIX/share/applications" 2>/dev/null || true
fi

msg "done — launch with: openwave"
