"""Dynamic sources × mixes GTK4 matrix.

MixMatrix emits source/mix CRUD, reorder/group and output/master requests.
SourceCell emits volume-changed(float), mute-toggled(bool), edit-clicked(),
remove-clicked(), move-clicked(int), switch-clicked(), fx-changed() and
fx-autotune(). Read fx_settings() after fx-changed; fx-autotune requests the
parent's calibration workflow. MixCell emits volume-changed(float) and
mute-toggled(bool). MixHeaderCell emits output-changed(str),
volume-changed(float), rename-clicked() and remove-clicked().

Programmatic setters do not emit requests. The parent owns routing/persistence,
connects source/cell signals, and restores state after reorder_sources() rebuilds
the rows. All signal callbacks receive the emitting widget first.
"""


import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GObject, Gdk, GLib, Pango  # noqa: E402

from . import icons
from .effects import FX_RANGES


def _emit_later(obj, signal, *args):
    """Emit `signal` once the current GTK frame has unwound.

    For signals whose handlers dismantle the very thing that is emitting —
    a popover being popped down, a row about to be destroyed by the reorder
    its own drop handler asks for. GTK is still inside the controller or the
    popup teardown at that moment, and pulling the widget out from under it
    is a use-after-free on a good day and an xdg_popup protocol error (which
    kills the client outright) on Wayland.
    """
    def _fire():
        obj.emit(signal, *args)
        return GLib.SOURCE_REMOVE

    GLib.idle_add(_fire)


def _percent_label():
    """A fixed-width percentage readout for a 0..1 slider.

    Monospace and width-limited so the row does not reflow as the number
    changes width between 0% and 100%.
    """
    lbl = Gtk.Label(label="0%", xalign=1, width_chars=4)
    lbl.add_css_class("dim-label")
    lbl.add_css_class("caption")
    lbl.add_css_class("monospace")
    return lbl


