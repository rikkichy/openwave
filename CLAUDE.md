# openwave — project notes

Linux control + Wave-Link-style mixer for the **Elgato Wave XLR**. GTK4 +
libadwaita UI, raw libusb control transfers for the hardware, PipeWire
null sinks + pw-loopback subprocesses for the mixer routing.

## Repo

- `github.com/rikkichy/openwave`, default branch `main`.
- Active work branch: `mix-matrix-ui` — carries the Wave-Link-style matrix
  redesign (phases 1 → 3b). Should merge to `main` when stabilised.
- `watchdog-hardening` branch has uncommitted byte-flow watchdog changes
  to `wavexlr/audio.py` + `wavexlr/daemon.py` that are unrelated to the
  matrix work; rebase or merge to keep history clean.

Local dev tree: `/opt/openwave`. Site-packages install via
`pkexec make install PREFIX=/usr/local` is what the live `openwave`
launcher actually imports.

## Architecture

```
wavexlr/
  device.py     — raw libusb, USB control transfers via wIndex=0x3303
  audio.py      — pw-cat keepalive + byte-flow watchdog (watchdog-hardening)
  daemon.py     — runit/systemd service entry; just holds the mic alive
  service.py    — systemd / runit / stub backends; install via pkexec
  setup.py      — first-run: udev rule, WirePlumber rule, mix sinks, service
  mixer.py      — pw-loopback subprocess manager (the matrix's plumbing)
  mixmatrix.py  — GTK4 matrix grid (sources × mixes), SourceCell + MixCell
  sources.py    — load/save ~/.config/openwave/sources.json
  sourcedialog.py — two-page Add Source dialog (app picker → name + icon)
  app.py        — WaveXLRWindow + WaveXLRApp, wires everything together
  tray.py       — system tray icon
  style.css     — green slider fill, blue thumb, dark cell cards
pipewire/
  52-openwave-mixes.conf  — three persistent null sinks (Personal/Chat/Record)
wireplumber/
  51-openwave-wave-xlr.conf  — suspend-disable + 48 kHz pin for the XLR
```

### The hardware trick

The Wave XLR's USB Class control transfers normally hit interface 0 (owned
by `snd-usb-audio`) when sent with `wIndex=0x3300`. Sending the same
transfer with `wIndex=0x3303` routes through interface 3 (unclaimed) but
the firmware only checks the `0x33` prefix — so the device responds while
the kernel never blocks. No driver detach, audio is never interrupted.
See `device.py:WINDEX`.

### The mixer mental model

- **Three mix destinations**: Personal Mix / Chat Mix / Record Mix, exposed
  as PipeWire null sinks (`openwave_{personal,chat,record}_mix`). Apps select
  them as output devices. Their monitor ports become inputs for voice chat
  / OBS.
- **Sources**: the Wave XLR mic is a fixed top row. Other sources are
  user-added via `+ Add Source`, bound by `application.name`.
- **Per cell**: a non-zero (source, mix) cell spawns one `pw-loopback`
  subprocess. Volume + mute are pushed via `wpctl set-volume / set-mute`.
- **One always-on loopback**: `openwave_loop_personal_to_hp` routes
  Personal Mix's monitor → Wave XLR's analog-stereo output so the user
  actually hears it.

## PipeWire routing — the gotcha that bit us hardest

`pw-loopback`'s `target.object` capture-side property is **a hint**, not a
constraint. When the target name doesn't resolve to a Source-class node
(null sink monitors, numeric stream ids, etc.), WirePlumber falls back to
**default-source autoconnect** — which on this system is the Wave XLR mic.
Result: every silent-looking loopback secretly hauled the mic into the
Personal Mix → HP route. User heard themselves with mic.personal=0.

**The fix lives in `mixer.py::_spawn_loopback`**:

1. Spawn with `--capture-props=node.autoconnect=false ... audio.channels=2 audio.position=[FL,FR]` so WP doesn't auto-link.
2. Spawn with a known `node.name` on the capture side (`{node_name}_cap`).
3. After spawn, `pw-link` each source output port to a capture input port
   via the `_ports(direction, node_name)` helper. Mono sources duplicate to
   both stereo inputs.

Do not "simplify" by reverting to just `target.object`. The auto-link
WILL re-hijack the loopback the moment the target name fails to resolve.

## pw-loopback child lifecycle — three layers, all required

A leaked `pw-loopback` survives across GUI launches and produces
escalating feedback (multiple Personal→HP loops stacking). Three layers
guard against this in `mixer.py`:

1. **`PR_SET_PDEATHSIG`** via a `preexec_fn` using ctypes. The kernel
   SIGTERMs the child immediately when our process dies — survives
   SIGKILL, segfaults, log-out. **Must** set ctypes argtypes explicitly
   and cast `signal.SIGTERM` to `int()`; without that, the IntEnum
   marshals wrong and the call silently no-ops.
2. **`atexit.register(_atexit_cleanup)`** as belt-and-suspenders for
   normal exits where `Adw.Application.do_shutdown`'s vfunc dispatch
   could miss.
3. **`_sweep_stale_loopbacks()`** at every `start()` — runs
   `pkill -f 'pw-loopback.*openwave_loop_'` before spawning fresh ones.
   Catches orphans from a previous bad shutdown.

Plus defensive exception handling in `_destroy_loopback` so one
already-dead child can't abort iteration in `stop()`.

## Slider snap-to-0

`Gtk.Scale` is continuous; dragging back to "visually 0" usually lands at
0.001–0.005, which keeps the loopback alive at imperceptible-but-not-silent
volume. Two guards in series:

- `Gtk.Scale(round_digits=2)` in `mixmatrix.py::MixCell.__init__` so the
  slider snaps to 1% steps and the UI can't *display* a sub-percent value.
