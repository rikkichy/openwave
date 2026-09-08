"""'Add Source': source kind, then a per-kind picker, then name + icon.

Page 0 forks between an application source and a hardware capture device. The
app branch can also bind an application that is not running, by typing its
name. The branches share only the final name/icon page, which is parameterised
so each supplies its own defaults and confirm handler, and each emits its own
signal so neither has to know the other exists.

Passing source= opens straight to the config page in edit mode: the pickers
list only what is present right now, and requiring the bound app to be playing
in order to rename its row would be nonsense.
"""

import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GObject  # noqa: E402

from . import icons
from . import sources as sources_module

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
    """Edit source identity using caller-owned, nonblocking audio snapshots.

    ``streams`` is an iterable of dictionaries with ``app_name`` and optional
    ``display_name``, ``media_name``, ``node_name`` and ``binary`` fields.
    ``captures`` contains ``name`` (node name) and ``description`` dictionaries.
    Pass ``Mixer.streams().values()`` and ``Mixer.capture_sources()``.

    Signals omit the emitting widget here:
      source-confirmed(name, comma_separated_bindings, icon_name, group)
      device-source-confirmed(name, node_name, icon_name, group)
      source-edited(id, name, comma_separated_bindings, icon_name, group)
    Device edits emit an empty bindings string and preserve their node binding.
    """
    __gsignals__ = {
        # (display_name, match_app_name, icon_name, group)
        "source-confirmed": (GObject.SignalFlags.RUN_FIRST, None, (str, str, str, str)),
        # (display_name, capture_node_name, icon_name, group)
        "device-source-confirmed": (GObject.SignalFlags.RUN_FIRST, None, (str, str, str, str)),
        # (source_id, display_name, binding, icon_name, group). `binding` is the
        # match_app_name for an app source and "" for a device source, whose
        # node_name is hardware and is not editable here.
        "source-edited": (GObject.SignalFlags.RUN_FIRST, None, (str, str, str, str, str)),
    }

    def __init__(self, source=None, *, exclude_nodes=(), exclude_apps=(),
                 streams=(), captures=()):
        super().__init__()
        self._source = source
        self._streams = tuple(streams)
        self._captures = tuple(captures)
        self._editing_device = (
            source is not None
            and sources_module.kind(source) == sources_module.KIND_DEVICE
        )
        self.set_title("Edit Source" if source else "Add Source")
        self.set_content_width(480)
        self.set_content_height(560)

        self._nav = Adw.NavigationView()
        self.set_child(self._nav)

        # Capture nodes that already have a matrix row.
        self._exclude_nodes = frozenset(exclude_nodes)
        # Compared case-insensitively, the same way stream matching does:
        # a row bound to "spotify" already covers the app reporting "Spotify".
        self._exclude_apps = frozenset(a.casefold() for a in exclude_apps)
        # None = nothing picked yet, "" = manual entry, else the picked app.
        # Every binding, comma-separated: a source can gather more than one
        # application, and an edit that showed only the first would silently
        # drop the rest on save.
        self._selected_app = (
            None if source is None else sources_module.format_bindings(source)
        )
        self._selected_device = None
        self._selected_icon = (source or {}).get("icon_name") or ICON_CHOICES[0][0]

        if source is None:
            self._nav.push(self._build_type_page())
        else:
            # Config page as the navigation root; it packs its own Cancel,
            # because the type page that normally carries one was never built.
            self._nav.push(self._build_config_page(
                show_app_row=not self._editing_device,
            ))

    def _build_type_page(self):
        """Fork between the source kinds.

        A separate first page rather than a mode switch on the app picker: the
        two flows share only the name/icon page, and keeping them in separate
        pages means the app picker needs no knowledge of devices at all.
        """
        page = Adw.NavigationPage(title="Add Source")

        view = Adw.ToolbarView()
        page.set_child(view)

        header = Adw.HeaderBar()
        view.add_top_bar(header)

        cancel_btn = Gtk.Button(label="Cancel")
        cancel_btn.connect("clicked", lambda _: self.close())
        header.pack_start(cancel_btn)

        clamp = Adw.Clamp(
            maximum_size=440,
            margin_start=12, margin_end=12, margin_top=12, margin_bottom=12,
        )
        view.set_content(clamp)

        outer = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL, spacing=12,
            valign=Gtk.Align.START,
        )
        clamp.set_child(outer)

        hint = Gtk.Label(
            label="What should this row carry into your mixes?",
            wrap=True, xalign=0,
        )
        hint.add_css_class("dim-label")
        outer.append(hint)

        listbox = Gtk.ListBox(selection_mode=Gtk.SelectionMode.NONE)
        listbox.add_css_class("boxed-list")
        outer.append(listbox)

        app_row = Adw.ActionRow(
            title="Application",
            subtitle="Follows every stream an app plays, now and later",
            activatable=True,
        )
        app_row.add_prefix(
            Gtk.Image.new_from_icon_name("applications-multimedia-symbolic")
        )
        app_row.add_suffix(Gtk.Image.new_from_icon_name("go-next-symbolic"))
        app_row.connect(
            "activated", lambda _r: self._nav.push(self._build_picker_page()),
        )
        listbox.append(app_row)

        device_row = Adw.ActionRow(
            title="Capture Device",
            subtitle="A microphone or line input, such as a headset mic",
            activatable=True,
        )
        device_row.add_prefix(
            Gtk.Image.new_from_icon_name("audio-input-microphone-symbolic")
        )
        device_row.add_suffix(Gtk.Image.new_from_icon_name("go-next-symbolic"))
        device_row.connect(
            "activated", lambda _r: self._nav.push(self._build_device_page()),
        )
        listbox.append(device_row)

        return page

    # ------------------------------------------------------- device picker
    def _build_device_page(self):
        page = Adw.NavigationPage(title="Pick Capture Device")

        view = Adw.ToolbarView()
        page.set_child(view)

        header = Adw.HeaderBar()
        view.add_top_bar(header)

        self._device_next_btn = Gtk.Button(label="Next")
        self._device_next_btn.add_css_class("suggested-action")
        self._device_next_btn.set_sensitive(False)
        self._device_next_btn.connect("clicked", self._on_device_next)
        header.pack_end(self._device_next_btn)

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
            label="Pick a microphone or line input. OpenWave mixes it into each "
                  "mix at the level you set, alongside the Wave's own mic.",
            wrap=True, xalign=0,
        )
        hint.add_css_class("dim-label")
        outer.append(hint)

        self._device_list = Gtk.ListBox(selection_mode=Gtk.SelectionMode.SINGLE)
        self._device_list.add_css_class("boxed-list")
        self._device_list.connect("row-selected", self._on_device_row_selected)
        outer.append(self._device_list)

        self._populate_devices()
        return page

    def _populate_devices(self):
        # Already-bound nodes are filtered out rather than shown disabled: the
        # Wave's own mic is the built-in row, and a second row for it would
        # double the same audio into every mix.
        devices = [
            d for d in self._captures if d["name"] not in self._exclude_nodes
        ]
        if not devices:
            empty = Adw.ActionRow(title="No other capture devices")
            empty.set_subtitle(
                "Connect a headset or microphone, then open this dialog again"
            )
            empty.set_sensitive(False)
            self._device_list.append(empty)
            return
        for device in devices:
            row = Adw.ActionRow(title=device["description"])
            # The node name disambiguates two inputs on one card that share a
            # description, and is what actually gets persisted.
            row.set_subtitle(device["name"])
            row.add_prefix(
                Gtk.Image.new_from_icon_name("audio-input-microphone-symbolic")
            )
            row._device = device  # noqa: SLF001
            self._device_list.append(row)

    def _on_device_row_selected(self, _box, row):
        self._selected_device = (
            getattr(row, "_device", None) if row is not None else None
        )
        self._device_next_btn.set_sensitive(self._selected_device is not None)

    def _on_device_next(self, _btn):
        if not self._selected_device:
            return
        self._nav.push(self._build_config_page(
            default_name=self._selected_device["description"],
            default_icon="microphone-sensitivity-high-symbolic",
            on_confirm=self._on_device_confirm,
            # A capture device has no application name, and confirm is gated
            # on that row being non-empty: leaving it in would make the device
            # flow impossible to complete.
            show_app_row=False,
        ))

    def _on_device_confirm(self, _btn):
        if not self._selected_device:
            return
        name = (
            self._name_row.get_text().strip()
            or self._selected_device["description"]
        )
        self.emit(
            "device-source-confirmed",
            name, self._selected_device["name"], self._selected_icon,
            self._group_text(),
        )
        self.close()

    # ------------------------------------------------------------ page 1
    def _build_picker_page(self):
        page = Adw.NavigationPage(title="Pick Application")

        view = Adw.ToolbarView()
        page.set_child(view)

        header = Adw.HeaderBar()
        view.add_top_bar(header)

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
            label="Pick an application that's currently playing audio, or enter one "
                  "manually if it isn't running yet. OpenWave will route any future "
                  "streams from that app through the new source row.",
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
        apps = {}
        for s in self._streams:
            if s["app_name"].casefold() in self._exclude_apps:
                continue
            apps.setdefault(s["app_name"], []).append(s)

        if not apps:
            title = ("No new apps playing audio" if self._exclude_apps
                     else "No audio streams playing")
            empty = Adw.ActionRow(title=title)
            empty.set_subtitle("Start playback in an app, or enter a name manually below")
            empty.set_sensitive(False)
            self._listbox.append(empty)

        for app_name in sorted(apps.keys()):
            # display_name is a label only; app_name below stays the match key,
            # so a row bound by its friendly title still captures by exact
            # application.name equality.
            row = Adw.ActionRow(
                title=apps[app_name][0].get("display_name") or app_name)
            sample = apps[app_name][0].get("media_name") or apps[app_name][0].get("node_name", "")
            if sample:
                row.set_subtitle(sample)
            row.add_prefix(Gtk.Image.new_from_icon_name("applications-multimedia-symbolic"))
            row._app_name = app_name  # noqa: SLF001
            self._listbox.append(row)

        # Always offered. An app that isn't running publishes no stream, so
        # without this row it could never be bound at all -- and with an empty
        # list the page is otherwise a dead end, since the placeholder row is
        # insensitive and Next stays disabled forever.
        manual = Adw.ActionRow(title="Enter manually…")
        manual.set_subtitle("Bind an application that isn't running yet")
        manual.add_prefix(Gtk.Image.new_from_icon_name("document-edit-symbolic"))
        manual._app_name = ""  # noqa: SLF001
        self._listbox.append(manual)

    def _on_row_selected(self, _box, row):
        # "" is the manual row: a real choice, just with nothing prefilled.
        # Compare against None, not truthiness, or it reads as "no selection".
        app = getattr(row, "_app_name", None) if row is not None else None
        self._selected_app = app
        self._next_btn.set_sensitive(app is not None)

    def _on_next(self, _btn):
        if self._selected_app is None:
            return
        self._nav.push(self._build_config_page())

    # ------------------------------------------------------------ page 2
    def _build_config_page(self, *, default_name=None, default_icon=None,
                           on_confirm=None, show_app_row=True):
        """Shared final page for every source flow.

        Every argument defaults to the app-picker behaviour, so the plain
        `self._build_config_page()` call in _on_next keeps working verbatim.

        show_app_row is the one that is not cosmetic. The Application entry is
        what makes a not-yet-running app bindable and a mis-bound source
        fixable, and confirm is gated on it being non-empty — but a capture
        device has no application name at all, so leaving the row in the device
        flow would leave confirm permanently insensitive and make device
        sources impossible to create.
        """
        editing = self._source is not None
        page = Adw.NavigationPage(title="Edit Source" if editing else "Name and Icon")

        view = Adw.ToolbarView()
        page.set_child(view)

        header = Adw.HeaderBar()
        view.add_top_bar(header)

        if editing:
            # This page is the navigation root, so NavigationView draws no back
            # button and the page that carries Cancel was never built.
            cancel_btn = Gtk.Button(label="Cancel")
            cancel_btn.connect("clicked", lambda _: self.close())
            header.pack_start(cancel_btn)

        self._confirm_btn = Gtk.Button(label="Save" if editing else "Add Source")
        self._confirm_btn.add_css_class("suggested-action")
        self._confirm_btn.connect("clicked", on_confirm or self._on_confirm)
        header.pack_end(self._confirm_btn)

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
        self._name_row.set_text(
            (self._source or {}).get("name")
            or default_name
            or self._selected_app
            or ""
        )
        self._name_row.connect("changed", self._on_binding_changed)
        name_group.add(self._name_row)

        # Application binding — app sources only.
        # None on the capture-device page, a list on the application page.
        self._bindings = None
        if show_app_row:
            app_group = Adw.PreferencesGroup(
                title="Applications",
                description="Audio from any of these is gathered under this "
                            "row's single fader.",
            )
            outer.append(app_group)

            # A managed list rather than a comma-separated entry. The seeded
            # rows carry a dozen names each, which is unreadable as one string
            # and impossible to edit a single entry out of.
            self._bindings = sources_module.parse_bindings(self._selected_app or "")
            self._bind_group = app_group
            self._bind_rows = []

            self._add_row = Adw.EntryRow(title="Add an application")
            add_btn = Gtk.Button(
                icon_name="list-add-symbolic", valign=Gtk.Align.CENTER,
                tooltip_text="Add this name",
            )
            add_btn.add_css_class("flat")
            add_btn.connect("clicked", lambda _b: self._add_binding_from_entry())
            self._add_row.add_suffix(add_btn)
            self._add_row.connect("entry-activated",
                                  lambda _r: self._add_binding_from_entry())
            self._add_row.connect("changed", lambda _r: self._sync_confirm())

            # Anything currently making sound, minus what is already bound --
            # the common case is "the app is running, I just do not know what
            # PipeWire calls it".
            self._running_btn = Gtk.MenuButton(
                label="From running apps", halign=Gtk.Align.START, margin_top=6,
            )
            self._running_btn.add_css_class("flat")
            self._running_pop = Gtk.Popover()
            self._running_btn.set_popover(self._running_pop)

            self._rebuild_bindings()
        elif editing:
            # A device source's binding is hardware, not text: show it, do not
            # offer to edit it. Re-pointing a row at a different capture device
            # means adding a new row.
            dev_group = Adw.PreferencesGroup(title="Capture Device")
            outer.append(dev_group)
            dev_row = Adw.ActionRow(
                title=self._source.get("node_name", ""),
                subtitle="The capture device this row is bound to",
            )
            dev_row.add_prefix(
                Gtk.Image.new_from_icon_name("audio-input-microphone-symbolic")
            )
            dev_group.add(dev_row)

        # Icon picker
        group_group = Adw.PreferencesGroup(
            title="Group",
            description="Sources sharing a group are mutually exclusive: "
                        "unmuting one mutes the others. Leave blank for none.",
        )
        outer.append(group_group)
        self._group_row = Adw.EntryRow(title="Group name")
        self._group_row.set_text((self._source or {}).get("group", "") or "")
        group_group.add(self._group_row)

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

        # Preserve stored icons even when they are no longer offered here.
        if default_icon:
            self._selected_icon = default_icon
        preselect = None
        for icon_name, tooltip in ICON_CHOICES:
            btn = Gtk.Image.new_from_icon_name(icons.resolve(icon_name))
            btn.set_pixel_size(28)
            child = Gtk.FlowBoxChild()
            child.set_child(btn)
            child.set_tooltip_text(tooltip)
            child._icon_name = icon_name  # noqa: SLF001
            flow.append(child)
            if icon_name == self._selected_icon:
                preselect = child
        flow.connect("selected-children-changed", self._on_icon_selected)
        icon_group.add(flow)

        if preselect is not None:
            flow.select_child(preselect)

        self._sync_confirm()
        return page

    def _on_binding_changed(self, _row):
        self._sync_confirm()

    def _sync_confirm(self):
        """A source that binds nothing can never be metered or routed, so refuse
        to create one rather than persisting dead config. With no Application
        row (a capture device) the name is the only requirement."""
        if self._bindings is not None:
            # A pending name in the entry counts: confirming without pressing +
            # first should not silently discard what was typed.
            pending = self._add_row.get_text().strip() if self._add_row else ""
            ok = bool(self._bindings or pending)
        else:
            ok = bool(self._name_row.get_text().strip())
        self._confirm_btn.set_sensitive(ok)

    def _rebuild_bindings(self):
        """Redraw one removable row per bound application."""
        for row in self._bind_rows:
            self._bind_group.remove(row)
        self._bind_rows = []

        for name in self._bindings:
            row = Adw.ActionRow(title=name)
            row.add_prefix(Gtk.Image.new_from_icon_name("application-x-executable-symbolic"))
            rm = Gtk.Button(
                icon_name="window-close-symbolic", valign=Gtk.Align.CENTER,
                tooltip_text=f"Stop matching {name}",
            )
            rm.add_css_class("flat")
            rm.connect("clicked", lambda _b, n=name: self._remove_binding(n))
            row.add_suffix(rm)
            self._bind_group.add(row)
            self._bind_rows.append(row)

        if not self._bindings:
            empty = Adw.ActionRow(
                title="No applications yet",
                subtitle="Add one below, or pick from what is playing",
            )
            empty.set_sensitive(False)
            self._bind_group.add(empty)
            self._bind_rows.append(empty)

        self._bind_group.add(self._add_row)
        self._bind_rows.append(self._add_row)
        self._bind_group.add(self._running_btn)
        self._bind_rows.append(self._running_btn)

        self._populate_running_menu()
        self._sync_confirm()

    def _populate_running_menu(self):
        """List what is playing now, excluding names already bound."""
        box = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL, spacing=2,
            margin_top=6, margin_bottom=6, margin_start=6, margin_end=6,
        )
        bound = {n.casefold() for n in self._bindings}
        names = []
        for stream in self._streams:
            for candidate in (stream.get("app_name"), stream.get("binary")):
                if candidate and candidate.casefold() not in bound and candidate not in names:
                    names.append(candidate)
        if not names:
            lbl = Gtk.Label(label="Nothing is playing", margin_top=6, margin_bottom=6)
            lbl.add_css_class("dim-label")
            box.append(lbl)
        for name in names:
            btn = Gtk.Button(label=name, halign=Gtk.Align.FILL)
            btn.add_css_class("flat")
            btn.connect("clicked", lambda _b, n=name: self._add_binding(n))
            box.append(btn)
        self._running_pop.set_child(box)

    def _add_binding_from_entry(self):
        text = self._add_row.get_text().strip()
        if text:
            self._add_row.set_text("")
            self._add_binding(text)

    def _add_binding(self, name):
        if name.casefold() not in {n.casefold() for n in self._bindings}:
            self._bindings.append(name)
        self._running_pop.popdown()
        self._rebuild_bindings()

    def _remove_binding(self, name):
        self._bindings = [n for n in self._bindings if n != name]
        self._rebuild_bindings()

    def _group_text(self):
        row = getattr(self, "_group_row", None)
        return row.get_text().strip() if row is not None else ""

    def _on_icon_selected(self, flow):
        sel = flow.get_selected_children()
        if sel:
            self._selected_icon = getattr(sel[0], "_icon_name", self._selected_icon)

    def _on_confirm(self, _btn):
        # Read the field, not _selected_app: with manual entry the picker's
        # value is "" and the entry is the only source of truth.
        if self._bindings is not None:
            # Fold in anything still sitting in the entry, unconfirmed.
            self._add_binding_from_entry()
            app = ", ".join(self._bindings)
        else:
            app = ""
        if self._source is None and not app:
            return
        name = self._name_row.get_text().strip() or app
        if not name:
            return
        if self._source is not None:
            # Carry the id so app.py routes this through sources.update() and
            # the row keeps its persisted per-mix levels.
            self.emit("source-edited", self._source["id"], name, app,
                      self._selected_icon, self._group_text())
        else:
            self.emit("source-confirmed", name, app, self._selected_icon,
                      self._group_text())
        self.close()
