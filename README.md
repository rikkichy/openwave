# OpenWave

OpenWave is a Linux control panel and PipeWire mixing matrix for Elgato Wave audio devices, built with GTK4 and libadwaita. It is an independent, reverse-engineered project, not an Elgato product.

## Hardware

| Device | Exact USB ID | Enabled controls |
|---|---|---|
| Wave XLR | `0fd9:007d` | Gain, mute, 48 V phantom power, headphone volume, low impedance mode |
| XLR Dock / Wave XLR MK.2 (`00a6` variant) | `0fd9:00a6` | The XLR profile's controls |
| Wave:3 | `0fd9:0070` | Gain, mute, headphone volume, microphone/PC monitor mix |

**`0fd9:00c7` is unverified and disabled.** A product name is not a compatibility guarantee. Enabled profiles are not a claim that every firmware, the `00a6` unit, or physical multi-device operation has been validated for this release. See [hardware support](docs/hardware-support.md).

Each connected unit has its own control queue. The sidebar identifies units by serial, with USB bus/address as a runtime fallback; selecting another unit does not retarget queued writes. **Before enabling 48 V, check the selected unit and microphone's power requirements.** Phantom power is never included in scenes.

## Mixing

- Add application or hardware-capture sources, and add, rename or remove independent mixes.
- Each source has a trim and each source-to-mix send has its own level/mute. Effective send level is **trim × send**.
- Application streams are assigned to one source and moved into its intake, not copied alongside their original playback. A claimed application's zero sends mean silence; remove its binding or source to stop managing it.
- Group alternative sources for exclusive switching. Unmuting one member mutes its peers; a group may also be entirely muted.
- Choose an output per mix, or **Not monitored** for capture-only use. Personal defaults to Automatic; other mixes default to Not monitored. A missing explicitly selected output stays silent instead of moving sound to another device.
- Select a mix as an input in OBS or a voice application: mixes are published as `openwave_capture_<mix_id>`, not only as monitor sources.
- Save and recall level scenes, or control the running application through its GApplication actions. Scene recall is ordered best-effort, with partial failures reported, not a hardware-atomic transaction.
- Apply source DSP with SWH LADSPA gate/SC4 compression. Calibration measures raw capture and proposes settings; only explicit acceptance applies them.

Keep a voice application's return audio out of the mix selected as its microphone. Internal OpenWave nodes are not eligible as physical outputs, but external loopbacks and acoustic feedback still need careful routing. See [routing, scenes and remote control](docs/ARCHITECTURE.md).

## Install

### Native Linux

On a mutable Arch, Debian/Ubuntu, Fedora, openSUSE or Void host, review and run the installer:

```sh
git clone https://github.com/rikkichy/openwave.git
cd openwave
./install.sh                       # PREFIX defaults to /usr/local
# PREFIX=/usr ./install.sh         # alternate install prefix
```

The installer installs distribution dependencies and uses root, sudo, doas or pkexec for installation. Do not run the GUI as root. For a direct install with dependencies already present:

```sh
make install PREFIX="$HOME/.local"
# sudo make install PREFIX=/usr/local
```

`Makefile` also supports `DESTDIR`, `PYTHON` and `SITEPKG` for packaging. Its default module directory is `<prefix>/share/openwave/site-packages`. Uninstall with the same prefix and any module-directory override used to install:

```sh
make uninstall PREFIX="$HOME/.local"
```

Package removal is separate from removing first-run host integration and user settings. Use OpenWave's setup controls for host integration before removing the application if that integration is no longer wanted.

Requirements: Python 3.10+, PyGObject, GTK4, libadwaita 1.5+, Adwaita icons, libusb 1.0, PipeWire tools (`pipewire`, `pw-cat`, `pw-cli`, `pw-dump`, `pw-link`, `pw-loopback`, `pw-top`), WirePlumber/`wpctl`, PulseAudio client tools/`pactl`, ALSA utilities and SWH LADSPA plugins. Native first-run USB setup uses polkit/`pkexec`. Package names include `swh-plugins` on Arch/Debian/Void and `ladspa-swh-plugins` on Fedora/openSUSE.