class MixMatrix(Gtk.Box):
    """Scrollable grid of source rows × mix columns."""

    __gsignals__ = {
        "add-source-clicked": (GObject.SignalFlags.RUN_FIRST, None, ()),
        "remove-source-clicked": (GObject.SignalFlags.RUN_FIRST, None, (str,)),
        "edit-source-clicked": (GObject.SignalFlags.RUN_FIRST, None, (str,)),
        # (source_id, delta) -- -1 to move a row up, +1 to move it down
        "move-source-clicked": (GObject.SignalFlags.RUN_FIRST, None, (str, int)),
        # Make this source the live one in its group
        "switch-source-clicked": (GObject.SignalFlags.RUN_FIRST, None, (str,)),
        # (dragged_id, target_id) -- put the first in the second's group
        "group-sources-clicked": (GObject.SignalFlags.RUN_FIRST, None, (str, str)),
        "add-mix-clicked": (GObject.SignalFlags.RUN_FIRST, None, ()),
        "rename-mix-clicked": (GObject.SignalFlags.RUN_FIRST, None, (str,)),
        "remove-mix-clicked": (GObject.SignalFlags.RUN_FIRST, None, (str,)),
        # (mix_id, output name — a sink node.name, OUTPUT_AUTO or OUTPUT_NONE)
        "mix-output-changed": (GObject.SignalFlags.RUN_FIRST, None, (str, str)),
        "mix-volume-changed": (GObject.SignalFlags.RUN_FIRST, None, (str, float)),
    }

    # Shown instead of deleting the only mix. The matrix's whole geometry is
    # sources × mixes; with no column left there is nothing to route into.
    LAST_MIX_REASON = "OpenWave needs at least one mix."

    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.VERTICAL)
        self.add_css_class("openwave-matrix")

        scroll = Gtk.ScrolledWindow(vexpand=True, hexpand=True)
        scroll.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
        self.append(scroll)

        wrapper = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10)
        scroll.set_child(wrapper)

        self._grid = Gtk.Grid(
            row_spacing=6,
            column_spacing=6,
            margin_start=12,
            margin_end=12,
            margin_top=12,
            # Matches the other three sides: at zero the last source row sat
            # flush against the window edge and read as clipped.
            margin_bottom=12,
        )
        wrapper.append(self._grid)

        self._mix_ids = []
        self._source_ids = []
        # How each row was built, so reorder_sources can rebuild it verbatim.
        self._source_specs = {}
        self._sources = {}
        self._headers = {}
        self._cells = {}

        corner = Gtk.Box()
        # Reserve room for the row's identity and controls.
        corner.set_size_request(400, 64)
        self._grid.attach(corner, 0, 0, 1, 1)

        # "+ Add Source" / "+ Add Mix" sit above the grid, at the top left,
        # rather than trailing below it: below, they moved down the window as
        # rows were added and ended up off-screen on a full matrix. Neither is
        # a grid column, so adding or removing one never renumbers them.
        add_row = Gtk.Box(
            orientation=Gtk.Orientation.HORIZONTAL,
            spacing=6,
            halign=Gtk.Align.START,
            margin_start=12, margin_end=12, margin_top=12, margin_bottom=2,
        )
        wrapper.prepend(add_row)
        self._add_btn = Gtk.Button(
            label="+ Add Source",
            halign=Gtk.Align.START,
        )
        self._add_btn.add_css_class("openwave-add-source")
        self._add_btn.connect("clicked", lambda _: self.emit("add-source-clicked"))
        add_row.append(self._add_btn)

        self._add_mix_btn = Gtk.Button(
            label="+ Add Mix",
            halign=Gtk.Align.START,
        )
        self._add_mix_btn.add_css_class("openwave-add-mix")
        self._add_mix_btn.connect("clicked", lambda _: self.emit("add-mix-clicked"))
        add_row.append(self._add_mix_btn)

    def add_mix(self, mix_id, *, title, subtitle, icon_name):
        if mix_id in self._mix_ids:
            return self._headers[mix_id]
        col = len(self._mix_ids) + 1
        header = MixHeaderCell(title=title, subtitle=subtitle, icon_name=icon_name)
        header.connect(
            "output-changed",
            lambda _h, name, mid=mix_id: self.emit("mix-output-changed", mid, name),
        )
        header.connect(
            "volume-changed",
            lambda _h, value, mid=mix_id: self.emit("mix-volume-changed", mid, value),
        )
        header.connect(
            "rename-clicked", lambda _h, mid=mix_id: self.emit("rename-mix-clicked", mid),
        )
        header.connect(
            "remove-clicked", lambda _h, mid=mix_id: self.emit("remove-mix-clicked", mid),
        )
        self._grid.attach(header, col, 0, 1, 1)
        self._mix_ids.append(mix_id)
        self._headers[mix_id] = header

        # A mix added after the rows exist still needs a cell in every row.
        for row_idx, source_id in enumerate(self._source_ids):
            cell = MixCell()
            self._grid.attach(cell, col, row_idx + 1, 1, 1)
            self._cells[(source_id, mix_id)] = cell

        self._sync_delete_sensitivity()
        return header

    def remove_mix(self, mix_id):
        if mix_id not in self._mix_ids:
            return
        idx = self._mix_ids.index(mix_id)
        # Column mirror of remove_source's remove_row: Gtk.Grid shifts every
        # column to the right of this one left by one, so the list index of the
        # remaining mixes stays exactly their grid column minus one.
        self._grid.remove_column(idx + 1)
        self._mix_ids.pop(idx)
        self._headers.pop(mix_id, None)
        for source_id in self._source_ids:
            self._cells.pop((source_id, mix_id), None)
        self._sync_delete_sensitivity()

    def _sync_delete_sensitivity(self):
        """Grey out Delete on every header while only one mix is left."""
        enabled = len(self._mix_ids) > 1
        for header in self._headers.values():
            header.set_delete_enabled(enabled, self.LAST_MIX_REASON)

    def set_mix_volume(self, mix_id, value):
        header = self._headers.get(mix_id)
        if header is not None:
            header.set_volume(value)

    def set_mix_level(self, mix_id, value):
        header = self._headers.get(mix_id)
        if header is not None:
            header.set_level(value)

    def set_mix_empty(self, mix_id, empty):
        header = self._headers.get(mix_id)
        if header is not None:
            header.set_empty(empty)

    def set_mix(self, mix_id, *, title=None, subtitle=None, icon_name=None):
        """Live-update a header's identity after a rename."""
        header = self._headers.get(mix_id)
        if header is None:
            return
        if title is not None:
            header.set_title(title)
        if subtitle is not None:
            header.set_subtitle(subtitle)
        if icon_name is not None:
            header.set_icon(icon_name)

    def set_mix_outputs(self, mix_id, entries, current, summary, monitored=True):
        """Refresh one header's output chooser and the routing it displays.

        `entries` is [(output name, label), ...] in menu order; `current` is
        the persisted choice; `summary` is the short text shown on the header.
        """
        header = self._headers.get(mix_id)
        if header is not None:
            header.set_outputs(entries, current, summary, monitored)

    def add_source(self, source_id, *, name, icon_name, has_level=False,
                   removable=False, editable=False, reorderable=False,
                   is_capture=False):
        row = len(self._source_ids) + 1
        source = SourceCell(
            name=name, icon_name=icon_name, has_level=has_level,
            removable=removable, editable=editable, reorderable=reorderable,
            is_capture=is_capture,
        )
        if editable:
            source.connect(
                "edit-clicked",
                lambda _s, sid=source_id: self.emit("edit-source-clicked", sid),
            )
        if removable:
            source.connect(
                "remove-clicked",
                lambda _s, sid=source_id: self.emit("remove-source-clicked", sid),
            )
        if reorderable:
            self._make_row_draggable(source, source_id)
        source.connect(
            "switch-clicked",
            lambda _s, sid=source_id: self.emit("switch-source-clicked", sid),
        )
        source.connect(
            "move-clicked",
            # Deferred for the same reason as the drop path: the reorder this
            # asks for destroys the row holding the button that was clicked,
            # while GTK is still inside that button's own emission.
            lambda _s, delta, sid=source_id: _emit_later(
                self, "move-source-clicked", sid, delta),
        )
        self._grid.attach(source, 0, row, 1, 1)
        self._sources[source_id] = source
        self._source_ids.append(source_id)
        # Remembered so reorder_sources can rebuild a row exactly as it was.
        self._source_specs[source_id] = dict(
            name=name, icon_name=icon_name, has_level=has_level,
            removable=removable, editable=editable, reorderable=reorderable,
            is_capture=is_capture,
        )

        for col_idx, mix_id in enumerate(self._mix_ids):
            cell = MixCell()
            self._grid.attach(cell, col_idx + 1, row, 1, 1)
            self._cells[(source_id, mix_id)] = cell

        return source

    def _make_row_draggable(self, cell, source_id):
        """Let a row be dragged onto another to take its place.

        The drop is expressed as a delta and pushed through the same
        move-source-clicked path the buttons used, so ordering, clamping and
        persistence stay in one place.
        """
        drag = Gtk.DragSource(actions=Gdk.DragAction.MOVE)
        drag.connect(
            "prepare",
            lambda _d, _x, _y, sid=source_id: Gdk.ContentProvider.new_for_value(sid),
        )

        def _begin(_source, drag_obj, widget=cell):
            # Drag the row's own likeness, so it is obvious what is moving.
            icon = Gtk.DragIcon.get_for_drag(drag_obj)
            paintable = Gtk.WidgetPaintable.new(widget)
            picture = Gtk.Picture.new_for_paintable(paintable)
            # A row remapped but not yet allocated (a workspace switch, a
            # window just unhidden) measures 0x0, and a 0x0 drag icon is an
            # invisible drag. Same fallback the drop-zone maths uses.
            picture.set_size_request(widget.get_width() or 320,
                                     widget.get_height() or 64)
            icon.set_child(picture)
            widget.set_opacity(0.35)

        drag.connect("drag-begin", _begin)
        drag.connect("drag-end", lambda _s, _d, _r, w=cell: w.set_opacity(1.0))
        drag.connect("drag-cancel",
                     lambda _s, _d, _r, w=cell: (w.set_opacity(1.0), False)[1])
        cell.add_controller(drag)

        drop = Gtk.DropTarget.new(GObject.TYPE_STRING, Gdk.DragAction.MOVE)
        drop.connect("drop", self._on_row_drop, source_id)
        drop.connect("motion", self._on_row_motion, source_id)
        drop.connect("leave", lambda _t, w=cell: self._clear_drop_hint(w))
        cell.add_controller(drop)

    # Fraction of a row's height at each end that means "move here" rather
    # than "group with this". The middle is the larger target because grouping
    # is the deliberate act; reordering is the one you can repeat cheaply.
    _EDGE_ZONE = 0.28

    def _drop_is_grouping(self, cell, y):
        height = cell.get_height() or 64
        return self._EDGE_ZONE * height <= y <= (1 - self._EDGE_ZONE) * height

    def _on_row_motion(self, target, _x, y, target_id):
        """Show which of the two outcomes a release would produce."""
        cell = self._sources.get(target_id)
        if cell is None:
            return Gdk.DragAction.MOVE
        grouping = self._drop_is_grouping(cell, y)
        cell.remove_css_class("openwave-drop-target")
        cell.remove_css_class("openwave-drop-group")
        cell.add_css_class(
            "openwave-drop-group" if grouping else "openwave-drop-target")
        return Gdk.DragAction.MOVE

    def _clear_drop_hint(self, cell):
        if cell is not None:
            cell.remove_css_class("openwave-drop-target")
            cell.remove_css_class("openwave-drop-group")

    def _on_row_drop(self, _target, value, _x, y, target_id):
        dragged = str(value)
        cell = self._sources.get(target_id)
        self._clear_drop_hint(cell)
        if dragged == target_id or dragged not in self._source_ids:
            return False
        # Both outcomes end in reorder_sources, which destroys every row —
        # including this one, whose GtkDropTarget GTK is still emitting from
        # and whose likeness the live GtkDragIcon is still painting. Deferred
        # so the drop finishes against widgets that still exist.
        if cell is not None and self._drop_is_grouping(cell, y):
            _emit_later(self, "group-sources-clicked", dragged, target_id)
            return True
        delta = self._source_ids.index(target_id) - self._source_ids.index(dragged)
        _emit_later(self, "move-source-clicked", dragged, delta)
        return True

    def set_source_group(self, source_id, group):
        cell = self._sources.get(source_id)
        if cell is not None and hasattr(cell, "set_group"):
            cell.set_group(group)

    def set_source(self, source_id, *, name=None, icon_name=None):
        """Update a row's label, and the spec a rebuild restores it from.

        Setting it on the widget alone is not enough: reorder_sources tears
        every row down and rebuilds it from _source_specs, so a name applied
        only to the cell is silently reverted by the next drag.
        """
        cell = self._sources.get(source_id)
        spec = self._source_specs.get(source_id)
        if name is not None:
            if cell is not None:
                cell.set_name(name)
            if spec is not None:
                spec["name"] = name
        if icon_name is not None:
            if cell is not None:
                cell.set_icon(icon_name)
            if spec is not None:
                spec["icon_name"] = icon_name

    def reorder_sources(self, order):
        """Redraw the source rows in `order`.

        Gtk.Grid has no row-move, so the rows are torn down and rebuilt. Every
        MixCell is recreated, so the caller must re-wire the cells afterwards --
        their widgets are new objects and carry no state.
        """
        # Rows omitted by the caller remain pinned ahead of the supplied order.
        pinned = [sid for sid in self._source_ids if sid not in order]
        specs = [(sid, self._source_specs[sid])
                 for sid in pinned + [s for s in order if s not in pinned]
                 if sid in self._source_specs]
        for _ in range(len(self._source_ids)):
            self._grid.remove_row(1)     # row 0 is the header; rows shift up
        self._source_ids = []
        self._sources = {}
        self._cells = {}
        self._source_specs = {}
        for sid, spec in specs:
            self.add_source(sid, **spec)

    def remove_source(self, source_id):
        if source_id not in self._source_ids:
            return
        idx = self._source_ids.index(source_id)
        self._grid.remove_row(idx + 1)
        self._source_ids.pop(idx)
        self._sources.pop(source_id, None)
        self._source_specs.pop(source_id, None)
        for mix_id in self._mix_ids:
            self._cells.pop((source_id, mix_id), None)

    def source(self, source_id):
        return self._sources.get(source_id)

    def cell(self, source_id, mix_id):
        return self._cells.get((source_id, mix_id))


