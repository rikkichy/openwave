"""StatusNotifierItem tray icon via D-Bus (no GTK3 dependency)."""

import logging

from gi.repository import Gio, GLib

from . import icons

ITEM_XML = """
<node>
  <interface name="org.kde.StatusNotifierItem">
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconName" type="s" access="read"/>
    <property name="IconThemePath" type="s" access="read"/>
    <property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
    <property name="Menu" type="o" access="read"/>
    <property name="ItemIsMenu" type="b" access="read"/>
    <method name="Activate">
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
    </method>
    <method name="SecondaryActivate">
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
    </method>
    <method name="ContextMenu">
      <arg name="x" type="i" direction="in"/>
      <arg name="y" type="i" direction="in"/>
    </method>
    <signal name="NewIcon"/>
    <signal name="NewToolTip"/>
    <signal name="NewStatus">
      <arg name="status" type="s" direction="out"/>
    </signal>
  </interface>
</node>
"""

MENU_XML = """
<node>
  <interface name="com.canonical.dbusmenu">
    <property name="Version" type="u" access="read"/>
    <property name="TextDirection" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconThemePath" type="as" access="read"/>
    <method name="GetLayout">
      <arg name="parentId" type="i" direction="in"/>
      <arg name="recursionDepth" type="i" direction="in"/>
      <arg name="propertyNames" type="as" direction="in"/>
      <arg name="revision" type="u" direction="out"/>
      <arg name="layout" type="(ia{sv}av)" direction="out"/>
    </method>
    <method name="GetGroupProperties">
      <arg name="ids" type="ai" direction="in"/>
      <arg name="propertyNames" type="as" direction="in"/>
      <arg name="properties" type="a(ia{sv})" direction="out"/>
    </method>
    <method name="GetProperty">
      <arg name="id" type="i" direction="in"/>
      <arg name="name" type="s" direction="in"/>
      <arg name="value" type="v" direction="out"/>
    </method>
    <method name="Event">
      <arg name="id" type="i" direction="in"/>
      <arg name="eventId" type="s" direction="in"/>
      <arg name="data" type="v" direction="in"/>
      <arg name="timestamp" type="u" direction="in"/>
    </method>
    <method name="AboutToShow">
      <arg name="id" type="i" direction="in"/>
      <arg name="needUpdate" type="b" direction="out"/>
    </method>
    <method name="AboutToShowGroup">
      <arg name="ids" type="ai" direction="in"/>
      <arg name="updatesNeeded" type="ai" direction="out"/>
      <arg name="idErrors" type="ai" direction="out"/>
    </method>
    <signal name="ItemsPropertiesUpdated">
      <arg name="updatedProps" type="a(ia{sv})" direction="out"/>
      <arg name="removedProps" type="a(ias)" direction="out"/>
    </signal>
    <signal name="LayoutUpdated">
      <arg name="revision" type="u"/>
      <arg name="parent" type="i"/>
    </signal>
    <signal name="ItemActivationRequested">
      <arg name="id" type="i" direction="out"/>
      <arg name="timestamp" type="u" direction="out"/>
    </signal>
  </interface>
</node>
"""


def compute(connected, hardware_muted, row_muted, display_name=None, *,
            icon_color="white"):
    """Return tray state; either hardware or matrix mute means not captured."""
    if icon_color not in icons.TRAY_ICON_COLORS:
        icon_color = icons.TRAY_ICON_COLORS[0]
    live_icon = f"openwave-{icon_color}"
    if not connected:
        return {
            "icon": live_icon,
            "status": "Active",
            "tooltip": "No device connected",
            "mute_label": "Mute Mic",
            "mute_enabled": False,
            "muted": False,
        }

    muted = bool(hardware_muted or row_muted)
    if muted:
        if hardware_muted and row_muted:
            detail = "Muted (hardware and matrix)"
        elif hardware_muted:
            detail = "Muted (hardware)"
        else:
            detail = "Muted (matrix row)"
    else:
        detail = "Live"

    return {
        "icon": "openwave-red" if muted else live_icon,
        "status": "Active",
        "tooltip": f"{display_name}: {detail}" if display_name else detail,
        "mute_label": "Unmute Mic" if muted else "Mute Mic",
        "mute_enabled": True,
        "muted": muted,
    }


