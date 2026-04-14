"""StatusNotifierItem tray icon via D-Bus (no GTK3 dependency)."""

from gi.repository import Gio, GLib

ITEM_XML = """
<node>
  <interface name="org.kde.StatusNotifierItem">
    <property name="Category" type="s" access="read"/>
    <property name="Id" type="s" access="read"/>
    <property name="Title" type="s" access="read"/>
    <property name="Status" type="s" access="read"/>
    <property name="IconName" type="s" access="read"/>
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


class TrayIcon:
    """Minimal StatusNotifierItem tray icon."""

    def __init__(self, on_activate=None, on_mute=None, on_quit=None):
        self._on_activate = on_activate
        self._on_mute = on_mute
        self._on_quit = on_quit
        self._bus = None
        self._item_reg_id = None
        self._menu_reg_id = None
        self._name_id = None
        self._revision = 1
        self._menu_items = {}  # id -> properties dict

    def register(self):
        self._bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        self._build_menu_items()

        # Register the menu object
        menu_info = Gio.DBusNodeInfo.new_for_xml(MENU_XML)
        self._menu_reg_id = self._bus.register_object(
            "/MenuBar",
            menu_info.interfaces[0],
            self._on_menu_call,
            self._on_menu_get_property,
            None,
        )

        # Register the SNI object
        item_info = Gio.DBusNodeInfo.new_for_xml(ITEM_XML)
        self._item_reg_id = self._bus.register_object(
            "/StatusNotifierItem",
            item_info.interfaces[0],
            self._on_item_call,
            self._on_item_get_property,
            None,
        )

        # Own a unique bus name for the item
        self._name_id = Gio.bus_own_name_on_connection(
            self._bus,
            "org.kde.StatusNotifierItem-openwave",
            Gio.BusNameOwnerFlags.NONE,
            None, None,
        )

        # Register with the StatusNotifierWatcher
        try:
            self._bus.call_sync(
                "org.kde.StatusNotifierWatcher",
                "/StatusNotifierWatcher",
                "org.kde.StatusNotifierWatcher",
                "RegisterStatusNotifierItem",
                GLib.Variant("(s)", ("org.kde.StatusNotifierItem-openwave",)),
                None,
                Gio.DBusCallFlags.NONE,
                -1, None,
            )
        except Exception:
            pass  # no watcher running — tray won't show but app still works

    def _on_item_call(self, conn, sender, path, iface, method, params, invocation):
        if method == "Activate":
            if self._on_activate:
                self._on_activate()
        invocation.return_value(None)

    def _on_item_get_property(self, conn, sender, path, iface, prop):
        props = {
            "Category": GLib.Variant("s", "Hardware"),
            "Id": GLib.Variant("s", "openwave"),
            "Title": GLib.Variant("s", "OpenWave"),
            "Status": GLib.Variant("s", "Active"),
            "IconName": GLib.Variant("s", "audio-input-microphone-symbolic"),
            "ToolTip": GLib.Variant("(sa(iiay)ss)", ("", [], "OpenWave", "Elgato Wave XLR Control")),
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
                "icon-name": GLib.Variant("s", "audio-input-microphone-symbolic"),
            },
            2: {
                "label": GLib.Variant("s", "Mute Mic"),
                "visible": GLib.Variant("b", True),
                "enabled": GLib.Variant("b", True),
                "icon-name": GLib.Variant("s", "microphone-sensitivity-muted-symbolic"),
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
            if event_id == "clicked":
                if item_id == 1 and self._on_activate:
                    self._on_activate()
                elif item_id == 2 and self._on_mute:
                    self._on_mute()
                elif item_id == 4 and self._on_quit:
                    self._on_quit()
            invocation.return_value(None)

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
            return GLib.Variant("as", [])
        return None
