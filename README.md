<p align="center">
  <img src="icons/openwave.svg" alt="OpenWave logo" width="128" height="128">
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

OpenWave is an open-source, reverse-engineered alternative to Elgato Wave Link for controlling **Wave XLR**, **XLR Dock**, and **Wave:3** hardware on Linux. Adjust your microphone and headphones from a native desktop app, and route application and capture audio through a PipeWire mixing matrix. OpenWave is an independent project, not an Elgato product.

## Features

- **Multiple devices** — Each supported unit has its own control queue, including two of the same model. The sidebar identifies units by serial, with USB bus/address as a runtime fallback. Selecting another unit does not retarget queued writes.
- **Microphone controls** — Adjust gain and mute, with 48 V phantom power on supported XLR devices.
- **Headphone controls** — Set volume, enable low impedance mode on supported devices, or adjust the Wave:3 monitor mix.
- **Hardware and system sync** — Polling at 10 Hz tracks physical buttons and knobs. Hardware mute and headphone volume synchronize with PipeWire/ALSA.
- **Hotplug and capture readiness** — Adding or removing a device preserves other connected units. The daemon maintains a keepalive per Wave input; the mixer requires raw capture readiness before Wave playback.
- **Dynamic mixing** — Add application or hardware sources, independent mixes, per-send levels and outputs, with published capture inputs for OBS and voice applications.
- **Scenes and remote controls** — Recall matrix levels and serial-bound hardware settings, or control the running app through typed session-bus actions. Scenes never include phantom power.
- **Capture DSP** — Low cut, gate, compression, EQ, delay and optional mono, with cancellable raw-input calibration and explicit proposal acceptance.
- **System tray and setup** — Keep OpenWave in the background, configure login preferences, and manage native USB/audio integration. Health monitoring is observation-only unless daemon recovery is explicitly enabled.

## Supported devices

|Device|USB ID|Available controls|
|---|---|---|
|**Wave XLR**|`0fd9:007d`|Gain, mute, 48 V phantom power, headphone volume, low impedance mode|
|**XLR Dock** (`00a6` variant)|`0fd9:00a6`|Gain, mute, 48 V phantom power, headphone volume, low impedance mode|
|**Wave:3**|`0fd9:0070`|Gain, mute, headphone volume, monitor mix|

Controls are enabled by the device profile. Phantom power and low impedance mode are available on the supported XLR models; monitor mix is available on Wave:3.

**The similarly named `0fd9:00c7` Dock variant is unverified and disabled.** A product name is not a compatibility guarantee. Enabled profiles do not imply validation of every firmware, the `00a6` unit, or physical multi-device operation for this release. See [hardware support](docs/hardware-support.md).

**Before enabling 48 V, check the selected unit and microphone's power requirements.** Scenes and calibration never change phantom power. Capture-to-USB control mapping requires an unambiguous physical identity; a recycled ALSA card number cannot identify a unit.

## Mixing

- Add, rename, reorder or remove sources and mixes. Each source has a trim and each source-to-mix send has its own level/mute: effective send level is **trim × send**.
- Application streams are claimed by one source and moved into its intake, not copied alongside their original playback. A claimed application's zero sends mean silence; remove its binding or source to stop managing it.
- Group alternative sources for exclusive switching. Unmuting one member mutes its peers; a group may also be entirely muted.
- Choose an output per mix, or **Not monitored** for capture-only use. Personal defaults to Automatic; other mixes default to Not monitored. A missing explicitly selected output stays silent instead of moving sound to another device.
- Select a published `openwave_capture_<mix_id>` input in OBS or a voice app. Successful master-level changes preserve its identity instead of reconnecting recording clients.
- Scene recall is ordered best-effort, with partial failures reported, not a hardware-atomic transaction. It respects selected-device gain locks and serial-bound device identity.
- SWH LADSPA provides gate/SC4 compression. Calibration measures raw capture without blocking GTK and applies nothing until explicit acceptance. Neutral settings bypass processing; an enabled chain that fails stays silent instead of exposing unprocessed audio.

Keep a voice application's return audio out of the mix selected as its microphone. Internal OpenWave nodes are not physical outputs, but external loopbacks and acoustic feedback still need careful routing. See [routing, scenes and remote control](docs/ARCHITECTURE.md).

## Installation

### Quick install

On a supported mutable distribution, review the installer before running it. With `curl`, `git`, and `make` available:

```bash
curl -fsSL https://raw.githubusercontent.com/rikkichy/openwave/main/install.sh | sh
```

The installer detects **Arch, Debian/Ubuntu, Fedora, openSUSE, or Void** and installs dependencies and application files. It uses root, sudo, doas or pkexec; the default prefix is `/usr/local`. **Do not run the GUI as root.**

### Install from a checkout

```bash
git clone https://github.com/rikkichy/openwave.git
cd openwave
./install.sh
```

For a packaging-style layout under `/usr`, use `PREFIX=/usr ./install.sh` instead of the last command. With dependencies already present, a user-prefix install is also supported:

```bash
make install PREFIX="$HOME/.local"
```

