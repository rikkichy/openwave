"""Confirmed, ownership-aware removal without importing GTK or opening audio/USB."""

import argparse
from dataclasses import dataclass
import errno
import getpass
import json
import os
from pathlib import Path
import shlex
import re
import shutil
import stat
import subprocess
import sys
import time
import tempfile

from . import desktop, installation, service, setup


@dataclass(frozen=True)
class Plan:
    installation: installation.Installation
    actions: tuple[str, ...]
    warnings: tuple[str, ...]
    blockers: tuple[str, ...]

    @property
    def can_execute(self):
        return not self.blockers and self.installation.method != "flatpak"

    @property
    def remove_application(self):
        return self.installation.method == "manual" and not self.blockers


@dataclass(frozen=True)
class Result:
    success: bool
    removed: tuple[str, ...]
    error: str | None
    guidance: str
    app_removed: bool


def _exists(path):
    return os.path.lexists(path)


def _linked(path):
    """Do not mutate configuration supplied through managed symlink trees."""
    path = Path(path)
    return any(part.is_symlink() for part in (path, *path.parents))


def _user_entries():
    config = Path(setup.CONFIG_ROOT)
    return (
        ("Start-at-login entry", Path(desktop.autostart_path())),
        ("Legacy start-at-login entry", config / "autostart/openwave-autostart.desktop"),
        ("User application-menu entry", Path(desktop.menu_entry_path())),
        ("WirePlumber audio rule", Path(setup.WIREPLUMBER_PATH)),
        ("PipeWire mix configuration", Path(setup.MIXES_PATH)),
    )


def _service_fragment():
    if service.backend_name == "systemd":
        path = Path(service._BACKEND.unit_path())
        if _exists(path):
            return path
        try:
            result = subprocess.run(
                ["systemctl", "--user", "show", service.SYSTEMD_UNIT,
                 "--property=FragmentPath", "--value"],
                capture_output=True, text=True, timeout=3,
            )
        except (OSError, subprocess.SubprocessError):
            return None
        value = result.stdout.strip()
        return Path(value) if result.returncode == 0 and value and Path(value).is_absolute() else None
    if service.backend_name == "runit":
        directory = Path("/etc/sv") / service.RUNIT_SERVICE
        active = Path("/var/service") / service.RUNIT_SERVICE
        if active.exists():
            directory = active.resolve()
        path = directory / "run"
        return path if _exists(path) else None
    return None


def _installed_binary(install, name):
    if install.prefix is not None:
        return install.prefix / "bin" / name
    return next((parent / "bin" / name for parent in install.module_dir.parents
                 if (parent / "bin" / name).is_file()), None)


def _daemon_matches(command, workdir, install):
    try:
        args = shlex.split(command)
    except ValueError:
        return False
    if not args:
        return False
    executable = args[0].lstrip("-@+!:")
    if Path(executable).name == "openwave-daemon":
        candidate = Path(executable) if os.path.isabs(executable) else Path(shutil.which(executable) or "")
        expected = _installed_binary(install, "openwave-daemon")
        return expected is not None and candidate.is_absolute() and candidate.resolve() == expected.resolve()
    module = len(args) >= 3 and args[1:3] == ["-m", "wavexlr.daemon"]
    legacy = len(args) >= 3 and args[1] == "-c" and args[2].strip() in (
        "from wavexlr.daemon import main; main()", "from wavexlr.daemon import main;main()",
    )
    if not (module or legacy):
        return False
    if workdir:
        return Path(workdir).resolve() == install.module_dir.parent.resolve()
    # A bare interpreter cannot prove which PYTHONPATH/installation it imports.
    return False


