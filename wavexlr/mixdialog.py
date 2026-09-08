"""Create / rename a mix — a single-page name + icon dialog.

Modelled on sourcedialog.AddSourceDialog, but one page instead of two: there
is nothing to pick first, so Cancel has to live on this page's own header bar
rather than on a preceding picker page.
"""

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GObject, Pango  # noqa: E402

from . import icons
from .mixes import DEFAULT_ICON

ICON_CHOICES = (
    ("audio-headphones-symbolic", "Headphones"),
    ("audio-speakers-symbolic", "Speakers"),
    ("system-users-symbolic", "Chat"),
    ("media-record-symbolic", "Record"),
    ("camera-video-symbolic", "Stream"),
    ("applications-games-symbolic", "Games"),
    ("audio-x-generic-symbolic", "Music"),
    ("microphone-sensitivity-high-symbolic", "Mic"),
    ("audio-card-symbolic", "Audio"),
    ("applications-multimedia-symbolic", "Media"),
    ("network-transmit-symbolic", "Send"),
    ("multimedia-player-symbolic", "Player"),
)


class MixDialog(Adw.Dialog):
    """Name + icon for a new or existing mix.

    Deliberately does not offer the sink or the PipeWire description: those are
    what other applications bind to, and mixes.update() refuses to change the
    sink at all. Renaming here is a display-only change.
    """

    __gsignals__ = {
        # (display_name, icon_name)
        "mix-confirmed": (GObject.SignalFlags.RUN_FIRST, None, (str, str)),
    }

    def __init__(self, *, heading="Add Mix", confirm_label="Add Mix",
                 name="", icon_name=DEFAULT_ICON):
        super().__init__()
        self.set_title(heading)
        self.set_content_width(460)
        self.set_content_height(430)

        self._selected_icon = icon_name or DEFAULT_ICON

        self._nav = Adw.NavigationView()
        self.set_child(self._nav)
        self._nav.push(self._build_page(heading, confirm_label, name))

    def _build_page(self, heading, confirm_label, name):
        page = Adw.NavigationPage(title=heading)

        view = Adw.ToolbarView()
        page.set_child(view)

        header = Adw.HeaderBar()
        view.add_top_bar(header)

        # Single-page dialog: unlike sourcedialog's config page, there is no
        # picker page behind this one to carry Cancel.
        cancel_btn = Gtk.Button(label="Cancel")
        cancel_btn.connect("clicked", lambda _: self.close())
        header.pack_start(cancel_btn)

        self._confirm_btn = Gtk.Button(label=confirm_label)
        self._confirm_btn.add_css_class("suggested-action")
        self._confirm_btn.connect("clicked", self._on_confirm)
        header.pack_end(self._confirm_btn)

        scroll = Gtk.ScrolledWindow(vexpand=True)
        view.set_content(scroll)

        clamp = Adw.Clamp(
            maximum_size=420,
            margin_start=12, margin_end=12, margin_top=12, margin_bottom=12,
        )
        scroll.set_child(clamp)

        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        clamp.set_child(outer)

        name_group = Adw.PreferencesGroup(title="Name")
        outer.append(name_group)

        self._name_row = Adw.EntryRow(title="Mix name")
        self._name_row.set_text(name)
        self._name_row.connect("changed", self._on_name_changed)
        self._name_row.connect("entry-activated", self._on_confirm)
        name_group.add(self._name_row)

        hint = Gtk.Label(
            label="The name is OpenWave's own label. The audio device other "
                  "applications record from keeps the name it was created "
                  "with, so renaming never breaks an OBS or Discord setup.",
            wrap=True, xalign=0,
        )
        hint.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
        hint.add_css_class("dim-label")
        hint.add_css_class("caption")
        outer.append(hint)

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

        # A stored icon outside the offered set (hand-edited mixdefs.json, or a
        # future default) is appended rather than silently swapped on save.
        choices = list(ICON_CHOICES)
        if self._selected_icon not in [icon for icon, _ in choices]:
            choices.append((self._selected_icon, "Current"))

        preselect = None
        for icon, tooltip in choices:
            img = Gtk.Image.new_from_icon_name(icons.resolve(icon))
            img.set_pixel_size(28)
            child = Gtk.FlowBoxChild()
            child.set_child(img)
            child.set_tooltip_text(tooltip)
            child._icon_name = icon  # noqa: SLF001
            flow.append(child)
            if icon == self._selected_icon:
                preselect = child
        flow.connect("selected-children-changed", self._on_icon_selected)
        icon_group.add(flow)

        if preselect is not None:
            flow.select_child(preselect)

        self._sync_confirm()
        return page

    def _on_name_changed(self, _row):
        self._sync_confirm()

    def _sync_confirm(self):
        self._confirm_btn.set_sensitive(bool(self._name_row.get_text().strip()))

    def _on_icon_selected(self, flow):
        sel = flow.get_selected_children()
        if sel:
            self._selected_icon = getattr(sel[0], "_icon_name", self._selected_icon)

    def _on_confirm(self, _widget):
        name = self._name_row.get_text().strip()
        if not name:
            return
        self.emit("mix-confirmed", name, self._selected_icon)
        self.close()
