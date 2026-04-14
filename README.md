# OpenWave

Linux control application for the **Elgato Wave XLR** microphone interface. A reverse-engineered replacement for Elgato Wave Link, built with GTK4 + Adwaita.

## Features

- **Microphone controls** — Gain, mute (syncs with hardware button)
- **Headphone controls** — Volume (syncs with hardware knob), low impedance mode
- **Hardware sync** — 10 Hz polling keeps the app in sync with physical controls
- **System integration** — Mute and HP volume sync bidirectionally with PipeWire/ALSA
- **Audio capture fix** — Systemd daemon prevents the firmware race condition where mic goes silent
- **System tray** — Runs in background with tray icon, mute from tray menu
- **First-run setup** — Configures udev permissions and audio service automatically

## How it works

The Wave XLR uses USB Class control transfers on endpoint 0 for device configuration. On Linux, `snd-usb-audio` normally blocks these transfers because `wIndex=0x3300` routes through interface 0 (owned by the audio driver). OpenWave uses `wIndex=0x3303` instead — the firmware only checks the `0x33` prefix, while the kernel sees interface 3 (unclaimed) and lets the transfer through. No driver detach needed, audio is never interrupted.

## Requirements

- Python 3.10+
- GTK4, libadwaita
- PipeWire (for audio capture fix)
- libusb 1.0

On Arch/CachyOS:
```bash
sudo pacman -S gtk4 libadwaita python-gobject pipewire
```

## Usage

```bash
git clone https://github.com/rikkichy/openwave.git
cd openwave
python3 -m wavexlr
```

On first launch, OpenWave will prompt to set up USB permissions (via polkit) and install the audio service.

### Start hidden in tray
```bash
python3 -m wavexlr -- --hide
```

### Desktop entry
Copy `wavexlr.desktop` to `~/.local/share/applications/` for app launcher integration.

## Architecture

```
wavexlr/
  device.py   — USB backend (raw libusb via ctypes, wIndex=0x3303 trick)
  app.py      — GTK4/Adwaita UI with 10Hz polling
  tray.py     — StatusNotifierItem tray icon via D-Bus
  audio.py    — PipeWire capture keepalive (fixes firmware race condition)
  daemon.py   — Systemd service entry point
  setup.py    — First-run udev + systemd setup
```

## Protocol documentation

See [`docs/protocol.md`](docs/protocol.md) for the full reverse-engineered USB protocol specification.

## Credits

USB protocol reverse-engineered from the macOS Wave Link application using Frida. Inspired by [GoXLR-on-Linux/goxlr-utility](https://github.com/GoXLR-on-Linux/goxlr-utility).

## License

MIT