def _service_owned(fragment, install):
    try:
        text = fragment.read_text(encoding="utf-8")
    except OSError:
        return False
    if service.backend_name == "systemd":
        values = dict(line.split("=", 1) for line in text.splitlines()
                      if "=" in line and not line.lstrip().startswith(("#", ";")))
        if not _daemon_matches(values.get("ExecStart", ""), values.get("WorkingDirectory"), install):
            return False
        try:
            command = service._BACKEND._user("show", service.SYSTEMD_UNIT, "--property=ExecStart", "--value", check=True).stdout.strip()
            workdir = service._BACKEND._user("show", service.SYSTEMD_UNIT, "--property=WorkingDirectory", "--value", check=True).stdout.strip()
        except (OSError, subprocess.SubprocessError):
            return False
        match = re.fullmatch(r"\{\s*path=([^;{}]+?)\s*;\s*argv\[\]=([^;{}]+?)\s*;\s*ignore_errors=(?:yes|no)\s*;[^{}]*\}", command)
        if match is None:
            return False
        try:
            arguments = shlex.split(match[2])
        except ValueError:
            return False
        if not arguments or arguments[0] != match[1]:
            return False
        return _daemon_matches(shlex.join(arguments), workdir or None, install)
    if service.backend_name == "runit":
        try:
            line = next(line for line in text.splitlines() if line.startswith("exec chpst -u "))
            args = shlex.split(line)
            if args[3] not in (getpass.getuser(), str(os.getuid())):
                return False
            return _daemon_matches(shlex.join(args[4:]), None, install)
        except (StopIteration, ValueError, IndexError):
            return False
    return False


def _entry_managed(path):
    return _linked(path) or installation._owned((path,)) is not None


def _service_managed(fragment):
    if service.backend_name == "systemd":
        return _entry_managed(fragment) or fragment != Path(service._BACKEND.unit_path())
    expected = Path("/etc/sv") / service.RUNIT_SERVICE
    link = Path("/var/service") / service.RUNIT_SERVICE
    targets = (fragment, fragment.parent / "log/run", link)
    return (_linked(fragment) or _linked(fragment.parent / "log/run")
            or installation._owned(tuple(path for path in targets if _exists(path))) is not None
            or (link.is_symlink() and link.resolve() != expected.resolve()))


def _desktop_owned(path, install):
    try:
        text = path.read_text(encoding="utf-8")
        values = dict(line.split("=", 1) for line in text.splitlines()
                      if "=" in line and not line.lstrip().startswith("#"))
        args = shlex.split(values.get("Exec", ""))
    except (OSError, ValueError):
        return False
    if values.get("Name") not in ("OpenWave", "WaveXLR", "Wave XLR") or not args:
        return False
    name = Path(args[0]).name
    if name in ("openwave", "openwave-daemon"):
        command = args[0] if os.path.isabs(args[0]) else shutil.which(args[0])
        expected = _installed_binary(install, name)
        return command is not None and expected is not None and Path(command).resolve() == expected.resolve()
    if any(args[index:index + 2] == ["-m", "wavexlr"] for index in range(len(args) - 1)):
        python_path = next((arg.removeprefix("PYTHONPATH=") for arg in args
                            if arg.startswith("PYTHONPATH=")), values.get("Path"))
        return bool(python_path) and Path(python_path).resolve() == install.module_dir.parent.resolve()
    return False


