"""First-run setup: udev rule, WirePlumber rule, audio service."""

import os
import subprocess

from . import service

UDEV_RULE = 'SUBSYSTEM=="usb", ATTR{idVendor}=="0fd9", ATTR{idProduct}=="007d", MODE="0666"'
UDEV_PATH = "/etc/udev/rules.d/99-openwave.rules"
UDEV_PATH_OLD = "/etc/udev/rules.d/99-wavexlr.rules"

_APP_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WIREPLUMBER_SOURCES = (
    os.path.join(_APP_DIR, "wireplumber", "51-openwave-wave-xlr.conf"),
    "/usr/share/openwave/wireplumber/51-openwave-wave-xlr.conf",
)
WIREPLUMBER_PATH = os.path.expanduser(
    "~/.config/wireplumber/wireplumber.conf.d/51-openwave-wave-xlr.conf"
)


def udev_installed():
    for path in (UDEV_PATH, UDEV_PATH_OLD):
        try:
            with open(path) as f:
                if "0fd9" in f.read():
                    return True
        except (FileNotFoundError, PermissionError):
            continue
    return False


def service_installed():
    return service.is_installed()


def wireplumber_installed():
    return os.path.exists(WIREPLUMBER_PATH)


def needs_setup():
    return (
        not udev_installed()
        or not service_installed()
        or not wireplumber_installed()
    )


def install_udev():
    """Install udev rule via pkexec."""
    script = f"""#!/bin/sh
echo '{UDEV_RULE}' > {UDEV_PATH}
udevadm control --reload-rules
udevadm trigger --subsystem-match=usb --attr-match=idVendor=0fd9 --attr-match=idProduct=007d
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
    for src in WIREPLUMBER_SOURCES:
        if os.path.exists(src):
            with open(src) as f:
                content = f.read()
            break
    else:
        raise FileNotFoundError(
            "WirePlumber rule source not found. Looked in: "
            + ", ".join(WIREPLUMBER_SOURCES)
        )
    os.makedirs(os.path.dirname(WIREPLUMBER_PATH), exist_ok=True)
    with open(WIREPLUMBER_PATH, "w") as f:
        f.write(content)
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

    if not service_installed():
        try:
            install_service()
            messages.append("Audio service installed and started")
        except Exception as e:
            return False, f"Failed to install service: {e}"

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

    if udev_installed():
        if uninstall_udev():
            messages.append("USB permissions removed")
        else:
            return False, "Failed to remove USB permissions (pkexec cancelled?)"

    return True, ". ".join(messages) if messages else "Already uninstalled"
