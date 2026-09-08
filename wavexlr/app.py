"""GTK4 control surface for independently owned Wave devices and audio mixes."""

import gi
gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GLib, GObject, Gio, Gdk
import json
import logging
import os
import sys
from concurrent.futures import ThreadPoolExecutor

from .device import WaveDevice, DeviceUnresponsiveError, scan
from .audio import SOURCE_MATCHES
from .meter import MeterMonitor
from .mixer import Mixer, claim_streams, stream_matches, source_sink_name, OUTPUT_AUTO, OUTPUT_NONE
from .mixmatrix import MixMatrix
from .mixdialog import MixDialog
from .sourcedialog import AddSourceDialog
from . import paths, setup, service, sources as sources_module, mixes as mixes_module, desktop as desktop_module

logging.basicConfig(level=logging.INFO, format="%(name)s: %(message)s")
KNOB_LABELS = {"gain": "Gain", "hp": "Headphones", "mix": "Monitor Mix"}

def _slider_row(scale):
    row = Adw.PreferencesRow(activatable=False, selectable=False)
    scale.set_margin_start(12)
    scale.set_margin_end(12)
    scale.set_margin_top(2)
    scale.set_margin_bottom(6)
    row.set_child(scale)
    return row


class WaveXLRWindow(Adw.ApplicationWindow):
    _UI_STATE = os.path.join(os.path.dirname(sources_module.CONFIG_PATH), "ui-state.json")
    _CELL_DEBOUNCE_MS = 150

    def __init__(self, **kwargs):
        super().__init__(**kwargs, title="OpenWave", default_width=1280, default_height=720)
        self.set_size_request(820, 480)
        self._restore_window_size()
        self._empty_device = WaveDevice()
        self.dev = self._empty_device
        self._devs = []
        self._device_keys = {}
        self._device_executors = {}
        self._device_states = {}
        self._polling_devices = set()
        self._device_failures = {}
        self._failed_units = set()
        self._closing_units = set()
        self._selector_updating = False
        self._usb_executor = ThreadPoolExecutor(max_workers=1, thread_name_prefix="openwave-discovery")
        self._shutting_down = False
        self._connect_pending = False
        self._reconnect_id = self._poll_id = self._stream_poll_id = None
        self._gain_timeout = self._hp_timeout = self._mix_timeout = None
        self._gain_max = 0x5000
        self._updating_ui = False
        self._last_state = None
        self._cell_debounce_ids = {}
        self._remote_levels = {}
        self._capture_mute_seen = {}
        self._service_problem = False
        self._sources = sources_module.load()
        self._mixes = mixes_module.load_seeded()
        self._offered_nodes = set(self._load_ui_state().get("offered_capture_nodes", []))
        self.meter = MeterMonitor()
        self._meter_targets = {}
        self.mixer = Mixer(capture_ready=lambda capture: self.meter.ready(
            capture["name"], capture["identity"]))
        self.mixer.set_mixes(self._mixes)
        self.mixer.set_sources(self._sources)
        self._build_ui()
        self._restore_gain_lock()
        self._wire_matrix_cells()
        self.connect("map", lambda *_: setattr(self.meter, "ui_suspended", False))
        self.connect("unmap", lambda *_: setattr(self.meter, "ui_suspended", True))
        self._update_service_status()
        self._start_meters()
        self.mixer.start()
        self._start_stream_poll()
        self._try_connect()
        self._schedule_reconnect()

    def _build_ui(self):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.set_content(box)
        header = Adw.HeaderBar()
        self._window_title = Adw.WindowTitle(title="OpenWave", subtitle="Disconnected")
        header.set_title_widget(self._window_title)
        self.service_btn = Gtk.MenuButton(icon_name="dialog-warning-symbolic", visible=False)
        service_pop = Gtk.Popover()
        service_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8,
                              margin_top=12, margin_bottom=12, margin_start=12, margin_end=12)
        self.service_label = Gtk.Label(xalign=0, wrap=True, max_width_chars=38)
        self.routing_label = Gtk.Label(xalign=0, wrap=True, max_width_chars=38)
        service_box.append(self.service_label)
        service_box.append(self.routing_label)
        self.uninstall_btn = Gtk.Button(label="Uninstall capture fix")
        self.uninstall_btn.connect("clicked", self._on_uninstall_clicked)
        service_box.append(self.uninstall_btn)
        service_pop.set_child(service_box)
        self.service_btn.set_popover(service_pop)
        header.pack_start(self.service_btn)
        refresh = Gtk.Button(icon_name="view-refresh-symbolic", tooltip_text="Reconnect")
        refresh.connect("clicked", lambda _: self._try_connect())
        header.pack_end(refresh)
        self.sidebar_toggle = Gtk.ToggleButton(icon_name="sidebar-show-symbolic",
                                               tooltip_text="Toggle device panel", active=False)
        header.pack_end(self.sidebar_toggle)
        box.append(header)
        self.split = Adw.OverlaySplitView(sidebar_position=Gtk.PackType.END,
            min_sidebar_width=320, max_sidebar_width=420, sidebar_width_fraction=0.30, vexpand=True)
        box.append(self.split)
        self.sidebar_toggle.bind_property("active", self.split, "show-sidebar",
            GObject.BindingFlags.BIDIRECTIONAL | GObject.BindingFlags.SYNC_CREATE)
        breakpoint = Adw.Breakpoint.new(Adw.BreakpointCondition.parse("max-width: 900sp"))
        breakpoint.add_setter(self.split, "collapsed", True)
        self.add_breakpoint(breakpoint)
        self.matrix = MixMatrix()
        self.split.set_content(self.matrix)
        for mid, mix in self._mixes.items():
            self.matrix.add_mix(mid, title=mix["name"], subtitle=mix["subtitle"], icon_name=mix["icon_name"])
        for source in self._sources.values():
            self._add_source_widget(source)
        for signal, handler in (
            ("add-source-clicked", self._on_add_source_clicked),
            ("remove-source-clicked", self._on_remove_source_clicked),
            ("edit-source-clicked", self._on_edit_source_clicked),
            ("move-source-clicked", self._on_move_source_clicked),
            ("switch-source-clicked", self._on_switch_source_clicked),
            ("group-sources-clicked", self._on_group_sources_clicked),
            ("add-mix-clicked", self._on_add_mix_clicked),
            ("rename-mix-clicked", self._on_rename_mix_clicked),
            ("remove-mix-clicked", self._on_remove_mix_clicked),
            ("mix-output-changed", self._on_mix_output_changed),
            ("mix-volume-changed", self._on_mix_volume_changed),
        ):
            self.matrix.connect(signal, handler)
        scroll = Gtk.ScrolledWindow(vexpand=True, hscrollbar_policy=Gtk.PolicyType.NEVER)
        clamp = Adw.Clamp(maximum_size=380, margin_start=12, margin_end=12,
                          margin_top=12, margin_bottom=12)
        scroll.set_child(clamp)
        sidebar = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        clamp.set_child(sidebar)
        self._build_device_pane(sidebar)
        self.split.set_sidebar(scroll)

    def _build_device_pane(self, parent):
        """Populate the sidebar: Microphone, Headphones, and device info."""
        # --- Device selector ---
        # Hidden with a single device: a dropdown with one entry is a
        # question with no answer. With two or more, the controls below
        # bind to whichever unit is chosen here; every unit keeps polling
        # and syncing regardless.
        self._selector_group = Adw.PreferencesGroup(visible=False)
        parent.append(self._selector_group)
        self.device_combo = Adw.ComboRow(title="Device")
        self.device_combo.connect("notify::selected",
                                  self._on_device_selected)
        self._selector_group.add(self.device_combo)

        # --- Mic controls ---
        mic_group = Adw.PreferencesGroup(title="Microphone")
        parent.append(mic_group)

        mute_row = Adw.SwitchRow(title="Mute", subtitle="Toggle microphone mute")
        mute_row.connect("notify::active", self._on_mute_changed)
        self.mute_row = mute_row
        mic_group.add(mute_row)

        gain_row = Adw.ActionRow(title="Gain")
        self.gain_label = Gtk.Label(label="—", width_chars=8, xalign=1)
        self.gain_label.add_css_class("monospace")
        gain_row.add_suffix(self.gain_label)

        # Preamp gain is set once and then wants leaving alone: a stray scroll
        # over the slider silently changes how loud you are to everyone else,
        # and nothing on screen makes that obvious afterwards.
        self.gain_lock = Gtk.ToggleButton(
            icon_name="changes-allow-symbolic", valign=Gtk.Align.CENTER,
            tooltip_text="Lock gain",
        )
        self.gain_lock.add_css_class("flat")
        self.gain_lock.connect("toggled", self._on_gain_lock_toggled)
        gain_row.add_suffix(self.gain_lock)
        mic_group.add(gain_row)

        self.gain_scale = Gtk.Scale(
            orientation=Gtk.Orientation.HORIZONTAL,
            hexpand=True,
            draw_value=False,
            adjustment=Gtk.Adjustment(lower=0x0000, upper=0x5000, step_increment=0x40, page_increment=0x200),
        )
        self.gain_scale.connect("value-changed", self._on_gain_changed)
        mic_group.add(_slider_row(self.gain_scale))

        phantom_row = Adw.SwitchRow(
            title="48V Phantom Power",
            subtitle="For condenser microphones. Leave off for dynamic mics.",
        )
        phantom_row.connect("notify::active", self._on_phantom_changed)
        self.phantom_row = phantom_row
        mic_group.add(phantom_row)

        knob_row = Adw.ActionRow(title="Knob Controls", subtitle="What the physical knob adjusts")
        self.knob_label = Gtk.Label(label="Gain")
        self.knob_label.add_css_class("dim-label")
        knob_row.add_suffix(self.knob_label)
        self.knob_row = knob_row
        mic_group.add(knob_row)

        # --- Headphone controls ---
        hp_group = Adw.PreferencesGroup(title="Headphones")
        parent.append(hp_group)

        hp_vol_row = Adw.ActionRow(title="Volume")
        self.hp_label = Gtk.Label(label="0.0 dB", width_chars=10, xalign=1)
        self.hp_label.add_css_class("monospace")
        hp_vol_row.add_suffix(self.hp_label)
        hp_group.add(hp_vol_row)

        self.hp_scale = Gtk.Scale(
            orientation=Gtk.Orientation.HORIZONTAL,
            hexpand=True,
            draw_value=False,
            adjustment=Gtk.Adjustment(lower=-60.0, upper=0.0, step_increment=0.5, page_increment=2.0),
        )
        self.hp_scale.connect("value-changed", self._on_hp_changed)
        hp_group.add(_slider_row(self.hp_scale))

        lowz_row = Adw.SwitchRow(title="Low Impedance", subtitle="For low impedance headphones")
        lowz_row.connect("notify::active", self._on_lowz_changed)
        self.lowz_row = lowz_row
        hp_group.add(lowz_row)

        mix_row = Adw.ActionRow(title="Monitor Mix", subtitle="Mic / PC monitoring balance")
        self.mix_label = Gtk.Label(label="—", width_chars=8, xalign=1)
        self.mix_label.add_css_class("monospace")
        mix_row.add_suffix(self.mix_label)
        self.mix_row = mix_row
        mix_row.set_visible(False)
        hp_group.add(mix_row)

        self.mix_scale = Gtk.Scale(
            orientation=Gtk.Orientation.HORIZONTAL,
            hexpand=True,
            draw_value=False,
            adjustment=Gtk.Adjustment(lower=0, upper=0x6400, step_increment=0x100, page_increment=0x800),
        )
        self.mix_scale.set_margin_start(12)
        self.mix_scale.set_margin_end(12)
        self.mix_scale.connect("value-changed", self._on_mix_changed)
        self.mix_scale_row = _slider_row(self.mix_scale)
        self.mix_scale_row.set_visible(False)
        hp_group.add(self.mix_scale_row)

        # Output routing is per mix and lives in each mix column's header
        # menu, not here — one device combo could only ever speak for one mix.

        # --- Startup ---
        startup_group = Adw.PreferencesGroup(title="Startup")
        parent.append(startup_group)

        enabled, hidden = desktop_module.autostart_state()
        self.autostart_row = Adw.SwitchRow(
            title="Start at login",
            subtitle="Keeps mixes routed before you open anything",
        )
        self.autostart_row.set_active(enabled)
        self._autostart_handler = self.autostart_row.connect(
            "notify::active", self._on_autostart_toggled)
        startup_group.add(self.autostart_row)

        self.tray_row = Adw.SwitchRow(
            title="Start in the tray",
            subtitle="No window on login; open it from the tray icon",
        )
        self.tray_row.set_active(hidden)
        # Only meaningful when something is starting it for you.
        self.tray_row.set_sensitive(enabled)
        self.tray_row.connect("notify::active", self._on_start_hidden_toggled)
        startup_group.add(self.tray_row)

        # --- Device info ---
        # Titleless group so the expander reads as a single collapsed line: it
        # is reference material, looked at once, and does not deserve a
        # permanent three-row card in a narrow sidebar.
        info_group = Adw.PreferencesGroup()
        parent.append(info_group)

        info_expander = Adw.ExpanderRow(title="Device Info")
        info_group.add(info_expander)

        self.fw_row = Adw.ActionRow(title="Firmware")
        self.fw_label = Gtk.Label(label="—")
        self.fw_label.add_css_class("dim-label")
        self.fw_row.add_suffix(self.fw_label)
        info_expander.add_row(self.fw_row)

        self.api_row = Adw.ActionRow(title="API")
        self.api_label = Gtk.Label(label="—")
        self.api_label.add_css_class("dim-label")
        self.api_row.add_suffix(self.api_label)
        info_expander.add_row(self.api_row)

        self.serial_row = Adw.ActionRow(title="Serial")
        self.serial_label = Gtk.Label(label="—")
        self.serial_label.add_css_class("dim-label")
        self.serial_row.add_suffix(self.serial_label)
        info_expander.add_row(self.serial_row)

    def _update_service_status(self):
        if setup.is_sandboxed():
            self._service_problem = True
            self.service_label.set_label("Sandbox: install USB permissions and the capture service on the host. Native setup is unavailable here.")
            self.uninstall_btn.set_visible(False)
            self.service_btn.set_visible(True)
            return
        def query():
            return service.is_running(), service.is_failed(), setup.anything_installed()
        def apply(result):
            active, failed, installed = result
            self._service_problem = not active
            self.service_label.set_label("Capture service running" if active else
                "Capture service failed; inspect its user journal" if failed else "Capture service not running")
            self.uninstall_btn.set_visible(installed)
            self.service_btn.set_visible(self._service_problem or bool(self.mixer.last_error()))
        self._usb_async(query, apply, lambda error: self._show_error("Service status unavailable", error))

    def _on_uninstall_clicked(self, btn):
        dialog = Adw.AlertDialog(
            heading="Uninstall Capture Fix?",
            body="This will remove the audio service, the WirePlumber rule, "
                 "the mix sinks and the USB permissions.\n\nYou can reinstall "
                 "them by restarting OpenWave.",
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("uninstall", "Uninstall")
        dialog.set_response_appearance("uninstall", Adw.ResponseAppearance.DESTRUCTIVE)
        dialog.set_default_response("cancel")
        dialog.choose(self, None, self._on_uninstall_response)

    def _on_uninstall_response(self, dialog, result):
        if dialog.choose_finish(result) != "uninstall":
            return
        self.matrix.set_sensitive(False)
        def uninstall():
            self.mixer.stop()
            return setup.run_uninstall()
        def done(result):
            success, message = result
            if success:
                self.get_application().quit()
            else:
                self._show_error("Uninstall failed", message + "\nReopen OpenWave to resume routing.")
        self._usb_async(uninstall, done, lambda error: self._show_error("Uninstall failed", error))

    def _usb_async(self, fn, on_done=None, on_error=None, *, device=None):
        """Dispatch discovery or one device's serialized operations."""
        if self._shutting_down:
            return None
        executor = (
            self._usb_executor if device is None
            else self._device_executors.get(device)
        )
        if executor is None:
            return None
        future = executor.submit(fn)

        def dispatch(callback, value):
            if not self._shutting_down and (
                device is None or device in self._device_executors
            ):
                callback(value)
            return False

        def finished(done):
            try:
                result = done.result()
            except Exception as error:
                if on_error:
                    GLib.idle_add(dispatch, on_error, error)
            else:
                if on_done:
                    GLib.idle_add(dispatch, on_done, result)

        future.add_done_callback(finished)
        return future

    def _device_async(self, method, *args):
        device = self.dev
        if not device.connected:
            return None
        return self._usb_async(
            lambda: getattr(device, method)(*args),
            on_error=lambda error: self._on_device_error(device, error),
            device=device,
        )

    def _try_connect(self):
        if self._connect_pending or self._shutting_down:
            return
        self._connect_pending = True

        def failed(error):
            self._connect_pending = False
            logging.getLogger("openwave.app").warning("USB discovery: %s", error)

        self._usb_async(scan, self._on_devices_scanned, failed)

    def _on_devices_scanned(self, entries):
        self._connect_pending = False
        wanted = {(profile.key, bus, addr) for profile, bus, addr in entries}
        self._failed_units.intersection_update(wanted)
        for device, key in list(self._device_keys.items()):
            if key not in wanted:
                self._retire_device(device)
        held = set(self._device_keys.values())
        for profile, bus, addr in entries:
            key = (profile.key, bus, addr)
            if key in held or key in self._failed_units or key in self._closing_units:
                continue
            device = WaveDevice()
            self._device_keys[device] = key
            self._device_executors[device] = ThreadPoolExecutor(
                max_workers=1, thread_name_prefix=f"openwave-usb-{bus}-{addr}"
            )

            def connect(dev=device, prof=profile, b=bus, a=addr):
                try:
                    dev.connect(prof, b, a)
                    dev.info = dev.read_device_info()
                    return dev.get_all()
                except Exception:
                    dev.disconnect()
                    raise

            self._usb_async(
                connect,
                lambda state, dev=device: self._on_device_connected(dev, state),
                lambda error, dev=device: self._on_device_error(dev, error),
                device=device,
            )

    def _on_device_connected(self, device, state):
        self._devs.append(device)
        self._devs.sort(key=lambda dev: self._device_keys[dev][1:])
        self._device_states[device] = state
        if self.dev not in self._devs:
            self._select_device(device)
        self._refresh_device_selector()
        self._start_polling()

    def _refresh_device_selector(self):
        self._selector_updating = True
        try:
            labels = []
            for device in self._devs:
                serial = device.info.get("serial") or device.usbbus
                labels.append(f"{device.profile.display_name} — {serial}")
            self.device_combo.set_model(Gtk.StringList.new(labels))
            if self.dev in self._devs:
                self.device_combo.set_selected(self._devs.index(self.dev))
            self._selector_group.set_visible(len(self._devs) > 1)
        finally:
            self._selector_updating = False

    def _on_device_selected(self, row, _param):
        index = row.get_selected()
        if not self._selector_updating and 0 <= index < len(self._devs):
            self._select_device(self._devs[index])

    def _select_device(self, device):
        # A delayed slider send belongs to the previous device, never the new
        # selection. Already queued operations captured their device object.
        for name in ("_gain_timeout", "_hp_timeout", "_mix_timeout"):
            source_id = getattr(self, name)
            if source_id:
                GLib.source_remove(source_id)
                setattr(self, name, None)
        self.dev = device
        self._last_state = None
        if device is self._empty_device:
            self._window_title.set_subtitle("Disconnected")
            self._window_title.add_css_class("dim-label")
            return
        self._updating_ui = True
        try:
            self._apply_profile(device.profile)
        finally:
            self._updating_ui = False
        self.device_combo.set_subtitle(
            f"USB {device.usbbus} · {device.info.get('serial', 'No serial')}"
        )
        self._apply_state(self._device_states[device])
        self._window_title.remove_css_class("dim-label")
        self.fw_label.set_label(device.info.get("fw_version", "—"))
        self.api_label.set_label(device.info.get("api_version", "—"))
        self.serial_label.set_label(device.info.get("serial", "—"))

    def _retire_device(self, device):
        executor = self._device_executors.pop(device, None)
        key = self._device_keys.pop(device, None)
        self._device_states.pop(device, None)
        self._device_failures.pop(device, None)
        self._polling_devices.discard(device)
        if device in self._devs:
            self._devs.remove(device)
        if self.dev is device:
            self._select_device(self._devs[0] if self._devs else self._empty_device)
        self._refresh_device_selector()
        if executor is not None:
            self._closing_units.add(key)
            executor.shutdown(wait=False, cancel_futures=True)

            def close():
                executor.shutdown(wait=True)
                device.disconnect()

            self._usb_async(close, lambda _: self._closing_units.discard(key))

    def _on_device_error(self, device, error):
        key = self._device_keys.get(device)
        logging.getLogger("openwave.app").warning("Device %s: %s", key, error)
        if key and isinstance(error, DeviceUnresponsiveError):
            self._failed_units.add(key)
        self._retire_device(device)
        if not self._devs and isinstance(error, DeviceUnresponsiveError):
            self._window_title.set_subtitle("Power-cycle Wave device")

    def _schedule_reconnect(self):
        if self._reconnect_id or self._shutting_down:
            return

        def watch():
            self._try_connect()
            return True

        self._reconnect_id = GLib.timeout_add_seconds(2, watch)

    def _start_polling(self):
        if self._poll_id is None:
            self._poll_id = GLib.timeout_add(100, self._poll_tick)

    def _stop_polling(self):
        if self._poll_id:
            GLib.source_remove(self._poll_id)
            self._poll_id = None

    def _poll_tick(self):
        for device in self._devs:
            if device in self._polling_devices:
                continue
            self._polling_devices.add(device)
            self._usb_async(
                device.get_all,
                lambda state, dev=device: self._on_poll_result(dev, state),
                lambda error, dev=device: self._on_poll_error(dev, error),
                device=device,
            )
        return True

    def _on_poll_result(self, device, state):
        self._polling_devices.discard(device)
        self._device_failures[device] = 0
        previous = self._device_states.get(device)
        self._device_states[device] = state
        if previous is not None and previous["mute"] != state["mute"]:
            for sid, source in list(self._sources.items()):
                if self._device_for_source(source) is device:
                    self._set_source_muted(sid, state["mute"], hardware=False)
        if device is self.dev and state != self._last_state:
            self._apply_state(state)
        self._notify_tray()

    def _on_poll_error(self, device, error):
        self._polling_devices.discard(device)
        failures = self._device_failures.get(device, 0) + 1
        self._device_failures[device] = failures
        if failures >= 3:
            self._on_device_error(device, error)

    def shutdown(self):
        """Quiesce every device worker before closing its libusb handle."""
        self._shutting_down = True
        for name in (
            "_reconnect_id", "_poll_id", "_stream_poll_id",
            "_gain_timeout", "_hp_timeout", "_mix_timeout",
        ):
            source_id = getattr(self, name, None)
            if source_id:
                GLib.source_remove(source_id)
                setattr(self, name, None)
        for source_id in self._cell_debounce_ids.values():
            GLib.source_remove(source_id)
        self._cell_debounce_ids.clear()
        self._usb_executor.shutdown(wait=True, cancel_futures=False)
        for device, executor in self._device_executors.items():
            executor.shutdown(wait=True, cancel_futures=True)
            device.disconnect()
        self._device_executors.clear()
        self._devs.clear()

    def _apply_profile(self, profile):
        """Adapt the UI to the connected device model."""
        self._gain_max = profile.gain_max
        self.gain_scale.get_adjustment().set_upper(profile.gain_max)
        self.knob_row.set_visible(profile.has_vol_select)
        self.lowz_row.set_visible(profile.has_low_z)
        self.phantom_row.set_visible(profile.has_phantom)
        self.mix_row.set_visible(profile.has_monitor_mix)
        self.mix_scale_row.set_visible(profile.has_monitor_mix)
        if profile.has_monitor_mix:
            self.mix_scale.get_adjustment().set_upper(profile.mix_max)
        self._window_title.set_subtitle(profile.display_name)

    def _format_gain(self, raw):
        scale = self.dev.profile.gain_scale if self.dev.profile else None
        if scale:
            return f"{raw / scale:.2f}".rstrip("0").rstrip(".") + " dB"
        return f"0x{raw:04X}"

    def _apply_state(self, state):
        """Update UI from device state dict (must be called on GTK thread)."""
        self._updating_ui = True
        self._last_state = state
        self.mute_row.set_active(state["mute"])
        self.gain_scale.set_value(state["gain_raw"])
        self.gain_label.set_label(self._format_gain(state["gain_raw"]))
        self.hp_scale.set_value(state["hp_volume_db"])
        self.hp_label.set_label(f"{state['hp_volume_db']:.1f} dB")
        if "low_impedance" in state:
            self.lowz_row.set_active(state["low_impedance"])
        if "phantom" in state:
            self.phantom_row.set_active(state["phantom"])
        if "volume_select" in state:
            self.knob_label.set_label(KNOB_LABELS.get(state["volume_select"], "Gain"))
        if "monitor_mix" in state:
            self.mix_scale.set_value(state["monitor_mix"])
            self.mix_label.set_label(f"{state['monitor_mix'] / 256:.0f}%")
        self._updating_ui = False

    def _on_mute_changed(self, row, _pspec):
        if self._updating_ui or not self.dev.connected:
            return
        muted = row.get_active()
        self._device_async("set_mute", muted)

    def _on_gain_changed(self, scale):
        if self._updating_ui or not self.dev.connected:
            return
        val = int(scale.get_value())
        self.gain_label.set_label(self._format_gain(val))
        # Debounce — only send after slider stops moving for 200ms
        if hasattr(self, '_gain_timeout') and self._gain_timeout:
            GLib.source_remove(self._gain_timeout)
        self._gain_timeout = GLib.timeout_add(200, self._send_gain, val)

    def _send_gain(self, val):
        self._gain_timeout = None
        self._device_async("set_gain_raw", val)
        return False

    def _on_hp_changed(self, scale):
        if self._updating_ui or not self.dev.connected:
            return
        db = scale.get_value()
        self.hp_label.set_label(f"{db:.1f} dB")
        if hasattr(self, '_hp_timeout') and self._hp_timeout:
            GLib.source_remove(self._hp_timeout)
        self._hp_timeout = GLib.timeout_add(200, self._send_hp, db)

    def _send_hp(self, db):
        self._hp_timeout = None
        self._device_async("set_hp_volume_db", db)
        return False

    def _on_lowz_changed(self, row, _pspec):
        if self._updating_ui or not self.dev.connected:
            return
        enabled = row.get_active()
        self._device_async("set_low_impedance", enabled)

    def _on_phantom_changed(self, row, _pspec):
        if self._updating_ui or not self.dev.connected:
            return
        enabled = row.get_active()
        self._device_async("set_phantom", enabled)

    def _on_mix_changed(self, scale):
        if self._updating_ui or not self.dev.connected:
            return
        val = int(scale.get_value())
        self.mix_label.set_label(f"{val / 256:.0f}%")
        if self._mix_timeout:
            GLib.source_remove(self._mix_timeout)
        self._mix_timeout = GLib.timeout_add(200, self._send_mix, val)

    def _send_mix(self, val):
        self._mix_timeout = None
        self._device_async("set_monitor_mix", val)
        return False

    def _wire_matrix_cells(self):
        """Bind each per-cell slider/mute to the mixer + restore persisted levels."""
        source_ids = list(self._sources.keys())
        for source_id in source_ids:
            for mix_id in self._mixes:
                self._wire_cell(source_id, mix_id)

    def _wire_cell(self, source_id, mix_id):
        cell = self.matrix.cell(source_id, mix_id)
        if cell is None:
            return
        state = self.mixer.get_cell(source_id, mix_id)
        cell.set_volume(state["volume"])
        cell.set_muted(state["muted"])
        cell.connect("volume-changed", self._on_cell_volume_changed, source_id, mix_id)
        cell.connect("mute-toggled", self._on_cell_mute_toggled, source_id, mix_id)

    def _start_stream_poll(self):
        """Poll for new/vanished PipeWire output streams every 2 s."""
        if self._stream_poll_id:
            GLib.source_remove(self._stream_poll_id)
        self._stream_poll_id = GLib.timeout_add_seconds(2, self._stream_poll_tick)

    def _stream_poll_tick(self):
        self.mixer.request_stream_poll()
        captures = self.mixer.capture_sources()
        self._add_discovered_inputs(captures)
        self._follow_capture_mutes()
        for sid in list(self._sources):
            self._refresh_source_meter(sid)
        for mid, mix in self._mixes.items():
            self._refresh_mix_meter(mid, mix)
            master = self.mixer.mix_volume(mid)
            if master is not None:
                self.matrix.set_mix_volume(mid, master[0])
        self._refresh_outputs()
        self._refresh_mix_emptiness()
        error = self.mixer.last_error()
        self.routing_label.set_label("Routing: " + error if error else "")
        self.routing_label.set_visible(bool(error))
        self.service_btn.set_visible(self._service_problem or bool(error))
        return not self._shutting_down

    def _start_meters(self):
        """Meter every source that has something to meter."""
        for source_id in self._sources.keys():
            self._refresh_source_meter(source_id)

    def _refresh_app_meter(self, source_id):
        source = self._sources[source_id]
        streams = self.mixer.streams()
        claimed = claim_streams(self._sources, streams).get(source_id, set())
        row = self.matrix.source(source_id)
        row.set_waiting(not claimed, "Routed by another source" if any(
            stream_matches(source, stream) for stream in streams.values()) else "Waiting for audio")
        if not claimed:
            self.meter.stop(source_id)
            self._meter_targets.pop(source_id, None)
            self._set_source_level(source_id, 0.0)
            return
        target = source_sink_name(source_id)
        if self._meter_targets.get(source_id) == target and self.meter.active(source_id):
            return
        self._meter_targets[source_id] = target
        self.meter.start(source_id, target, lambda level, sid=source_id: self._set_source_level(sid, level), capture_sink=True)

    def _set_source_level(self, source_id, level):
        self._remote_levels[f"src:{source_id}"] = round(float(level), 4)
        cell = self.matrix.source(source_id)
        if cell is not None:
            cell.set_level(level)

    def _on_add_source_clicked(self, _matrix):
        dialog = AddSourceDialog(exclude_nodes=self._bound_capture_nodes(),
            exclude_apps=self._bound_app_names(), streams=self.mixer.streams().values(),
            captures=self.mixer.capture_sources())
        dialog.connect("source-confirmed", self._on_source_confirmed)
        dialog.connect("device-source-confirmed", self._on_device_source_confirmed)
        dialog.present(self)

    def _on_source_confirmed(self, _dialog, name, match_app_name, icon_name,
                             group=""):
        source = sources_module.new_source(
            name=name, match_app_name=match_app_name, icon_name=icon_name,
        )
        if group:
            source["group"] = group
        self._install_source(source)

    def _on_remove_source_clicked(self, _matrix, source_id):
        source = self._sources.get(source_id, {})
        name = source.get("name", "this source")
        if sources_module.is_protected(source):
            body = (f"This deletes “{name}” and its mix levels. If the device "
                    f"is plugged back in, the row is offered again.")
        else:
            body = (f"This deletes “{name}” and its mix levels. The bound "
                    f"application itself is not affected.")
        dialog = Adw.AlertDialog(
            heading="Remove source?",
            body=body,
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("remove", "Remove")
        dialog.set_response_appearance("remove", Adw.ResponseAppearance.DESTRUCTIVE)
        dialog.set_default_response("cancel")
        dialog.choose(self, None, lambda d, r: self._on_remove_response(d, r, source_id))

    def _on_remove_response(self, dialog, result, source_id):
        if dialog.choose_finish(result) != "remove" or source_id not in self._sources:
            return
        source = self._sources[source_id]
        if source.get("protected") and self.mixer.capture_device_present(source["node_name"]):
            return
        self._cancel_cell_edits(source_id=source_id)
        candidate = {sid: record for sid, record in self._sources.items() if sid != source_id}
        sources_module.save(candidate)
        self._sources = candidate
        self.mixer.set_sources(candidate)
        self.mixer.remove_source(source_id)
        self.meter.stop(source_id)
        self._meter_targets.pop(source_id, None)
        self._remote_levels.pop("src:" + source_id, None)
        self.matrix.remove_source(source_id)
        self._offered_nodes.discard(source.get("node_name"))
        self._save_ui_state()

    def _on_cell_volume_changed(self, _cell, value, source_id, mix_id):
        # During a drag, value-changed fires continuously; coalesce into a
        # single set_cell after the slider settles.
        key = (source_id, mix_id)
        prev = self._cell_debounce_ids.pop(key, None)
        if prev is not None:
            GLib.source_remove(prev)
        self._cell_debounce_ids[key] = GLib.timeout_add(
            self._CELL_DEBOUNCE_MS,
            self._flush_cell_volume, source_id, mix_id, value,
        )

    def _flush_cell_volume(self, source_id, mix_id, value):
        self._cell_debounce_ids.pop((source_id, mix_id), None)
        self.set_cell_volume(source_id, mix_id, value)
        return False

    def _on_cell_mute_toggled(self, _cell, muted, source_id, mix_id):
        if source_id not in self._sources or mix_id not in self._mixes:
            return
        state = self.mixer.get_cell(source_id, mix_id)
        self.mixer.set_cell(source_id, mix_id, state["volume"], muted)
        self._refresh_mix_emptiness()

    def _on_gain_lock_toggled(self, btn):
        locked = btn.get_active()
        self.gain_scale.set_sensitive(not locked)
        btn.set_icon_name(
            "changes-prevent-symbolic" if locked else "changes-allow-symbolic"
        )
        btn.set_tooltip_text("Gain locked \u2014 click to unlock" if locked
                             else "Lock gain")
        self._save_ui_state()

    def _restore_gain_lock(self):
        state = self._load_ui_state()
        if state.get("gain_locked"):
            self.gain_lock.set_active(True)   # toggled fires and applies it

    def _load_ui_state(self):
        try:
            with open(self._UI_STATE) as f:
                state = json.load(f)
        except (OSError, ValueError):
            return {}
        return state if isinstance(state, dict) else {}

    def _restore_window_size(self):
        state = self._load_ui_state()
        width, height = state.get("width"), state.get("height")
        if isinstance(width, int) and isinstance(height, int) \
                and width >= 820 and height >= 480:
            self.set_default_size(width, height)
        else:
            # First run: without this the window opens at the 820x480
            # MINIMUM, which clips the matrix on every axis. Sized to show
            # the seeded rows and three mix columns with room to breathe,
            # while still fitting a 1366x768 laptop panel.
            self.set_default_size(1280, 720)
        if state.get("maximized"):
            self.maximize()

    def _save_ui_state(self):
        previous = self._load_ui_state()
        size = (self.get_width(), self.get_height())
        if self.is_maximized() or min(size) <= 0:
            size = (previous.get("width", 1280), previous.get("height", 720))
        try:
            sources_module._atomic_write(self._UI_STATE, {
                "width": size[0], "height": size[1], "maximized": self.is_maximized(),
                "offered_capture_nodes": sorted(self._offered_nodes),
                "gain_locked": self.gain_lock.get_active(),
            })
        except OSError as error:
            logging.warning("Cannot save interface preferences: %s", error)

    def _on_autostart_toggled(self, row, _param):
        enabled, _hidden = desktop_module.set_autostart(
            row.get_active(), self.tray_row.get_active())
        self.tray_row.set_sensitive(enabled)
        if enabled != row.get_active():
            # The file could not be written; show what is actually true
            # rather than a switch that lies about the next login.
            with GObject.signal_handler_block(row, self._autostart_handler):
                row.set_active(enabled)

    def _on_start_hidden_toggled(self, row, _param):
        if self.autostart_row.get_active():
            desktop_module.set_autostart(True, row.get_active())

    def _output_entries(self, mix_id, sinks, default_sink):
        """(entries, current, summary, monitored) for one mix's header menu."""
        current = self.mixer.get_output(mix_id)
        resolved = self.mixer.resolve_output(
            mix_id, sinks=sinks, default_sink=default_sink,
        )
        descriptions = {sink["name"]: sink["description"] for sink in sinks}

        auto_label = "Automatic"
        if current == OUTPUT_AUTO and resolved in descriptions:
            # Only name the device when Automatic is what is actually in force:
            # with an explicit sink chosen, resolve_output returns that sink,
            # and labelling Automatic with it would claim a resolution that is
            # not the one Automatic would pick.
            auto_label = f"Automatic — {descriptions[resolved]}"

        # Automatic stays first: it is the entry that describes the default
        # behaviour, and a mix with no stored choice lands on it.
        entries = [(OUTPUT_AUTO, auto_label), (OUTPUT_NONE, "Not monitored")]
        entries += [(sink["name"], sink["description"]) for sink in sinks]
        if current not in [name for name, _ in entries]:
            # A remembered device that is currently absent: show it rather than
            # silently substituting a sentinel.
            entries.append((current, f"{current} (unavailable)"))

        if current == OUTPUT_NONE:
            summary, monitored = "Not monitored", False
        elif resolved is None:
            summary, monitored = "No output", False
        else:
            summary, monitored = descriptions.get(resolved, resolved), True
        return entries, current, summary, monitored

    def _refresh_mix_meter(self, mix_id, mix):
        """Point a meter at the mix's sink (its monitor carries the audio).

        Re-pointed idempotently from the stream tick: installing mixes
        destroys and recreates their sinks, which kills the pw-cat under
        the meter — running() going false is how that is noticed.
        """
        key = f"mix:{mix_id}"
        sink = mix.get("sink")
        if not sink:
            return
        if self._meter_targets.get(key) == sink and self.meter.active(key):
            return
        self._meter_targets[key] = sink
        def _on_mix_level(level, mid=mix_id):
            self._remote_levels[f"mix:{mid}"] = round(float(level), 4)
            self.matrix.set_mix_level(mid, level)

        self.meter.start(key, sink, _on_mix_level, capture_sink=True)

    def _stop_mix_meter(self, mix_id):
        key = f"mix:{mix_id}"
        if self._meter_targets.pop(key, None) is not None:
            self.meter.stop(key)

    def _on_add_mix_clicked(self, _matrix):
        dialog = MixDialog(
            heading="Add Mix", confirm_label="Add Mix",
            name="", icon_name=mixes_module.DEFAULT_ICON,
        )
        dialog.connect("mix-confirmed", self._on_mix_created)
        dialog.present(self)

    def _on_rename_mix_clicked(self, _matrix, mix_id):
        mix = self._mixes.get(mix_id)
        if mix is None:
            return
        dialog = MixDialog(
            heading="Rename Mix", confirm_label="Save",
            name=mix.get("name", ""),
            icon_name=mix.get("icon_name", mixes_module.DEFAULT_ICON),
        )
        dialog.connect("mix-confirmed", self._on_mix_renamed, mix_id)
        dialog.present(self)

    def _on_remove_mix_clicked(self, _matrix, mix_id):
        if len(self._mixes) <= 1:
            return  # the header control is already insensitive; belt and braces
        mix = self._mixes.get(mix_id)
        if mix is None:
            return
        name = mix.get("name", "this mix")
        description = mix.get("description") or mix.get("sink", "")
        dialog = Adw.AlertDialog(
            heading="Delete mix?",
            body=f"“{name}” and its levels for every source are deleted, "
                 f"and the “{description}” audio device disappears. "
                 f"Anything recording or listening to it — OBS, Discord — "
                 f"loses that input until it is pointed somewhere else.",
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("delete", "Delete")
        dialog.set_response_appearance("delete", Adw.ResponseAppearance.DESTRUCTIVE)
        dialog.set_default_response("cancel")
        dialog.choose(
            self, None, lambda d, r: self._on_remove_mix_response(d, r, mix_id),
        )

    def _refresh_source_meter(self, source_id):
        """Point a source's meter at whatever currently carries its audio."""
        source = self._sources.get(source_id)
        if not source:
            return
        if sources_module.kind(source) == sources_module.KIND_DEVICE:
            self._refresh_device_meter(source_id, source)
        else:
            self._refresh_app_meter(source_id)

    def _set_source_waiting(self, source_id, waiting, hint="Waiting for audio"):
        cell = self.matrix.source(source_id)
        if cell is not None:
            cell.set_waiting(waiting, hint)

    def remote_levels(self):
        """Every live meter's latest peak, as JSON, for the `levels` action.

        Fed by the same callbacks that move the bars — publishing costs a
        dict write per meter frame, and reading is one Describe. A remote
        polls this only while a dial with a meter is actually on screen.
        """
        return json.dumps(self._remote_levels)

    def _bound_app_names(self):
        """Application names some row already matches, so the picker cannot
        offer a duplicate. claim_streams() gives every stream exactly one
        owner regardless, so a duplicate could never double-route -- but it
        would sit in the matrix as a silently inert fader, which reads as
        broken. Built from bindings() so multi-name rows cover all of theirs.
        """
        return {
            name
            for source in self._sources.values()
            for name in sources_module.bindings(source)
        }

    def _bound_capture_nodes(self):
        """Capture nodes that already have a row, so the picker cannot make a
        duplicate. The Wave's own mic is in the set: it is the built-in row,
        and a second row for it would double the same audio into every mix."""
        nodes = {
            source.get("node_name")
            for source in self._sources.values()
            if sources_module.kind(source) == sources_module.KIND_DEVICE
        }
        # mixer.mic is deliberately NOT excluded: the Wave's own input gets a
        # row like any other, and the device controls in the sidebar are a
        # separate concern from whether it appears in the matrix.
        return {node for node in nodes if node}

    def _on_device_source_confirmed(self, _dialog, name, node_name, icon_name,
                                    group=""):
        # Queue the re-snapshot before installing: the reconcile that
        # _install_source triggers refuses to wire a node the snapshot has not
        # seen, and the worker runs queued tasks in insertion order, so the
        # refresh lands first. Doing it synchronously would put a pw-dump on
        # the GTK thread in a click handler.
        self.mixer.request_capture_poll()
        source = sources_module.new_device_source(
            name=name, node_name=node_name, icon_name=icon_name,
        )
        if group:
            source["group"] = group
        self._install_source(source)

    def _show_error(self, heading, error):
        logging.warning("%s: %s", heading, error)
        dialog = Adw.AlertDialog(heading=heading, body=str(error))
        dialog.add_response("ok", "OK")
        dialog.choose(self, None, lambda d, r: d.choose_finish(r))

    def _refresh_outputs(self):
        sinks, default = self.mixer.output_sinks(), self.mixer.default_sink()
        for mid in self._mixes:
            self.matrix.set_mix_outputs(mid, *self._output_entries(mid, sinks, default))

    def _on_mix_output_changed(self, _matrix, mix_id, output):
        self.mixer.set_output(mix_id, output)
        self._refresh_outputs()

    def _on_mix_volume_changed(self, _matrix, mix_id, value):
        self.mixer.set_mix_volume(mix_id, value)

    def _on_mix_created(self, _dialog, name, icon_name):
        mix = mixes_module.new_mix(name=name, icon_name=icon_name)
        self._mixes = mixes_module.add(self._mixes, mix)
        self.mixer.set_mixes(self._mixes)
        self.matrix.add_mix(mix["id"], title=mix["name"], subtitle=mix["subtitle"], icon_name=icon_name)
        for sid in self._sources:
            self._wire_cell(sid, mix["id"])
        self._refresh_outputs()

    def _on_mix_renamed(self, _dialog, name, icon_name, mix_id):
        if mix_id not in self._mixes:
            return
        self._mixes = mixes_module.update(self._mixes, mix_id, name=name, icon_name=icon_name)
        self.mixer.set_mixes(self._mixes)
        self.matrix.set_mix(mix_id, title=name, icon_name=icon_name)

    def _cancel_cell_edits(self, source_id=None, mix_id=None):
        for key, timer in list(self._cell_debounce_ids.items()):
            if (source_id is None or key[0] == source_id) and (mix_id is None or key[1] == mix_id):
                GLib.source_remove(timer)
                del self._cell_debounce_ids[key]

    def _on_remove_mix_response(self, dialog, result, mix_id):
        if dialog.choose_finish(result) != "delete" or mix_id not in self._mixes:
            return
        self._cancel_cell_edits(mix_id=mix_id)
        self._mixes = mixes_module.remove(self._mixes, mix_id)
        self.mixer.set_mixes(self._mixes)
        self._stop_mix_meter(mix_id)
        self.matrix.remove_mix(mix_id)
        self._remote_levels.pop("mix:" + mix_id, None)

    def _refresh_mix_emptiness(self):
        cells = self.mixer.cells()
        for mid in self._mixes:
            fed = any(not source.get("muted") and source.get("level", 1.0) > 0
                and cells.get(f"{sid}.{mid}", {}).get("volume", 0) > 0
                and not cells.get(f"{sid}.{mid}", {}).get("muted")
                for sid, source in self._sources.items())
            self.matrix.set_mix_empty(mid, not fed)

    def _refresh_device_meter(self, source_id, source):
        node = source["node_name"]
        capture = next((item for item in self.mixer.capture_sources() if item["name"] == node), None)
        row = self.matrix.source(source_id)
        if row is not None:
            row.set_available(capture is not None, reason="Capture device not connected")
            if source.get("protected"):
                row.set_removable(capture is None, tooltip="Remove disconnected device")
        if capture is None:
            self.meter.stop(source_id)
            self._meter_targets.pop(source_id, None)
            self._set_source_level(source_id, 0.0)
            return
        target = (node, capture.get("identity"))
        if self._meter_targets.get(source_id) == target and self.meter.active(source_id):
            return
        self._meter_targets[source_id] = target
        self.meter.start(source_id, node,
            lambda level, sid=source_id: self._set_source_level(sid, level),
            identity=capture["identity"])

    def _add_source_widget(self, source):
        sid = source["id"]
        self.matrix.add_source(sid, name=source["name"], icon_name=source["icon_name"],
            has_level=True, removable=not source.get("protected"), editable=True,
            reorderable=True, is_capture=sources_module.kind(source) == sources_module.KIND_DEVICE)
        self._wire_source_row(sid)

    def _wire_source_row(self, source_id):
        source = self._sources[source_id]
        row = self.matrix.source(source_id)
        row.set_volume(source.get("level", 1.0))
        row.set_muted(source.get("muted", False))
        self.matrix.set_source_group(source_id, sources_module.group(source))
        row.connect("volume-changed", self._on_source_level_changed, source_id)
        row.connect("mute-toggled", self._on_source_mute_toggled, source_id)

    def _install_source(self, source):
        self._sources = sources_module.add(self._sources, source)
        self.mixer.set_sources(self._sources)
        self._add_source_widget(source)
        for mid in self._mixes:
            self._wire_cell(source["id"], mid)
        self._refresh_source_meter(source["id"])

    def _add_discovered_inputs(self, captures):
        waves = [item for item in captures if item["name"].startswith(SOURCE_MATCHES)]
        bound = self._bound_capture_nodes()
        for capture in waves:
            node = capture["name"]
            if node in bound or node in self._offered_nodes:
                continue
            source = sources_module.new_device_source(name=capture["description"], node_name=node)
            source["protected"] = True
            source["channels"] = capture.get("channels", 2)
            self._install_source(source)
            self._offered_nodes.add(node)
            if len(waves) == 1 and "mic" not in self._sources:
                legacy = {key.split(".", 1)[1]: state for key, state in self.mixer.cells().items() if key.startswith("mic.")}
                for mid, state in legacy.items():
                    if mid in self._mixes:
                        self.mixer.set_cell(source["id"], mid, state["volume"], state["muted"])
                        cell = self.matrix.cell(source["id"], mid)
                        cell.set_volume(state["volume"])
                        cell.set_muted(state["muted"])
                if legacy:
                    self.mixer.remove_source("mic")
            self._save_ui_state()

    def _on_edit_source_clicked(self, _matrix, source_id):
        if source_id not in self._sources:
            return
        dialog = AddSourceDialog(source=self._sources[source_id],
            streams=self.mixer.streams().values(), captures=self.mixer.capture_sources())
        dialog.connect("source-edited", self._on_source_edited)
        dialog.present(self)

    def _on_source_edited(self, _dialog, source_id, name, binding, icon_name, group=""):
        if source_id not in self._sources:
            return
        fields = dict(name=name, icon_name=icon_name, group=group)
        if group.strip() and group.strip() != sources_module.group(self._sources[source_id]):
            if any(sid != source_id and sources_module.group(record) == group.strip()
                   and not record["muted"] for sid, record in self._sources.items()):
                fields["muted"] = True
        if sources_module.kind(self._sources[source_id]) == sources_module.KIND_APP:
            fields["match_app_names"] = sources_module.parse_bindings(binding)
        self._sources = sources_module.update(self._sources, source_id, **fields)
        self.mixer.set_sources(self._sources)
        self.matrix.set_source(source_id, name=name, icon_name=icon_name)
        self.matrix.set_source_group(source_id, group)
        self.matrix.source(source_id).set_muted(self._sources[source_id]["muted"])
        if "muted" in fields:
            self._sync_hw_mute(self._sources[source_id], fields["muted"])
        self.meter.stop(source_id)
        self._meter_targets.pop(source_id, None)
        self._refresh_source_meter(source_id)

    def _on_move_source_clicked(self, _matrix, source_id, delta):
        if source_id not in self._sources:
            return
        self._sources = sources_module.reorder(self._sources, source_id, delta)
        self.mixer.set_sources(self._sources)
        self.matrix.reorder_sources(list(self._sources))
        for sid in self._sources:
            self._wire_source_row(sid)
        self._wire_matrix_cells()

    def set_source_volume(self, source_id, level):
        if source_id not in self._sources:
            return False
        level = sources_module.level(level)
        self._sources = sources_module.update(self._sources, source_id, level=level)
        self.mixer.set_source_level(source_id, level, self._sources[source_id]["muted"])
        self.matrix.source(source_id).set_volume(level)
        self._refresh_mix_emptiness()
        return True

    def _on_source_level_changed(self, _row, level, source_id):
        self.set_source_volume(source_id, level)

    def _set_source_muted(self, source_id, muted, *, hardware=True):
        source = self._sources.get(source_id)
        if source is None:
            return None
        group = sources_module.group(source)
        changes = {}
        for sid, record in self._sources.items():
            value = bool(muted) if sid == source_id else record["muted"]
            if not muted and group and sid != source_id and sources_module.group(record) == group:
                value = True
            if value != record["muted"]:
                changes[sid] = value
        candidate = {sid: dict(record, muted=changes.get(sid, record["muted"]))
                     for sid, record in self._sources.items()}
        sources_module.save(candidate)
        self._sources = candidate
        # One desired-state snapshot prevents an intermediate double-live group.
        self.mixer.set_sources(candidate)
        for sid, value in sorted(changes.items(), key=lambda item: not item[1]):
            self.matrix.source(sid).set_muted(value)
            if hardware or sid != source_id:
                self._sync_hw_mute(candidate[sid], value)
        self._refresh_mix_emptiness()
        self._notify_tray()
        return bool(muted)

    def _on_source_mute_toggled(self, _row, muted, source_id):
        self._set_source_muted(source_id, muted)

    def toggle_source_mute(self, source_id):
        if source_id not in self._sources:
            return None
        return self._set_source_muted(source_id, not self._sources[source_id]["muted"])

    def _device_for_source(self, source):
        capture = next((item for item in self.mixer.capture_sources()
                        if item["name"] == source.get("node_name")), None)
        if capture is None:
            return None
        candidates = [dev for dev in self._devs if
            (capture.get("alsa_card") is not None and str(capture["alsa_card"]) == str(dev.alsa_card))
            or (capture.get("serial") and capture["serial"] == dev.info.get("serial"))]
        return candidates[0] if len(candidates) == 1 else None

    def _sync_hw_mute(self, source, muted):
        if sources_module.kind(source) != sources_module.KIND_DEVICE:
            return
        device = self._device_for_source(source)
        if device is None:
            self.mixer.set_capture_mute(source["node_name"], muted)
            return
        self._usb_async(lambda: device.set_mute(muted),
            on_error=lambda error: self._on_device_error(device, error), device=device)

    def _follow_capture_mutes(self):
        mutes = self.mixer.capture_mutes()
        previous = self._capture_mute_seen
        self._capture_mute_seen = mutes
        for sid, source in list(self._sources.items()):
            node = source.get("node_name")
            if node not in mutes or node not in previous or self._device_for_source(source) is not None:
                continue
            if mutes[node] != previous[node] and mutes[node] != source["muted"]:
                self._set_source_muted(sid, mutes[node], hardware=False)

    def _notify_tray(self):
        application = self.get_application()
        if isinstance(application, WaveXLRApp):
            application.refresh_tray()

    def capture_rows_muted(self):
        captures = [record for record in self._sources.values()
                    if sources_module.kind(record) == sources_module.KIND_DEVICE]
        return bool(captures) and all(record["muted"] for record in captures)

    def source_groups(self):
        groups = [sources_module.group(record) for record in self._sources.values()]
        return sorted(name for name in set(groups) if name and groups.count(name) > 1)

    def switch_group(self, name):
        members = [sid for sid, record in self._sources.items() if sources_module.group(record) == name]
        if len(members) < 2:
            return ""
        live = next((sid for sid in members if not self._sources[sid]["muted"]), None)
        target = members[0] if live is None else members[(members.index(live) + 1) % len(members)]
        self._set_source_muted(target, False)
        return target

    def _on_switch_source_clicked(self, _matrix, source_id):
        source = self._sources.get(source_id)
        if source is None:
            return
        if source["muted"]:
            self._set_source_muted(source_id, False)
        else:
            self.switch_group(sources_module.group(source))

    def _on_group_sources_clicked(self, _matrix, dragged_id, target_id):
        if dragged_id == target_id or dragged_id not in self._sources or target_id not in self._sources:
            return
        target = self._sources[target_id]
        group = sources_module.group(target) or target["name"]
        self._sources[dragged_id]["group"] = group
        self._sources[target_id]["group"] = group
        self.matrix.set_source_group(dragged_id, group)
        self.matrix.set_source_group(target_id, group)
        self._set_source_muted(target_id, False)

    def set_cell_volume(self, source_id, mix_id, volume):
        if source_id not in self._sources or mix_id not in self._mixes:
            return False
        volume = sources_module.level(volume)
        state = self.mixer.get_cell(source_id, mix_id)
        self.mixer.set_cell(source_id, mix_id, volume, state["muted"])
        self.matrix.cell(source_id, mix_id).set_volume(volume)
        self._refresh_mix_emptiness()
        return True

    def toggle_cell_mute(self, source_id, mix_id):
        if source_id not in self._sources or mix_id not in self._mixes:
            return None
        state = self.mixer.get_cell(source_id, mix_id)
        muted = not state["muted"]
        self.mixer.set_cell(source_id, mix_id, state["volume"], muted)
        self.matrix.cell(source_id, mix_id).set_muted(muted)
        self._refresh_mix_emptiness()
        return muted


class WaveXLRApp(Adw.Application):
    def __init__(self):
        super().__init__(
            application_id="com.github.openwave",
            flags=Gio.ApplicationFlags.HANDLES_COMMAND_LINE,
        )
        self._window = None
        self._start_hidden = False
        self._tray = None
        self.add_main_option(
            "hide", 0, GLib.OptionFlags.NONE, GLib.OptionArg.NONE,
            "Start hidden in system tray", None,
        )

    def do_command_line(self, command_line):
        options = command_line.get_options_dict()
        if options.contains("hide"):
            self._start_hidden = True
        self.activate()
        return 0

    def do_activate(self):
        if not self._window:
            self._load_css()
            # The user unit is OpenWave-owned state, so keep it aligned with
            # the installed app without hiding a routine upgrade behind the
            # first-run dialog. Failures still fall through to that repair UI.
            if not setup.is_sandboxed() and service.is_installed() and service.needs_refresh():
                try:
                    service.install()
                except Exception as e:
                    logging.getLogger("openwave.app").warning(
                        "Failed to refresh audio service: %s", e
                    )
            if not setup.is_sandboxed() and setup.needs_setup():
                self._show_setup_dialog()
                return
            self._window = WaveXLRWindow(application=self)
            # Hide-to-tray on close instead of quitting
            self._window.connect("close-request", self._on_close_request)
            self._setup_tray()
            if self._start_hidden and self._tray is not None:
                self._start_hidden = False  # only first launch
                return
        self._window.present()

    def _load_css(self):
        """Load OpenWave's stylesheet — alongside the .py files, or under share/."""
        beside_module = os.path.join(
            os.path.dirname(os.path.abspath(__file__)), "style.css"
        )
        css_path = (
            beside_module
            if os.path.exists(beside_module)
            else paths.data_file("style.css")
        )
        if css_path is None:
            return
        provider = Gtk.CssProvider()
        provider.load_from_path(css_path)
        display = Gdk.Display.get_default()
        if display is not None:
            Gtk.StyleContext.add_provider_for_display(
                display, provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION
            )

    def do_shutdown(self):
        if self._tray is not None:
            self._tray.unregister()
            self._tray = None
        if self._window is not None:
            window = self._window
            window._save_ui_state()
            window.shutdown()
            window.mixer.stop()
            window.meter.stop_all()
        Adw.Application.do_shutdown(self)


    def _on_close_request(self, window):
        if self._tray:
            window.set_visible(False)
            return True  # prevent destroy, keep running in tray
        return False  # normal close → quit

    def _setup_tray(self):
        from .tray import TrayIcon
        tray = TrayIcon(on_activate=self._show_window, on_open=self._show_window,
                        on_mute=self._toggle_mute, on_quit=self._quit_app)
        if tray.register():
            self._tray = tray
            self.hold()
            self.refresh_tray()


    def _toggle_mute(self):
        if self._window and self._window.dev.connected:
            current = self._window._last_state and self._window._last_state.get("mute", False)
            self._window._device_async("set_mute", not current)

    def _quit_app(self):
        if self._tray is not None:
            self.release()
        self.quit()


    def _show_window(self):
        if self._window:
            self._window.present()

    def _show_setup_dialog(self):
        dialog = Adw.AlertDialog(
            heading="First-Time Setup",
            body="OpenWave needs to configure USB permissions and install the audio service.\n\nYou may be prompted for your password.",
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("setup", "Set Up")
        dialog.set_response_appearance("setup", Adw.ResponseAppearance.SUGGESTED)
        dialog.set_default_response("setup")

        tmp_win = Adw.ApplicationWindow(application=self)
        tmp_win.present()

        dialog.choose(tmp_win, None, self._on_setup_response, tmp_win)

    def _on_setup_response(self, dialog, result, tmp_win):
        response = dialog.choose_finish(result)
        tmp_win.close()

        if response != "setup":
            self.quit()
            return

        success, message = setup.run_setup()
        if success:
            replug_dialog = Adw.AlertDialog(
                heading="Setup Complete",
                body=f"{message}.\n\nPlease replug your Elgato Wave device, then click Continue.",
            )
            replug_dialog.add_response("continue", "Continue")
            replug_dialog.set_default_response("continue")

            tmp_win2 = Adw.ApplicationWindow(application=self)
            tmp_win2.present()
            replug_dialog.choose(tmp_win2, None, self._on_replug_done, tmp_win2)
        else:
            err_dialog = Adw.AlertDialog(
                heading="Setup Failed",
                body=message,
            )
            err_dialog.add_response("ok", "OK")
            err_win = Adw.ApplicationWindow(application=self)
            err_win.present()
            err_dialog.choose(err_win, None, lambda d, r, w: (w.close(), self.quit()), err_win)

    def _on_replug_done(self, dialog, result, tmp_win):
        dialog.choose_finish(result)
        tmp_win.close()
        win = WaveXLRWindow(application=self)
        self._window = win
        win.present()

    def refresh_tray(self):
        if self._tray is not None and self._window is not None:
            window = self._window
            self._tray.set_state(bool(window._devs),
                hardware_muted=any(state["mute"] for state in window._device_states.values()),
                row_muted=window.capture_rows_muted(),
                display_name=window.dev.profile.display_name if window.dev.profile else None)



def main():
    app = WaveXLRApp()
    return app.run(sys.argv)