def _plan(install):
    actions, warnings, blockers = [], [], []
    if install.problem:
        blockers.append(install.problem)
    if install.method == "unknown" and not blockers:
        blockers.append("The installation cannot be identified safely; no files will be removed.")
    if install.method == "flatpak":
        warnings.append("Host USB permissions and audio services must be removed from a native host installation.")
        return Plan(install, (), tuple(warnings), tuple(blockers))
    fragment = _service_fragment()
    if fragment is not None:
        if not _service_owned(fragment, install):
            blockers.append(f"The service at {fragment} does not identify this installation. It will not be changed.")
        elif _service_managed(fragment):
            actions.append("Stop the capture service")
            warnings.append(f"Keep manager-owned service definition: {fragment}. Remove it through its manager/configuration.")
        else:
            actions.append(f"Stop and remove the capture service: {fragment}")
    for label, path in _user_entries():
        if not _exists(path):
            continue
        if _entry_managed(path):
            warnings.append(f"Keep externally managed configuration: {path}")
        elif path.suffix == ".desktop" and not _desktop_owned(path, install):
            warnings.append(f"Keep a desktop entry for a different or unrecognized installation: {path}")
        elif not path.is_file():
            blockers.append(f"Expected an OpenWave configuration file, not a directory/device: {path}")
        else:
            actions.append(f"Remove {label.lower()}: {path}")
    for path in map(Path, (setup.UDEV_PATH, setup.UDEV_PATH_OLD)):
        if not _exists(path):
            continue
        if _entry_managed(path):
            warnings.append(f"Keep manager-owned USB rule: {path}")
        elif not path.is_file():
            blockers.append(f"Expected a USB rule file: {path}")
        else:
            actions.append(f"Remove OpenWave USB rule: {path}")
    if install.method == "manual":
        try:
            _preflight_application(install)
        except (OSError, ValueError, RuntimeError) as error:
            blockers.append(str(error))
        actions.append(f"Remove {len(install.files)} recorded application files from {install.prefix}")
        if install.legacy:
            warnings.append("Legacy installation: removal is limited to the identified OpenWave files.")
    return Plan(install, tuple(actions), tuple(warnings), tuple(blockers))


def inspect():
    """Read ownership and removal scope; do not start GTK, audio, USB or services."""
    return _plan(installation.inspect_installation())


def describe(plan):
    install = plan.installation
    lines = [f"Installation: {install.method}", f"Location: {install.prefix or install.module_dir}"]
    if plan.actions:
        lines += ["", "Planned actions:", *(f"• {action}" for action in plan.actions)]
    else:
        lines += ["", "No removable native integration was found."]
    lines += ["", "Settings and saved scenes are kept unless you explicitly choose to delete them.",
              "Shared dependencies such as PipeWire and GTK are never removed."]
    if plan.warnings:
        lines += ["", *(f"Note: {warning}" for warning in plan.warnings)]
    if plan.blockers:
        lines += ["", *(f"Cannot continue: {problem}" for problem in plan.blockers)]
    if install.guidance:
        lines += ["", install.guidance]
    return "\n".join(lines)


def _request_app_stop(module_dir, timeout=30):
    """Use the existing same-user session bus without auto-starting a GUI."""
    if not os.environ.get("DBUS_SESSION_BUS_ADDRESS"):
        return
    try:
        from gi.repository import Gio, GLib
    except ImportError:
        return  # The process check below refuses removal if OpenWave is still running.
    try:
        connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    except GLib.Error:
        return
    flags = Gio.DBusCallFlags.NO_AUTO_START
    def owns_name():
        reply = connection.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus",
            "org.freedesktop.DBus", "NameHasOwner", GLib.Variant("(s)", ("com.github.openwave",)),
            None, flags, 2000, None)
        return reply.unpack()[0]
    if not owns_name():
        return
    try:
        connection.call_sync("com.github.openwave", "/com/github/openwave", "org.gtk.Actions", "Activate",
            GLib.Variant("(sava{sv})", ("prepare-uninstall", [GLib.Variant("s", str(module_dir.resolve()))], {})),
            None, flags, 3000, None)
    except GLib.Error as error:
        raise RuntimeError("Close the running OpenWave window and tray before uninstalling; it could not be coordinated.") from error
    deadline = time.monotonic() + timeout
    while owns_name():
        if time.monotonic() >= deadline:
            raise RuntimeError("OpenWave has not finished stopping. No application files were removed; close it and retry.")
        try:
            reply = connection.call_sync("com.github.openwave", "/com/github/openwave", "org.gtk.Actions", "Describe",
                GLib.Variant("(s)", ("prepare-uninstall",)), None, flags, 2000, None)
            values = reply.unpack()[0][2]
            state = values[0] if values else ""
        except GLib.Error:
            if not owns_name():
                return
            raise RuntimeError("The running OpenWave version cannot coordinate removal. Close it and retry.")
        if isinstance(state, str) and state.startswith("error:"):
            raise RuntimeError(state.removeprefix("error:").strip())
        time.sleep(0.1)


