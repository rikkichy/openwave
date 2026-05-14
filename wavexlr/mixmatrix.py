"""Mix matrix widget — two-axis sources × mixes grid.

Pure GTK4/libadwaita. No external deps. Phase 1 is structural: the mic source
is wired to real device state via the parent app; other sources and per-cell
mix routing are placeholders until PipeWire mix-sink backend lands (v0.3.0).
"""

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GObject  # noqa: E402


class MixMatrix(Gtk.Box):
    """Scrollable grid of source rows × mix columns."""

    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.add_css_class("openwave-matrix")

        scroll = Gtk.ScrolledWindow(vexpand=True, hexpand=True)
        scroll.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
        self.append(scroll)

        self._grid = Gtk.Grid(
            row_spacing=6,
            column_spacing=6,
            margin_start=12,
            margin_end=12,
            margin_top=12,
            margin_bottom=12,
        )
        scroll.set_child(self._grid)

        self._mix_ids = []
        self._source_ids = []
        self._sources = {}
        self._cells = {}

        corner = Gtk.Box()
        corner.set_size_request(260, 64)
        self._grid.attach(corner, 0, 0, 1, 1)

    def add_mix(self, mix_id, *, title, subtitle, icon_name):
        col = len(self._mix_ids) + 1
        header = MixHeaderCell(title=title, subtitle=subtitle, icon_name=icon_name)
        self._grid.attach(header, col, 0, 1, 1)
        self._mix_ids.append(mix_id)

    def add_source(self, source_id, *, name, icon_name, has_level=False):
        row = len(self._source_ids) + 1
        source = SourceCell(name=name, icon_name=icon_name, has_level=has_level)
        self._grid.attach(source, 0, row, 1, 1)
        self._sources[source_id] = source
        self._source_ids.append(source_id)

        for col_idx, mix_id in enumerate(self._mix_ids):
            cell = MixCell()
            cell.set_sensitive(False)
            cell.set_tooltip_text(
                "Per-mix routing arrives in v0.3.0 with the PipeWire backend"
            )
            self._grid.attach(cell, col_idx + 1, row, 1, 1)
            self._cells[(source_id, mix_id)] = cell

        return source

    def source(self, source_id):
        return self._sources.get(source_id)


class MixHeaderCell(Gtk.Box):
    """Column header at the top of each mix."""

    def __init__(self, *, title, subtitle, icon_name):
        super().__init__(
            orientation=Gtk.Orientation.HORIZONTAL,
            spacing=10,
        )
        self.add_css_class("openwave-mix-header")
        self.add_css_class("card")
        self.set_size_request(220, 64)

        inner = Gtk.Box(
            orientation=Gtk.Orientation.HORIZONTAL,
            spacing=10,
            margin_start=14,
            margin_end=14,
            margin_top=10,
            margin_bottom=10,
            hexpand=True,
        )
        self.append(inner)

        icon = Gtk.Image.new_from_icon_name(icon_name)
        icon.set_pixel_size(22)
        inner.append(icon)

        text = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL, spacing=2, hexpand=True, valign=Gtk.Align.CENTER
        )
        inner.append(text)

        title_lbl = Gtk.Label(label=title, xalign=0)
        title_lbl.add_css_class("heading")
        text.append(title_lbl)

        subtitle_lbl = Gtk.Label(label=subtitle, xalign=0)
        subtitle_lbl.add_css_class("dim-label")
        subtitle_lbl.add_css_class("caption")
        text.append(subtitle_lbl)


