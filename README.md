<p align="center">
  <img src="openwave.svg" alt="OpenWave logo" width="128" height="128">
</p>

<h1 align="center">OpenWave</h1>

<p align="center">
  <strong>Elgato Wave controls, native to Linux.</strong><br>
  Built with Python, GTK4, and libadwaita.
</p>

<p align="center">
  <a href="#supported-devices">Supported devices</a> ·
  <a href="#installation">Install</a> ·
  <a href="#usage">Usage</a> ·
  <a href="#how-it-works">How it works</a>
</p>

OpenWave is an open-source, reverse-engineered alternative to Elgato Wave Link for controlling **Wave XLR**, **XLR Dock**, and **Wave:3** hardware on Linux. Adjust your microphone and headphones from a native desktop app, with changes kept in sync with your device and audio system.

## Features

- **Multiple devices** — Every supported unit is opened and polled independently, including two of the same model. The sidebar selects a unit by serial or USB bus/address fallback; queued controls stay bound to that unit.
- **Hotplug** — Adding or removing a device preserves the other connected units. The capture daemon maintains one keepalive per Wave input.
- **Microphone controls** — Adjust gain and mute, with 48 V phantom power on supported XLR devices.
- **Headphone controls** — Set volume, enable low impedance mode on supported devices, or adjust the Wave:3 monitor mix.
- **Hardware and system sync** — Polling at 10 Hz tracks physical buttons and knobs. Mute and headphone volume sync bidirectionally with PipeWire/ALSA.
- **Audio capture fix** — Starts capture before playback, keeps it pinned while running, and releases it after the audio graph shuts down to address the firmware race during warm reboots.
- **System tray** — Keep OpenWave running in the background and mute directly from the tray menu.
- **Guided setup** — Configure USB permissions and the audio service from the app, with support for systemd and runit.

## Supported devices

| Device | USB ID | Available controls |
| --- | --- | --- |
| **Wave XLR** | `0fd9:007d` | Gain, mute, 48 V phantom power, headphone volume, low impedance mode |
| **XLR Dock** (`00a6` variant) | `0fd9:00a6` | Gain, mute, 48 V phantom power, headphone volume, low impedance mode |
| **Wave:3** | `0fd9:0070` | Gain, mute, headphone volume, monitor mix |

Controls are enabled by the device profile. Phantom power and low impedance mode are available on the supported XLR models; monitor mix is available on Wave:3.

The similarly named `0fd9:00c7` Dock variant is unverified and is not enabled.

## Installation

### Quick install

With `curl`, `git`, and `make` available, run:

```bash
curl -fsSL https://raw.githubusercontent.com/rikkichy/openwave/main/install.sh | sh
```

The installer detects **Arch, Debian/Ubuntu, Fedora, openSUSE, or Void** and installs the dependencies and application. The default installation prefix is `/usr/local`.

### Install from a checkout

```bash
git clone https://github.com/rikkichy/openwave.git
cd openwave
./install.sh
```

For a packaging-style layout under `/usr`, use this instead of the last command:

```bash
PREFIX=/usr ./install.sh
```

### Requirements

- Python 3.10+
- GTK4 and libadwaita
- PipeWire and PulseAudio client tools (`pw-*`, `pactl`)
- ALSA utilities (`aplay`, `amixer`)
- libusb 1.0

## Usage

Launch an installed copy:

```bash
openwave
```

Or run from a checkout with the dependencies installed:

```bash
python3 -m wavexlr
```

On first launch, OpenWave prompts you to set up USB permissions and the audio service. USB permission changes use polkit. Reconnect the device when prompted after setup.

### Run in the background

Start hidden in the system tray:

```bash
openwave --hide
```

From a checkout, use `python3 -m wavexlr --hide`.

### Start at login

For the default `/usr/local` installation:

```bash
mkdir -p ~/.config/autostart
cp /usr/local/share/openwave/openwave-autostart.desktop ~/.config/autostart/
```

For a `PREFIX=/usr` or PKGBUILD installation, use `/usr/share/openwave/openwave-autostart.desktop` as the source instead.

The installer also adds an **OpenWave** app launcher entry under `$PREFIX/share/applications`.

