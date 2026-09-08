#!/bin/bash
# Requires Debian tools, rpm, Python, GTK runtime, dbus-run-session and Xvfb.
# Optional first argument is an exact tag, checked against VERSION and HEAD.
set -euo pipefail
ROOT=$(git rev-parse --show-toplevel)
cd "$ROOT"
if [[ $# -gt 0 ]]; then
    V=$(python3 packaging/version.py --tag "$1")
    [[ $(git rev-parse "$1^{commit}") == $(git rev-parse HEAD) ]]
else
    V=$(python3 packaging/version.py)
fi
OUT="$ROOT/dist"
mkdir -p "$OUT"
# Refuse to mix this build with artifacts left by a different run.
shopt -s nullglob
artifacts=("$OUT"/*)
((${#artifacts[@]} == 0)) || { echo 'dist must be empty' >&2; exit 1; }
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
SOURCE="openwave-$V.tar.gz"
git archive --format=tar.gz --prefix="openwave-$V/" -o "$OUT/$SOURCE" HEAD
tar -xzf "$OUT/$SOURCE" -C "$WORK"
cd "$WORK/openwave-$V"
[[ $(python3 packaging/version.py) == "$V" ]]
STAGE="$WORK/deb"
make install DESTDIR="$STAGE" PREFIX=/usr PYTHON=/usr/bin/python3
# Exercise the installed launchers, not imports from the checkout.
PYTHON=/usr/bin/python3 dbus-run-session -- xvfb-run -a sh packaging/smoke-install.sh "$STAGE/usr"
mkdir -p "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<EOF
Package: openwave
Version: $V
Section: sound
Priority: optional
Architecture: all
Depends: python3 (>= 3.10), python3-gi, gir1.2-gtk-4.0, gir1.2-adw-1, libadwaita-1-0 (>= 1.5), adwaita-icon-theme, libusb-1.0-0, pipewire, pipewire-bin, wireplumber, alsa-utils, pulseaudio-utils, swh-plugins, pkexec
Maintainer: rikkichy <rikkichy@users.noreply.github.com>
Homepage: https://github.com/rikkichy/openwave
Description: Elgato Wave control panel and PipeWire mixing matrix
 Route application and device sources to independent mixes and outputs.
EOF
dpkg-deb --build --root-owner-group "$STAGE" "$OUT/openwave_${V}_all.deb"
RPMROOT="$WORK/rpmbuild"
mkdir -p "$RPMROOT/SOURCES"
cp "$OUT/$SOURCE" "$RPMROOT/SOURCES/"
# Disable host distro Python byte-compilation: this is a noarch private tree,
# interpreted on the target system, not against the release runner's Python.
rpmbuild --define "_topdir $RPMROOT" --define "openwave_version $V" \
    --define 'dist %{nil}' --define '__os_install_post %{nil}' \
    -bb packaging/rpm/openwave.spec
cp "$RPMROOT"/RPMS/noarch/openwave-*.rpm "$OUT/"
# Generate a standalone, checksummed AUR recipe without changing the checkout.
python3 - "$OUT" "$V" <<'PY'
import hashlib
from pathlib import Path
import sys
out, version = Path(sys.argv[1]), sys.argv[2]
archive = f'openwave-{version}.tar.gz'
checksum = hashlib.sha256((out / archive).read_bytes()).hexdigest()
recipe = Path('PKGBUILD').read_text()
recipe = recipe.replace('pkgver=$(cat "${startdir:-.}/VERSION")', f'pkgver={version}')
recipe = recipe.replace('source=()', 'source=("https://github.com/rikkichy/openwave/releases/download/v$pkgver/openwave-$pkgver.tar.gz")')
recipe = recipe.replace('sha256sums=()', f"sha256sums=('{checksum}')")
recipe = recipe.replace('cd "$startdir"', 'cd "$srcdir/openwave-$pkgver"')
(out / 'PKGBUILD').write_text(recipe)
PY
cd "$OUT"
sha256sum "$SOURCE" openwave_*.deb openwave-*.rpm PKGBUILD > sha256sums.txt
