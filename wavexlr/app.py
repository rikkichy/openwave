"""OpenWave — GTK4 + Adwaita control application for Elgato Wave devices."""

import gi
gi.require_version('Gtk', '4.0')
gi.require_version('Adw', '1')

from gi.repository import Gtk, Adw, GLib, GObject, Gio, Gdk
import logging
import os
import sys
from concurrent.futures import ThreadPoolExecutor

from .device import DeviceNotReadyError, DeviceUnresponsiveError, WaveDevice
from .meter import MeterMonitor
from .mixer import Mixer
from .mixmatrix import MixMatrix
from .sourcedialog import AddSourceDialog
from . import paths, setup, service, sources as sources_module

logging.basicConfig(level=logging.INFO, format="%(name)s: %(message)s")

KNOB_LABELS = {"gain": "Gain", "hp": "Headphones", "mix": "Monitor Mix"}


class WaveXLRWindow(Adw.ApplicationWindow):
    def __init__(self, **kwargs):
        super().__init__(**kwargs, title="OpenWave", default_width=1100, default_height=620)
        self.set_size_request(900, 520)
        self.dev = WaveDevice()
        # All control transfers share endpoint 0. Keep them on one worker:
        # overlapping polls can queue behind a one-second USB timeout and keep
        # hammering a device that is already failing.
        self._usb_executor = ThreadPoolExecutor(
            max_workers=1, thread_name_prefix="openwave-usb"
        )
        self._shutting_down = False
        self._connect_pending = False
        self._reconnect_id = None
        self._poll_pending = False
        self._poll_failures = 0
        self._gain_max = 0x5000
        self._updating_ui = False
        self._last_state = None
        self._poll_id = None
        self._stream_poll_id = None
        self._gain_timeout = None
        self._hp_timeout = None
        self._mix_timeout = None
        # Debounce slider events to coalesce a flurry of value-changed signals
        # during a drag into one set_cell. {(source_id, mix_id): timeout_id}.
        self._cell_debounce_ids = {}
        self._sources = sources_module.load()

        self._build_ui()
        self._update_service_status()
        self.mixer = Mixer()
        self.mixer.set_sources(self._sources)
        self.mixer.start()
        self.meter = MeterMonitor()
        self._meter_targets = {}
        self._wire_matrix_cells()
        self._start_meters()
        self._start_stream_poll()
        self._try_connect()

    def _build_ui(self):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.set_content(box)

        # Header bar
        header = Adw.HeaderBar()
        self.status_label = Gtk.Label(label="Disconnected")
        self.status_label.add_css_class("dim-label")
        header.set_title_widget(self.status_label)

        refresh_btn = Gtk.Button(icon_name="view-refresh-symbolic", tooltip_text="Reconnect")
        refresh_btn.connect("clicked", lambda _: self._try_connect())
        header.pack_end(refresh_btn)

        # Sidebar toggle (placed at the end so it sits next to the close button)
        self.sidebar_toggle = Gtk.ToggleButton(
            icon_name="sidebar-show-symbolic",
            tooltip_text="Toggle device panel",
            active=True,
        )
        header.pack_end(self.sidebar_toggle)
        box.append(header)

        # --- Split view: matrix (content) | device controls (sidebar) ---------
        self.split = Adw.OverlaySplitView(
            sidebar_position=Gtk.PackType.END,
            min_sidebar_width=320,
            max_sidebar_width=420,
            sidebar_width_fraction=0.30,
            vexpand=True,
        )
        box.append(self.split)

        self.sidebar_toggle.bind_property(
            "active", self.split, "show-sidebar",
            GObject.BindingFlags.BIDIRECTIONAL | GObject.BindingFlags.SYNC_CREATE,
        )

        # Auto-collapse the sidebar into an overlay on narrow windows.
        bp = Adw.Breakpoint.new(Adw.BreakpointCondition.parse("max-width: 900sp"))
        bp.add_setter(self.split, "collapsed", True)
        self.add_breakpoint(bp)

        # --- Content: mix matrix ---------------------------------------------
        self.matrix = MixMatrix()
        self.split.set_content(self.matrix)

        self.matrix.add_mix(
            "personal", title="Personal Mix",
            subtitle="What you hear",
            icon_name="audio-headphones-symbolic",
        )
        self.matrix.add_mix(
            "chat", title="Chat Mix",
            subtitle="To voice apps (v0.3.0)",
            icon_name="system-users-symbolic",
        )
        self.matrix.add_mix(
            "record", title="Record Mix",
            subtitle="To OBS / recording (v0.3.0)",
            icon_name="media-record-symbolic",
        )

        self.mic_source = self.matrix.add_source(
            "mic", name="Microphone",
            icon_name="audio-input-microphone-symbolic",
            has_level=True,
        )
        self.mic_source.connect("volume-changed", self._on_mic_matrix_volume_changed)
        self.mic_source.connect("mute-toggled", self._on_mic_matrix_mute_toggled)

        # User-defined app sources (persisted)
        for source_id, source in self._sources.items():
            self.matrix.add_source(
                source_id,
                name=source.get("name", source_id),
                icon_name=source.get("icon_name", "applications-multimedia-symbolic"),
                has_level=True,
                removable=True,
            )

        self.matrix.connect("add-source-clicked", self._on_add_source_clicked)
        self.matrix.connect("remove-source-clicked", self._on_remove_source_clicked)

        # --- Sidebar: device controls -----------------------------------------
        sidebar_scroll = Gtk.ScrolledWindow(
            vexpand=True,
            hscrollbar_policy=Gtk.PolicyType.NEVER,
            vscrollbar_policy=Gtk.PolicyType.AUTOMATIC,
        )
        sidebar_clamp = Adw.Clamp(
            maximum_size=380,
            margin_start=12, margin_end=12, margin_top=12, margin_bottom=12,
        )
        sidebar_scroll.set_child(sidebar_clamp)

        sidebar_content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        sidebar_clamp.set_child(sidebar_content)
        self._build_device_pane(sidebar_content)

        self.split.set_sidebar(sidebar_scroll)

    def _build_device_pane(self, parent):
        """Populate the right-hand column with Audio / Mic / HP / Device Info groups."""
        # --- Audio fix status ---
        status_group = Adw.PreferencesGroup(title="Audio")
        parent.append(status_group)

        self.audio_status_row = Adw.ActionRow(
            title="Capture Fix",
            subtitle="Keeps mic capture active to prevent the race condition"
        )
        self.audio_status_icon = Gtk.Image(icon_name="emblem-ok-symbolic")
        self.audio_status_icon.add_css_class("dim-label")
        self.audio_status_row.add_suffix(self.audio_status_icon)

        self.uninstall_btn = Gtk.Button(icon_name="user-trash-symbolic", valign=Gtk.Align.CENTER, tooltip_text="Uninstall capture fix")
        self.uninstall_btn.add_css_class("flat")
        self.uninstall_btn.connect("clicked", self._on_uninstall_clicked)
        self.audio_status_row.add_suffix(self.uninstall_btn)

        status_group.add(self.audio_status_row)

        # --- Mic controls ---
        mic_group = Adw.PreferencesGroup(title="Microphone")
        parent.append(mic_group)

        mute_row = Adw.SwitchRow(title="Mute", subtitle="Toggle microphone mute")
        mute_row.connect("notify::active", self._on_mute_changed)
        self.mute_row = mute_row
        mic_group.add(mute_row)

        gain_row = Adw.ActionRow(title="Gain")
        self.gain_label = Gtk.Label(label="0x0000", width_chars=8, xalign=1)
        self.gain_label.add_css_class("monospace")
        gain_row.add_suffix(self.gain_label)
        mic_group.add(gain_row)

        self.gain_scale = Gtk.Scale(
            orientation=Gtk.Orientation.HORIZONTAL,
            hexpand=True,
            draw_value=False,
            adjustment=Gtk.Adjustment(lower=0x0000, upper=0x5000, step_increment=0x40, page_increment=0x200),
        )
        self.gain_scale.set_margin_start(12)
        self.gain_scale.set_margin_end(12)
        self.gain_scale.connect("value-changed", self._on_gain_changed)
        parent.append(self.gain_scale)

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
        self.hp_scale.set_margin_start(12)
        self.hp_scale.set_margin_end(12)
        self.hp_scale.connect("value-changed", self._on_hp_changed)
        parent.append(self.hp_scale)

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
        self.mix_scale.set_visible(False)
        self.mix_scale.connect("value-changed", self._on_mix_changed)
        parent.append(self.mix_scale)

        # --- Device info ---
        info_group = Adw.PreferencesGroup(title="Device Info")
        parent.append(info_group)

        self.fw_row = Adw.ActionRow(title="Firmware")
        self.fw_label = Gtk.Label(label="—")
        self.fw_label.add_css_class("dim-label")
        self.fw_row.add_suffix(self.fw_label)
        info_group.add(self.fw_row)

        self.api_row = Adw.ActionRow(title="API Version")
        self.api_label = Gtk.Label(label="—")
        self.api_label.add_css_class("dim-label")
        self.api_row.add_suffix(self.api_label)
        info_group.add(self.api_row)

        self.serial_row = Adw.ActionRow(title="Serial")
        self.serial_label = Gtk.Label(label="—")
        self.serial_label.add_css_class("dim-label")
        self.serial_row.add_suffix(self.serial_label)
        info_group.add(self.serial_row)

    def _update_service_status(self):
        """Check if the audio service is running."""
        active = service.is_running()

        if active:
            self.audio_status_icon.set_from_icon_name("emblem-ok-symbolic")
            self.audio_status_icon.remove_css_class("dim-label")
            self.audio_status_row.set_subtitle("Audio service running")
            self.uninstall_btn.set_visible(True)
        else:
            self.audio_status_icon.set_from_icon_name("dialog-warning-symbolic")
            # Distinguish a service that never came up from one that is not
            # installed at all: both leave the capture fix off, but only the
            # first has anything to read in `journalctl --user -u openwave`.
            if service.is_failed():
                subtitle = "Audio service failed to start"
            elif service.is_installed():
                subtitle = "Audio service installed but not running"
            else:
                subtitle = "Audio service not running"
            self.audio_status_row.set_subtitle(subtitle)
            self.uninstall_btn.set_visible(False)

    def _on_uninstall_clicked(self, btn):
        dialog = Adw.AlertDialog(
            heading="Uninstall Capture Fix?",
            body="This will remove the audio service and USB permissions.\n\nYou can reinstall them by restarting OpenWave.",
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("uninstall", "Uninstall")
        dialog.set_response_appearance("uninstall", Adw.ResponseAppearance.DESTRUCTIVE)
        dialog.set_default_response("cancel")
        dialog.choose(self, None, self._on_uninstall_response)

    def _on_uninstall_response(self, dialog, result):
        response = dialog.choose_finish(result)
        if response != "uninstall":
            return
        success, message = setup.run_uninstall()
        self._update_service_status()
        if not success:
            err = Adw.AlertDialog(heading="Uninstall Failed", body=message)
            err.add_response("ok", "OK")
            err.choose(self, None, lambda d, r: d.choose_finish(r))

    def _usb_async(self, fn, on_done=None, on_error=None):
        """Run a USB operation on the single device worker."""
        if self._shutting_down:
            return None

        future = self._usb_executor.submit(fn)

        def _finished(done):
            if self._shutting_down:
                return
            try:
                result = done.result()
            except Exception as e:
                if on_error:
                    GLib.idle_add(on_error, e)
            else:
                if on_done:
                    GLib.idle_add(on_done, result)

        future.add_done_callback(_finished)
        return future

    def _try_connect(self):
        if self._connect_pending or self._shutting_down:
            return
        if self._reconnect_id:
            GLib.source_remove(self._reconnect_id)
            self._reconnect_id = None
        self._stop_polling()
        self._connect_pending = True
        self.status_label.set_label("Connecting...")

        def _connect():
            self.dev.disconnect()
            try:
                self.dev.connect()
                info = {}
                try:
                    info = self.dev.read_device_info()
                except Exception:
                    pass
                return {"state": self.dev.get_all(), "info": info}
            except Exception:
                self.dev.disconnect()
                raise

        def _done(result):
            self._connect_pending = False
            self._poll_pending = False
            self._poll_failures = 0
            self._apply_profile(self.dev.profile)
            logging.getLogger("openwave.app").info(
                "Connected to %s", self.dev.profile.display_name
            )
            self.status_label.remove_css_class("dim-label")
            self._apply_state(result["state"])
            info = result["info"]
            self.fw_label.set_label(info.get("fw_version", "—"))
            self.api_label.set_label(info.get("api_version", "—"))
            self.serial_label.set_label(info.get("serial", "—"))
            self._start_polling()

        def _fail(e):
            self._connect_pending = False
            logging.getLogger("openwave.app").warning(
                "Device connection failed: %s", e
            )
            if isinstance(e, DeviceUnresponsiveError):
                self.status_label.set_label("Power-cycle Wave device")
            else:
                self.status_label.set_label("Disconnected")
            self.status_label.add_css_class("dim-label")
            if isinstance(e, DeviceNotReadyError):
                self._schedule_reconnect()

        self._usb_async(_connect, _done, _fail)

    def _schedule_reconnect(self):
        if self._reconnect_id or self._shutting_down:
            return

        def _retry():
            self._reconnect_id = None
            self._try_connect()
            return False

        self._reconnect_id = GLib.timeout_add_seconds(2, _retry)

    def _start_polling(self):
        """Start 10 Hz polling to sync hardware state."""
        if self._poll_id:
            GLib.source_remove(self._poll_id)
        self._poll_id = GLib.timeout_add(100, self._poll_tick)

    def _stop_polling(self):
        if self._poll_id:
            GLib.source_remove(self._poll_id)
            self._poll_id = None

    def _poll_tick(self):
        """Queue one poll when the previous poll has completed."""
        if not self.dev.connected:
            self._poll_id = None
            return False
        if self._poll_pending:
            return True
        self._poll_pending = True
        self._usb_async(
            self.dev.get_all, self._on_poll_result, self._on_poll_error
        )
        return True

    def _on_poll_result(self, state):
        self._poll_pending = False
        self._poll_failures = 0
        if state != self._last_state:
            self._apply_state(state)

    def _on_poll_error(self, e):
        self._poll_pending = False
        self._poll_failures += 1
        logging.getLogger("openwave.app").warning(
            "Device poll failed (%d/3): %s", self._poll_failures, e
        )
        if self._poll_failures < 3:
            return
        if isinstance(e, DeviceUnresponsiveError):
            self.status_label.set_label("Power-cycle Wave device")
        else:
            self.status_label.set_label("Disconnected")
        self.status_label.add_css_class("dim-label")
        self._stop_polling()
        self._usb_async(self.dev.disconnect)

    def shutdown(self):
        """Stop queued USB work before closing the shared libusb handle."""
        self._shutting_down = True
        if self._reconnect_id:
            GLib.source_remove(self._reconnect_id)
            self._reconnect_id = None
        self._stop_polling()
        self._usb_executor.shutdown(wait=True, cancel_futures=True)
        self.dev.disconnect()

    def _apply_profile(self, profile):
        """Adapt the UI to the connected device model."""
        self._gain_max = profile.gain_max
        self.gain_scale.get_adjustment().set_upper(profile.gain_max)
        self.knob_row.set_visible(profile.has_vol_select)
        self.lowz_row.set_visible(profile.has_low_z)
        self.mix_row.set_visible(profile.has_monitor_mix)
        self.mix_scale.set_visible(profile.has_monitor_mix)
        if profile.has_monitor_mix:
            self.mix_scale.get_adjustment().set_upper(profile.mix_max)
        self.mic_source.set_name(profile.display_name)
        self.status_label.set_label(f"OpenWave — {profile.display_name}")

    def _format_gain(self, raw):
        scale = self.dev.profile.gain_scale if self.dev.profile else None
        if scale:
            return f"{raw / scale:.1f} dB"
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
        if "volume_select" in state:
            self.knob_label.set_label(KNOB_LABELS.get(state["volume_select"], "Gain"))
        if "monitor_mix" in state:
            self.mix_scale.set_value(state["monitor_mix"])
            self.mix_label.set_label(f"{state['monitor_mix'] / 256:.0f}%")
        self.mic_source.set_volume(state["gain_raw"] / self._gain_max)
        self.mic_source.set_muted(state["mute"])
        self._updating_ui = False

    def _on_usb_error(self, e):
        logging.getLogger("openwave.app").warning("Device write failed: %s", e)
        if isinstance(e, DeviceUnresponsiveError):
            self.status_label.set_label("Power-cycle Wave device")
        else:
            self.status_label.set_label("Disconnected")
        self.status_label.add_css_class("dim-label")
        self._stop_polling()
        self._usb_async(self.dev.disconnect)

    def _on_mute_changed(self, row, _pspec):
        if self._updating_ui or not self.dev.connected:
            return
        muted = row.get_active()
        self._usb_async(lambda: self.dev.set_mute(muted), on_error=self._on_usb_error)

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
        self._usb_async(lambda: self.dev.set_gain_raw(val), on_error=self._on_usb_error)
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
        self._usb_async(lambda: self.dev.set_hp_volume_db(db), on_error=self._on_usb_error)
        return False

    def _on_lowz_changed(self, row, _pspec):
        if self._updating_ui or not self.dev.connected:
            return
        enabled = row.get_active()
        self._usb_async(lambda: self.dev.set_low_impedance(enabled), on_error=self._on_usb_error)

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
        self._usb_async(lambda: self.dev.set_monitor_mix(val), on_error=self._on_usb_error)
        return False

    def _on_mic_matrix_volume_changed(self, _source, value):
        if self._updating_ui or not self.dev.connected:
            return
        raw = int(value * self._gain_max)
        self.gain_label.set_label(self._format_gain(raw))
        self._updating_ui = True
        self.gain_scale.set_value(raw)
        self._updating_ui = False
        if self._gain_timeout:
            GLib.source_remove(self._gain_timeout)
        self._gain_timeout = GLib.timeout_add(200, self._send_gain, raw)

    def _on_mic_matrix_mute_toggled(self, _source, muted):
        if self._updating_ui or not self.dev.connected:
            return
        self._updating_ui = True
        self.mute_row.set_active(muted)
        self._updating_ui = False
        self._usb_async(lambda: self.dev.set_mute(muted), on_error=self._on_usb_error)

    def _wire_matrix_cells(self):
        """Bind each per-cell slider/mute to the mixer + restore persisted levels."""
        source_ids = ["mic"] + list(self._sources.keys())
        for source_id in source_ids:
            for mix_id in ("personal", "chat", "record"):
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
        self.mixer.poll_streams()
        for source_id in list(self._sources.keys()):
            self._refresh_app_meter(source_id)
        return True

    def _start_meters(self):
        """Begin metering the mic + any app source that already has a matching stream."""
        if self.mixer.mic:
            self.meter.start(
                "mic", self.mixer.mic,
                lambda level: self._set_source_level("mic", level),
            )
        for source_id in self._sources.keys():
            self._refresh_app_meter(source_id)

    def _refresh_app_meter(self, source_id):
        """Re-point the meter at the first currently-matching stream, or stop it
        if none match. Called on stream-poll changes and source add."""
        source = self._sources.get(source_id)
        if not source:
            return
        match = source.get("match_app_name")
        streams = self.mixer.streams()
        candidate = next(
            (s for s in streams.values() if s.get("app_name") == match), None,
        )
        current = self._meter_targets.get(source_id)
        if candidate is None:
            if current is not None:
                self.meter.stop(source_id)
                self._meter_targets.pop(source_id, None)
                self._set_source_level(source_id, 0.0)
            return
        if current == candidate["id"]:
            return  # already metering this stream
        self.meter.start(
            source_id, candidate["node_name"],
            lambda level, sid=source_id: self._set_source_level(sid, level),
        )
        self._meter_targets[source_id] = candidate["id"]

    def _set_source_level(self, source_id, level):
        cell = self.matrix.source(source_id)
        if cell is not None:
            cell.set_level(level)

    def _on_add_source_clicked(self, _matrix):
        dialog = AddSourceDialog()
        dialog.connect("source-confirmed", self._on_source_confirmed)
        dialog.present(self)

    def _on_source_confirmed(self, _dialog, name, match_app_name, icon_name):
        source = sources_module.new_source(
            name=name, match_app_name=match_app_name, icon_name=icon_name,
        )
        self._sources = sources_module.add(self._sources, source)
        self.matrix.add_source(
            source["id"],
            name=source["name"],
            icon_name=source["icon_name"],
            has_level=True,
            removable=True,
        )
        self._wire_cell(source["id"], "personal")
        self._wire_cell(source["id"], "chat")
        self._wire_cell(source["id"], "record")
        self.mixer.set_sources(self._sources)
        self.mixer.poll_streams()
        self._refresh_app_meter(source["id"])

    def _on_remove_source_clicked(self, _matrix, source_id):
        source = self._sources.get(source_id, {})
        name = source.get("name", "this source")
        dialog = Adw.AlertDialog(
            heading="Remove source?",
            body=f"This deletes “{name}” and its mix levels. The bound application "
                 f"itself is not affected.",
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("remove", "Remove")
        dialog.set_response_appearance("remove", Adw.ResponseAppearance.DESTRUCTIVE)
        dialog.set_default_response("cancel")
        dialog.choose(self, None, lambda d, r: self._on_remove_response(d, r, source_id))

    def _on_remove_response(self, dialog, result, source_id):
        if dialog.choose_finish(result) != "remove":
            return
        self.meter.stop(source_id)
        self._meter_targets.pop(source_id, None)
        self.matrix.remove_source(source_id)
        self._sources = sources_module.remove(self._sources, source_id)
        self.mixer.remove_source(source_id)

    _CELL_DEBOUNCE_MS = 150

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
        cur = self.mixer.get_cell(source_id, mix_id)
        self.mixer.set_cell(source_id, mix_id, value, cur["muted"])
        return False  # one-shot

    def _on_cell_mute_toggled(self, _cell, muted, source_id, mix_id):
        cur = self.mixer.get_cell(source_id, mix_id)
        self.mixer.set_cell(source_id, mix_id, cur["volume"], muted)


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
            if setup.needs_setup():
                self._show_setup_dialog()
                return
            self._window = WaveXLRWindow(application=self)
            # Hide-to-tray on close instead of quitting
            self._window.connect("close-request", self._on_close_request)
            self._setup_tray()
            if self._start_hidden:
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
        """Tear down device and audio subprocesses before exit."""
        if self._window is not None:
            self._window.shutdown()
            if hasattr(self._window, "meter"):
                self._window.meter.stop_all()
            if hasattr(self._window, "mixer"):
                self._window.mixer.stop()
        Adw.Application.do_shutdown(self)

    def _on_close_request(self, window):
        if self._tray:
            window.set_visible(False)
            return True  # prevent destroy, keep running in tray
        return False  # normal close → quit

    def _setup_tray(self):
        from .tray import TrayIcon
        self._tray = TrayIcon(
            on_activate=self._toggle_window,
            on_mute=self._toggle_mute,
            on_quit=self._quit_app,
        )
        self._tray.register()
        # Keep app alive when window is hidden
        self.hold()

    def _toggle_mute(self):
        if self._window and self._window.dev.connected:
            current = self._window._last_state and self._window._last_state.get("mute", False)
            self._window._usb_async(
                lambda: self._window.dev.set_mute(not current),
                on_error=self._window._on_usb_error,
            )

    def _quit_app(self):
        self.release()
        self.quit()

    def _toggle_window(self):
        if self._window:
            if self._window.get_visible():
                self._window.set_visible(False)
            else:
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



def main():
    app = WaveXLRApp()
    app.run(sys.argv)
