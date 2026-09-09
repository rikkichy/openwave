<p align="center">
  <img src="icons/openwave.svg" alt="OpenWave logo" width="128" height="128">
</p>

<h1 align="center">OpenWave</h1>

<p align="center">
  <strong>Elgato Wave controls, native to Linux.</strong><br>
  Built with Rust, GTK4, and libadwaita.
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
- Saved source, send and master volumes keep Python's normalized `wpctl`/Pulse meaning. Existing values are not reinterpreted as linear PCM gain or rewritten during load.
- Application streams are claimed by one source and moved into its intake, not copied alongside their original playback. A claimed application's zero sends mean silence; remove its binding or source to stop managing it.
- Group alternative sources for exclusive switching. Unmuting one member mutes its peers; a group may also be entirely muted.
- Choose an output per mix, or **Not monitored** for capture-only use. Personal defaults to Automatic; other mixes default to Not monitored. A missing explicitly selected output stays silent instead of moving sound to another device.
- Select a published `openwave_capture_<mix_id>` input in OBS or a voice app. Successful master-level changes preserve its identity instead of reconnecting recording clients.
- Scene recall is ordered best-effort, with partial failures reported, not a hardware-atomic transaction. It respects selected-device gain locks and serial-bound device identity.
- SWH LADSPA provides gate/SC4 compression. Calibration measures raw capture without blocking GTK and applies nothing until explicit acceptance. Neutral settings bypass processing; an enabled chain that fails stays silent instead of exposing unprocessed audio.

Keep a voice application's return audio out of the mix selected as its microphone. Internal OpenWave nodes are not physical outputs, but external loopbacks and acoustic feedback still need careful routing. See [routing, scenes and remote control](docs/ARCHITECTURE.md).

## Installation

### Quick install

On a supported mutable distribution, review the installer before running it. Run as your login user, with `curl`, `git`, and `make` available:

```bash
curl -fsSL https://raw.githubusercontent.com/rikkichy/openwave/main/install.sh | sh
```

The installer supports **Arch, Debian/Ubuntu, Fedora, openSUSE, or Void** when the required library versions are available. It installs native dependencies and Rust **1.98.1**, then builds and stages the application as your login user. Only dependency installation and system-file copying request administrator authorization through sudo, doas or pkexec. The default prefix is `/usr/local`; final privileged copying uses a validated root-owned bootstrap, never an elevated Cargo build. **Do not run the installer or GUI as root, or use `sudo make install`.**

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

The native layout puts `openwave`, `openwave-daemon`, `openwave-diag` and `openwave-probe` in `<prefix>/bin`, the private `openwave-maintenance` helper in `<prefix>/libexec`, and assets plus the installation receipt in `<prefix>/share/openwave`. The helper is not a public command or a way to elevate a user-owned checkout.

`Makefile` supports `PREFIX`, `DESTDIR`, `BINARY_DIR` and `CARGO_BUILD_FLAGS`. It builds missing binaries in `target/release` by default and checks their version against canonical `VERSION`. `DESTDIR` is a separate absolute staging root, not a runtime prefix. Package builders must record the appropriate `INSTALL_METHOD`; a manual receipt cannot override package-manager ownership.

An existing installation is not blindly overwritten. The installer can retire a validated legacy application only after explicit confirmation, preserving settings and user integration. Ambiguous, modified or package-owned layouts require resolving the reported conflict first. Installation is not atomic: on interruption, retain the prepared inputs and follow the printed continuation instructions; never elevate a staged binary or an unverified bootstrap.

### Build and run from source

