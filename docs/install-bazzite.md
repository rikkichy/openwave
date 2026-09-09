# Bazzite and Fedora Atomic installation

Run OpenWave on the **host** so it uses the host's PipeWire session, ALSA cards and USB permissions. A native checkout or user-prefix installation avoids modifying the immutable `/usr` image. This guidance also applies to Silverblue/Kinoite-style systems, but is not a claim of a completed build or hardware test on each image.

## Native host route

Check runtime and build dependencies on the host; image contents differ:

```sh
rpm -q gtk4 libadwaita adwaita-icon-theme libusb1 \
  pipewire pipewire-utils wireplumber alsa-utils pulseaudio-utils \
  ladspa-swh-plugins polkit git make gcc pkgconf-pkg-config \
  gtk4-devel libadwaita-devel libusb1-devel
```

GTK **4.14+** and libadwaita **1.5+** are required. Use your image's supported package-layering procedure for missing packages. For example, if the development libraries and DSP plugins are missing:

```sh
rpm-ostree install gtk4-devel libadwaita-devel libusb1-devel ladspa-swh-plugins
```

Reboot into the updated deployment when required. On images that do not support `rpm-ostree install`, follow the image vendor's procedure instead. Do not use `dnf` against the immutable host. `install.sh` rejects Atomic/bootc hosts; it is not a layering tool.

Install [Rustup](https://rustup.rs/) for your ordinary user, then clone, build and launch on the host:

```sh
git clone https://github.com/rikkichy/openwave.git "$HOME/openwave"
cd "$HOME/openwave"
rustup toolchain install 1.98.1 --profile minimal
cargo build --locked --workspace --bins
./target/debug/openwave
```

The checkout pins Rust **1.98.1** and locked crate dependencies. Build all workspace binaries: source runs require the sibling private maintenance helper, daemon and checkout assets, not just the GUI executable. Keep the checkout and build directory at those paths while launchers or the service refer to them. Alternatively, with dependencies present, install into your home as the login user:

```sh
make install PREFIX="$HOME/.local"
"$HOME/.local/bin/openwave"
```

The public binaries live in `<prefix>/bin`, the private maintenance helper in `<prefix>/libexec`, and assets plus the hashed installation receipt in `<prefix>/share/openwave`. Add `~/.local/bin` to your session's PATH if needed. Do not use `sudo make install` or elevate a helper from your checkout/home directory.

Use **Application menu → Uninstall OpenWave…** or `"$HOME/.local/bin/openwave" --uninstall` to inspect and confirm removal of a manual user-prefix installation and eligible owned integration without a checkout. `--dry-run` only inspects; `--yes` supplies explicit noninteractive confirmation. Settings/scenes are preserved unless separately selected; package-managed files and declarative configuration stay with their manager. Changed recorded files, unrecorded siblings and symlink boundaries are not blindly deleted. If removal is interrupted, retain its recovery bundle and use the exact printed retry command. `make uninstall` is only confirmed application-files removal, not service/settings cleanup; see [uninstall options](../README.md#uninstall).

## First-run host integration

Prepare USB permissions through your administrator **before** using native setup from a checkout or user prefix. Its helper is user-owned and cannot safely be elevated; the GUI's privileged USB installer is available only with a trusted root-owned native installation. Do not copy a user-writable helper into a privileged command line to bypass this check.

For the enabled profiles, OpenWave's supplied policy is `/etc/udev/rules.d/99-openwave.rules` with:

```udev
SUBSYSTEM=="usb", ATTR{idVendor}=="0fd9", ATTR{idProduct}=="007d", MODE="0666"
SUBSYSTEM=="usb", ATTR{idVendor}=="0fd9", ATTR{idProduct}=="00a6", MODE="0666"
SUBSYSTEM=="usb", ATTR{idVendor}=="0fd9", ATTR{idProduct}=="0070", MODE="0666"
```

`/etc` remains host configuration even when `/usr` is immutable. Have the administrator install/reload the rules and reconnect the device when safe. This policy grants local users raw access to these devices; administrators may choose a site-specific access policy instead. Do not overwrite a package-owned or existing administrator rule. Administrator-managed rules also need administrator-managed removal.

The native GUI can then configure user integration; review it before applying changes:

- The capture daemon is a **user** service (`openwave.service` on systemd), not a root audio process. Its unit lives under `~/.config/systemd/user`.
- WirePlumber/PipeWire configuration is per-user host configuration. Setup writes it for the next relevant audio-service start and does not restart host PipeWire/WirePlumber. Plan any restart yourself outside recordings or calls.
- Login/tray preferences are managed in the application. Close the tray process too before vendor-level diagnostics or probing.

Health remedies remain disabled unless the daemon is explicitly started with `--auto-recover`. Ordinary capture keepalive does not imply permission for destructive recovery. See [troubleshooting](troubleshooting.md).

## Experimental Flatpak boundary

The [manifest](../packaging/flatpak/com.github.openwave.yml) packages the native panel, routing tools and DSP dependencies for GNOME Platform/SDK **50** with Rust **1.98.1**. The [build entrypoint](../packaging/flatpak/build.sh) consumes a prepared vendored release archive and its SHA-256, requires a native `x86_64` builder, and writes into an empty output directory using a private Flatpak installation. A bare checkout is not the offline release input.

This is an experimental packaging route, not a promise of a published or validated Flatpak artifact. ARM is outside the release target; physical-device acceptance must not be inferred from a manifest or package build. An installed artifact is launched with `flatpak run com.github.openwave`.

The sandbox can access the allowed audio sockets and raw devices, but **cannot perform host setup**. It does not install host udev rules, host capture services or host audio configuration, and does not restart host PipeWire/WirePlumber. Do not grant blanket home/system access, host command execution or privilege escalation just to bypass this boundary.

Prepare USB permissions and any native capture service **outside the sandbox**, using the native host installation described above or administrator-managed host configuration. Do not leave the native GUI running alongside the Flatpak GUI: only one vendor-control client should own a device. The capture-only native daemon is distinct from the GUI's USB vendor-control ownership.

A sandbox alone is not a replacement for the host keepalive service on hardware that needs capture-before-playback handling. Avoid distrobox/container workarounds for this service: access to the host graph, ALSA and USB must be deliberate, not assumed from a working desktop window.