class MixHeaderCell(Gtk.Box):
    """Column header at the top of each mix: identity, routing, and its menu.

    The menu is a Gtk.Popover of ordinary widgets rather than a Gio.Menu: the
    output list changes with the hardware and differs per mix, and a Gio.Menu
    would mean installing and tearing down a set of Gio actions per column.
    """

    __gsignals__ = {
        "output-changed": (GObject.SignalFlags.RUN_FIRST, None, (str,)),
        "volume-changed": (GObject.SignalFlags.RUN_FIRST, None, (float,)),
        "rename-clicked": (GObject.SignalFlags.RUN_FIRST, None, ()),
        "remove-clicked": (GObject.SignalFlags.RUN_FIRST, None, ()),
    }

    def __init__(self, *, title, subtitle, icon_name):
        super().__init__(
            orientation=Gtk.Orientation.HORIZONTAL,
            spacing=10,
        )
        self.add_css_class("openwave-mix-header")
        self.add_css_class("card")
        # Taller than the 64px data cells because a third line — the live
        # output — is worth seeing without opening the menu. Only row 0 grows;
        # the corner box beside it simply stretches to match.
        self.set_size_request(220, 78)

        self._updating = False
        self._current_output = None

        inner = Gtk.Box(
            orientation=Gtk.Orientation.HORIZONTAL,
            spacing=10,
            margin_start=14,
            margin_end=6,
            margin_top=8,
            margin_bottom=8,
            hexpand=True,
        )
        self.append(inner)

        self._icon = Gtk.Image.new_from_icon_name(icons.resolve(icon_name))
        self._icon.set_pixel_size(22)
        inner.append(self._icon)

        text = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL, spacing=1, hexpand=True,
            valign=Gtk.Align.CENTER,
        )
        inner.append(text)

        # max_width_chars is what actually caps the label: an ellipsizing GTK
        # label still requests its full natural width without it, and
        # set_size_request(220, …) is a minimum, so a long user-typed name
        # would otherwise stretch the whole column. width_chars pins the
        # natural width to the same value so every column comes out identical
        # regardless of how long or short its name happens to be.
        self._title_lbl = Gtk.Label(label=title, xalign=0)
        self._title_lbl.set_ellipsize(Pango.EllipsizeMode.END)
        self._title_lbl.set_width_chars(14)
        self._title_lbl.set_max_width_chars(14)
        self._title_lbl.add_css_class("heading")
        self._title_lbl.set_tooltip_text(title)
        text.append(self._title_lbl)

        self._subtitle_lbl = Gtk.Label(label=subtitle, xalign=0)
        self._subtitle_lbl.set_ellipsize(Pango.EllipsizeMode.END)
        self._subtitle_lbl.set_width_chars(16)
        self._subtitle_lbl.set_max_width_chars(16)
        self._subtitle_lbl.add_css_class("dim-label")
        self._subtitle_lbl.add_css_class("caption")
        self._subtitle_lbl.set_visible(bool(subtitle))
        text.append(self._subtitle_lbl)

        # Hidden until the app has resolved the routing, so the header never
        # shows a placeholder that reads like a real device.
        self._out_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        self._out_box.set_visible(False)
        text.append(self._out_box)

        self._out_icon = Gtk.Image.new_from_icon_name("audio-speakers-symbolic")
        self._out_icon.set_pixel_size(12)
        self._out_icon.add_css_class("dim-label")
        self._out_box.append(self._out_icon)

        self._out_lbl = Gtk.Label(label="", xalign=0, hexpand=True)
        self._out_lbl.set_ellipsize(Pango.EllipsizeMode.END)
        self._out_lbl.set_width_chars(16)
        self._out_lbl.set_max_width_chars(16)
        self._out_lbl.add_css_class("dim-label")
        self._out_lbl.add_css_class("caption")
        self._out_box.append(self._out_lbl)

        # Master volume + live level. The master is a plain PipeWire sink
        # volume anything may move (pavucontrol, a media key, a scene), so
        # the slider is set from observation as much as it drives — writes
        # go out through volume-changed, external moves come back through
        # set_volume with the handler blocked.
        vol_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        text.append(vol_row)
        self._vol_scale = Gtk.Scale(
            orientation=Gtk.Orientation.HORIZONTAL,
            draw_value=False,
            adjustment=Gtk.Adjustment(
                lower=0.0, upper=1.0, step_increment=0.01, page_increment=0.05
            ),
            hexpand=True,
            valign=Gtk.Align.CENTER,
            round_digits=2,
        )
        self._vol_scale.add_css_class("openwave-mix-slider")
        self._vol_scale.set_tooltip_text("Mix master volume")
        self._vol_handler = self._vol_scale.connect(
            "value-changed", self._on_volume_changed)
        vol_row.append(self._vol_scale)
        self._vol_pct = Gtk.Label(label="", xalign=1)
        self._vol_pct.add_css_class("dim-label")
        self._vol_pct.add_css_class("caption")
        self._vol_pct.set_width_chars(4)
        vol_row.append(self._vol_pct)

        self._level = Gtk.LevelBar(
            orientation=Gtk.Orientation.HORIZONTAL,
            mode=Gtk.LevelBarMode.CONTINUOUS,
            min_value=0.0,
            max_value=1.0,
            valign=Gtk.Align.CENTER,
        )
        self._level.set_size_request(-1, 6)
        self._level.add_css_class("openwave-level")
        self._level.add_offset_value(Gtk.LEVEL_BAR_OFFSET_LOW, 0.70)
        self._level.add_offset_value(Gtk.LEVEL_BAR_OFFSET_HIGH, 0.90)
        self._level.add_offset_value(Gtk.LEVEL_BAR_OFFSET_FULL, 1.00)
        text.append(self._level)

        self._menu_btn = Gtk.MenuButton(
            icon_name="view-more-symbolic",
            valign=Gtk.Align.CENTER,
            tooltip_text="Output, rename, delete",
        )
        self._menu_btn.add_css_class("flat")
        self._menu_btn.add_css_class("circular")
        self._menu_btn.set_popover(self._build_popover())
        inner.append(self._menu_btn)

    # ----- popover -----
    @staticmethod
    def _menu_row_button(icon_name, label, label_css=None):
        btn = Gtk.Button(hexpand=True)
        btn.add_css_class("flat")
        row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
        row.append(Gtk.Image.new_from_icon_name(icons.resolve(icon_name)))
        lbl = Gtk.Label(label=label, xalign=0, hexpand=True)
        if label_css:
            lbl.add_css_class(label_css)
        row.append(lbl)
        btn.set_child(row)
        return btn

    def _build_popover(self):
        pop = Gtk.Popover()
        box = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL, spacing=8,
            margin_start=8, margin_end=8, margin_top=8, margin_bottom=8,
        )
        box.set_size_request(272, -1)
        pop.set_child(box)

        heading = Gtk.Label(label="Output", xalign=0)
        heading.add_css_class("heading")
        box.append(heading)

        scroll = Gtk.ScrolledWindow(
            hscrollbar_policy=Gtk.PolicyType.NEVER,
            vscrollbar_policy=Gtk.PolicyType.AUTOMATIC,
            propagate_natural_height=True,
            max_content_height=260,
        )
        box.append(scroll)

        self._out_list = Gtk.ListBox(selection_mode=Gtk.SelectionMode.SINGLE)
        self._out_list.add_css_class("boxed-list")
        self._out_list.connect("row-selected", self._on_output_row_selected)
        scroll.set_child(self._out_list)

        box.append(Gtk.Separator())

        rename_btn = self._menu_row_button("document-edit-symbolic", "Rename Mix…")
        rename_btn.connect("clicked", self._on_rename_clicked)
        box.append(rename_btn)

        # The tooltip hangs off a sensitive wrapper as well as the button:
        # an insensitive GTK4 widget is skipped by picking and never gets the
        # motion event that would show its own tooltip.
        self._delete_wrap = Gtk.Box()
        box.append(self._delete_wrap)
        self._delete_btn = self._menu_row_button(
            "user-trash-symbolic", "Delete Mix", label_css="error",
        )
        self._delete_btn.connect("clicked", self._on_delete_clicked)
        self._delete_wrap.append(self._delete_btn)

        # Belt and braces for the tooltip: a disabled button with no visible
        # explanation reads as a bug.
        self._delete_hint = Gtk.Label(label="", xalign=0, wrap=True, visible=False)
        self._delete_hint.add_css_class("dim-label")
        self._delete_hint.add_css_class("caption")
        box.append(self._delete_hint)

        return pop

    def _popdown(self):
        pop = self._menu_btn.get_popover()
        if pop is not None:
            pop.popdown()

    def _on_output_row_selected(self, _box, row):
        if self._updating or row is None:
            return
        name = getattr(row, "_output_name", None)
        # GTK re-emits row-selected when the popover is first mapped, because
        # the selection made on the unrealised list is re-applied then. Compare
        # against the value we last displayed rather than trusting the signal:
        # re-picking the current output is a no-op either way.
        if name is None or name == self._current_output:
            return
        self._current_output = name
        self._popdown()
        self.emit("output-changed", name)

    def _on_rename_clicked(self, _btn):
        self._popdown()
        _emit_later(self, "rename-clicked")

    def _on_delete_clicked(self, _btn):
        self._popdown()
        _emit_later(self, "remove-clicked")

    def _on_volume_changed(self, scale):
        value = scale.get_value()
        self._vol_pct.set_label(f"{round(value * 100):d}%")
        self.emit("volume-changed", value)

    # ----- setters -----
    def set_volume(self, value):
        """Reflect the master without firing the changed signal."""
        value = max(0.0, min(1.0, value))
        with GObject.signal_handler_block(self._vol_scale, self._vol_handler):
            self._vol_scale.set_value(value)
        self._vol_pct.set_label(f"{round(value * 100):d}%")

    def set_level(self, value):
        """Display a raw peak using cubic fader taper and ~140 ms peak decay.

        The decay factor assumes the monitor's approximately 15 Hz updates.
        """
        shown = max(0.0, min(1.0, value)) ** (1.0 / 3.0)
        held = getattr(self, "_level_held", 0.0) * 0.72
        self._level_held = max(shown, held)
        self._level.set_value(self._level_held)

    def set_title(self, title):
        self._title_lbl.set_label(title)
        self._title_lbl.set_tooltip_text(title)

    def set_subtitle(self, subtitle):
        self._subtitle_lbl.set_label(subtitle or "")
        self._subtitle_lbl.set_visible(bool(subtitle))

    def set_icon(self, icon_name):
        self._icon.set_from_icon_name(icons.resolve(icon_name))

    def set_empty(self, empty):
        """Mark the column as carrying nothing.

        A mix whose cells are all at zero is silent, and looks identical to a
        working one: the sink exists, apps can select it, and it plays nothing.
        Saying so here is the difference between "misconfigured" and "broken",
        which is not otherwise visible anywhere.
        """
        if getattr(self, "_empty", None) == empty:
            return
        self._empty = empty
        if empty:
            self._out_lbl.set_label("No sources routed")
            self._out_lbl.set_tooltip_text(
                "Every source is at zero for this mix, so it carries no audio. "
                "Raise a slider in this column."
            )
            self._out_icon.set_from_icon_name("dialog-information-symbolic")
        else:
            self._out_lbl.set_label(getattr(self, "_out_summary", ""))
            self._out_lbl.set_tooltip_text(None)
            self._out_icon.set_from_icon_name(
                "audio-speakers-symbolic" if getattr(self, "_monitored", True)
                else "audio-volume-muted-symbolic"
            )

    def set_outputs(self, entries, current, summary, monitored=True):
        """Rebuild the chooser. `entries` is [(output name, label), ...]."""
        self._updating = True
        try:
            child = self._out_list.get_first_child()
            while child is not None:
                nxt = child.get_next_sibling()
                self._out_list.remove(child)
                child = nxt
            selected = None
            for name, label in entries:
                row = Gtk.ListBoxRow()
                lbl = Gtk.Label(
                    label=label, xalign=0,
                    margin_start=12, margin_end=12, margin_top=8, margin_bottom=8,
                )
                lbl.set_ellipsize(Pango.EllipsizeMode.END)
                lbl.set_max_width_chars(28)
                row.set_child(lbl)
                row._output_name = name  # noqa: SLF001
                self._out_list.append(row)
                if name == current:
                    selected = row
            if selected is not None:
                self._out_list.select_row(selected)
            self._current_output = current
        finally:
            self._updating = False

        self._monitored = monitored
        self._out_summary = summary
        self._out_lbl.set_label(summary)
        self._out_lbl.set_tooltip_text(summary)
        self._out_icon.set_from_icon_name(
            "audio-speakers-symbolic" if monitored else "audio-volume-muted-symbolic"
        )
        self._out_box.set_visible(True)
        if getattr(self, "_empty", False):
            # Re-assert after the icon and label above, which would otherwise
            # overwrite it: an empty column keeps saying so, because where it
            # routes is moot until something feeds it.
            self._empty = None
            self.set_empty(True)

    def set_delete_enabled(self, enabled, reason=""):
        self._delete_btn.set_sensitive(enabled)
        tip = None if enabled else (reason or None)
        self._delete_btn.set_tooltip_text(tip)
        self._delete_wrap.set_tooltip_text(tip)
        self._delete_hint.set_label(reason or "")
        self._delete_hint.set_visible(not enabled)