`Makefile` supports `DESTDIR`, `PYTHON` and `SITEPKG`; the default module directory is `<prefix>/share/openwave/site-packages`.

### Requirements

- Python 3.10+, PyGObject, GTK4, libadwaita **1.5+**, and Adwaita icons.
- libusb 1.0 and ALSA utilities (`aplay`, `amixer`).
- PipeWire tools: `pipewire`, `pw-cat`, `pw-cli`, `pw-dump`, `pw-link`, `pw-loopback` and `pw-top`.
- WirePlumber/`wpctl` and PulseAudio client tools/`pactl`.
- SWH LADSPA plugins: `swh-plugins` on Arch/Debian/Void; `ladspa-swh-plugins` on Fedora/openSUSE.
- Polkit/`pkexec` for native first-run USB setup.

### Nix

```bash
nix run github:rikkichy/openwave
```

The flake exports `packages.<system>.openwave` and a default package for `x86_64-linux` and `aarch64-linux`. On NixOS, add the package to `services.udev.packages` for declarative USB permissions, as well as installing it for your user. Launchers carry runtime tool paths and the LADSPA search path. This describes the packaging contract, not validation of every target build.

### Bazzite / Fedora Atomic

Use the [native host checkout or user-prefix installation](docs/install-bazzite.md). Do not use the mutable-distribution installer to modify an Atomic system, or assume a distrobox has host USB, ALSA and PipeWire integration.

### Flatpak (experimental)

The manifest supports experimental **panel and routing** use. With GNOME Platform/SDK 49 and `flatpak-builder` installed:

```bash
flatpak-builder --user --install --force-clean build-flatpak packaging/flatpak/com.github.openwave.yml
flatpak run com.github.openwave
```

The sandbox does not install host udev rules, services or audio configuration, and does not restart host audio services. Configure those through the native host installation. Raw-device permission does not replace host USB permissions. This is a build recipe, not a claim that a Flatpak build or hardware run has passed; see [sandbox boundaries](docs/install-bazzite.md).

## Usage

Launch an installed copy, inspect informational options, or run from a checkout:

```bash
openwave
openwave --help
openwave --version               # no GTK, audio or USB startup
python3 -m wavexlr               # from a checkout
```

Native first-run setup offers USB permissions, user audio configuration and capture-keepalive service setup. Read the prompt before approving host changes: audio reconfiguration may interrupt active sessions. Reconnect the device when prompted.

### Run in the background

```bash
openwave --hide
```

From a checkout, use `python3 -m wavexlr --hide`. A hidden launch requires a working tray host; without one, the window remains available.

Open **Application menu → Settings → Tray icon color** to choose **White** (the default) or **Black** for your panel. The choice is saved across launches. When a connected microphone is muted, the tray icon turns **red**; after unmuting, it returns to your selected color. A disconnected device keeps the selected color, with its disconnected status shown in the tooltip.

### Start at login

Login preferences are available in the app. Alternatively, for the default `/usr/local` installation:

```bash
mkdir -p ~/.config/autostart
cp /usr/local/share/openwave/openwave-autostart.desktop ~/.config/autostart/
```

For a `PREFIX=/usr` installation, use `/usr/share/openwave/openwave-autostart.desktop` instead. The installer also adds an app launcher under `$PREFIX/share/applications`.

### Audio service and init systems

|Init system|Setup and behavior|
|---|---|
|**systemd**|Installs/enables the user unit `openwave.service`; installation and status checks do not require root.|
|**runit**|Requires administrator-managed service installation; the app does not provide an automatic privileged installer.|
|**Other / not detected**|Automatic audio-service management is unsupported.|

Health is **observation-only by default**, in both the GUI and daemon:

```bash
openwave-daemon --help
openwave-daemon --version
# openwave-daemon --auto-recover  # explicitly permits bounded disruptive remedies
```

Do not start a second daemon beside the installed service. `--auto-recover` permits bounded card-profile cycles and sink suspend/resume on confirmed faults; it does not promise to repair every silent device. Capture xruns and no-data faults share a recovery budget; muted, unknown, absent or recreated observations cannot refill it. See [troubleshooting](docs/troubleshooting.md).

### Uninstall

Choose **Application menu → Uninstall OpenWave…**. The same action is available in first-run setup, including failed setup and the replug/Continue screen.

For a manual or one-line installation, the dialog removes OpenWave's application files, capture service, owned audio/USB rules and start-at-login entries. **Settings and saved scenes are kept by default**; deleting them requires selecting the separate checkbox. Shared dependencies such as PipeWire and GTK are never removed.

The headless CLI works without opening the GUI, audio devices or USB controls:

```bash
openwave --uninstall --dry-run          # inspect ownership and planned actions
openwave --uninstall                    # confirm interactively
openwave --uninstall --yes              # explicitly confirm without a prompt
openwave --uninstall --delete-settings  # also remove settings and saved scenes
```

Run this as your login user, **not with sudo**. Administrator permission is requested only for owned system files that require it. The uninstaller coordinates with the running copy of the same installation and waits for workers to stop; it never broadly kills other audio applications.

