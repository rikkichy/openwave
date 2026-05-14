"""Service-management abstraction.

A backend is selected at import time so the rest of OpenWave stays init-system
agnostic. Currently supported:

    systemd  — user unit installed under ~/.config/systemd/user
    runit    — system service under /etc/sv (install/uninstall use pkexec)
    stub     — neither detected (e.g. macOS, Windows); read-only no-op

Selection rule, in order:
    1. systemd  if `systemctl` is on PATH
    2. runit    if /var/service is a directory and `sv` is on PATH
    3. stub     otherwise

Exposed at module scope: is_running(), is_installed(), install(), uninstall(),
start(), stop(), plus `backend_name` for diagnostics.
"""

import getpass
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

SYSTEMD_UNIT = "openwave.service"
RUNIT_SERVICE = "wavexlr-audio"

_APP_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


class _Stub:
    name = "stub"
    _MSG = "No supported init system detected."

    def is_running(self): return False
    def is_installed(self): return False
    def install(self): raise RuntimeError(self._MSG)
    def uninstall(self): raise RuntimeError(self._MSG)
    def start(self): raise RuntimeError(self._MSG)
    def stop(self): raise RuntimeError(self._MSG)


class _Systemd:
    name = "systemd"

    def _user(self, *args, check=False):
        return subprocess.run(
            ["systemctl", "--user", *args],
            capture_output=True, text=True, check=check,
        )

    def is_running(self):
        try:
            r = subprocess.run(
                ["systemctl", "--user", "is-active", SYSTEMD_UNIT],
                capture_output=True, text=True, timeout=3,
            )
            return r.stdout.strip() == "active"
        except Exception:
            return False

    def is_installed(self):
        r = self._user("is-enabled", SYSTEMD_UNIT)
        return r.stdout.strip() == "enabled"

    def install(self):
        service_dir = os.path.expanduser("~/.config/systemd/user")
        os.makedirs(service_dir, exist_ok=True)
        python = shutil.which("python3") or "/usr/bin/python3"
        content = f"""[Unit]
Description=OpenWave Audio Manager
After=pipewire.service wireplumber.service

[Service]
Type=simple
ExecStart={python} -c "from wavexlr.daemon import main; main()"
WorkingDirectory={_APP_DIR}
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
"""
        with open(os.path.join(service_dir, SYSTEMD_UNIT), "w") as f:
            f.write(content)
        self._user("daemon-reload", check=True)
        self._user("enable", "--now", SYSTEMD_UNIT, check=True)

    def uninstall(self):
        self._user("stop", SYSTEMD_UNIT)
        self._user("disable", SYSTEMD_UNIT)
        path = os.path.join(os.path.expanduser("~/.config/systemd/user"), SYSTEMD_UNIT)
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
        self._user("daemon-reload")

    def start(self):
        self._user("start", SYSTEMD_UNIT, check=True)

    def stop(self):
        self._user("stop", SYSTEMD_UNIT)


def _pkexec_script(script_body):
    """Write `script_body` to a temp file and run it via pkexec.

    Raises RuntimeError on failure (including user-cancelled polkit prompt).
    """
    with tempfile.NamedTemporaryFile("w", suffix=".sh", delete=False) as f:
        f.write(script_body)
        tmp = f.name
    os.chmod(tmp, 0o755)
    try:
        r = subprocess.run(["pkexec", tmp], capture_output=True, text=True)
        if r.returncode != 0:
            msg = (r.stderr or r.stdout or "pkexec cancelled").strip()
            raise RuntimeError(msg)
    finally:
        try:
            os.unlink(tmp)
        except FileNotFoundError:
            pass


def _daemon_proc_alive():
    """Scan /proc for any 'python -m wavexlr.daemon' process.

    Used as a fallback when `sv check` cannot read the supervise/ FIFO (mode
    0700 on stock Void) — the daemon itself runs as the user under chpst, so
    its /proc entry is always readable by the GUI.
    """
    proc = Path("/proc")
    if not proc.is_dir():
        return False
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            cmdline = (entry / "cmdline").read_bytes()
        except (OSError, PermissionError):
            continue
        parts = cmdline.split(b"\0")
        if b"wavexlr.daemon" in parts:
            return True
    return False


class _Runit:
    name = "runit"

    _LINK = Path("/var/service") / RUNIT_SERVICE

    def is_running(self):
        try:
            r = subprocess.run(
                ["sv", "check", RUNIT_SERVICE],
                capture_output=True, text=True, timeout=10,
            )
            if r.returncode == 0:
                return True
            # `sv` reports permission errors on stdout, not stderr — check both.
            msg = (r.stdout + r.stderr).lower()
            if "access denied" in msg or "unable to open" in msg:
                return _daemon_proc_alive()
            return False
        except Exception:
            return False

    def is_installed(self):
        return self._LINK.exists()

    def install(self):
        user = getpass.getuser()
        python = shutil.which("python3") or "/usr/bin/python3"
        script = f"""#!/bin/sh
set -e
mkdir -p /etc/sv/{RUNIT_SERVICE}/log /var/log/{RUNIT_SERVICE}
cat > /etc/sv/{RUNIT_SERVICE}/run <<'RUN'
#!/bin/sh
exec 2>&1
exec chpst -u {user} {python} -c "from wavexlr.daemon import main; main()"
RUN
cat > /etc/sv/{RUNIT_SERVICE}/log/run <<'LOG'
#!/bin/sh
exec svlogd -tt /var/log/{RUNIT_SERVICE}
LOG
chmod 755 /etc/sv/{RUNIT_SERVICE}/run /etc/sv/{RUNIT_SERVICE}/log/run
ln -sf /etc/sv/{RUNIT_SERVICE} /var/service/{RUNIT_SERVICE}
"""
        _pkexec_script(script)

    def uninstall(self):
        script = f"""#!/bin/sh
sv down {RUNIT_SERVICE} 2>/dev/null || true
sleep 1
rm -f /var/service/{RUNIT_SERVICE}
rm -rf /etc/sv/{RUNIT_SERVICE}
rm -rf /var/log/{RUNIT_SERVICE}
"""
        _pkexec_script(script)

    def start(self):
        r = subprocess.run(
            ["sv", "up", RUNIT_SERVICE], capture_output=True, text=True,
        )
        if r.returncode != 0:
            raise RuntimeError(r.stderr.strip() or "sv up failed")

    def stop(self):
        r = subprocess.run(
            ["sv", "down", RUNIT_SERVICE], capture_output=True, text=True,
        )
        if r.returncode != 0:
            raise RuntimeError(r.stderr.strip() or "sv down failed")


def _detect_backend():
    if shutil.which("systemctl") is not None:
        return _Systemd()
    if Path("/var/service").is_dir() and shutil.which("sv") is not None:
        return _Runit()
    return _Stub()


_BACKEND = _detect_backend()
backend_name = _BACKEND.name


def is_running():
    return _BACKEND.is_running()


def is_installed():
    return _BACKEND.is_installed()


def install():
    _BACKEND.install()


def uninstall():
    _BACKEND.uninstall()


def start():
    _BACKEND.start()


def stop():
    _BACKEND.stop()
