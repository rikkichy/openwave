"""OpenWave — GTK4 + Adwaita control application for the Elgato Wave XLR."""

import gi
gi.require_version('Gtk', '4.0')
gi.require_version('Adw', '1')

from gi.repository import Gtk, Adw, GLib, Gio
import logging
import sys
import threading

from .device import WaveXLR
from . import setup
import subprocess

logging.basicConfig(level=logging.INFO, format="%(name)s: %(message)s")


class WaveXLRWindow(Adw.ApplicationWindow):
    def __init__(self, **kwargs):
        super().__init__(**kwargs, title="OpenWave", default_width=380, default_height=560)
        self.xlr = WaveXLR()
        self._updating_ui = False
        self._last_state = None
        self._poll_id = None

        self._build_ui()
        self._update_service_status()
        self._try_connect()

    def _build_ui(self):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.set_content(box)

        # Header bar
        header = Adw.HeaderBar()
        self.status_label = Gtk.Label(label="Disconnected")
        self.status_label.add_css_class("dim-label")
        header.set_title_widget(self.status_label)

        # Refresh button
        refresh_btn = Gtk.Button(icon_name="view-refresh-symbolic", tooltip_text="Reconnect")
        refresh_btn.connect("clicked", lambda _: self._try_connect())
        header.pack_end(refresh_btn)
        box.append(header)

        # Scrollable content
        scroll = Gtk.ScrolledWindow(vexpand=True)
        box.append(scroll)

        clamp = Adw.Clamp(maximum_size=600, margin_start=12, margin_end=12, margin_top=12, margin_bottom=12)
        scroll.set_child(clamp)

        content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        clamp.set_child(content)

        # --- Audio fix status ---
        status_group = Adw.PreferencesGroup(title="Audio")
        content.append(status_group)

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
        content.append(mic_group)

        # Mute
        mute_row = Adw.SwitchRow(title="Mute", subtitle="Toggle microphone mute")
        mute_row.connect("notify::active", self._on_mute_changed)
        self.mute_row = mute_row
        mic_group.add(mute_row)

        # Gain
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
        content.append(self.gain_scale)

        # Knob mode indicator
        knob_row = Adw.ActionRow(title="Knob Controls", subtitle="What the physical knob adjusts")
        self.knob_label = Gtk.Label(label="Gain")
        self.knob_label.add_css_class("dim-label")
        knob_row.add_suffix(self.knob_label)
        mic_group.add(knob_row)

        # --- Headphone controls ---
        hp_group = Adw.PreferencesGroup(title="Headphones")
        content.append(hp_group)

        # HP Volume
        hp_vol_row = Adw.ActionRow(title="Volume")
        self.hp_label = Gtk.Label(label="0.0 dB", width_chars=10, xalign=1)
        self.hp_label.add_css_class("monospace")
        hp_vol_row.add_suffix(self.hp_label)
        hp_group.add(hp_vol_row)

        self.hp_scale = Gtk.Scale(
            orientation=Gtk.Orientation.HORIZONTAL,
            hexpand=True,
            draw_value=False,
            adjustment=Gtk.Adjustment(lower=-30.5, upper=0.0, step_increment=0.5, page_increment=2.0),
        )
        self.hp_scale.set_margin_start(12)
        self.hp_scale.set_margin_end(12)
        self.hp_scale.connect("value-changed", self._on_hp_changed)
        content.append(self.hp_scale)

        # Low impedance
        lowz_row = Adw.SwitchRow(title="Low Impedance", subtitle="For low impedance headphones")
        lowz_row.connect("notify::active", self._on_lowz_changed)
        self.lowz_row = lowz_row
        hp_group.add(lowz_row)

        # --- Device info ---
        info_group = Adw.PreferencesGroup(title="Device Info")
        content.append(info_group)

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
        """Check if the wavexlr systemd service is running."""
        try:
            r = subprocess.run(
                ["systemctl", "--user", "is-active", "openwave.service"],
                capture_output=True, text=True, timeout=3,
            )
            active = r.stdout.strip() == "active"
        except Exception:
            active = False

        if active:
            self.audio_status_icon.set_from_icon_name("emblem-ok-symbolic")
            self.audio_status_icon.remove_css_class("dim-label")
            self.audio_status_row.set_subtitle("Audio service running")
            self.uninstall_btn.set_visible(True)
        else:
            self.audio_status_icon.set_from_icon_name("dialog-warning-symbolic")
            self.audio_status_row.set_subtitle("Audio service not running")
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
        """Run fn in a background thread; call on_done/on_error on GTK thread."""
        def _worker():
            try:
                result = fn()
                if on_done:
                    GLib.idle_add(on_done, result)
            except Exception as e:
                if on_error:
                    GLib.idle_add(on_error, e)
        threading.Thread(target=_worker, daemon=True).start()

    def _try_connect(self):
        self.status_label.set_label("Connecting...")
        def _connect():
            self.xlr.disconnect()
            self.xlr.connect()
            info = {}
            try:
                info = self.xlr.read_device_info()
            except Exception:
                pass
            return {"state": self.xlr.get_all(), "info": info}
        def _done(result):
            self.status_label.set_label("OpenWave")
            self.status_label.remove_css_class("dim-label")
            self._apply_state(result["state"])
            info = result["info"]
            self.fw_label.set_label(info.get("fw_version", "—"))
            self.api_label.set_label(info.get("api_version", "—"))
            self.serial_label.set_label(info.get("serial", "—"))
            self._start_polling()
        def _fail(e):
            self.status_label.set_label("Disconnected")
            self.status_label.add_css_class("dim-label")
        self._usb_async(_connect, _done, _fail)

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
        """Called every 100ms — read device state in background."""
        if not self.xlr.connected:
            self._poll_id = None
            return False  # stop polling
        # Only poll if not already busy with a user-initiated write
        self._usb_async(self.xlr.get_all, self._on_poll_result, self._on_poll_error)
        return True  # keep polling

    def _on_poll_result(self, state):
        if state != self._last_state:
            self._apply_state(state)

    def _on_poll_error(self, e):
        self.status_label.set_label("Disconnected")
        self.status_label.add_css_class("dim-label")
        self.xlr.disconnect()
        self._stop_polling()

    def _apply_state(self, state):
        """Update UI from device state dict (must be called on GTK thread)."""
        self._updating_ui = True
        self._last_state = state
        self.mute_row.set_active(state["mute"])
        self.gain_scale.set_value(state["gain_raw"])
        self.gain_label.set_label(f"0x{state['gain_raw']:04X}")
        self.hp_scale.set_value(state["hp_volume_db"])
        self.hp_label.set_label(f"{state['hp_volume_db']:.1f} dB")
        self.lowz_row.set_active(state["low_impedance"])
        self.knob_label.set_label("Headphones" if state["volume_select"] == "hp" else "Gain")
        self._updating_ui = False

    def _on_usb_error(self, e):
        self.status_label.set_label("Disconnected")
        self.status_label.add_css_class("dim-label")
        self.xlr.disconnect()
        self._stop_polling()

    def _on_mute_changed(self, row, _pspec):
        if self._updating_ui or not self.xlr.connected:
            return
        muted = row.get_active()
        self._usb_async(lambda: self.xlr.set_mute(muted), on_error=self._on_usb_error)

    def _on_gain_changed(self, scale):
        if self._updating_ui or not self.xlr.connected:
            return
        val = int(scale.get_value())
        self.gain_label.set_label(f"0x{val:04X}")
        # Debounce — only send after slider stops moving for 200ms
        if hasattr(self, '_gain_timeout') and self._gain_timeout:
            GLib.source_remove(self._gain_timeout)
        self._gain_timeout = GLib.timeout_add(200, self._send_gain, val)

    def _send_gain(self, val):
        self._gain_timeout = None
        self._usb_async(lambda: self.xlr.set_gain_raw(val), on_error=self._on_usb_error)
        return False

    def _on_hp_changed(self, scale):
        if self._updating_ui or not self.xlr.connected:
            return
        db = scale.get_value()
        self.hp_label.set_label(f"{db:.1f} dB")
        if hasattr(self, '_hp_timeout') and self._hp_timeout:
            GLib.source_remove(self._hp_timeout)
        self._hp_timeout = GLib.timeout_add(200, self._send_hp, db)

    def _send_hp(self, db):
        self._hp_timeout = None
        self._usb_async(lambda: self.xlr.set_hp_volume_db(db), on_error=self._on_usb_error)
        return False

    def _on_lowz_changed(self, row, _pspec):
        if self._updating_ui or not self.xlr.connected:
            return
        enabled = row.get_active()
        self._usb_async(lambda: self.xlr.set_low_impedance(enabled), on_error=self._on_usb_error)


class WaveXLRApp(Adw.Application):
    def __init__(self, start_hidden=False):
        super().__init__(application_id="com.github.openwave")
        self._window = None
        self._start_hidden = start_hidden
        self._tray = None

    def do_activate(self):
        if not self._window:
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

    def _toggle_mute(self):
        if self._window and self._window.xlr.connected:
            current = self._window._last_state and self._window._last_state.get("mute", False)
            self._window._usb_async(
                lambda: self._window.xlr.set_mute(not current),
                on_error=self._window._on_usb_error,
            )
        # Hold the app alive when window is hidden
        self.hold()

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
                body=f"{message}.\n\nPlease replug your Wave XLR, then click Continue.",
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

    def do_shutdown(self):
        if self._window:
            self._window._stop_polling()
            self._window.xlr.disconnect()
        Adw.Application.do_shutdown(self)


def main():
    hidden = "--hide" in sys.argv
    argv = [a for a in sys.argv if a != "--hide"]
    app = WaveXLRApp(start_hidden=hidden)
    app.set_flags(Gio.ApplicationFlags.DEFAULT_FLAGS)
    app.run(argv)
