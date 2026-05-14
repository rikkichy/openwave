"""'Add Source' picker — lists currently-playing PipeWire streams by app name."""

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GObject  # noqa: E402

from .mixer import list_audio_streams


class AddSourceDialog(Adw.Dialog):
    __gsignals__ = {
        # (display_name, match_app_name)
        "source-confirmed": (GObject.SignalFlags.RUN_FIRST, None, (str, str)),
    }

    def __init__(self):
        super().__init__()
        self.set_title("Add Source")
        self.set_content_width(480)
        self.set_content_height(540)

        view = Adw.ToolbarView()
        self.set_child(view)

        header = Adw.HeaderBar()
        view.add_top_bar(header)

        cancel_btn = Gtk.Button(label="Cancel")
        cancel_btn.connect("clicked", lambda _: self.close())
        header.pack_start(cancel_btn)

        self._add_btn = Gtk.Button(label="Add")
        self._add_btn.add_css_class("suggested-action")
        self._add_btn.set_sensitive(False)
        self._add_btn.connect("clicked", self._on_add)
        header.pack_end(self._add_btn)

        scroll = Gtk.ScrolledWindow(vexpand=True)
        view.set_content(scroll)

        clamp = Adw.Clamp(
            maximum_size=440,
            margin_start=12, margin_end=12, margin_top=12, margin_bottom=12,
        )
        scroll.set_child(clamp)

        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        clamp.set_child(outer)

        hint = Gtk.Label(
            label="Pick an application that's currently playing audio. "
                  "OpenWave will route any future streams from this app through the new source row.",
            wrap=True, xalign=0,
        )
        hint.add_css_class("dim-label")
        outer.append(hint)

        self._listbox = Gtk.ListBox()
        self._listbox.set_selection_mode(Gtk.SelectionMode.SINGLE)
        self._listbox.add_css_class("boxed-list")
        self._listbox.connect("row-selected", self._on_row_selected)
        outer.append(self._listbox)

        self._selected_app = None
        self._populate()

    def _populate(self):
        streams = list_audio_streams()
        apps = {}
        for s in streams:
            apps.setdefault(s["app_name"], []).append(s)

        if not apps:
            empty = Adw.ActionRow(title="No audio streams playing")
            empty.set_subtitle("Start playback in an app, then click + Add Source again")
            empty.set_sensitive(False)
            self._listbox.append(empty)
            return

        for app_name in sorted(apps.keys()):
            row = Adw.ActionRow(title=app_name)
            sample = apps[app_name][0].get("media_name") or apps[app_name][0].get("node_name", "")
            if sample:
                row.set_subtitle(sample)
            icon = Gtk.Image.new_from_icon_name("applications-multimedia-symbolic")
            row.add_prefix(icon)
            row._app_name = app_name  # noqa: SLF001 - simple attr piggyback
            self._listbox.append(row)

    def _on_row_selected(self, _box, row):
        if row is None:
            self._selected_app = None
            self._add_btn.set_sensitive(False)
            return
        self._selected_app = getattr(row, "_app_name", None)
        self._add_btn.set_sensitive(self._selected_app is not None)

    def _on_add(self, _btn):
        if self._selected_app:
            self.emit("source-confirmed", self._selected_app, self._selected_app)
        self.close()