class SourceCell(Gtk.Box):
    """Leftmost cell of a source row: icon, name, master mute + volume."""

    __gsignals__ = {
        "volume-changed": (GObject.SignalFlags.RUN_FIRST, None, (float,)),
        "mute-toggled": (GObject.SignalFlags.RUN_FIRST, None, (bool,)),
    }

    def __init__(self, *, name, icon_name, has_level):
        super().__init__(
            orientation=Gtk.Orientation.HORIZONTAL,
            spacing=10,
        )
        self.add_css_class("openwave-source-cell")
        self.add_css_class("card")
        self.set_size_request(260, 64)

        inner = Gtk.Box(
            orientation=Gtk.Orientation.HORIZONTAL,
            spacing=8,
            margin_start=12,
            margin_end=12,
            margin_top=10,
            margin_bottom=10,
            hexpand=True,
        )
        self.append(inner)

        icon = Gtk.Image.new_from_icon_name(icon_name)
        icon.set_pixel_size(26)
        inner.append(icon)

        self._name_lbl = Gtk.Label(label=name, xalign=0, hexpand=True, ellipsize=3)
        self._name_lbl.add_css_class("heading")
        inner.append(self._name_lbl)

        self._mute_btn = Gtk.ToggleButton(valign=Gtk.Align.CENTER)
        self._mute_btn.add_css_class("flat")
        self._mute_btn.add_css_class("circular")
        self._mute_icon = Gtk.Image.new_from_icon_name("audio-volume-high-symbolic")
        self._mute_btn.set_child(self._mute_icon)
        self._mute_handler = self._mute_btn.connect("toggled", self._on_mute_toggled)
        inner.append(self._mute_btn)

        self._scale = Gtk.Scale(
            orientation=Gtk.Orientation.HORIZONTAL,
            draw_value=False,
            adjustment=Gtk.Adjustment(
                lower=0.0, upper=1.0, step_increment=0.01, page_increment=0.05
            ),
            valign=Gtk.Align.CENTER,
        )
        self._scale.add_css_class("openwave-mix-slider")
        self._scale.set_size_request(110, -1)
        self._scale_handler = self._scale.connect("value-changed", self._on_value_changed)
        inner.append(self._scale)

        self._level = None
        if has_level:
            self._level = Gtk.Image.new_from_icon_name("audio-input-microphone-symbolic")
            self._level.add_css_class("success")
            self._level.set_valign(Gtk.Align.CENTER)
            inner.append(self._level)

    def set_volume(self, value):
        """Update the master slider without firing the changed signal."""
        with GObject.signal_handler_block(self._scale, self._scale_handler):
            self._scale.set_value(max(0.0, min(1.0, value)))

    def set_muted(self, muted):
        """Update the mute toggle without firing its signal."""
        with GObject.signal_handler_block(self._mute_btn, self._mute_handler):
            self._mute_btn.set_active(muted)
        self._reflect_mute_icon(muted)

    def _reflect_mute_icon(self, muted):
        self._mute_icon.set_from_icon_name(
            "audio-volume-muted-symbolic" if muted else "audio-volume-high-symbolic"
        )
        if self._level is not None:
            if muted:
                self._level.add_css_class("dim-label")
                self._level.remove_css_class("success")
            else:
                self._level.remove_css_class("dim-label")
                self._level.add_css_class("success")

    def _on_value_changed(self, scale):
        self.emit("volume-changed", scale.get_value())

    def _on_mute_toggled(self, btn):
        muted = btn.get_active()
        self._reflect_mute_icon(muted)
        self.emit("mute-toggled", muted)


class MixCell(Gtk.Box):
    """Grid intersection: small mute toggle + horizontal volume slider."""

    def __init__(self):
        super().__init__(
            orientation=Gtk.Orientation.HORIZONTAL,
            spacing=8,
        )
        self.add_css_class("openwave-mix-cell")
        self.add_css_class("card")
        self.set_size_request(220, 64)

        inner = Gtk.Box(
            orientation=Gtk.Orientation.HORIZONTAL,
            spacing=8,
            margin_start=12,
            margin_end=12,
            margin_top=10,
            margin_bottom=10,
            hexpand=True,
        )
        self.append(inner)

        self._mute_btn = Gtk.ToggleButton(valign=Gtk.Align.CENTER)
        self._mute_btn.add_css_class("flat")
        self._mute_btn.add_css_class("circular")
        self._mute_btn.set_child(Gtk.Image.new_from_icon_name("audio-volume-high-symbolic"))
        inner.append(self._mute_btn)

        self._scale = Gtk.Scale(
            orientation=Gtk.Orientation.HORIZONTAL,
            draw_value=False,
            adjustment=Gtk.Adjustment(
                lower=0.0, upper=1.0, step_increment=0.01, page_increment=0.05
            ),
            valign=Gtk.Align.CENTER,
            hexpand=True,
        )
        self._scale.add_css_class("openwave-mix-slider")
        self._scale.set_value(0.5)
        inner.append(self._scale)
