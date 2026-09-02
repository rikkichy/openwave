"""First-run setup: udev rule, WirePlumber rule, audio service."""

import os
import subprocess

from . import paths, service

UDEV_RULES = (
    'SUBSYSTEM=="usb", ATTR{idVendor}=="0fd9", ATTR{idProduct}=="007d", MODE="0666"',  # Wave XLR
    'SUBSYSTEM=="usb", ATTR{idVendor}=="0fd9", ATTR{idProduct}=="0070", MODE="0666"',  # Wave:3
)
UDEV_PATH = "/etc/udev/rules.d/99-openwave.rules"
UDEV_PATH_OLD = "/etc/udev/rules.d/99-wavexlr.rules"

WIREPLUMBER_NAME = "51-openwave-wave-xlr.conf"
WIREPLUMBER_PATH = os.path.expanduser(
    "~/.config/wireplumber/wireplumber.conf.d/" + WIREPLUMBER_NAME
)

MIXES_NAME = "52-openwave-mixes.conf"
MIXES_PATH = os.path.expanduser(
    "~/.config/pipewire/pipewire.conf.d/" + MIXES_NAME
)


def wireplumber_source():
    return paths.data_file("wireplumber", WIREPLUMBER_NAME)


def mixes_source():
    return paths.data_file("pipewire", MIXES_NAME)


def udev_installed():
    for path in (UDEV_PATH, UDEV_PATH_OLD):
        try:
            with open(path) as f:
                content = f.read()
            if all(pid in content for pid in ("007d", "0070")):
                return True
        except (FileNotFoundError, PermissionError):
            continue
    return False


def service_installed():
    return service.is_installed()


def service_needs_refresh():
    """Whether an installed service still starts the daemon this install ships.

    Separate from service_installed() so run_uninstall() keeps removing a unit
    that has gone stale, which a stricter service_installed() would make it
    walk past and leave enabled.
    """
    return service.needs_refresh()


def wireplumber_installed():
    try:
        with open(WIREPLUMBER_PATH) as f:
            installed = f.read()
    except OSError:
        return False
    src = wireplumber_source()
    if src is None:
        return True
    try:
        with open(src) as f:
            return f.read() == installed
    except OSError:
        return True


def mixes_installed():
    return os.path.exists(MIXES_PATH)


def needs_setup():
    return (
        not udev_installed()
        or not service_installed()
        or service_needs_refresh()
        or not wireplumber_installed()
        or not mixes_installed()
    )


def anything_installed():
    """Whether any part of the integration is still on disk.

    Not the inverse of needs_setup(): a partial install both needs setup and
    has things left to remove.
    """
    return (
        udev_installed()
        or service_installed()
        or wireplumber_installed()
        or mixes_installed()
    )


def install_udev():
    """Install udev rules via pkexec."""
    rules = "\n".join(UDEV_RULES)
    script = f"""#!/bin/sh
cat > {UDEV_PATH} <<'EOF'
{rules}
EOF
udevadm control --reload-rules
udevadm trigger --subsystem-match=usb --attr-match=idVendor=0fd9 --attr-match=idProduct=007d
udevadm trigger --subsystem-match=usb --attr-match=idVendor=0fd9 --attr-match=idProduct=0070
# Also chmod the device node directly so no replug is needed
for dev in /dev/bus/usb/*/; do
    for f in "$dev"*; do
        if udevadm info --query=property "$f" 2>/dev/null | grep -q 'ID_VENDOR_ID=0fd9'; then
            chmod 0666 "$f"
        fi
    done
done
"""
    tmp = "/tmp/openwave-udev-setup.sh"
    with open(tmp, "w") as f:
        f.write(script)
    os.chmod(tmp, 0o755)

    r = subprocess.run(["pkexec", tmp], capture_output=True, text=True)
    os.unlink(tmp)
    return r.returncode == 0


def install_service():
    """Install and enable the audio service via the active backend."""
    service.install()
    return True


def install_wireplumber():
    """Drop the suspend-disable rule into the user's WirePlumber config."""
    src = wireplumber_source()
    if src is None:
        raise FileNotFoundError(
            f"WirePlumber rule source not found: share/openwave/wireplumber/"
            f"{WIREPLUMBER_NAME} is missing from this install"
        )
    with open(src) as f:
        content = f.read()
    os.makedirs(os.path.dirname(WIREPLUMBER_PATH), exist_ok=True)
    with open(WIREPLUMBER_PATH, "w") as f:
        f.write(content)
    return True


MIX_SINKS = (
    ("openwave_personal_mix", "OpenWave Personal Mix"),
    ("openwave_chat_mix", "OpenWave Chat Mix"),
    ("openwave_record_mix", "OpenWave Record Mix"),
)