class SourceCell(Gtk.Box):
    """Leftmost cell of a source row: icon, name, master mute + volume."""

    __gsignals__ = {
        "volume-changed": (GObject.SignalFlags.RUN_FIRST, None, (float,)),
        "mute-toggled": (GObject.SignalFlags.RUN_FIRST, None, (bool,)),
        "remove-clicked": (GObject.SignalFlags.RUN_FIRST, None, ()),
        # (delta) -- -1 to move this row up, +1 to move it down
        "move-clicked": (GObject.SignalFlags.RUN_FIRST, None, (int,)),
        # Make this the live source in its group
        "switch-clicked": (GObject.SignalFlags.RUN_FIRST, None, ()),
        "edit-clicked": (GObject.SignalFlags.RUN_FIRST, None, ()),
        # DSP popover moved; read the values back with fx_settings()
        "fx-changed": (GObject.SignalFlags.RUN_FIRST, None, ()),
        # "Auto" pressed in the DSP popover: run the calibration wizard
        "fx-autotune": (GObject.SignalFlags.RUN_FIRST, None, ()),
    }

    def __init__(self, *, name, icon_name, has_level, removable=False,
                 editable=False, reorderable=False, is_capture=False):
        super().__init__(
            orientation=Gtk.Orientation.HORIZONTAL,
            spacing=10,
        )
        self.add_css_class("openwave-source-cell")
        self.add_css_class("card")
        self.set_size_request(400, 64)
        # A microphone row is muted at the microphone, not at a speaker: the
        # playback icons there read as "this output is silenced".
        self._is_capture = is_capture

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

        if reorderable:
            handle = Gtk.Image.new_from_icon_name(
                icons.resolve("list-drag-handle-symbolic"))
            handle.set_pixel_size(14)
            handle.add_css_class("dim-label")
            handle.set_tooltip_text("Drag to reorder")
            inner.append(handle)

        self._icon = Gtk.Image.new_from_icon_name(icons.resolve(icon_name))
        self._icon.set_pixel_size(26)
        inner.append(self._icon)

        text = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL,
            spacing=0,
            hexpand=True,
            valign=Gtk.Align.CENTER,
        )
        inner.append(text)

        self._name_lbl = Gtk.Label(label=name, xalign=0, hexpand=True, ellipsize=3)
        # Without a width request the label yields all its space to the
        # controls beside it and renders as a bare ellipsis.
        self._name_lbl.set_width_chars(10)
        self._name_lbl.set_tooltip_text(name)

        # Group badge. A grouping that is only visible by opening each row's
        # edit dialog is a grouping nobody knows they have.
        self._group_lbl = Gtk.Label(label="", xalign=0, visible=False)
        self._group_lbl.add_css_class("openwave-group-badge")
        self._group_lbl.add_css_class("caption")
        text.append(self._group_lbl)
        self._name_lbl.add_css_class("heading")
        text.append(self._name_lbl)

        # Only idle applications need the second status line.
        self._status_lbl = Gtk.Label(label="", xalign=0, ellipsize=3, visible=False)
        self._status_lbl.add_css_class("dim-label")
        self._status_lbl.add_css_class("caption")
        text.append(self._status_lbl)

        # None, not False: the first set_waiting call must always apply.
        self._waiting = None

        self._mute_btn = Gtk.ToggleButton(valign=Gtk.Align.CENTER)
        self._mute_btn.add_css_class("flat")
        self._mute_btn.add_css_class("circular")
        # Shown only on a grouped row: one press makes this the live source
        # and silences its group-mates, rather than unmuting one and
        # remembering to mute the other.
        self._switch_btn = Gtk.Button(
            valign=Gtk.Align.CENTER, visible=False,
            tooltip_text="Switch to this source",
        )
        self._switch_btn.add_css_class("flat")
        self._switch_btn.add_css_class("circular")
        # Hidden means blanked-in-place, never removed: every optional
        # control keeps its column or the sliders zigzag across rows.
        self._switch_btn.set_visible(True)
        self._reserve(self._switch_btn, False)
        # Two opposing arrows rather than a radio dot: this is an action --
        # "make this one live" -- not a state to read. The state is already on
        # the row, which is red when muted.
        self._switch_icon = Gtk.Image.new_from_icon_name(
            "mail-send-receive-symbolic")
        self._switch_btn.set_child(self._switch_icon)
        self._switch_btn.connect("clicked", lambda _b: self.emit("switch-clicked"))
        inner.append(self._switch_btn)

        self._mute_icon = Gtk.Image.new_from_icon_name(
            "audio-input-microphone-symbolic" if is_capture
            else "audio-volume-high-symbolic"
        )
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
            round_digits=2,
        )
        self._pct_lbl = _percent_label()
        self._scale.add_css_class("openwave-mix-slider")
        self._scale.set_size_request(110, -1)
        self._scale_handler = self._scale.connect("value-changed", self._on_value_changed)
        inner.append(self._scale)
        inner.append(self._pct_lbl)

        self._level = None
        if has_level:
            self._level = Gtk.LevelBar(
                orientation=Gtk.Orientation.HORIZONTAL,
                mode=Gtk.LevelBarMode.CONTINUOUS,
                min_value=0.0,
                max_value=1.0,
                valign=Gtk.Align.CENTER,
            )
            self._level.set_size_request(56, 8)
            self._level.add_css_class("openwave-level")
            # Color stops: green up to 0.7, amber to 0.9, red above.
            self._level.add_offset_value(Gtk.LEVEL_BAR_OFFSET_LOW, 0.70)
            self._level.add_offset_value(Gtk.LEVEL_BAR_OFFSET_HIGH, 0.90)
            self._level.add_offset_value(Gtk.LEVEL_BAR_OFFSET_FULL, 1.00)
            inner.append(self._level)

        # A text label, deliberately: no icon theme ships an "effects"
        # glyph everywhere, Breeze drew the broken-image box here, and
        # "FX" is the clearer button anyway. Built for EVERY row and
        # merely blanked on non-capture ones, because the controls to its
        # left only line up across rows if each optional widget keeps its
        # column when idle.
        self._fx_widgets = None
        self._fx_btn = Gtk.MenuButton(
            label="FX",
            valign=Gtk.Align.CENTER,
            tooltip_text="Effects: low cut, gate, compressor, EQ, delay",
        )
        self._fx_btn.add_css_class("flat")
        if is_capture:
            self._fx_btn.set_popover(self._build_fx_popover())
        else:
            self._reserve(self._fx_btn, False)
        inner.append(self._fx_btn)

        if editable:
            edit_btn = Gtk.Button(
                icon_name="document-edit-symbolic",
                valign=Gtk.Align.CENTER,
                tooltip_text="Edit source",
            )
            edit_btn.add_css_class("flat")
            edit_btn.add_css_class("circular")
            edit_btn.connect("clicked", lambda _: self.emit("edit-clicked"))
            inner.append(edit_btn)

        self._remove_btn = None
        if removable:
            self._remove_btn = Gtk.Button(
                icon_name="window-close-symbolic",
                valign=Gtk.Align.CENTER,
                tooltip_text="Remove source",
            )
            self._remove_btn.add_css_class("flat")
            self._remove_btn.add_css_class("circular")
            self._remove_btn.connect(
                "clicked", lambda _: self.emit("remove-clicked"))
            inner.append(self._remove_btn)

    __FX_SIGNAL = "fx-changed"

    def _build_fx_popover(self):
        """The per-microphone DSP controls: low cut, tone, delay, mono.

        Widgets are the state; fx_settings() reads them and set_fx() writes
        them with signals blocked, mirroring how every other control here
        round-trips. Emission is per-gesture — the app debounces the
        respawn, not the popover.
        """
        pop = Gtk.Popover()
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8,
                      margin_top=12, margin_bottom=12,
                      margin_start=12, margin_end=12)
        pop.set_child(box)

        def row(label, widget):
            r = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=10)
            lbl = Gtk.Label(label=label, xalign=0)
            lbl.set_width_chars(9)
            r.append(lbl)
            r.append(widget)
            box.append(r)

        auto_btn = Gtk.Button(label="Auto-calibrate microphone")
        # Defer calibration until the popover's Wayland grab is released.
        auto_btn.connect("clicked",
                         lambda _b: (pop.popdown(),
                                     _emit_later(self, "fx-autotune")))
        box.append(auto_btn)

        self._fx_lowcut = Gtk.DropDown.new_from_strings(
            ["Off", "80 Hz", "120 Hz"])
        self._fx_lowcut.connect("notify::selected", self._on_fx_changed)
        row("Low cut", self._fx_lowcut)

        def switch():
            s = Gtk.Switch(halign=Gtk.Align.START, valign=Gtk.Align.CENTER)
            s.connect("notify::active", self._on_fx_changed)
            return s

        def scale(lo, hi, step, digits=0):
            s = Gtk.Scale(
                orientation=Gtk.Orientation.HORIZONTAL,
                draw_value=True, digits=digits,
                adjustment=Gtk.Adjustment(
                    lower=lo, upper=hi, step_increment=step,
                    page_increment=step * 5),
                hexpand=True,
            )
            s.set_size_request(160, -1)
            s.connect("value-changed", self._on_fx_changed)
            return s

        self._fx_gate = switch()
        row("Gate", self._fx_gate)
        self._fx_gate_thresh = scale(*FX_RANGES["gate_thresh"], 1)
        row("Gate dB", self._fx_gate_thresh)

        self._fx_comp = switch()
        row("Comp", self._fx_comp)
        self._fx_comp_thresh = scale(*FX_RANGES["comp_thresh"], 1)
        row("Comp dB", self._fx_comp_thresh)
        self._fx_comp_ratio = scale(*FX_RANGES["comp_ratio"], 0.5, digits=1)
        row("Ratio", self._fx_comp_ratio)

        # A slider whose effect is off is either misleading (it moves,
        # nothing happens) or a statement of intent. Both, resolved:
        # the sliders dim while their switch is off, and dragging one
        # anyway flips the switch — choosing a threshold IS enabling.
        def bind(sw, *scales):
            def sync(*_a):
                for s in scales:
                    s.set_sensitive(sw.get_active())
            sw.connect("notify::active", sync)
            sync()
            for s in scales:
                def enable(_s, sw=sw):
                    if not getattr(self, "_fx_updating", False) \
                            and not sw.get_active():
                        sw.set_active(True)
                s.connect("value-changed", enable)

        bind(self._fx_gate, self._fx_gate_thresh)
        bind(self._fx_comp, self._fx_comp_thresh, self._fx_comp_ratio)

        def eq_scale():
            s = Gtk.Scale(
                orientation=Gtk.Orientation.HORIZONTAL,
                draw_value=True, digits=0,
                adjustment=Gtk.Adjustment(
                    lower=-12, upper=12, step_increment=1, page_increment=3),
                hexpand=True,
            )
            s.set_size_request(160, -1)
            s.add_mark(0, Gtk.PositionType.BOTTOM, None)
            s.connect("value-changed", self._on_fx_changed)
            return s

        self._fx_eq_low = eq_scale()
        self._fx_eq_mid = eq_scale()
        self._fx_eq_high = eq_scale()
        row("Low dB", self._fx_eq_low)
        row("Mid dB", self._fx_eq_mid)
        row("High dB", self._fx_eq_high)

        self._fx_delay = Gtk.SpinButton(
            adjustment=Gtk.Adjustment(
                lower=0, upper=500, step_increment=5, page_increment=25),
            climb_rate=1, digits=0,
        )
        self._fx_delay.connect("value-changed", self._on_fx_changed)
        row("Delay ms", self._fx_delay)

        self._fx_mono = Gtk.Switch(halign=Gtk.Align.START,
                                   valign=Gtk.Align.CENTER)
        self._fx_mono.connect("notify::active", self._on_fx_changed)
        row("Mono", self._fx_mono)

        self._fx_updating = False
        return pop

    def _on_fx_changed(self, *_args):
        if getattr(self, "_fx_updating", False):
            return
        self.emit(self.__FX_SIGNAL)

    def fx_settings(self):
        """The popover's current values in the effects.DEFAULT_FX schema."""
        lowcut = (0, 80, 120)[self._fx_lowcut.get_selected()]
        return {
            "lowcut": lowcut,
            "gate": bool(self._fx_gate.get_active()),
            "gate_thresh": float(self._fx_gate_thresh.get_value()),
            "comp": bool(self._fx_comp.get_active()),
            "comp_thresh": float(self._fx_comp_thresh.get_value()),
            "comp_ratio": float(self._fx_comp_ratio.get_value()),
            "eq_low": float(self._fx_eq_low.get_value()),
            "eq_mid": float(self._fx_eq_mid.get_value()),
            "eq_high": float(self._fx_eq_high.get_value()),
            "delay_ms": int(self._fx_delay.get_value()),
            "mono": bool(self._fx_mono.get_active()),
        }

    def set_fx(self, fx):
        """Load stored settings into the popover without emitting."""
        if self._fx_widgets is None and not hasattr(self, "_fx_lowcut"):
            return
        self._fx_updating = True
        try:
            self._fx_lowcut.set_selected(
                {0: 0, 80: 1, 120: 2}.get(int(fx.get("lowcut", 0)), 0))
            self._fx_gate.set_active(bool(fx.get("gate", False)))
            self._fx_gate_thresh.set_value(fx.get("gate_thresh", -50.0))
            self._fx_comp.set_active(bool(fx.get("comp", False)))
            self._fx_comp_thresh.set_value(fx.get("comp_thresh", -18.0))
            self._fx_comp_ratio.set_value(fx.get("comp_ratio", 3.0))
            self._fx_eq_low.set_value(fx.get("eq_low", 0.0))
            self._fx_eq_mid.set_value(fx.get("eq_mid", 0.0))
            self._fx_eq_high.set_value(fx.get("eq_high", 0.0))
            self._fx_delay.set_value(fx.get("delay_ms", 0))
            self._fx_mono.set_active(bool(fx.get("mono", False)))
        finally:
            self._fx_updating = False

    @staticmethod
    def _reserve(widget, shown):
        """Blank a control in place instead of removing it.

        Rows line up column by column only while every optional widget
        keeps its allocation; set_visible collapses the slot and shifts
        everything beside it, which is how the sliders came to zigzag.
        """
        widget.set_opacity(1.0 if shown else 0.0)
        widget.set_sensitive(shown)
        widget.set_can_target(shown)

    def set_removable(self, removable, tooltip="Remove source"):
        """Show or blank the remove button on a row that owns one.

        Auto-discovered device rows are built with the button and normally
        blank it: while the hardware is connected, removing its row would
        only make it come back confusing. Unplugged, the row is clutter the
        user may clear — so removability follows presence.
        """
        if self._remove_btn is not None:
            self._reserve(self._remove_btn, removable)
            self._remove_btn.set_tooltip_text(tooltip if removable else None)

    def set_group(self, group):
        """Show which exclusivity group this row is in, if any."""
        group = (group or "").strip()
        self._reserve(self._switch_btn, bool(group))
        self._group_lbl.set_label(f"\u2b24 {group}" if group else "")
        self._group_lbl.set_visible(bool(group))
        self._group_lbl.set_tooltip_text(
            f"Only one source in \u201c{group}\u201d is live at a time"
            if group else None
        )

    def set_name(self, name):
        self._name_lbl.set_label(name)
        self._name_lbl.set_tooltip_text(name)

    def set_icon(self, icon_name):
        self._icon.set_from_icon_name(icons.resolve(icon_name))
    def set_available(self, available, *, reason="Device not connected"):
        """Dim the row when the device behind it is gone.

        The controls stay live on purpose: the level is persisted whether or
        not the device is present, so one set while a headset is off takes
        effect the moment it comes back.
        """
        if available:
            self._name_lbl.remove_css_class("dim-label")
            self.set_tooltip_text(None)
        else:
            self._name_lbl.add_css_class("dim-label")
            self.set_tooltip_text(reason)

    def _sync_percent(self):
        if getattr(self, "_pct_lbl", None) is not None:
            self._pct_lbl.set_label(f"{round(self._scale.get_value() * 100):d}%")

    def set_volume(self, value):
        """Update the master slider without firing the changed signal."""
        with GObject.signal_handler_block(self._scale, self._scale_handler):
            self._scale.set_value(max(0.0, min(1.0, value)))
        # The changed handler is blocked above, so the readout is updated here.
        self._sync_percent()

    def set_level(self, value):
        """Update the audio activity meter (0.0–1.0). No-op if not enabled."""
        if self._level is not None:
            self._level.set_value(max(0.0, min(1.0, value)))
    def set_waiting(self, waiting, hint="Waiting for audio"):
        """Show or clear the 'bound application is not playing' state.

        A bound-but-idle source should read as waiting, not broken: the row
        dims and gains a hint line, but stays interactive so levels can be set
        up before the application is launched.

        Called on every stream-poll tick, so it no-ops unless something
        actually changed rather than churning the layout twice a second.
        """
        waiting = bool(waiting)
        state = (waiting, hint if waiting else "")
        if state == self._waiting:
            return
        self._waiting = state
        self._status_lbl.set_label(hint if waiting else "")
        self._status_lbl.set_visible(waiting)
        self.set_tooltip_text(hint if waiting else None)
        if waiting:
            self.add_css_class("openwave-source-waiting")
        else:
            self.remove_css_class("openwave-source-waiting")

    def set_muted(self, muted):
        """Update the mute toggle without firing its signal."""
        with GObject.signal_handler_block(self._mute_btn, self._mute_handler):
            self._mute_btn.set_active(muted)
        self._reflect_mute_icon(muted)

    def _reflect_mute_icon(self, muted):
        if getattr(self, "_is_capture", False):
            icon = ("microphone-sensitivity-muted-symbolic" if muted
                    else "audio-input-microphone-symbolic")
        else:
            icon = ("audio-volume-muted-symbolic" if muted
                    else "audio-volume-high-symbolic")
        self._mute_icon.set_from_icon_name(icon)
        self._mute_btn.set_tooltip_text("Unmute" if muted else "Mute")
        if getattr(self, "_switch_btn", None) is not None:
            # Deliberately always sensitive. A control that greys out exactly
            # when you press it reads as broken, and switching to the source
            # that is already live is harmless.
            self._switch_btn.set_tooltip_text(
                "Switch to this source"
                if muted else "This source is already live"
            )
        # A muted row should be obvious at a glance down the column, not a
        # difference of one small icon.
        for widget in (self, self._name_lbl, self._mute_icon):
            if muted:
                widget.add_css_class("openwave-muted")
            else:
                widget.remove_css_class("openwave-muted")
        if self._level is not None:
            if muted:
                self._level.add_css_class("dim-label")
                self._level.remove_css_class("success")
            else:
                self._level.remove_css_class("dim-label")
                self._level.add_css_class("success")

    def _on_value_changed(self, scale):
        self._sync_percent()
        self.emit("volume-changed", scale.get_value())

    def _on_mute_toggled(self, btn):
        muted = btn.get_active()
        self._reflect_mute_icon(muted)
        self.emit("mute-toggled", muted)


