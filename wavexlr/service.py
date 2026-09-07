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

Exposed at module scope: is_running(), is_installed(), is_failed(),
needs_refresh(), install(), uninstall(), start(), stop(), plus `backend_name`
for diagnostics.
"""

import getpass
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from . import paths

SYSTEMD_UNIT = "openwave.service"
RUNIT_SERVICE = "wavexlr-audio"

_APP_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# Prefixes holding content-addressed builds, which are never updated in place.
# A path under one of these names one exact build of one exact version, so an
# upgrade does not change it -- it strands it, and the collector deletes it.
_IMMUTABLE_STORES = ("/nix/store/", "/gnu/store/")

# Stable indirections into those stores, in PATH precedence order. Each is a
# symlink the package manager repoints as part of an upgrade, so a path
# through one names whichever build is current rather than a fixed one.
_DURABLE_BINDIRS = (
    "~/.nix-profile/bin",
    "~/.local/state/nix/profile/bin",
    "/etc/profiles/per-user/{user}/bin",
    "/run/current-system/sw/bin",
)


def _durable_bin(path, name):
    """Rewrite a store path to an equivalent that survives an upgrade.

    A unit naming a store path directly is correct until exactly the next
    upgrade: the new build lands on a new path, the old one is collected, and
    ExecStart points at a file that is gone. That surfaces as 203/EXEC on a
    RestartSec= timer -- a loop nothing reports, since `is-enabled` still says
    enabled and the service was last seen working.

    Going through a profile symlink means the service can lag the GUI by one
    build when a checkout runs against an installed package. That is the right
    way round: the alternative is a service that stops existing.
    """
    if not path.startswith(_IMMUTABLE_STORES):
        return path
    user = getpass.getuser()
    for bindir in _DURABLE_BINDIRS:
        candidate = os.path.join(
            os.path.expanduser(bindir.format(user=user)), name
        )
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            return candidate
    return path


def _daemon_launcher():
    """The installed openwave-daemon launcher, or None if there is not one."""
    for candidate in (paths.bin_file("openwave-daemon"),
                      shutil.which("openwave-daemon")):
        if candidate:
            return _durable_bin(candidate, "openwave-daemon")
    return None


def _daemon_command():
    """Command a service manager should run to start the audio daemon.

    Prefer the launcher installed beside this package: it is the only form
    guaranteed to carry the interpreter and import path this install was built
    with. `shutil.which("python3")` was wrong twice over -- it can select an
    interpreter that cannot import wavexlr at all, and when nothing is on PATH
    the /usr/bin/python3 fallback names a file that need not exist, which fails
    203/EXEC inside a unit where Restart= turns it into a silent loop.
    """
    launcher = _daemon_launcher()
    if launcher:
        return launcher

    # Running from a source checkout with nothing installed. sys.executable is
    # at least the interpreter that imported us, so the import path matches.
    return f"{shlex.quote(sys.executable)} -m wavexlr.daemon"


def _daemon_workdir():
    """Directory the daemon command has to run in, or None if it does not care.

    Only the source-checkout fallback needs one, to resolve `-m wavexlr.daemon`
    against the tree it was launched from. An installed launcher carries its
    own import path, and a WorkingDirectory= it does not need is one more path
    that can go stale underneath it -- a directory that has stopped existing
    fails a unit the same 203/EXEC way a missing ExecStart does.
    """
    return None if _daemon_launcher() else _APP_DIR


def _program_exists(command):
    """Whether the program a service's start command names is still present."""
    try:
        argv = shlex.split(command)
    except ValueError:
        return False
    if not argv:
        return False
    # systemd allows "-", "@", "+", "!" and ":" as ExecStart prefixes.
    program = argv[0].lstrip("-@+!:")
    if os.path.isabs(program):
        return os.path.isfile(program) and os.access(program, os.X_OK)
    return shutil.which(program) is not None