def _openwave_processes():
    """Identify, never kill, remaining same-user OpenWave processes."""
    found = []
    proc = Path("/proc")
    if not proc.is_dir():
        raise RuntimeError("Cannot verify running processes on this platform. No application files were removed.")
    for path in proc.iterdir():
        if not path.name.isdigit() or int(path.name) == os.getpid():
            continue
        try:
            if os.geteuid() != 0 and path.stat().st_uid != os.getuid():
                continue
            args = (path / "cmdline").read_bytes().decode("utf-8", errors="replace").split("\0")
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
        module = any(args[index] == "-m" and args[index + 1] in (
            "wavexlr", "wavexlr.daemon", "wavexlr.probe", "wavexlr.uninstall")
            for index in range(len(args) - 1))
        legacy = any(args[index] == "-c" and args[index + 1].strip() in (
            "from wavexlr.daemon import main; main()", "from wavexlr.daemon import main;main()")
            for index in range(len(args) - 1))
        if module or legacy or (args and Path(args[0]).name in ("openwave", "openwave-daemon", "openwave-probe")):
            found.append(path.name)
    return found


def _assert_stopped(timeout=3.0):
    deadline = time.monotonic() + timeout
    remaining = _openwave_processes()
    while remaining and time.monotonic() < deadline:
        time.sleep(0.05)
        remaining = _openwave_processes()
    if remaining:
        raise RuntimeError("OpenWave processes are still running (PID " + ", ".join(remaining) +
                           "). Close them and retry; no broad process-kill operation is performed.")