With [Rustup](https://rustup.rs/) and the native development dependencies below available, build as an ordinary user from the checkout root:

```bash
rustup toolchain install 1.98.1 --profile minimal
cargo build --locked --workspace --bins
./target/debug/openwave
```

`rust-toolchain.toml` pins Rust 1.98.1; `Cargo.lock` pins crate dependencies. Build **all workspace binaries**, not only the GUI: direct source runs need the sibling maintenance helper and daemon, as well as the checkout's assets and matching `VERSION`. Keep the checkout and build directory in place while a service refers to them. `make build` produces the release binaries instead. A Nix development environment is also available with `nix develop path:.`.

### Requirements

- GTK **4.14+**, libadwaita **1.5+**, and Adwaita icons.
- Supported release target: **x86-64 Linux**. Native package, CI, Nix and experimental Flatpak release inputs use this target.
- libusb 1.0 and ALSA utilities (`aplay`, `amixer`).
- PipeWire tools: `pipewire`, `pw-cat`, `pw-cli`, `pw-dump`, `pw-link`, `pw-loopback` and `pw-top`.
- WirePlumber/`wpctl` and PulseAudio client tools/`pactl`.
- SWH LADSPA plugins: `swh-plugins` on Arch/Debian/Void; `ladspa-swh-plugins` on Fedora/openSUSE.
- Polkit/`pkexec` for native first-run USB setup.
- Source builds additionally need Rust **1.98.1**, a C compiler/linker, `pkg-config`, Make, and GTK4/libadwaita/libusb development headers. OpenWave has no Python runtime dependency.
- GTK-free maintenance/release-source preparation still requires GLib/GIO and libusb development metadata (`libglib2.0-dev` and `libusb-1.0-0-dev` on Debian/Ubuntu).

### Nix

```bash
nix run github:rikkichy/openwave
```

The flake exports `packages.x86_64-linux.openwave` and its default package. On NixOS, add the package to `services.udev.packages` for declarative USB permissions, as well as installing it for your user. Launchers carry runtime tool paths and the LADSPA search path. Building a package does not establish physical acceptance for every enabled device profile.

### Bazzite / Fedora Atomic

Use the [native host checkout or user-prefix installation](docs/install-bazzite.md). Do not use the mutable-distribution installer to modify an Atomic system, or assume a distrobox has host USB, ALSA and PipeWire integration.

### Flatpak (experimental)

The experimental [Flatpak recipe](packaging/flatpak/com.github.openwave.yml) targets **panel and routing** use with GNOME Platform/SDK **50** and Rust **1.98.1**. The [build entrypoint](packaging/flatpak/build.sh) requires a prepared vendored release archive, its SHA-256, a native x86-64 runner and an empty output directory; a bare checkout is not the offline release input.

This is a packaging route, not a claim of a published artifact or successful build/hardware run on every target. The sandbox does not install host udev rules, services or audio configuration, and does not restart host audio services. Configure those outside the sandbox. Raw-device permission does not replace host USB permissions; see [sandbox boundaries](docs/install-bazzite.md).

## Usage

Launch an installed copy or inspect informational options:

```bash
openwave
openwave --help
openwave --version               # no GTK, audio or USB startup
```

For source runs, use `./target/debug/openwave` after the complete build above. Native first-run setup offers USB permissions, per-user audio configuration and capture-keepalive service setup. USB permission changes require a trusted root-owned installed helper; source and user-prefix builds must use administrator-managed USB rules instead. Setup writes audio configuration for the next relevant audio-service start rather than restarting host PipeWire/WirePlumber. Review changes before interrupting an active session yourself, and reconnect the device when prompted.

### Run in the background

```bash
openwave --hide
```

From a built checkout, use `./target/debug/openwave --hide`. A hidden launch requires a working tray host; without one, the window remains available.

Open **Application menu → Settings → Tray icon color** to choose **White** (the default) or **Black** for your panel. The choice is saved across launches. When a connected microphone is muted, the tray icon turns **red**; after unmuting, it returns to your selected color. A disconnected device keeps the selected color, with its disconnected status shown in the tooltip.

### Upgrade existing launchers

New menu/autostart entries use a stable profile launcher when it is proven to start this installation. Entries from an earlier native build may instead contain that build's canonical executable path. Keep the previous installation available and inspect a confirmed handoff before deleting it or garbage-collecting its Nix generation:

```sh
openwave --migrate-launchers-from /absolute/previous/bin/openwave --dry-run
openwave --migrate-launchers-from /absolute/previous/bin/openwave --yes
```

Use the exact old executable named by the entry; an older Nix entry may name `bin/.openwave-wrapped`. Select the new build in the recognized current profile (`~/.nix-profile` or the per-user/system Nix profile); invoking a versioned store binary directly does not create a stable launcher. Without `--yes`, mutation requires an interactive confirmation. Inspection starts no GUI, USB or audio workers. The handoff preserves login/hidden intent and rechecks the old installation and entry identities; foreign, package-managed, linked, changed or unproven entries remain protected. Missing previous authority is not inferred from launcher text. See [launcher and removal conflicts](docs/troubleshooting.md#installation-and-removal-conflicts).

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

Manual installs carry a bounded, hashed `install-manifest.json` receipt, so uninstalling does not require the original checkout. Identifiable legacy layouts are supported; ambiguous ownership or modified recorded files block deletion instead of guessing. Unrecorded siblings and symlink boundaries are preserved. Partial failure or cancellation reports completed steps; those steps are not rolled back. A private recovery bundle prints a confirmed retry command that works even after application files have been removed. Keep that bundle until recovery finishes; its record does not grant authority to delete changed or unrelated files.

`make uninstall` explicitly confirms **application-files-only** removal. It does not remove user integration or settings, stop live workers, or elevate for system files. For a writable manual installation, stop its GUI and capture service first, then use the same `PREFIX` and any staging `DESTDIR` used at installation. It needs a built or installed maintenance helper (`BINARY_DIR` selects the build directory). Use the GUI/CLI uninstaller for live system installations; package-owned files remain the manager's responsibility.

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

For Wave:3, dial mode values are `1` = gain, `2` = headphones, and `3` = mix. Per-model constants and capabilities live in [`openwave-core/src/profiles.rs`](crates/openwave-core/src/profiles.rs). See [protocol documentation](docs/protocol.md) for scope and safe probing.
</details>

### Device probing and diagnostics

The engineer-only probe provides `dump`, `watch` and `poke`. **Quit OpenWave, including its tray, before vendor probing:** the device services vendor transfers from only one process at a time.

```bash
openwave-probe --help                # installed binary
./target/debug/openwave-probe --help  # after the complete source build
```

For ordinary problem reports, start with privacy-reduced diagnostics:

```bash
openwave-diag -o openwave-diagnostics.txt
```

From a built checkout, use `./target/debug/openwave-diag -o openwave-diagnostics.txt`. Reports include the native compiler/build target; no interpreter or module-path setup is needed.

Diagnostics do not open USB vendor handles unless `--device` is supplied. `--full` adds private details, **not** USB permission. Close OpenWave before `--device`; review reports before posting to [the issue tracker](https://github.com/rikkichy/openwave/issues). See [diagnostic privacy](docs/troubleshooting.md).

## Architecture

|Crate / files|Responsibility|
|---|---|
|[`openwave-core`](crates/openwave-core/src): `profiles.rs`, `protocol.rs`|Enabled capabilities and validated USB configuration blocks|
|[`openwave-core`](crates/openwave-core/src): `model.rs`, `routing.rs`, `scenes.rs`, `effects.rs`, `calibration.rs`, `health.rs`|Compatible state schemas, routing/scene rules, DSP and health policy|
|[`openwave-runtime`](crates/openwave-runtime/src): `device.rs`, `controller.rs`, `controller/`|libusb ownership, serialized per-device work and state reconciliation|
|[`openwave-runtime`](crates/openwave-runtime/src): `mixer.rs`, `audio.rs`, `meter.rs`, `calibration.rs`|Worker-owned graph, application claims, capture readiness, raw measurements and DSP routing|
|[`openwave-runtime`](crates/openwave-runtime/src): `health.rs`, `recovery.rs`, `service.rs`, `bin/openwave-daemon.rs`|Observe-only health, opt-in bounded remedies and capture service|
|[`openwave-runtime`](crates/openwave-runtime/src): `paths.rs`, `store.rs`, `installation.rs`, `uninstall.rs`, `setup.rs`|Assets/state, installation receipts, confirmed removal and native host setup|
|[`openwave-runtime`](crates/openwave-runtime/src): `diag.rs`, `probe.rs`, `bin/`|Public diagnostic/probe binaries and private maintenance entrypoint|
|[`openwave-desktop`](crates/openwave-desktop/src): `app.rs`, `actions.rs`, `ui/`, `tray.rs`, `icons.rs`|GTK/libadwaita interface, compatible session-bus actions and supplied StatusNotifierItem artwork|

Detailed contracts: [architecture/state/actions](docs/ARCHITECTURE.md), [hardware scope](docs/hardware-support.md), and [installation boundaries](docs/install-bazzite.md).

## Credits

The USB protocol was reverse-engineered from the macOS Wave Link application using Frida. Documentation and adapted contributions incorporate work by Zedwil / NyleGarcia. Inspired by [GoXLR-on-Linux/goxlr-utility](https://github.com/GoXLR-on-Linux/goxlr-utility).

## License

OpenWave is licensed under the [MIT License](https://github.com/rikkichy/openwave/blob/main/LICENSE).