### Nix

```sh
nix run github:rikkichy/openwave
```

The flake exports `packages.<system>.openwave` and a default package for `x86_64-linux` and `aarch64-linux`. On NixOS, add that package to `services.udev.packages` for declarative USB permissions, as well as installing it for your user. The native launchers carry their runtime tool paths and LADSPA search path. This describes the packaging contract, not a claim that all target builds have been run.

### Bazzite / Fedora Atomic

Use the [native host checkout or user-prefix installation](docs/install-bazzite.md). Do not use the mutable-distribution installer to modify an Atomic system, and do not assume a distrobox has host USB, ALSA and PipeWire integration.

### Flatpak (experimental)

The repository includes a manifest for experimental **panel and routing** use. With the GNOME Platform and SDK 49 and `flatpak-builder` installed:

```sh
flatpak-builder --user --install --force-clean build-flatpak packaging/flatpak/com.github.openwave.yml
flatpak run com.github.openwave
```

The sandbox does **not** install host udev rules, host service units or host audio configuration, and does not restart host audio services. Configure those on the host through the native installation; see [Bazzite and sandbox boundaries](docs/install-bazzite.md). Raw-device permission in a manifest does not replace host USB permissions. This is a build recipe, not a statement that a Flatpak build or hardware run has passed.

## Run and set up

```sh
openwave                         # installed launcher
openwave --hide                  # hidden only when a tray host is available
openwave --help
openwave --version               # reads VERSION; no GTK, audio or USB startup
python3 -m wavexlr               # from a checkout
```

Native first-run setup offers USB permissions, user audio configuration and capture-keepalive service setup. Read the prompt before approving host changes: audio reconfiguration may interrupt active sessions. The systemd backend uses `openwave.service` under the user's service manager. Runit requires administrator-managed service installation; other init systems do not gain an automatic service installer. Login/tray preferences are available in the application.

The capture daemon maintains one keepalive per supported Wave input. The mixer requires raw capture readiness before Wave playback. The GUI also observes capture and output health without enabling disruptive remedies. Daemon health monitoring is **observation-only by default**:

```sh
openwave-daemon --help
openwave-daemon --version
# openwave-daemon --auto-recover  # explicitly allows bounded disruptive remedies
```

Do not start a second daemon beside an installed running service. `--auto-recover` permits bounded card-profile cycles and sink suspend/resume on confirmed faults; it is not a promise to repair every silent device. See [troubleshooting](docs/troubleshooting.md).

## Reporting problems

Start with the installed privacy-reduced diagnostic command:

```sh
openwave-diag -o openwave-diagnostics.txt
```

From a source checkout, use `python3 -m wavexlr.diag` instead. Native packages install a private Python module tree: use `openwave-diag` and the engineer-only `openwave-probe` launchers outside a checkout, rather than assuming the system Python can import `wavexlr`.

The report does not open USB vendor handles unless `--device` is supplied. `--full` adds private details, **not** USB permission. Close OpenWave, including its tray, before using `--device`. Review any report before posting it to [the issue tracker](https://github.com/rikkichy/openwave/issues). See [diagnostic privacy and audio faults](docs/troubleshooting.md).

## Engineering documentation

- [Architecture, state, scenes and remote control](docs/ARCHITECTURE.md)
- [Exact hardware scope and identity](docs/hardware-support.md)
- [USB protocol and engineer-only probing](docs/protocol.md)

## Credits and license

USB protocol work originates in reverse engineering the macOS Wave Link application with Frida. Documentation incorporates work by Zedwil. Inspired by [goxlr-utility](https://github.com/GoXLR-on-Linux/goxlr-utility). OpenWave is MIT licensed; see [LICENSE](https://github.com/rikkichy/openwave/blob/main/LICENSE).