**Package-managed installations** retain their package files. OpenWave can clean up its own native integration, then displays the package-manager or Nix configuration instructions. Flatpak directs removal to the host and cannot remove native host integration. Externally managed symlinks and package-owned configuration are retained.

Manual installs carry an exact installation inventory, so uninstalling does not require the original checkout. Identifiable legacy layouts are supported; ambiguous ownership or modified recorded files block deletion instead of guessing. Partial failures are reported with completed steps and a retry option. If application files were already partially removed, a private recovery bundle provides a printed retry command without requiring a checkout.

`make uninstall` is an explicit **application-files-only** compatibility/build target; it does not remove user integration or settings. Use the same `PREFIX`, `SITEPKG` and any staging `DESTDIR` used at installation.

## How it works

Wave devices use USB Class control transfers on **endpoint 0** for configuration. On Linux, `snd-usb-audio` normally blocks transfers using `wIndex=0x3300`, because interface 0 belongs to the audio driver.

OpenWave uses **`wIndex=0x3303`**. The firmware checks the `0x33` prefix, while the kernel sees unclaimed interface 3. This permits controls without detaching the audio driver.

<details>
<summary><strong>USB protocol and configuration layout</strong></summary>

Supported models use `bRequest=0x85` to read and `bRequest=0x05` to write configuration. Wave XLR and the `0fd9:00a6` XLR Dock share a **34-byte** block; Wave:3 uses a **16-byte** block.

|Field|Wave XLR / XLR Dock|Wave:3|
|---|---|---|
|Gain (`uint16`, Q8.8 dB)|`0`|`0`|
|Mute|`4`|`4`|
|48 V phantom power|`6`|—|
|Headphone volume (`int16`, Q8.8)|`9`|`7`|
|Monitor mix (`uint16`, Q8.8 percent)|—|`10`|
|Knob / dial mode|`14`|`12`|
|Low impedance mode|`33`|—|

For Wave:3, dial mode values are `1` = gain, `2` = headphones, and `3` = mix. Per-model constants and capabilities live in [`wavexlr/profiles.py`](wavexlr/profiles.py). See [protocol documentation](docs/protocol.md) for scope and safe probing.
</details>

### Device probing and diagnostics

The engineer-only probe provides `dump`, `watch` and `poke`. **Quit OpenWave, including its tray, before vendor probing:** the device services vendor transfers from only one process at a time.

```bash
openwave-probe --help            # installed launcher
python3 -m wavexlr.probe --help   # from a checkout
```

For ordinary problem reports, start with privacy-reduced diagnostics:

```bash
openwave-diag -o openwave-diagnostics.txt
```

Use `python3 -m wavexlr.diag` from a checkout. Native packages use a private Python module tree; system Python is not guaranteed to import `wavexlr` outside that checkout.

Diagnostics do not open USB vendor handles unless `--device` is supplied. `--full` adds private details, **not** USB permission. Close OpenWave before `--device`; review reports before posting to [the issue tracker](https://github.com/rikkichy/openwave/issues). See [diagnostic privacy](docs/troubleshooting.md).

## Architecture

|Module|Responsibility|
|---|---|
|[`device.py`](wavexlr/device.py), [`profiles.py`](wavexlr/profiles.py)|Raw libusb controls, exact physical identity and model-specific capabilities|
|[`app.py`](wavexlr/app.py), [`scheduler.py`](wavexlr/scheduler.py)|GTK interface, polling and per-device control queues|
|[`mixer.py`](wavexlr/mixer.py)|Worker-owned graph mutations, stream claims, sends, outputs and published captures|
|[`sources.py`](wavexlr/sources.py), [`mixes.py`](wavexlr/mixes.py)|Stable source/mix definitions|
|[`scenes.py`](wavexlr/scenes.py)|Safe level snapshots and serial-bound hardware recall|
|[`effects.py`](wavexlr/effects.py), [`calibrate.py`](wavexlr/calibrate.py)|Validated DSP configuration and cancellable raw-input measurements|
|[`meter.py`](wavexlr/meter.py), [`audio.py`](wavexlr/audio.py)|Bounded raw metering and capture keepalives|
|[`health.py`](wavexlr/health.py), [`recovery.py`](wavexlr/recovery.py)|Observe-only health and bounded opt-in remedies|
|[`daemon.py`](wavexlr/daemon.py), [`service.py`](wavexlr/service.py)|Headless entry point and service-manager integration|
|[`setup.py`](wavexlr/setup.py), [`tray.py`](wavexlr/tray.py)|Native setup and StatusNotifierItem tray integration|

Detailed contracts: [architecture/state/actions](docs/ARCHITECTURE.md), [hardware scope](docs/hardware-support.md), and [installation boundaries](docs/install-bazzite.md).

## Credits

The USB protocol was reverse-engineered from the macOS Wave Link application using Frida. Documentation and adapted contributions incorporate work by Zedwil / NyleGarcia. Inspired by [GoXLR-on-Linux/goxlr-utility](https://github.com/GoXLR-on-Linux/goxlr-utility).

## License

OpenWave is licensed under the [MIT License](https://github.com/rikkichy/openwave/blob/main/LICENSE).
