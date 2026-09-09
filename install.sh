#!/bin/sh
# Mutable Arch, Debian/Ubuntu, Fedora, openSUSE and Void source installer.
# Run as the login user. PREFIX defaults to /usr/local; ~/.local is supported.
set -eu

REPO=https://github.com/rikkichy/openwave.git
PREFIX=${PREFIX:-/usr/local}
RUST_VERSION=1.98.1
msg() { printf ':: %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
quote() { printf "'"; printf '%s' "$1" | sed "s/'/'\\\\''/g"; printf "'"; }

[ "$(id -u)" != 0 ] || die 'Run install.sh as your login user, without sudo. Only dependency installation and final system-file copying request administrator authorization.'
if [ -e /run/ostree-booted ] || [ -e /run/bootc ] || [ -e /usr/share/ublue-os ]; then
    die 'Atomic/bootc hosts are not supported by this mutable installer. Use the host image package-layering procedure for GTK/libadwaita/libusb/audio/build dependencies, then build as your user and use make install PREFIX="$HOME/.local". Administrator-managed USB rules are separate; do not run dnf against the immutable host.'
fi
case "$PREFIX" in /*) ;; *) die 'PREFIX must be an absolute canonical path';; esac
case "$PREFIX" in /|*/|*'/../'*|*'/./'*|*'//'*) die 'PREFIX must name a non-root canonical directory without trailing slash';; esac
[ "$(realpath -m -- "$PREFIX")" = "$PREFIX" ] || die 'PREFIX or an ancestor is a symlink or noncanonical path; use its canonical location explicitly.'

SUDO=
for candidate in sudo doas pkexec; do
    if command -v "$candidate" >/dev/null 2>&1; then SUDO=$candidate; break; fi
done
as_root() {
    [ -n "$SUDO" ] || die 'Install the required dependencies as administrator, then use a writable user prefix; sudo, doas or pkexec is required for system installation.'
    "$SUDO" "$@"
}

# Invoke package managers directly, never an elevated shell or cargo build.
if command -v pacman >/dev/null 2>&1; then
    msg 'Installing Arch native dependencies'
    as_root pacman -S --needed --noconfirm git make gcc clang pkgconf curl ca-certificates rustup gtk4 libadwaita adwaita-icon-theme libusb pipewire wireplumber alsa-utils libpulse swh-plugins polkit
elif command -v apt-get >/dev/null 2>&1; then
    msg 'Installing Debian/Ubuntu native dependencies (GTK 4.14+, libadwaita 1.5+)'
    as_root apt-get update
    as_root apt-get install -y git make build-essential clang pkg-config curl ca-certificates libgtk-4-dev libadwaita-1-dev libusb-1.0-0-dev adwaita-icon-theme pipewire pipewire-bin wireplumber alsa-utils pulseaudio-utils swh-plugins pkexec
elif command -v dnf >/dev/null 2>&1; then
    msg 'Installing Fedora native dependencies'
    as_root dnf install -y git make gcc gcc-c++ clang pkgconf-pkg-config curl ca-certificates gtk4-devel libadwaita-devel libusb1-devel adwaita-icon-theme pipewire pipewire-utils wireplumber alsa-utils pulseaudio-utils ladspa-swh-plugins polkit
elif command -v zypper >/dev/null 2>&1; then
    msg 'Installing openSUSE native dependencies'
    as_root zypper install -y git make gcc gcc-c++ clang pkg-config curl ca-certificates gtk4-devel libadwaita-devel libusb-1_0-devel adwaita-icon-theme pipewire pipewire-tools wireplumber alsa-utils pulseaudio-utils ladspa-swh-plugins polkit
elif command -v xbps-install >/dev/null 2>&1; then
    msg 'Installing Void native dependencies'
    as_root xbps-install -Sy git make base-devel clang pkg-config curl ca-certificates gtk4-devel libadwaita-devel libusb-devel adwaita-icon-theme pipewire wireplumber alsa-utils pulseaudio-utils swh-plugins polkit
else
    die 'No supported mutable package manager (pacman / apt / dnf / zypper / xbps).'
fi
pkg-config --atleast-version=4.14 gtk4 || die 'The host provides GTK older than 4.14; use a supported newer host, Nix or the Flatpak build.'
pkg-config --atleast-version=1.5 libadwaita-1 || die 'The host provides libadwaita older than 1.5; use a supported newer host, Nix or the Flatpak build.'