- `Mixer.set_cell` clamps `volume < 0.01` to `0.0` so persisted state
  never holds sub-threshold dust.

Do not remove either guard. Both are needed because rounding the visual
position doesn't prevent the signal from emitting the underlying float.

## Service detection on runit

`sv check wavexlr-audio` from a non-root user fails because the supervise
FIFO is mode 0700. The fallback is `_daemon_proc_alive()` in `service.py`,
which scans `/proc/*/cmdline`. The daemon is launched as
`python3 -c "from wavexlr.daemon import main; main()"` — `wavexlr.daemon`
lives **inside** the `-c` argument, so equality on cmdline parts always
fails. Use substring match on the joined cmdline. The bug shipped briefly
as the "Audio service not running" indicator lying on Void.

## Service install via pkexec

The runit backend's `install()` formerly raised RuntimeError telling the
user to set up `/etc/sv/wavexlr-audio` by hand. It now writes the run +
log/run scripts as a here-doc into a pkexec sh wrapper
(`service.py::_pkexec_script`), embedding `getpass.getuser()` so
`chpst -u` drops to the invoking account.

## Phase history (mix-matrix-ui branch)

- **Phase 1** — Two-pane UI: `Adw.OverlaySplitView` with the device
  controls on the right sidebar (collapses to overlay below 900sp via
  `Adw.Breakpoint`), matrix on the left. Mic row only, per-cell sliders
  disabled.
- **Phase 2a** — `pipewire/52-openwave-mixes.conf` declares three null
  sinks. `setup.install_mixes` writes the user config and also
  `pw-cli create-node`s them live so a fresh installer sees them
  immediately without a PipeWire restart.
- **Phase 2b** — `wavexlr/mixer.py` arrives. Per-cell sliders become
  live; mic→mix loopbacks spawn on non-zero volume; persistent state in
  `~/.config/openwave/mixes.json`.
- **Phase 3** — `+ Add Source` button, `wavexlr/sourcedialog.py`,
  `wavexlr/sources.py`. Bind by `application.name`, 2 Hz poll
  reconciles loopbacks as streams come and go. Sources persist in
  `~/.config/openwave/sources.json`.
- **Phase 3b** — Two-page dialog: pick app → name + icon (12 symbolic
  icons in a `Gtk.FlowBox`). × remove button on app source rows via
  `Gtk.Grid.remove_row`.

## Decisions deliberately rejected — don't re-propose

- **Source master slider scaling per-cell volumes.** Wave Link does this;
  we don't (yet). The master slider on app source rows is currently
  decorative. Adding scaling is a follow-up but easy to get wrong
  (effective = master × cell, plus mute logic). If/when added: gate
  behind an obvious UX so users understand why per-cell sliders don't
  reach max anymore.
- **Reverting to just `target.object` for loopback capture.** See the
  routing gotcha above.
- **systemd-only service management.** Void/Artix/Devuan-runit users
  exist; the runit backend with pkexec install is required.
- **Auto-detecting `.desktop` files for app source icons.** The
  12-icon FlowBox covers the common cases without `.desktop` parsing
  fragility (Snap/Flatpak naming, missing files, hicolor lookups). If a
  user wants a fancier icon, they edit `sources.json` by hand for now.

## Things worth knowing that aren't obvious from the code

- The Wave XLR has hardware-level direct monitoring controlled by an
  unknown byte in the device's config block. `device.py` only maps 5 of
  the 34 config bytes (`OFF_GAIN`, `OFF_MUTE`, `OFF_HP_VOL`,
  `OFF_VOL_SELECT`, `OFF_LOW_Z`). The user *should not* hear themselves
  via the hardware monitor on this unit, but it's a known dark corner
  if a future user reports phantom monitoring with all loopbacks killed.
- The PulseAudio compat layer exposes null-sink monitors with a
  `.monitor` suffix (`openwave_personal_mix.monitor` in `pactl list short
  sources`), but the underlying PipeWire node has only the bare name
  (`openwave_personal_mix`). Don't target the `.monitor` name in
  `target.object` — it doesn't resolve any more reliably than the bare
  one, and the manual `pw-link` approach (this file's routing gotcha
  section) sidesteps the issue entirely.
- `Gtk.Grid.remove_row(position)` shifts all later rows up automatically.
  Used by `mixmatrix.MixMatrix.remove_source` so we don't have to
  rebuild the grid on every removal.
- The streams-poll cadence is 2 Hz via `GLib.timeout_add_seconds(2, …)`
  in `app.py::_start_stream_poll`. Slower than ideal for instant
  attach-on-open, fast enough to not waste CPU.
- `Adw.OverlaySplitView` (libadwaita 1.4+) and `Adw.Dialog` /
  `Adw.NavigationView` (1.5+) are used unconditionally. If targeting
  older distros, these need fallbacks.

## Repo / commit conventions

- Author: `rikkichy <mewshoneko@gmail.com>`. **Do not** include the
  `Co-Authored-By: Claude…` trailer.
- Direct pushes to `main` are normal; PRs are not used.
- Commit subjects are short imperative ("Add multi-distro install.sh",
  "Snap mix sliders to 0 instead of imperceptible-but-not-silent").
- Push is shared state — narrate before doing it.
- `PREFIX` defaults to `/usr/local` in both the Makefile and
  `install.sh`. PKGBUILD overrides to `/usr` for packaging. Keep these
  matched if changing defaults — `make uninstall` will sweep the wrong
  tree otherwise.
- `pkexec make install PREFIX=/usr/local` from `/opt/openwave` re-pushes
  edits to `site-packages` for the live launcher. The launcher resolves
  `style.css` and the WirePlumber / PipeWire config sources from
  `/usr/local/share/openwave/...` candidates first, then `/usr/share/...`.