class MixCell(Gtk.Box):
    """Grid intersection: small mute toggle + horizontal volume slider."""

    __gsignals__ = {
        "volume-changed": (GObject.SignalFlags.RUN_FIRST, None, (float,)),
        "mute-toggled": (GObject.SignalFlags.RUN_FIRST, None, (bool,)),
    }

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
            hexpand=True,
            round_digits=2,
        )
        self._pct_lbl = _percent_label()
        self._scale.add_css_class("openwave-mix-slider")

        # A new cell routes nothing, so it starts muted and says so. Leaving it
        # unmuted at 0% shows an armed-looking control that carries no audio.
        with GObject.signal_handler_block(self._mute_btn, self._mute_handler):
            self._mute_btn.set_active(True)
        self._reflect_mute(True)
        self._scale_handler = self._scale.connect("value-changed", self._on_value_changed)
        inner.append(self._scale)
        inner.append(self._pct_lbl)

    def _sync_percent(self):
        if getattr(self, "_pct_lbl", None) is not None:
            self._pct_lbl.set_label(f"{round(self._scale.get_value() * 100):d}%")

    def set_volume(self, value):
        with GObject.signal_handler_block(self._scale, self._scale_handler):
            self._scale.set_value(max(0.0, min(1.0, value)))
        # The changed handler is blocked above, so the readout is updated here.
        self._sync_percent()

    def _reflect_mute(self, muted):
        self._mute_icon.set_from_icon_name(
            "audio-volume-muted-symbolic" if muted else "audio-volume-high-symbolic"
        )
        self._mute_btn.set_tooltip_text("Unmute" if muted else "Mute")
        for widget in (self, self._mute_icon):
            if muted:
                widget.add_css_class("openwave-muted")
            else:
                widget.remove_css_class("openwave-muted")

    def set_muted(self, muted):
        with GObject.signal_handler_block(self._mute_btn, self._mute_handler):
            self._mute_btn.set_active(muted)
        self._reflect_mute(muted)

    def _on_value_changed(self, scale):
        self._sync_percent()
        # A cell at zero and a muted cell mean the same thing, and letting them
        # disagree produces a slider at 0% next to an unmuted icon, or a slider
        # the user raises with no sound because a mute they forgot is still on.
        should_mute = scale.get_value() <= 0.0
        if should_mute != self._mute_btn.get_active():
            with GObject.signal_handler_block(self._mute_btn, self._mute_handler):
                self._mute_btn.set_active(should_mute)
            self._reflect_mute(should_mute)
            self.emit("mute-toggled", should_mute)
        self.emit("volume-changed", scale.get_value())

    def _on_mute_toggled(self, btn):
        muted = btn.get_active()
        self._reflect_mute(muted)
        self.emit("mute-toggled", muted)