def _mix_sink_exists(name):
    """Return True if a PipeWire/Pulse sink with this name is already live."""
    try:
        r = subprocess.run(
            ["pactl", "list", "short", "sinks"],
            capture_output=True, text=True, timeout=3,
        )
    except (FileNotFoundError, subprocess.SubprocessError):
        return False
    return any(line.split("\t", 2)[1] == name for line in r.stdout.splitlines() if "\t" in line)


def _create_mix_sink_live(name, description):
    """Spawn a null sink immediately so it appears without a PipeWire restart."""
    if _mix_sink_exists(name):
        return
    args = (
        "{ "
        "factory.name=support.null-audio-sink "
        f"node.name={name} "
        f'node.description="{description}" '
        "media.class=Audio/Sink "
        "audio.position=[FL FR] "
        "object.linger=true "
        "}"
    )
    try:
        subprocess.run(
            ["pw-cli", "create-node", "adapter", args],
            capture_output=True, text=True, timeout=5,
        )
    except (FileNotFoundError, subprocess.SubprocessError):
        # pw-cli unavailable or PipeWire not reachable — the config file
        # we just wrote will take effect on next PipeWire load.
        pass


def install_mixes():
    """Drop the three virtual mix sinks into the user's PipeWire config."""
    src = mixes_source()
    if src is None:
        raise FileNotFoundError(
            f"Mix sinks config source not found: share/openwave/pipewire/"
            f"{MIXES_NAME} is missing from this install"
        )
    with open(src) as f:
        content = f.read()
    os.makedirs(os.path.dirname(MIXES_PATH), exist_ok=True)
    with open(MIXES_PATH, "w") as f:
        f.write(content)
    for name, desc in MIX_SINKS:
        _create_mix_sink_live(name, desc)
    return True


def run_setup():
    """Run full first-time setup. Returns (success, message)."""
    messages = []

    if not udev_installed():
        if install_udev():
            messages.append("USB permissions configured")
        else:
            return False, "Failed to set up USB permissions (pkexec cancelled?)"

    # Install the WirePlumber rule before starting the service so the daemon's
    # pw-cat attaches to a node that already has suspend disabled.
    if not wireplumber_installed():
        try:
            install_wireplumber()
            messages.append(
                "WirePlumber rule installed (restart wireplumber to apply)"
            )
        except Exception as e:
            return False, f"Failed to install WirePlumber rule: {e}"

    if not mixes_installed():
        try:
            install_mixes()
            messages.append(
                "Mix sinks installed (restart PipeWire to apply)"
            )
        except Exception as e:
            return False, f"Failed to install mix sinks: {e}"

    if not service_installed():
        try:
            install_service()
            messages.append("Audio service installed and started")
        except Exception as e:
            return False, f"Failed to install service: {e}"
    elif service_needs_refresh():
        try:
            install_service()
            messages.append("Audio service updated for this install")
        except Exception as e:
            return False, f"Failed to update service: {e}"

    return True, ". ".join(messages) if messages else "Already configured"


def uninstall_service():
    """Stop, disable, and remove the audio service via the active backend."""
    service.uninstall()


def uninstall_wireplumber():
    """Remove the WirePlumber rule from the user's config."""
    try:
        os.unlink(WIREPLUMBER_PATH)
    except FileNotFoundError:
        return False
    return True


def uninstall_mixes():
    """Remove the mix sinks config from the user's PipeWire config."""
    try:
        os.unlink(MIXES_PATH)
    except FileNotFoundError:
        return False
    return True


def uninstall_udev():
    """Remove udev rule via pkexec."""
    script = f"""#!/bin/sh
rm -f {UDEV_PATH} {UDEV_PATH_OLD}
udevadm control --reload-rules
"""
    tmp = "/tmp/openwave-udev-remove.sh"
    with open(tmp, "w") as f:
        f.write(script)
    os.chmod(tmp, 0o755)
    r = subprocess.run(["pkexec", tmp], capture_output=True, text=True)
    try:
        os.unlink(tmp)
    except FileNotFoundError:
        pass
    return r.returncode == 0


def run_uninstall():
    """Remove capture fix service, WirePlumber rule, and udev rule. Returns (success, message)."""
    messages = []

    if service_installed():
        try:
            uninstall_service()
            messages.append("Audio service removed")
        except Exception as e:
            return False, f"Failed to remove service: {e}"

    if wireplumber_installed():
        if uninstall_wireplumber():
            messages.append("WirePlumber rule removed")

    if mixes_installed():
        if uninstall_mixes():
            messages.append("Mix sinks removed")

    if udev_installed():
        if uninstall_udev():
            messages.append("USB permissions removed")
        else:
            return False, "Failed to remove USB permissions (pkexec cancelled?)"

    return True, ". ".join(messages) if messages else "Already uninstalled"
