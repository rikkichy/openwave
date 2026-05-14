"""'Add Source' picker — two pages: app picker, then name + icon config."""

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GObject  # noqa: E402

from .mixer import list_audio_streams

ICON_CHOICES = (
    ("applications-multimedia-symbolic", "Generic"),
    ("applications-games-symbolic", "Games"),
    ("input-gaming-symbolic", "Controller"),
    ("audio-x-generic-symbolic", "Music"),
    ("multimedia-player-symbolic", "Player"),
    ("user-available-symbolic", "Voice"),
    ("system-users-symbolic", "Chat"),
    ("web-browser-symbolic", "Browser"),
    ("video-display-symbolic", "Video"),
    ("preferences-desktop-multimedia-symbolic", "Media"),
    ("audio-headphones-symbolic", "Headphones"),
    ("microphone-sensitivity-high-symbolic", "Mic"),
)


class AddSourceDialog(Adw.Dialog):
    __gsignals__ = {
        # (display_name, match_app_name, icon_name)
        "source-confirmed": (GObject.SignalFlags.RUN_FIRST, None, (str, str, str)),
    }

    def __init__(self):
        super().__init__()
        self.set_title("Add Source")
        self.set_content_width(480)
        self.set_content_height(560)

        self._nav = Adw.NavigationView()
        self.set_child(self._nav)

        self._selected_app = None
        self._selected_icon = ICON_CHOICES[0][0]

        self._nav.push(self._build_picker_page())

    # ------------------------------------------------------------ page 1
    def _build_picker_page(self):
        page = Adw.NavigationPage(title="Pick Application")

        view = Adw.ToolbarView()
        page.set_child(view)

        header = Adw.HeaderBar()
        view.add_top_bar(header)

        cancel_btn = Gtk.Button(label="Cancel")
        cancel_btn.connect("clicked", lambda _: self.close())
        header.pack_start(cancel_btn)

        self._next_btn = Gtk.Button(label="Next")
        self._next_btn.add_css_class("suggested-action")
        self._next_btn.set_sensitive(False)
        self._next_btn.connect("clicked", self._on_next)
        header.pack_end(self._next_btn)

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

        self._populate_apps()
        return page

    def _populate_apps(self):
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
            row.add_prefix(Gtk.Image.new_from_icon_name("applications-multimedia-symbolic"))
            row._app_name = app_name  # noqa: SLF001
            self._listbox.append(row)

    def _on_row_selected(self, _box, row):
        if row is None:
            self._selected_app = None
            self._next_btn.set_sensitive(False)
            return
        self._selected_app = getattr(row, "_app_name", None)
        self._next_btn.set_sensitive(self._selected_app is not None)

    def _on_next(self, _btn):
        if not self._selected_app:
            return
        self._nav.push(self._build_config_page())

    # ------------------------------------------------------------ page 2
    def _build_config_page(self):
        page = Adw.NavigationPage(title="Name and Icon")

        view = Adw.ToolbarView()
        page.set_child(view)

        header = Adw.HeaderBar()
        view.add_top_bar(header)

        add_btn = Gtk.Button(label="Add Source")
        add_btn.add_css_class("suggested-action")
        add_btn.connect("clicked", self._on_confirm)
        header.pack_end(add_btn)

        scroll = Gtk.ScrolledWindow(vexpand=True)
        view.set_content(scroll)

        clamp = Adw.Clamp(
            maximum_size=440,
            margin_start=12, margin_end=12, margin_top=12, margin_bottom=12,
        )
        scroll.set_child(clamp)

        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        clamp.set_child(outer)

        # Name group
        name_group = Adw.PreferencesGroup(title="Name")
        outer.append(name_group)

        self._name_row = Adw.EntryRow(title="Source name")
        self._name_row.set_text(self._selected_app or "")
        name_group.add(self._name_row)

        # Icon picker
        icon_group = Adw.PreferencesGroup(title="Icon")
        outer.append(icon_group)

        flow = Gtk.FlowBox(
            selection_mode=Gtk.SelectionMode.SINGLE,
            max_children_per_line=6,
            min_children_per_line=4,
            column_spacing=6,
            row_spacing=6,
            margin_start=4, margin_end=4, margin_top=8, margin_bottom=8,
            homogeneous=True,
        )
        flow.add_css_class("openwave-icon-picker")
        first_child = None
        for icon_name, tooltip in ICON_CHOICES:
            btn = Gtk.Image.new_from_icon_name(icon_name)
            btn.set_pixel_size(28)
            child = Gtk.FlowBoxChild()
            child.set_child(btn)
            child.set_tooltip_text(tooltip)
            child._icon_name = icon_name  # noqa: SLF001
            flow.append(child)
            if first_child is None:
                first_child = child
        flow.connect("selected-children-changed", self._on_icon_selected)
        icon_group.add(flow)

        if first_child is not None:
            flow.select_child(first_child)
            self._selected_icon = first_child._icon_name  # noqa: SLF001

        return page

    def _on_icon_selected(self, flow):
        sel = flow.get_selected_children()
        if sel:
            self._selected_icon = getattr(sel[0], "_icon_name", self._selected_icon)

    def _on_confirm(self, _btn):
        if not self._selected_app:
            return
        name = self._name_row.get_text().strip() or self._selected_app
        self.emit("source-confirmed", name, self._selected_app, self._selected_icon)
        self.close()