def _trusted_for_root(path):
    """Elevated deletion must not take authority from another user's writable tree."""
    path = Path(path)
    for part in (path, *path.parents):
        if not _exists(part):
            continue
        info = part.lstat()
        if stat.S_ISLNK(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022:
            raise RuntimeError(f"Refusing elevated removal based on an untrusted installation path: {part}")


def _needs_root(files):
    return os.geteuid() != 0 and any(_exists(path) and not os.access(path.parent, os.W_OK | os.X_OK) for path in files)


def _preflight_application(install):
    installation.validate_installation(install)
    if _needs_root((*install.files, *install.directories)):
        _trusted_for_root(install.receipt or install.module_dir)
        for path in (*install.files, *install.directories):
            _trusted_for_root(path.parent)
        _trusted_for_root(Path(sys.executable).resolve())


def _removal_order(install):
    """Keep the receipt, locator and launchers until other removals succeed."""
    def rank(path):
        if path == install.receipt:
            return 4
        if path.name == "install-location.json":
            return 3
        if install.prefix is not None and path.parent == install.prefix / "bin":
            return 2
        return 1 if path.is_relative_to(install.module_dir) else 0
    return sorted(install.files, key=lambda path: (rank(path), str(path)))


def _remove_application(install):
    _preflight_application(install)
    files = _removal_order(install)
    directories = sorted(install.directories, key=lambda path: (-len(path.parts), str(path)))
    if _needs_root((*files, *directories)):
        interpreter = Path(sys.executable).resolve()
        _trusted_for_root(interpreter)
        code = ("import errno,os\nfiles=" + repr([str(path) for path in files]) +
                "\ndirectories=" + repr([str(path) for path in directories]) + "\n" +
                "for path in files:\n"
                " try: os.unlink(path)\n"
                " except FileNotFoundError: pass\n"
                "for path in directories:\n"
                " try: os.rmdir(path)\n"
                " except OSError as error:\n"
                "  if error.errno not in (errno.ENOENT,errno.ENOTEMPTY,errno.EEXIST): raise\n")
        service._pkexec_script("#!/bin/sh\nset -eu\nexec " + shlex.quote(str(interpreter)) + " -I -c " + shlex.quote(code) + "\n")
    else:
        for path in files:
            try:
                path.unlink()
            except FileNotFoundError:
                pass
        for path in directories:
            try:
                path.rmdir()
            except OSError as error:
                if error.errno not in (errno.ENOENT, errno.ENOTEMPTY, errno.EEXIST):
                    raise
    remaining = [path for path in files if _exists(path)]
    if remaining:
        raise RuntimeError("Application files remain: " + ", ".join(map(str, remaining)))
    return tuple(path for path in directories if _exists(path))


def _remove_integration(install, removed, progress):
    fragment = _service_fragment()
    if fragment is not None:
        if not _service_owned(fragment, install):
            raise RuntimeError(f"Refusing to change a service that does not identify this installation: {fragment}")
        progress("Stopping the capture service")
        if _service_managed(fragment):
            service.stop()
            removed.append("Capture service stopped; manager-owned definition retained")
        else:
            service.uninstall()
            removed.append("Capture service removed")
    _assert_stopped()
    for label, path in _user_entries():
        if not _exists(path) or _entry_managed(path):
            continue
        if path.suffix == ".desktop" and not _desktop_owned(path, install):
            continue
        if not path.is_file():
            raise RuntimeError(f"Configuration path changed unexpectedly: {path}")
        progress(f"Removing {label.lower()}")
        path.unlink()
        removed.append(label + " removed")
    rules = [path for path in map(Path, (setup.UDEV_PATH, setup.UDEV_PATH_OLD)) if _exists(path) and not _entry_managed(path)]
    if rules:
        if any(not path.is_file() for path in rules):
            raise RuntimeError("A USB rule path changed unexpectedly; no privileged removal was attempted.")
        progress("Removing OpenWave USB rules (administrator permission may be required)")
        script = "#!/bin/sh\nset -eu\nrm -f -- " + " ".join(shlex.quote(str(path)) for path in rules) + "\nudevadm control --reload-rules\n"
        service._pkexec_script(script)
        if any(_exists(path) for path in rules):
            raise RuntimeError("USB rule removal did not complete.")
        removed.append("OpenWave USB rules removed")


_recovery_bundles = {}


def _recovery_base():
    return Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state") / "openwave-uninstall"


def _recovery_key(install):
    return (install.prefix, install.module_dir, install.identities)


def _recovery_command(directory):
    return shlex.join([str(Path(sys.executable).resolve()), str(directory / "retry.py"), "--yes"])


def _prepare_recovery(install, delete_settings):
    """Keep executable retry support outside files this operation can remove."""
    key = _recovery_key(install)
    existing = _recovery_bundles.get(key)
    if existing is not None and (existing / "retry.py").is_file():
        return existing
    if Path(__file__).resolve().parent != install.module_dir.resolve():
        return None
    base = _recovery_base()
    base.mkdir(mode=0o700, parents=True, exist_ok=True)
    if _linked(base) or base.stat().st_uid != os.getuid() or base.stat().st_mode & 0o077:
        raise RuntimeError(f"Recovery directory is not private to this user: {base}")
    directory = Path(tempfile.mkdtemp(prefix="openwave-remove-", dir=base))
    try:
        package = directory / "wavexlr"
        package.mkdir()
        for source in Path(__file__).resolve().parent.glob("*.py"):
            shutil.copyfile(source, package / source.name)
        record = {
            "schema": 1, "delete_settings": bool(delete_settings),
            "method": install.method, "prefix": str(install.prefix),
            "module_dir": str(install.module_dir),
            "files": list(map(str, install.files)), "directories": list(map(str, install.directories)),
            "receipt": str(install.receipt) if install.receipt is not None else None,
            "guidance": install.guidance, "legacy": install.legacy,
            "identities": [(str(path), digest) for path, digest in install.identities],
        }
        (directory / "plan.json").write_text(json.dumps(record), encoding="utf-8")
        (directory / "retry.py").write_text(
            "from pathlib import Path\nimport sys\n"
            "root = Path(__file__).resolve().parent\n"
            "sys.path.insert(0, str(root))\n"
            "from wavexlr.uninstall import main\n"
            "raise SystemExit(main(['--resume', str(root / 'plan.json'), *sys.argv[1:]]))\n",
            encoding="utf-8",
        )
    except BaseException:
        shutil.rmtree(directory)
        raise
    _recovery_bundles[key] = directory
    return directory


def _files_gone(install):
    return install.method == "manual" and bool(install.files) and all(not _exists(path) for path in install.files)


def _load_recovery(path):
    path = Path(path).absolute()
    directory = path.parent
    if (path.name != "plan.json" or not directory.name.startswith("openwave-remove-")
            or directory.parent.resolve() != _recovery_base().resolve()
            or _linked(path) or directory.stat().st_uid != os.getuid()
            or directory.stat().st_mode & 0o077 or path.stat().st_size > 4 * 1024 * 1024):
        raise ValueError("Unrecognized or untrusted recovery bundle")
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
        if (type(data["schema"]) is not int or data["schema"] != 1 or data["method"] != "manual"
                or type(data["legacy"]) is not bool or type(data["delete_settings"]) is not bool
                or data["legacy"] != (data["receipt"] is None)):
            raise ValueError("Unrecognized recovery record")
        install = installation.Installation(
            method="manual", prefix=Path(data["prefix"]), module_dir=Path(data["module_dir"]),
            files=tuple(map(Path, data["files"])), directories=tuple(map(Path, data["directories"])),
            receipt=Path(data["receipt"]) if data["receipt"] is not None else None,
            guidance=data["guidance"], legacy=data["legacy"],
            identities=tuple((Path(name), digest) for name, digest in data["identities"]),
        )
    except (KeyError, TypeError) as error:
        raise ValueError("Malformed recovery record") from error
    if _files_gone(install):
        return Plan(install, (), ("Recorded application files are already absent; leftover directories are preserved.",), ()), bool(data["delete_settings"])
    # A writable recovery record is never authority to change a root inventory.
    installation.validate_installation(install)
    return _plan(install), bool(data["delete_settings"])


def execute(plan, *, delete_settings=False, stop_running_app=True, progress=None):
    """Run a previously confirmed plan; return truthful partial results on failure."""
    removed = []
    notify = progress or (lambda message: None)
    install = plan.installation
    recovery = _recovery_bundles.get(_recovery_key(install))
    if not plan.can_execute:
        return Result(False, (), "; ".join(plan.blockers) or "This installation must be removed by its package manager.", install.guidance, False)
    if os.geteuid() == 0:
        return Result(False, (), "Run openwave --uninstall as your login user; it requests administrator permission only when needed.", install.guidance, False)
    if _files_gone(install):
        if recovery is not None:
            shutil.rmtree(recovery)
            _recovery_bundles.pop(_recovery_key(install), None)
        return Result(True, (), None, "Recorded application files are already absent. Any leftover directories were preserved.", True)
    try:
        if plan.remove_application:
            _preflight_application(install)
            recovery = _prepare_recovery(install, delete_settings)
        if stop_running_app:
            notify("Waiting for OpenWave to stop its workers")
            _request_app_stop(install.module_dir)
        _remove_integration(install, removed, notify)
        if delete_settings:
            settings = Path(setup.CONFIG_ROOT) / "openwave"
            if _exists(settings):
                if _linked(settings) or not settings.is_dir():
                    raise RuntimeError(f"Settings are externally managed or not an ordinary directory: {settings}")
                notify("Removing settings and saved scenes")
                shutil.rmtree(settings)
                removed.append("Settings and saved scenes removed")
        residuals = ()
        if plan.remove_application:
            notify("Removing OpenWave application files")
            _assert_stopped()
            residuals = _remove_application(install)
            removed.append("OpenWave application files removed")
        guidance = install.guidance
        if residuals:
            guidance += ("\n" if guidance else "") + "Unrecorded files were preserved in: " + ", ".join(map(str, residuals))
        if recovery is not None:
            shutil.rmtree(recovery)
            _recovery_bundles.pop(_recovery_key(install), None)
        return Result(True, tuple(removed), None, guidance, plan.remove_application)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        guidance = install.guidance
        if recovery is not None:
            guidance += ("\n" if guidance else "") + "Retry without a checkout:\n" + _recovery_command(recovery)
        return Result(False, tuple(removed), str(error), guidance, False)


def _file_only(args):
    """Explicit Makefile compatibility/build removal: never remove user integration."""
    if not args.prefix or not args.sitepkg:
        raise ValueError("--files-only requires --prefix and --sitepkg")
    prefix = Path(args.prefix).absolute()
    module = Path(args.sitepkg).absolute() / "wavexlr"
    if args.destdir:
        stage = Path(args.destdir).absolute()
        if stage.resolve() == Path("/"):
            raise ValueError("DESTDIR must identify a separate staging tree, not /")
        prefix = stage / prefix.relative_to("/")
        module = stage / module.relative_to("/")
    install = installation.inspect_installation(module_dir=module, launcher=prefix / "bin/openwave")
    if install.method != "manual" or install.problem:
        raise ValueError(install.problem or install.guidance or "Refusing file-only removal of a non-manual installation")
    _preflight_application(install)
    print(f"Remove recorded OpenWave application files from {install.prefix}. User integration and settings are not changed.")
    if args.dry_run:
        return 0
    if not args.yes and (not sys.stdin.isatty() or input("Continue? [y/N] ").strip().lower() not in ("y", "yes")):
        return 2
    if not args.destdir:
        _assert_stopped()
    residuals = _remove_application(install)
    print("OpenWave application files removed.")
    if residuals:
        print("Unrecorded files preserved in:", ", ".join(map(str, residuals)))
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(prog="openwave --uninstall", description=__doc__)
    parser.add_argument("--yes", action="store_true", help="Confirm the displayed removal plan without an interactive prompt")
    parser.add_argument("--delete-settings", action="store_true", help="Also remove this user's OpenWave settings and saved scenes")
    parser.add_argument("--dry-run", action="store_true", help="Show the plan without stopping anything or deleting files")
    parser.add_argument("--files-only", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--prefix", help=argparse.SUPPRESS)
    parser.add_argument("--sitepkg", help=argparse.SUPPRESS)
    parser.add_argument("--destdir", default="", help=argparse.SUPPRESS)
    parser.add_argument("--resume", help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    try:
        if args.files_only:
            if args.delete_settings:
                parser.error("--files-only cannot delete user settings")
            return _file_only(args)
        if args.prefix or args.sitepkg or args.destdir:
            parser.error("Installation path overrides require --files-only")
        if args.resume:
            plan, saved_delete = _load_recovery(args.resume)
            args.delete_settings = args.delete_settings or saved_delete
        else:
            plan = inspect()
        print(describe(plan))
        if args.delete_settings:
            print(f"\nAlso delete settings and saved scenes: {Path(setup.CONFIG_ROOT) / 'openwave'}")
        if args.dry_run:
            return 1 if plan.blockers else 0
        if not plan.can_execute:
            print("\nNo changes made.")
            return 1 if plan.blockers else 0
        if not args.yes:
            if not sys.stdin.isatty():
                print("\nNo changes made. Run interactively or pass --yes to confirm.", file=sys.stderr)
                return 2
            question = "Uninstall OpenWave?" if plan.remove_application else "Clean up native integration?"
            if input(question + " [y/N] ").strip().lower() not in ("y", "yes"):
                print("Cancelled; no changes made.")
                return 0
        result = execute(plan, delete_settings=args.delete_settings, progress=lambda text: print(text, flush=True))
        for message in result.removed:
            print(message)
        if result.error:
            print("Removal incomplete: " + result.error, file=sys.stderr)
        elif result.app_removed:
            print("OpenWave uninstalled.")
        else:
            print("Native cleanup complete. Application/package files were not removed.")
        if result.guidance:
            print(result.guidance)
        if args.resume:
            if result.success:
                shutil.rmtree(Path(args.resume).parent)
            else:
                print("Retry without a checkout:\n" + _recovery_command(Path(args.resume).parent))
        return 0 if result.success else 1
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as error:
        print("Removal incomplete: " + str(error), file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("Removal interrupted; completed steps are not rolled back.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())
