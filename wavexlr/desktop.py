"""Desktop integration: the app drawer entry and starting at login.

Both are freedesktop .desktop files in the user's own directories, so neither
needs privileges and neither belongs in the first-run setup dialog that asks
for a password. The menu entry is written on every launch if it is missing or
stale; autostart is a choice, so it is only ever written when asked for.
"""

import os
import shutil
import sys
import tempfile
import shlex

APP_ID = "openwave"
NAME = "OpenWave"
# Identity must match the packaged wavexlr.desktop: whichever entry the user
# ends up with depends only on install path, so the two disagreeing means the
# app renames itself depending on how it was installed.
COMMENT = "The audio mixing matrix for Linux"
ICON = "openwave"
ICON_FALLBACK = "audio-input-microphone"
# One main category only. AudioVideo plus Settings validates, but
# desktop-file-validate warns it may list the app twice in the menu,
# and a mixer belongs under Audio rather than under system settings.
CATEGORIES = "AudioVideo;Audio;Mixer;"


def _data_home():
    return os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")


def _config_home():
    return os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config")


def menu_entry_path():
    return os.path.join(_data_home(), "applications", f"{APP_ID}.desktop")


def autostart_path():
    return os.path.join(_config_home(), "autostart", f"{APP_ID}.desktop")


def _exec_arg(value):
    """Quote one argument using the Desktop Entry Exec escaping rules."""
    value = value.replace("%", "%%")
    for char in ("\\", '"', "`", "$"):
        value = value.replace(char, "\\" + char)
    return '"' + value.replace("\\", "\\\\") + '"'


def launch_command():
    """How to start OpenWave again, from however it was started this time.

    `openwave` on PATH when there is one, because that survives the checkout
    moving. Otherwise the running interpreter and the module, with an absolute
    path: a desktop file has no working directory to inherit, so a bare
    "python3 -m wavexlr" would only work when the checkout happens to be the
    session's cwd, which it never is at login.
    """
    installed = shutil.which(APP_ID)
    if installed:
        return _exec_arg(installed)
    # PYTHONPATH rather than a flag or a Path= key: a desktop file inherits no
    # working directory, so "python3 -m wavexlr" would only resolve when the
    # checkout happened to be the session's cwd, which at login it never is.
    checkout = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    return " ".join(_exec_arg(arg) for arg in (
        "env", f"PYTHONPATH={checkout}", sys.executable, "-m", "wavexlr"))


def icon_name():
    """The themed icon when it is installed, a stock one when it is not.

    A run-in-place checkout has no openwave.svg in any icon directory, and a
    .desktop entry naming an unresolvable icon renders as the generic broken
    gear — worse than the stock microphone. The check mirrors where the
    Makefile and the Nix wrapper put the icon: hicolor under each XDG data
    dir.
    """
    data_dirs = [_data_home()] + (
        os.environ.get("XDG_DATA_DIRS") or "/usr/local/share:/usr/share"
    ).split(":")
    for base in filter(None, data_dirs):
        if os.path.isfile(os.path.join(
                base, "icons", "hicolor", "scalable", "apps", f"{ICON}.svg")):
            return ICON
    return ICON_FALLBACK


def _render(exec_command, autostart=False):
    lines = [
        "[Desktop Entry]",
        "Type=Application",
        f"Name={NAME}",
        f"Comment={COMMENT}",
        f"Exec={exec_command}",
        f"Icon={icon_name()}",
        f"Categories={CATEGORIES}",
        "Terminal=false",
        # Without this the tray icon and the window are two entries in the
        # dock, because the shell has no way to tell they are one app.
        f"StartupWMClass=com.github.openwave",
        "X-GNOME-UsesNotifications=true",
    ]
    if autostart:
        # Honoured by GNOME and KDE; ignored elsewhere, where the file simply
        # being present is what enables it.
        lines.append("X-GNOME-Autostart-enabled=true")
    return "\n".join(lines) + "\n"


def _write(path, contents):
    directory = os.path.dirname(path)
    os.makedirs(directory, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".openwave-", suffix=".tmp", dir=directory)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(contents)
        os.replace(tmp, path)
    finally:
        try:
            os.unlink(tmp)
        except FileNotFoundError:
            pass


def ensure_menu_entry():
    """Put OpenWave in the app drawer, rewriting a stale entry.

    Rewritten rather than only created, because the Exec line embeds where
    OpenWave was found: an entry written from a checkout that has since been
    installed properly would otherwise keep launching the old path forever.
    """
    path = menu_entry_path()
    wanted = _render(launch_command())
    try:
        try:
            with open(path, encoding="utf-8") as handle:
                if handle.read() == wanted:
                    return False
        except FileNotFoundError:
            pass
        _write(path, wanted)
    except OSError:
        return False
    return True


def autostart_state():
    """(enabled, hidden) for starting at login."""
    path = autostart_path()
    try:
        with open(path, encoding="utf-8") as handle:
            contents = handle.read()
    except OSError:
        return False, False
    values = dict(line.split("=", 1) for line in contents.splitlines()
                  if "=" in line and not line.lstrip().startswith("#"))
    enabled = (values.get("X-GNOME-Autostart-enabled") != "false"
               and values.get("Hidden") != "true")
    try:
        hidden = "--hide" in shlex.split(values.get("Exec", ""))
    except ValueError:
        hidden = False
    return enabled, hidden


def set_autostart(enabled, hidden=False):
    """Turn starting at login on or off. Returns the new (enabled, hidden)."""
    path = autostart_path()
    if not enabled:
        try:
            os.remove(path)
        except FileNotFoundError:
            pass
        except OSError:
            return autostart_state()
        return False, hidden
    command = launch_command() + (" --hide" if hidden else "")
    try:
        _write(path, _render(command, autostart=True))
    except OSError:
        return autostart_state()
    return True, hidden