class TrayIcon:
    """StatusNotifierItem with no-argument activate/open/mute/quit callbacks.

    register() returns False if no host can display it. unregister() releases
    both exported objects; call it on application shutdown. set_state accepts
    the selected device's optional display_name and white/black icon_color.
    """

    def __init__(self, on_activate=None, on_mute=None, on_quit=None,
                 on_open=None):
        self._on_activate = on_activate
        # Separate from on_activate: clicking the icon may toggle, but the
        # menu item reads "Open OpenWave" and must open. It is also the only
        # way back to a window that was started hidden.
        self._on_open = on_open or on_activate
        self._on_mute = on_mute
        self._on_quit = on_quit
        self._bus = None
        self._item_reg_id = None
        self._menu_reg_id = None
        self._revision = 1
        self._menu_items = {}  # id -> properties dict
        # Nothing is known before the first poll, and "no device" is the
        # honest reading of that -- not "live", which would be a guess in the
        # one direction this icon must never guess.
        self._state = compute(False, False, False)

    @staticmethod
    def host_available(bus=None):
        """True when something on this session bus will actually draw us.

        Asked before anything is allowed to depend on the tray existing.
        GNOME ships no StatusNotifier host of its own -- the watcher name
        appears only when an AppIndicator extension is installed -- so on a
        stock GNOME desktop a tray icon is registered successfully and drawn
        nowhere, which is indistinguishable from working right up until the
        window is hidden into it.
        """
        try:
            bus = bus or Gio.bus_get_sync(Gio.BusType.SESSION, None)
            reply = bus.call_sync(
                "org.freedesktop.DBus", "/org/freedesktop/DBus",
                "org.freedesktop.DBus", "NameHasOwner",
                GLib.Variant("(s)", ("org.kde.StatusNotifierWatcher",)),
                GLib.VariantType.new("(b)"), Gio.DBusCallFlags.NONE, 2000,
                None,
            )
            if not reply.unpack()[0]:
                return False
            reply = bus.call_sync(
                "org.kde.StatusNotifierWatcher", "/StatusNotifierWatcher",
                "org.freedesktop.DBus.Properties", "Get",
                GLib.Variant("(ss)", (
                    "org.kde.StatusNotifierWatcher",
                    "IsStatusNotifierHostRegistered")),
                GLib.VariantType.new("(v)"), Gio.DBusCallFlags.NONE, 2000,
                None,
            )
        except GLib.Error:
            return False
        return bool(reply.unpack()[0])

    def register(self):
        """Publish the tray item only when a host is registered to draw it."""
        self.unregister()
        try:
            bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            if not self.host_available(bus):
                return False
            self._bus = bus
            self._build_menu_items()
            menu_info = Gio.DBusNodeInfo.new_for_xml(MENU_XML)
            self._menu_reg_id = bus.register_object(
                "/MenuBar", menu_info.interfaces[0],
                self._on_menu_call, self._on_menu_get_property, None)
            item_info = Gio.DBusNodeInfo.new_for_xml(ITEM_XML)
            self._item_reg_id = bus.register_object(
                "/StatusNotifierItem", item_info.interfaces[0],
                self._on_item_call, self._on_item_get_property, None)
            bus.call_sync(
                "org.kde.StatusNotifierWatcher", "/StatusNotifierWatcher",
                "org.kde.StatusNotifierWatcher", "RegisterStatusNotifierItem",
                GLib.Variant("(s)", (bus.get_unique_name(),)),
                None, Gio.DBusCallFlags.NONE, 2000, None)
            return True
        except GLib.Error:
            self.unregister()
            return False

    def unregister(self):
        """Release exports, including partially completed registration."""
        bus, self._bus = self._bus, None
        for attr in ("_item_reg_id", "_menu_reg_id"):
            registration = getattr(self, attr)
            setattr(self, attr, None)
            if bus is not None and registration is not None:
                bus.unregister_object(registration)

    @staticmethod
    def _invoke(callback):
        if callback is not None:
            callback()
        return False

    def set_state(self, connected, hardware_muted=False, row_muted=False,
                  display_name=None, *, icon_color="white"):
        """Show what the microphone is actually doing. Returns True if it moved.

        Announced only on a real change: the poll behind this runs at 10 Hz,
        and a host redraws on every NewIcon it is handed.
        """
        new = compute(connected, hardware_muted, row_muted, display_name,
                      icon_color=icon_color)
        if new == self._state:
            return False

        icon_changed = new["icon"] != self._state["icon"]
        tooltip_changed = new["tooltip"] != self._state["tooltip"]
        menu_changed = (
            new["mute_label"] != self._state["mute_label"]
            or new["mute_enabled"] != self._state["mute_enabled"]
        )
        self._state = new
        self._build_menu_items()

        if self._bus is None:
            return True  # not registered yet; the values are already right
        if icon_changed:
            self._emit_item("NewIcon", None)
        if tooltip_changed:
            self._emit_item("NewToolTip", None)
        if menu_changed:
            self._emit_menu_properties(2)
        return True

    def _emit_item(self, name, params):
        """A host that has gone away must not take the application with it."""
        try:
            self._bus.emit_signal(
                None, "/StatusNotifierItem", "org.kde.StatusNotifierItem",
                name, params)
        except GLib.Error as e:
            logging.debug("tray: could not emit %s: %s", name, e)

    def _emit_menu_properties(self, item_id):
        props = self._menu_items.get(item_id, {})
        try:
            self._bus.emit_signal(
                None, "/MenuBar", "com.canonical.dbusmenu",
                "ItemsPropertiesUpdated",
                GLib.Variant("(a(ia{sv})a(ias))", ([(item_id, props)], [])))
        except GLib.Error as e:
            logging.debug("tray: could not emit ItemsPropertiesUpdated: %s", e)

    def _on_item_call(self, conn, sender, path, iface, method, params, invocation):
        invocation.return_value(None)
        if method == "Activate":
            GLib.idle_add(self._invoke, self._on_activate)
        elif method == "SecondaryActivate":
            GLib.idle_add(self._invoke, self._on_mute)
        elif method == "ContextMenu":
            GLib.idle_add(self._invoke, self._on_open)

    def _on_item_get_property(self, conn, sender, path, iface, prop):
        if prop == "IconThemePath":
            return GLib.Variant("s", icons.theme_path(self._state["icon"]))
        props = {
            "Category": GLib.Variant("s", "Hardware"),
            "Id": GLib.Variant("s", "openwave"),
            "Title": GLib.Variant("s", "OpenWave"),
            "Status": GLib.Variant("s", self._state["status"]),
            "IconName": GLib.Variant("s", self._state["icon"]),
            "ToolTip": GLib.Variant(
                "(sa(iiay)ss)",
                ("", [], "OpenWave", self._state["tooltip"])),
            "Menu": GLib.Variant("o", "/MenuBar"),
            "ItemIsMenu": GLib.Variant("b", False),
        }
        return props.get(prop)

    def _build_menu_items(self):
        """Build the menu item tree and cache properties by id."""
        self._menu_items = {
            0: {"children-display": GLib.Variant("s", "submenu")},
            1: {
                "label": GLib.Variant("s", "Open OpenWave"),
                "visible": GLib.Variant("b", True),
                "enabled": GLib.Variant("b", True),
                "icon-name": GLib.Variant("s", "openwave"),
            },
            2: {
                "label": GLib.Variant("s", self._state["mute_label"]),
                "visible": GLib.Variant("b", True),
                "enabled": GLib.Variant("b", self._state["mute_enabled"]),
                "icon-name": GLib.Variant(
                    "s", icons.resolve("microphone-sensitivity-muted-symbolic")),
            },
            3: {
                "type": GLib.Variant("s", "separator"),
                "visible": GLib.Variant("b", True),
            },
            4: {
                "label": GLib.Variant("s", "Quit"),
                "visible": GLib.Variant("b", True),
                "enabled": GLib.Variant("b", True),
                "icon-name": GLib.Variant("s", "application-exit-symbolic"),
            },
        }

    def _make_layout(self, item_id, depth):
        """Build a (ia{sv}av) variant for an item, recursing into children."""
        props = self._menu_items.get(item_id, {})
        children = []
        if item_id == 0 and depth != 0:
            for child_id in [1, 2, 3, 4]:
                child = self._make_layout(child_id, depth - 1 if depth > 0 else -1)
                children.append(child)
        return GLib.Variant("(ia{sv}av)", (item_id, props, children))

    def _on_menu_call(self, conn, sender, path, iface, method, params, invocation):
        if method == "GetLayout":
            parent_id = params[0]
            depth = params[1]
            # propertyNames (params[2]) is ignored — we always return all properties
            layout = self._make_layout(parent_id, depth)
            ret = GLib.Variant.new_tuple(GLib.Variant("u", self._revision), layout)
            invocation.return_value(ret)

        elif method == "GetGroupProperties":
            ids = params[0]
            # propertyNames (params[1]) ignored — return all
            result = []
            for item_id in ids:
                props = self._menu_items.get(item_id, {})
                result.append((item_id, props))
            invocation.return_value(GLib.Variant("(a(ia{sv}))", (result,)))

        elif method == "GetProperty":
            item_id = params[0]
            prop_name = params[1]
            props = self._menu_items.get(item_id, {})
            val = props.get(prop_name, GLib.Variant("s", ""))
            invocation.return_value(GLib.Variant("(v)", (val,)))

        elif method == "Event":
            item_id = params[0]
            event_id = params[1]
            invocation.return_value(None)
            if event_id == "clicked":
                callback = {1: self._on_open, 4: self._on_quit}.get(item_id)
                if item_id == 2 and self._state["mute_enabled"]:
                    callback = self._on_mute
                if callback is not None:
                    GLib.idle_add(self._invoke, callback)

        elif method == "AboutToShow":
            invocation.return_value(GLib.Variant("(b)", (False,)))

        elif method == "AboutToShowGroup":
            invocation.return_value(GLib.Variant("(aiai)", ([], [])))

        else:
            invocation.return_value(None)

    def _on_menu_get_property(self, conn, sender, path, iface, prop):
        if prop == "Version":
            return GLib.Variant("u", 3)
        if prop == "TextDirection":
            return GLib.Variant("s", "ltr")
        if prop == "Status":
            return GLib.Variant("s", "normal")
        if prop == "IconThemePath":
            theme_path = icons.theme_path("openwave")
            return GLib.Variant("as", [theme_path] if theme_path else [])
        return None
