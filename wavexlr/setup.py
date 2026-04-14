"""First-run setup: udev rule, systemd service."""

import os
import subprocess
import shutil

UDEV_RULE = 'SUBSYSTEM=="usb", ATTR{idVendor}=="0fd9", ATTR{idProduct}=="007d", MODE="0666"'
UDEV_PATH = "/etc/udev/rules.d/99-openwave.rules"
UDEV_PATH_OLD = "/etc/udev/rules.d/99-wavexlr.rules"
SERVICE_NAME = "openwave.service"
APP_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


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
    r = subprocess.run(
        ["systemctl", "--user", "is-enabled", SERVICE_NAME],
        capture_output=True, text=True,
    )
    return r.stdout.strip() == "enabled"


def needs_setup():
    return not udev_installed() or not service_installed()


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
    """Install and enable the systemd user service."""
    service_dir = os.path.expanduser("~/.config/systemd/user")
    os.makedirs(service_dir, exist_ok=True)

    python = shutil.which("python3") or "/usr/bin/python3"

    content = f"""[Unit]
Description=OpenWave Audio Manager
After=pipewire.service wireplumber.service

[Service]
Type=simple
ExecStart={python} -c "from wavexlr.daemon import main; main()"
WorkingDirectory={APP_DIR}
Restart=on-failure
RestartSec=3

[Install]
WantedBy=default.target
"""
    path = os.path.join(service_dir, SERVICE_NAME)
    with open(path, "w") as f:
        f.write(content)

    subprocess.run(["systemctl", "--user", "daemon-reload"], check=True)
    subprocess.run(["systemctl", "--user", "enable", "--now", SERVICE_NAME], check=True)
    return True


def run_setup():
    """Run full first-time setup. Returns (success, message)."""
    messages = []

    if not udev_installed():
        if install_udev():
            messages.append("USB permissions configured")
        else:
            return False, "Failed to set up USB permissions (pkexec cancelled?)"

    if not service_installed():
        try:
            install_service()
            messages.append("Audio service installed and started")
        except Exception as e:
            return False, f"Failed to install service: {e}"

    return True, ". ".join(messages) if messages else "Already configured"


def uninstall_service():
    """Stop, disable, and remove the systemd user service."""
    subprocess.run(["systemctl", "--user", "stop", SERVICE_NAME], capture_output=True)
    subprocess.run(["systemctl", "--user", "disable", SERVICE_NAME], capture_output=True)
    path = os.path.join(os.path.expanduser("~/.config/systemd/user"), SERVICE_NAME)
    try:
        os.unlink(path)
    except FileNotFoundError:
        pass
    subprocess.run(["systemctl", "--user", "daemon-reload"], capture_output=True)


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
    """Remove capture fix service and udev rule. Returns (success, message)."""
    messages = []

    if service_installed():
        uninstall_service()
        messages.append("Audio service removed")

    if udev_installed():
        if uninstall_udev():
            messages.append("USB permissions removed")
        else:
            return False, "Failed to remove USB permissions (pkexec cancelled?)"

    return True, ". ".join(messages) if messages else "Already uninstalled"