class _Stub:
    name = "stub"
    _MSG = "No supported init system detected."

    def is_running(self): return False
    def is_installed(self): return False
    def is_failed(self): return False
    def needs_refresh(self): return False
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

    def is_failed(self):
        return self._user("is-failed", SYSTEMD_UNIT).stdout.strip() == "failed"

    def unit_path(self):
        return os.path.join(
            os.path.expanduser("~/.config/systemd/user"), SYSTEMD_UNIT
        )

    def needs_refresh(self):
        """Whether the installed unit differs from this install's contract.

        `is_installed()` answers a narrower question -- systemd is willing to
        report a unit enabled even when its command vanished or its ordering is
        stale, either of which leaves the capture fix unavailable when needed.
        """
        try:
            with open(self.unit_path()) as f:
                installed = f.read()
        except OSError:
            return False  # nothing to refresh; install() is the path for that

        expected = self._unit_text()
        return (
            installed != expected
            or not _program_exists(_daemon_command())
        )

    def _unit_text(self):
        lines = [
            "[Unit]",
            "Description=OpenWave Audio Manager",
            # Start before the audio graph and stop after it. The capture pin
            # must survive playback teardown or Wave firmware can remain wedged
            # across a warm reboot, whose USB bus reset does not remove power.
            "Wants=pipewire.service wireplumber.service",
            "Before=pipewire.service wireplumber.service",
            # A unit that cannot start retries on the RestartSec= timer with
            # no limit of its own, which is a loop rather than a failure: it
            # never reaches `failed`, where both systemctl and the GUI would
            # show it. Five attempts inside a minute still rides out PipeWire
            # coming up late.
            "StartLimitIntervalSec=60",
            "StartLimitBurst=5",
            "",
            "[Service]",
            "Type=simple",
            f"ExecStart={_daemon_command()}",
        ]
        workdir = _daemon_workdir()
        if workdir:
            lines.append(f"WorkingDirectory={workdir}")
        lines += [
            "Restart=on-failure",
            "RestartSec=3",
            "",
            "[Install]",
            "WantedBy=default.target",
            "",
        ]
        return "\n".join(lines)

    def install(self):
        service_dir = os.path.expanduser("~/.config/systemd/user")
        os.makedirs(service_dir, exist_ok=True)
        with open(self.unit_path(), "w") as f:
            f.write(self._unit_text())
        self._user("daemon-reload", check=True)
        self._user("enable", SYSTEMD_UNIT, check=True)
        # Clears both a `failed` result and a start-rate lockout, neither of
        # which `start` would get past. Restarting rather than starting is
        # what makes this double as the repair path: an already-enabled unit
        # left over from a previous install is running the old ExecStart, and
        # `start` on it is a no-op.
        self._user("reset-failed", SYSTEMD_UNIT)
        self._user("restart", SYSTEMD_UNIT, check=True)

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
        # The daemon is launched as `python3 -c "from wavexlr.daemon import main; main()"`;
        # "wavexlr.daemon" sits inside the -c argument, so use substring match.
        if b"wavexlr.daemon" in cmdline:
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

    def is_failed(self):
        # runsv keeps restarting a service that exits; there is no terminal
        # failed state to report.
        return False

    def needs_refresh(self):
        """Whether the installed run script still starts what we ship now."""
        run = Path("/etc/sv") / RUNIT_SERVICE / "run"
        try:
            script = run.read_text()
        except OSError:
            return False
        return _daemon_command() not in script

    def install(self):
        user = getpass.getuser()
        script = f"""#!/bin/sh
set -e
mkdir -p /etc/sv/{RUNIT_SERVICE}/log /var/log/{RUNIT_SERVICE}
cat > /etc/sv/{RUNIT_SERVICE}/run <<'RUN'
#!/bin/sh
exec 2>&1
exec chpst -u {user} {_daemon_command()}
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


def is_failed():
    return _BACKEND.is_failed()


def needs_refresh():
    return _BACKEND.needs_refresh()


def install():
    _BACKEND.install()


def uninstall():
    _BACKEND.uninstall()


def start():
    _BACKEND.start()


def stop():
    _BACKEND.stop()