### Audio service and init systems

OpenWave detects the service backend at runtime:

| Init system | Setup and behavior |
| --- | --- |
| **systemd** | Installs and enables the user unit at `~/.config/systemd/user/openwave.service`. Service installation and status checks do not require root. |
| **runit** | Uses polkit to install `/etc/sv/wavexlr-audio` and enable it in `/var/service`. The daemon runs as your user through `chpst`. |
| **Other / not detected** | Automatic audio-service management is unsupported. |

On runit, the GUI checks service status with `sv check`. If the supervise FIFO is inaccessible to the current user, as on stock Void, it falls back to scanning `/proc` for the daemon.

### Uninstall

For a full cleanup, first use **Uninstall capture fix** in the app to remove the audio service, audio configuration, and USB permissions. Then remove the application files from a checkout:

```bash
sudo make -C /path/to/openwave uninstall PREFIX=/usr/local
```

Use the same `PREFIX` you installed with. If you enabled startup at login, also remove the autostart entry:

```bash
rm -f ~/.config/autostart/openwave-autostart.desktop
```

## How it works

Wave devices use USB Class control transfers on **endpoint 0** for configuration. On Linux, `snd-usb-audio` normally blocks these transfers because `wIndex=0x3300` routes through interface 0, which belongs to the audio driver.

OpenWave uses **`wIndex=0x3303`**. The firmware checks only the `0x33` prefix, while the kernel sees the unclaimed interface 3 and allows the transfer. This lets OpenWave control the device without detaching its audio driver.

<details>
<summary><strong>USB protocol and configuration layout</strong></summary>

All supported models use `bRequest=0x85` to read and `bRequest=0x05` to write configuration. The Wave XLR and `0fd9:00a6` XLR Dock share a **34-byte** configuration block; Wave:3 uses a **16-byte** block.

Offsets below are in bytes:

| Field | Wave XLR / XLR Dock | Wave:3 |
| --- | --- | --- |
| Gain (`uint16`, Q8.8 dB) | `0` | `0` |
| Mute | `4` | `4` |
| 48 V phantom power | `6` | — |
| Headphone volume (`int16`, Q8.8) | `9` | `7` |
| Monitor mix (`uint16`, Q8.8 percent) | — | `10` |
| Knob / dial mode | `14` | `12` |
| Low impedance mode | `33` | — |

For Wave:3, dial mode values are `1` = gain, `2` = headphones, and `3` = mix. Per-model constants and capabilities live in [`wavexlr/profiles.py`](wavexlr/profiles.py).

</details>

### Device probing

The [`wavexlr.probe`](wavexlr/probe.py) CLI provides `dump`, `watch`, and `poke` commands to verify a device against its profile and help map new fields.

**Quit OpenWave, including its tray process, before probing:** the device services vendor transfers from only one process at a time.

```bash
python3 -m wavexlr.probe --help
```

## Architecture

| Module | Responsibility |
| --- | --- |
| [`device.py`](wavexlr/device.py) | USB backend using raw libusb through `ctypes` and `wIndex=0x3303` |
| [`profiles.py`](wavexlr/profiles.py) | Per-model protocol constants and capabilities |
| [`probe.py`](wavexlr/probe.py) | Protocol verification CLI: `dump`, `watch`, `poke` |
| [`app.py`](wavexlr/app.py) | GTK4/libadwaita interface with 10 Hz polling |
| [`tray.py`](wavexlr/tray.py) | StatusNotifierItem tray icon over D-Bus |
| [`audio.py`](wavexlr/audio.py) | PipeWire capture keepalive for the firmware race condition |
| [`daemon.py`](wavexlr/daemon.py) | Audio-service entry point |
| [`service.py`](wavexlr/service.py) | systemd and runit service management |
| [`setup.py`](wavexlr/setup.py) | First-run USB permissions and audio setup |

## Credits

The USB protocol was reverse-engineered from the macOS Wave Link application using Frida. Inspired by [GoXLR-on-Linux/goxlr-utility](https://github.com/GoXLR-on-Linux/goxlr-utility).

## License

OpenWave is licensed under the [MIT License](LICENSE).
