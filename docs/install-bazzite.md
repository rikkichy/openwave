# Bazzite and Fedora Atomic installation

Run OpenWave on the **host** so it uses the host's PipeWire session, ALSA cards and USB permissions. A native checkout or user-prefix installation avoids modifying the immutable `/usr` image. This guidance also applies to Silverblue/Kinoite-style systems, but is not a claim of a completed build or hardware test on each image.

## Native host route

Check the host dependencies; image contents differ, so do not assume GNOME or KDE images already include everything:

```sh
rpm -q python3 python3-gobject gtk4 libadwaita adwaita-icon-theme libusb1 \
  pipewire pipewire-utils wireplumber alsa-utils pulseaudio-utils \
  ladspa-swh-plugins polkit git make
```

Use your image's supported package-layering procedure for missing packages. For example, if PyGObject, libadwaita and the DSP plugins are missing:

```sh
rpm-ostree install python3-gobject libadwaita ladspa-swh-plugins
```

Reboot into the updated deployment when required. Do not run `install.sh` here: its mutable-Fedora path uses `dnf`, not Atomic layering.

Clone and launch on the host:

```sh
git clone https://github.com/rikkichy/openwave.git "$HOME/openwave"
cd "$HOME/openwave"
python3 -m wavexlr --help
python3 -m wavexlr --version
python3 -m wavexlr
```

Keep the checkout at that path while launchers or the service refer to it. Alternatively, with dependencies present, install into your home:

```sh
make install PREFIX="$HOME/.local"
"$HOME/.local/bin/openwave"
```

The default module location is `<prefix>/share/openwave/site-packages`, not the host Python's read-only `/usr/lib` tree. Add `~/.local/bin` to your session's PATH if needed. Uninstall this layout with `make uninstall PREFIX="$HOME/.local"`; host setup and saved user settings are separate.

## First-run host integration

The native GUI offers setup; review it before applying changes during an active audio session:

- USB permissions are installed under `/etc/udev/rules.d` using polkit/`pkexec`. `/etc` is host configuration, even when `/usr` is immutable. Rules are limited to the three enabled USB PIDs; the provided permission mode grants local users raw access to those devices, so administrators may prefer a site-specific access policy.
- The capture daemon is a **user** service (`openwave.service` on systemd), not a root audio process. Its unit lives under `~/.config/systemd/user`.
- WirePlumber/PipeWire configuration is per-user host configuration. Setup may reconfigure or restart host audio; it is an explicit native operation, not a sandbox entitlement.
- Login/tray preferences are managed in the application. Close the tray process too before vendor-level diagnostics or probing.

Health remedies remain disabled unless the daemon is explicitly started with `--auto-recover`. Ordinary capture keepalive does not imply permission for destructive recovery. See [troubleshooting](troubleshooting.md).

## Experimental Flatpak boundary

The manifest packages the panel, routing tools and DSP dependencies. With `flatpak-builder`, GNOME Platform 49 and SDK 49 available, build from the repository root:

```sh
flatpak-builder --user --install --force-clean build-flatpak packaging/flatpak/com.github.openwave.yml
flatpak run com.github.openwave
```

This is an experimental local build route, not a promise of a published or validated Flatpak artifact.

The sandbox can access the allowed audio sockets and raw devices, but **cannot perform host setup**. It does not install host udev rules, host capture services or host audio configuration, and does not restart host PipeWire/WirePlumber. Do not grant blanket home/system access, host command execution or privilege escalation just to bypass this boundary.

Prepare USB permissions and any native capture service **outside the sandbox**, using the native host installation described above or administrator-managed host configuration. Do not leave the native GUI running alongside the Flatpak GUI: only one vendor-control client should own a device. The capture-only native daemon is distinct from the GUI's USB vendor-control ownership.

A sandbox alone is not a replacement for the host keepalive service on hardware that needs capture-before-playback handling. Avoid distrobox/container workarounds for this service: access to the host graph, ALSA and USB must be deliberate, not assumed from a working desktop window.