WORK=$(mktemp -d "${TMPDIR:-/tmp}/openwave-install-XXXXXXXXXX")
WORK=$(realpath -e -- "$WORK")
BOOTSTRAP=
BOOTSTRAP_TRUSTED=0
BOOTSTRAP_ID=
BOOTSTRAP_HELPER_ID=
HELPER_SHA256=
RECEIPT_SHA256=
LEGACY_PENDING=0
SUCCESS=0
INSTALL_COMPLETE=0
PAYLOAD_READY=0
PRIVILEGED=0
cleanup() {
    if [ "$SUCCESS" = 1 ]; then
        # Only this invocation's private build/staging directory, never installation files.
        rm -rf -- "$WORK"
    else
        if [ "$INSTALL_COMPLETE" = 1 ]; then
            warn "The native payload was installed, but installer cleanup did not finish. Prepared inputs are retained at $WORK"
            if [ -n "$BOOTSTRAP" ]; then
                warn "Ask the administrator to inspect and remove only this invocation's verified $BOOTSTRAP/openwave-maintenance (if still present) and its then-empty bootstrap directory."
            fi
            return
        fi
        warn "Installation did not complete. Prepared inputs are retained at $WORK"
        if [ -n "$BOOTSTRAP" ]; then
            if [ "$BOOTSTRAP_TRUSTED" = 1 ]; then
                warn "The trusted native bootstrap was retained at $BOOTSTRAP"
                if [ "$LEGACY_PENDING" = 1 ]; then
                    warn 'Finish the confirmed legacy retirement before installing the retained payload:'
                    quote "$SUDO"; printf ' '; quote "$BOOTSTRAP/openwave-maintenance"; printf ' retire-legacy --prefix '; quote "$PREFIX"; printf ' --yes\n'
                fi
            else
                warn "Bootstrap trust was not established or revalidation failed; do not execute it. Ask the administrator to inspect the exact retained directory $BOOTSTRAP before retrying."
            fi
        fi
        if [ "$PAYLOAD_READY" = 1 ]; then
            if [ "$PRIVILEGED" = 1 ]; then
                if [ "$BOOTSTRAP_TRUSTED" = 1 ]; then
                    warn 'After resolving the reported conflict, install the exact retained stage and accepted receipt with:'
                    quote "$SUDO"; printf ' '; quote "$BOOTSTRAP/openwave-maintenance"; printf ' install-payload --stage '; quote "$WORK/stage"; printf ' --prefix '; quote "$PREFIX"; printf ' --expected-sha256 '; quote "$RECEIPT_SHA256"; printf '\n'
                    warn "After successful continuation, the administrator may remove only the verified $BOOTSTRAP/openwave-maintenance and its then-empty bootstrap directory."
                else
                    warn 'No verified trusted helper is available. Retry install.sh as the login user to provision one with administrator authorization; never elevate make or a staged binary.'
                fi
            else
                warn 'After resolving the reported conflict, install the retained prepared payload as your login user with:'
                printf 'make -C '; quote "$PAYLOAD"; printf ' install '; quote "PREFIX=$PREFIX"; printf ' '; quote "BINARY_DIR=$PAYLOAD/bin"; printf ' INSTALL_METHOD=manual\n'
            fi
        fi
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' HUP TERM

# rustup is installed and run only under the login user's home.
if ! command -v rustup >/dev/null 2>&1; then
    if [ -x "${CARGO_HOME:-$HOME/.cargo}/bin/rustup" ]; then
        PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
        export PATH
    else
        msg 'Installing the official Rust toolchain manager for this user'
        curl --proto '=https' --tlsv1.2 --fail --show-error --location https://sh.rustup.rs -o "$WORK/rustup-init.sh"
        sh "$WORK/rustup-init.sh" -y --profile minimal --default-toolchain none --no-modify-path
        PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
        export PATH
    fi
fi
rustup toolchain install "$RUST_VERSION" --profile minimal --component rustfmt --component clippy --component rust-src
export RUSTUP_TOOLCHAIN="$RUST_VERSION"
# Prefer rustup's proxies even if a different distro compiler precedes them.
PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
export PATH

if [ -f Cargo.toml ] && [ -f Cargo.lock ] && [ -f Makefile ] && [ -f wavexlr.desktop ]; then
    SRC=$(pwd -P)
    msg "Using checkout: $SRC"
else
    SRC="$WORK/source"
    git clone --depth 1 "$REPO" "$SRC"
fi
msg 'Building the complete native release as the login user'
make -C "$SRC" build CARGO="rustup run $RUST_VERSION cargo" RUSTC="rustup run $RUST_VERSION rustc"

# Freeze all install inputs before any legacy retirement. Final installation uses
# this private prepared payload, not changing files in the original checkout.
PAYLOAD="$WORK/payload"
install -dm700 "$PAYLOAD"
for file in Makefile VERSION wavexlr.desktop openwave-autostart.desktop \
    data/style.css wireplumber/51-openwave-wave-xlr.conf pipewire/52-openwave-mixes.conf \
    com.github.openwave.metainfo.xml icons/openwave.svg icons/openwave-white.svg \
    icons/openwave-black.svg icons/openwave-red.svg README.md LICENSE \
    packaging/asset-attribution.txt docs/ARCHITECTURE.md docs/hardware-support.md \
    docs/install-bazzite.md docs/protocol.md docs/troubleshooting.md; do
    install -Dm644 "$SRC/$file" "$PAYLOAD/$file"
done
BUILD_BIN=${BINARY_DIR:-${CARGO_TARGET_DIR:-target}/release}
case "$BUILD_BIN" in /*) ;; *) BUILD_BIN="$SRC/$BUILD_BIN";; esac
install -dm755 "$PAYLOAD/bin"
for binary in openwave openwave-daemon openwave-diag openwave-probe openwave-maintenance; do
    install -m755 "$BUILD_BIN/$binary" "$PAYLOAD/bin/$binary"
done
HELPER="$PAYLOAD/bin/openwave-maintenance"
make -C "$PAYLOAD" install PREFIX="$PREFIX" DESTDIR="$WORK/stage" BINARY_DIR="$PAYLOAD/bin" INSTALL_METHOD=manual
HELPER="$WORK/stage$PREFIX/libexec/openwave-maintenance"
HELPER_SHA256=$(sha256sum -- "$HELPER")
HELPER_SHA256=${HELPER_SHA256%% *}
RECEIPT_SHA256=$(sha256sum -- "$WORK/stage$PREFIX/share/openwave/install-manifest.json")
RECEIPT_SHA256=${RECEIPT_SHA256%% *}

# Determine whether final copying needs privilege without creating the target.
ancestor=$PREFIX
while [ ! -e "$ancestor" ]; do ancestor=$(dirname -- "$ancestor"); done
PRIVILEGED=0
[ -w "$ancestor" ] || PRIVILEGED=1
PAYLOAD_READY=1

# An elevated bootstrap may never live under user-writable/symlinked ancestry.
trusted_ancestors() {
    path=$1
    while :; do
        if [ -e "$path" ] || [ -L "$path" ]; then
            [ ! -L "$path" ] && [ -d "$path" ] || die "Untrusted administrator directory: $path"
            owner=$(stat -c %u -- "$path")
            mode=$(stat -c %a -- "$path")
            [ "$owner" = 0 ] && [ "$((0$mode & 022))" = 0 ] || die "Administrator bootstrap requires root-owned, non-group/world-writable ancestry: $path. Use a user prefix or administrator-managed installation."
        fi
        [ "$path" != / ] || break
        path=$(dirname -- "$path")
    done
}

verify_bootstrap() {
    BOOTSTRAP_TRUSTED=0
    trusted_ancestors "$BOOTSTRAP"
    [ "$(stat -c %a -- "$BOOTSTRAP")" = 755 ] || die 'Bootstrap directory must permit trusted traversal.'
    [ "$(stat -c '%d:%i' -- "$BOOTSTRAP")" = "$BOOTSTRAP_ID" ] || die 'Bootstrap directory was replaced; preserving it for administrator inspection.'
    bootstrap_helper="$BOOTSTRAP/openwave-maintenance"
    [ ! -L "$bootstrap_helper" ] && [ -f "$bootstrap_helper" ] || die 'Bootstrap helper must be a regular non-symlink file.'
    [ "$(stat -c %u -- "$bootstrap_helper")" = 0 ] || die 'Bootstrap helper is not administrator-owned.'
    [ "$(stat -c %a -- "$bootstrap_helper")" = 755 ] || die 'Bootstrap helper permissions are not trusted.'
    [ "$(stat -c '%d:%i' -- "$bootstrap_helper")" = "$BOOTSTRAP_HELPER_ID" ] || die 'Bootstrap helper was replaced; preserving it for administrator inspection.'
    bootstrap_digest=$(sha256sum -- "$bootstrap_helper")
    [ "${bootstrap_digest%% *}" = "$HELPER_SHA256" ] || die 'Bootstrap differs from the accepted staged native helper; preserving it for administrator inspection.'
    BOOTSTRAP_TRUSTED=1
}

prepare_bootstrap() {
    [ -z "$BOOTSTRAP" ] || return 0
    trusted_ancestors "$PREFIX/libexec"
    msg 'Administrator authorization is required to copy the prepared helper into a trusted bootstrap outside any old installation inventory.'
    as_root install -dm755 "$PREFIX/libexec"
    trusted_ancestors "$PREFIX/libexec"
    BOOTSTRAP=$(as_root mktemp -d "$PREFIX/libexec/openwave-bootstrap-XXXXXXXXXX")
    BOOTSTRAP_ID=$(stat -c '%d:%i' -- "$BOOTSTRAP")
    trusted_ancestors "$BOOTSTRAP"
    as_root install -m755 "$HELPER" "$BOOTSTRAP/openwave-maintenance"
    as_root chmod 755 "$BOOTSTRAP"
    BOOTSTRAP_HELPER_ID=$(stat -c '%d:%i' -- "$BOOTSTRAP/openwave-maintenance")
    verify_bootstrap
}

if ! "$HELPER" record-install --check --prefix "$PREFIX" --method manual; then
    warn 'The destination cannot be overwritten. Checking whether bounded legacy retirement is possible.'
    "$HELPER" retire-legacy --prefix "$PREFIX" --dry-run || die 'Resolve the reported package/ownership/conflict first. For an unprovable historical layout, use that existing installation’s confirmed uninstaller while preserving settings before retrying.'
    printf 'Retire only this validated legacy application at %s, preserving settings and user integration? Type yes: ' "$PREFIX" >&2
    answer=
    if [ -r /dev/tty ]; then IFS= read -r answer </dev/tty || :; fi
    [ "$answer" = yes ] || die 'Legacy retirement was not confirmed; nothing has been retired.'
    LEGACY_PENDING=1
    if [ "$PRIVILEGED" = 1 ]; then
        prepare_bootstrap
        verify_bootstrap
        as_root "$BOOTSTRAP/openwave-maintenance" retire-legacy --prefix "$PREFIX" --yes
    else
        "$HELPER" retire-legacy --prefix "$PREFIX" --yes
    fi
    LEGACY_PENDING=0
fi

msg "Installing the prepared native payload to $PREFIX (not an atomic upgrade)"
if [ "$PRIVILEGED" = 1 ]; then
    prepare_bootstrap
    verify_bootstrap
    as_root "$BOOTSTRAP/openwave-maintenance" install-payload --stage "$WORK/stage" --prefix "$PREFIX" --expected-sha256 "$RECEIPT_SHA256"
else
    make -C "$PAYLOAD" install PREFIX="$PREFIX" BINARY_DIR="$PAYLOAD/bin" INSTALL_METHOD=manual
fi
INSTALL_COMPLETE=1
if [ -n "$BOOTSTRAP" ]; then
    # Exact files created above only; application inventory removal is native.
    verify_bootstrap
    as_root rm -- "$BOOTSTRAP/openwave-maintenance"
    BOOTSTRAP_TRUSTED=0
    as_root rmdir -- "$BOOTSTRAP"
    BOOTSTRAP=
    BOOTSTRAP_TRUSTED=0
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    if [ "$PRIVILEGED" = 1 ]; then
        as_root update-desktop-database -q "$PREFIX/share/applications" || warn 'Desktop database refresh failed.'
    else
        update-desktop-database -q "$PREFIX/share/applications" || warn 'Desktop database refresh failed.'
    fi
fi
SUCCESS=1
msg "Installed. Launch $PREFIX/bin/openwave as your user. Settings and integration were preserved; review first-run setup separately."
