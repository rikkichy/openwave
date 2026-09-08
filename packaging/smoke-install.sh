#!/bin/sh
# Run under dbus-run-session -- xvfb-run -a. Never open audio devices/services.
set -eu
PREFIX=$(realpath "${1:?usage: smoke-install.sh INSTALLED_PREFIX}")
PYTHON=${PYTHON:-python3}
SITEPKG=${SITEPKG:-$PREFIX/share/openwave/site-packages}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT HUP INT TERM
export HOME="$WORK" XDG_CONFIG_HOME="$WORK/config" XDG_DATA_HOME="$WORK/data"
export PYTHONDONTWRITEBYTECODE=1
cd "$WORK"
for launcher in openwave openwave-daemon; do
    timeout 15 "$PREFIX/bin/$launcher" --help
    actual=$(timeout 15 "$PREFIX/bin/$launcher" --version)
    expected=$(cat "$PREFIX/share/openwave/VERSION")
    case "$actual" in *"$expected"*) ;; *) echo "Wrong installed version: $actual" >&2; exit 1;; esac
done
export PYTHONPATH="$SITEPKG"
"$PYTHON" - "$PREFIX" <<'PY'
import importlib
from pathlib import Path
import pkgutil
import sys
import gi

gi.require_version('Gtk', '4.0')
gi.require_version('Adw', '1')
from gi.repository import Adw, GLib, Gtk
import wavexlr

prefix = Path(sys.argv[1]).resolve()
assert Path(wavexlr.__file__).resolve().is_relative_to(prefix)
for module in pkgutil.walk_packages(wavexlr.__path__, 'wavexlr.'):
    if module.name not in {'wavexlr.__main__', 'wavexlr.daemon'}:
        importlib.import_module(module.name)
from wavexlr.paths import data_file
for resource in [('VERSION',), ('pipewire', '52-openwave-mixes.conf'),
                 ('wireplumber', '51-openwave-wave-xlr.conf'), ('icons', 'openwave.svg')]:
    path = data_file(*resource)
    assert path and Path(path).resolve().is_relative_to(prefix), resource
Gtk.init()
Adw.init()
window = Gtk.Window(title='Installed OpenWave GTK smoke')
window.set_child(Gtk.Label(label='Installed imports and GTK initialized'))
window.present()
context = GLib.MainContext.default()
while context.pending():
    context.iteration(False)
assert window.get_realized()
window.destroy()
print('Installed launchers, all module imports, runtime assets and GTK passed')
PY
