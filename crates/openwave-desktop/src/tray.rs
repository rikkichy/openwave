use crate::icons::Icons;
use glib::{
    Variant,
    variant::{ObjectPath, StaticVariantType, ToVariant},
};
use openwave_core::model::{AppSnapshot, Lifecycle, OperationError, Result, UnitId, UnitSnapshot};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM: &str = "org.kde.StatusNotifierItem";
const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU: &str = "com.canonical.dbusmenu";
const MENU_PATH: &str = "/MenuBar";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

const ITEM_XML: &str = r#"<node><interface name="org.kde.StatusNotifierItem">
<property name="Category" type="s" access="read"/>
<property name="Id" type="s" access="read"/>
<property name="Title" type="s" access="read"/>
<property name="Status" type="s" access="read"/>
<property name="IconName" type="s" access="read"/>
<property name="IconThemePath" type="s" access="read"/>
<property name="ToolTip" type="(sa(iiay)ss)" access="read"/>
<property name="Menu" type="o" access="read"/>
<property name="ItemIsMenu" type="b" access="read"/>
<method name="Activate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
<method name="SecondaryActivate"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
<method name="ContextMenu"><arg name="x" type="i" direction="in"/><arg name="y" type="i" direction="in"/></method>
<signal name="NewIcon"/><signal name="NewToolTip"/>
<signal name="NewStatus"><arg name="status" type="s" direction="out"/></signal>
</interface></node>"#;

const MENU_XML: &str = r#"<node><interface name="com.canonical.dbusmenu">
<property name="Version" type="u" access="read"/>
<property name="TextDirection" type="s" access="read"/>
<property name="Status" type="s" access="read"/>
<property name="IconThemePath" type="as" access="read"/>
<method name="GetLayout">
<arg name="parentId" type="i" direction="in"/><arg name="recursionDepth" type="i" direction="in"/><arg name="propertyNames" type="as" direction="in"/>
<arg name="revision" type="u" direction="out"/><arg name="layout" type="(ia{sv}av)" direction="out"/>
</method>
<method name="GetGroupProperties"><arg name="ids" type="ai" direction="in"/><arg name="propertyNames" type="as" direction="in"/><arg name="properties" type="a(ia{sv})" direction="out"/></method>
<method name="GetProperty"><arg name="id" type="i" direction="in"/><arg name="name" type="s" direction="in"/><arg name="value" type="v" direction="out"/></method>
<method name="Event"><arg name="id" type="i" direction="in"/><arg name="eventId" type="s" direction="in"/><arg name="data" type="v" direction="in"/><arg name="timestamp" type="u" direction="in"/></method>
<method name="AboutToShow"><arg name="id" type="i" direction="in"/><arg name="needUpdate" type="b" direction="out"/></method>
<method name="AboutToShowGroup"><arg name="ids" type="ai" direction="in"/><arg name="updatesNeeded" type="ai" direction="out"/><arg name="idErrors" type="ai" direction="out"/></method>
<signal name="ItemsPropertiesUpdated"><arg name="updatedProps" type="a(ia{sv})" direction="out"/><arg name="removedProps" type="a(ias)" direction="out"/></signal>
<signal name="LayoutUpdated"><arg name="revision" type="u"/><arg name="parent" type="i"/></signal>
<signal name="ItemActivationRequested"><arg name="id" type="i"/><arg name="timestamp" type="u"/></signal>
</interface></node>"#;

type Properties = BTreeMap<String, Variant>;

pub struct TrayCallbacks {
    pub open: Rc<dyn Fn()>,
    pub toggle_mute: Rc<dyn Fn(UnitId)>,
    pub quit: Rc<dyn Fn()>,
    pub host_changed: Rc<dyn Fn(bool)>,
}

pub struct Tray {
    inner: Rc<Inner>,
}

struct Inner {
    connection: gio::DBusConnection,
    icons: Rc<Icons>,
    callbacks: TrayCallbacks,
    item_info: gio::DBusInterfaceInfo,
    menu_info: gio::DBusInterfaceInfo,
    menu_path: ObjectPath,
    state: RefCell<Presentation>,
    inputs: RefCell<Option<PresentationInputs>>,
    menu: RefCell<BTreeMap<i32, Properties>>,
    // gio 0.22 exposes two distinct WatcherId types under one root name.
    watch: RefCell<Option<Box<dyn FnOnce()>>>,
    signals: RefCell<Vec<gio::SignalSubscription>>,
    owner: RefCell<Option<String>>,
    exports: RefCell<Vec<gio::RegistrationId>>,
    active: Cell<bool>,
    known: Cell<bool>,
    stopped: Cell<bool>,
    generation: Cell<u64>,
    query: Cell<u64>,
}

#[derive(PartialEq, Eq)]
struct Presentation {
    icon: String,
    tooltip: String,
    label: String,
    selected: Option<UnitId>,
    enabled: bool,
}

fn effective_mute(unit: &UnitSnapshot) -> Option<bool> {
    unit.desired_mute
        .or_else(|| unit.state.known().map(|state| state.muted))
}

/// Keep meter-only snapshots allocation-free on the tray path.
struct PresentationInputs {
    units: Vec<(UnitId, Option<bool>, String)>,
    selected: Option<UnitId>,
    black: bool,
    writable: bool,
}

impl PresentationInputs {
    fn matches(&self, snapshot: &AppSnapshot) -> bool {
        self.selected == snapshot.selected_unit
            && self.black == (snapshot.preferences.tray_icon_color == "black")
            && self.writable
                == matches!(snapshot.lifecycle, Lifecycle::Starting | Lifecycle::Running)
            && self.units.len() == snapshot.units.len()
            && self
                .units
                .iter()
                .zip(snapshot.units.iter())
                .all(|((id, mute, serial), unit)| {
                    *id == unit.id && *mute == effective_mute(unit) && *serial == unit.info.serial
                })
    }

    fn capture(snapshot: &AppSnapshot) -> Self {
        Self {
            units: snapshot
                .units
                .iter()
                .map(|unit| (unit.id, effective_mute(unit), unit.info.serial.clone()))
                .collect(),
            selected: snapshot.selected_unit,
            black: snapshot.preferences.tray_icon_color == "black",
            writable: matches!(snapshot.lifecycle, Lifecycle::Starting | Lifecycle::Running),
        }
    }
}

impl Presentation {
    fn from_snapshot(snapshot: &AppSnapshot) -> Self {
        let selected = snapshot
            .selected_unit
            .and_then(|id| snapshot.units.iter().find(|unit| unit.id == id));
        let muted_count = snapshot
            .units
            .iter()
            .filter(|unit| effective_mute(unit) == Some(true))
            .count();
        let color = if snapshot.preferences.tray_icon_color == "black" {
            "black"
        } else {
            "white"
        };
        let icon = if muted_count > 0 {
            "openwave-red".into()
        } else {
            format!("openwave-{color}")
        };
        let (mut tooltip, label, enabled) = if let Some(unit) = selected {
            let profile = unit.id.profile.profile().display_name;
            let name = if unit.info.serial.is_empty() {
                format!("{profile} (USB {}:{})", unit.id.bus, unit.id.address)
            } else {
                format!("{profile} ({})", unit.info.serial)
            };
            let muted = effective_mute(unit);
            let detail = match muted {
                Some(true) => "Muted",
                Some(false) => "Live",
                None => "Status unavailable",
            };
            (
                format!("Selected {name}: {detail}"),
                format!(
                    "{} {name}",
                    if muted == Some(true) {
                        "Unmute"
                    } else {
                        "Mute"
                    }
                ),
                muted.is_some(),
            )
        } else {
            (
                if snapshot.units.is_empty() {
                    "No device connected".into()
                } else {
                    "No device selected".into()
                },
                "Mute Mic (no device selected)".into(),
                false,
            )
        };
        let others = muted_count.saturating_sub(usize::from(
            selected.is_some_and(|unit| effective_mute(unit) == Some(true)),
        ));
        if others > 0 {
            tooltip.push_str(&format!(
                "\n{others} other connected {} muted",
                if others == 1 {
                    "device is"
                } else {
                    "devices are"
                }
            ));
        }
        Self {
            icon,
            tooltip,
            label,
            selected: selected.map(|unit| unit.id),
            enabled: enabled
                && matches!(snapshot.lifecycle, Lifecycle::Starting | Lifecycle::Running),
        }
    }
}

impl Tray {
    pub fn new(
        connection: gio::DBusConnection,
        icons: Rc<Icons>,
        callbacks: TrayCallbacks,
    ) -> Result<Self> {
        let item_info = gio::DBusNodeInfo::for_xml(ITEM_XML)
            .map_err(bus_error)?
            .lookup_interface(ITEM)
            .ok_or_else(|| OperationError::invalid("Missing tray interface"))?;
        let menu_info = gio::DBusNodeInfo::for_xml(MENU_XML)
            .map_err(bus_error)?
            .lookup_interface(MENU)
            .ok_or_else(|| OperationError::invalid("Missing menu interface"))?;
        let menu_path = ObjectPath::try_from(MENU_PATH)
            .map_err(|error| OperationError::invalid(error.to_string()))?;
        let inner = Rc::new(Inner {
            connection,
            icons,
            callbacks,
            item_info,
            menu_info,
            menu_path,
            state: RefCell::new(Presentation::from_snapshot(&AppSnapshot::default())),
            menu: RefCell::new(BTreeMap::new()),
            watch: RefCell::new(None),
            signals: RefCell::new(Vec::new()),
            owner: RefCell::new(None),
            exports: RefCell::new(Vec::new()),
            active: Cell::new(false),
            known: Cell::new(false),
            stopped: Cell::new(false),
            generation: Cell::new(0),
            query: Cell::new(0),
            inputs: RefCell::new(None),
        });
        inner.rebuild_menu();
        let appeared = Rc::downgrade(&inner);
        let vanished = Rc::downgrade(&inner);
        let watch = gio::bus_watch_name_on_connection(
            &inner.connection,
            WATCHER,
            gio::BusNameWatcherFlags::NONE,
            move |_, _, owner| {
                if let Some(inner) = appeared.upgrade() {
                    inner.watcher_appeared(owner);
                }
            },
            move |_, _| {
                if let Some(inner) = vanished.upgrade() {
                    inner.watcher_vanished();
                }
            },
        );
        *inner.watch.borrow_mut() = Some(Box::new(move || gio::bus_unwatch_name(watch)));
        Ok(Self { inner })
    }

    pub fn host_active(&self) -> bool {
        self.inner.active.get()
    }
    pub fn host_known(&self) -> bool {
        self.inner.known.get()
    }

    pub fn update(&self, snapshot: &AppSnapshot) {
        if self.inner.stopped.get()
            || self
                .inner
                .inputs
                .borrow()
                .as_ref()
                .is_some_and(|inputs| inputs.matches(snapshot))
        {
            return;
        }
        *self.inner.inputs.borrow_mut() = Some(PresentationInputs::capture(snapshot));
        let next = Presentation::from_snapshot(snapshot);
        let (icon, tooltip, menu) = {
            let previous = self.inner.state.borrow();
            if *previous == next {
                return;
            }
            (
                previous.icon != next.icon,
                previous.tooltip != next.tooltip,
                previous.label != next.label || previous.enabled != next.enabled,
            )
        };
        *self.inner.state.borrow_mut() = next;
        if menu {
            self.inner.rebuild_menu();
        }
        if !self.host_active() {
            return;
        }
        if icon {
            self.inner.emit(ITEM_PATH, ITEM, "NewIcon", None);
        }
        if tooltip {
            self.inner.emit(ITEM_PATH, ITEM, "NewToolTip", None);
        }
        if menu {
            let properties = self
                .inner
                .menu
                .borrow()
                .get(&2)
                .cloned()
                .unwrap_or_default();
            self.inner.emit(
                MENU_PATH,
                MENU,
                "ItemsPropertiesUpdated",
                Some(&(vec![(2_i32, properties)], Vec::<(i32, Vec<String>)>::new()).to_variant()),
            );
        }
    }

    pub fn shutdown(&self) {
        if self.inner.stopped.replace(true) {
            return;
        }
        if let Some(unwatch) = self.inner.watch.borrow_mut().take() {
            unwatch();
        }
        self.inner.watcher_vanished();
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn bus_error(error: glib::Error) -> OperationError {
    OperationError::unavailable(error.to_string())
}

impl Inner {
    fn watcher_appeared(self: &Rc<Self>, owner: &str) {
        if self.stopped.get() {
            return;
        }
        self.signals.borrow_mut().clear();
        self.owner.borrow_mut().take();
        self.query.set(self.query.get().wrapping_add(1));
        self.deactivate_pending();
        *self.owner.borrow_mut() = Some(owner.to_owned());
        for interface in [WATCHER, PROPERTIES] {
            let weak = Rc::downgrade(self);
            let subscription = self.connection.subscribe_to_signal(
                Some(owner),
                Some(interface),
                None,
                Some(WATCHER_PATH),
                None,
                gio::DBusSignalFlags::NONE,
                move |signal| {
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    if inner.stopped.get() {
                        return;
                    }
                    match signal.signal_name {
                        "StatusNotifierHostRegistered" => inner.query_host(),
                        "StatusNotifierHostUnregistered" => {
                            inner.deactivate();
                            inner.query_host();
                        }
                        "PropertiesChanged" => {
                            if let Some((interface, changes, invalidated)) =
                                signal.parameters.get::<(String, Properties, Vec<String>)>()
                            {
                                if interface != WATCHER {
                                    return;
                                }
                                if changes.contains_key("IsStatusNotifierHostRegistered")
                                    || invalidated
                                        .iter()
                                        .any(|name| name == "IsStatusNotifierHostRegistered")
                                {
                                    if changes
                                        .get("IsStatusNotifierHostRegistered")
                                        .and_then(|value| value.get::<bool>())
                                        != Some(true)
                                    {
                                        inner.deactivate();
                                    }
                                    inner.query_host();
                                }
                            }
                        }
                        _ => {}
                    }
                },
            );
            self.signals.borrow_mut().push(subscription);
        }
        self.query_host();
    }

    fn watcher_vanished(&self) {
        self.signals.borrow_mut().clear();
        self.owner.borrow_mut().take();
        self.query.set(self.query.get().wrapping_add(1));
        self.deactivate();
    }

    fn deactivate_pending(&self) {
        self.known.set(false);
        self.generation.set(self.generation.get().wrapping_add(1));
        let registrations = self.exports.take();
        for registration in registrations {
            if let Err(error) = self.connection.unregister_object(registration) {
                log::debug!("Unregistering tray: {error}");
            }
        }
        if self.active.replace(false) {
            (self.callbacks.host_changed)(false);
        }
    }
    fn deactivate(&self) {
        let was_known = self.known.get();
        let was_active = self.active.get();
        self.deactivate_pending();
        self.known.set(true);
        if !was_known && !was_active {
            (self.callbacks.host_changed)(false);
        }
    }

    fn query_host(self: &Rc<Self>) {
        let Some(owner) = self.owner.borrow().clone() else {
            return;
        };
        let query = self.query.get().wrapping_add(1);
        self.query.set(query);
        let weak = Rc::downgrade(self);
        self.connection.call(
            Some(&owner),
            WATCHER_PATH,
            PROPERTIES,
            "Get",
            Some(&(WATCHER, "IsStatusNotifierHostRegistered").to_variant()),
            Some(&<(Variant,)>::static_variant_type()),
            gio::DBusCallFlags::NO_AUTO_START,
            2000,
            None::<&gio::Cancellable>,
            move |reply| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if inner.stopped.get() || inner.query.get() != query {
                    return;
                }
                let available = reply
                    .ok()
                    .and_then(|value| value.get::<(Variant,)>())
                    .and_then(|(value,)| value.get::<bool>())
                    == Some(true);
                if available {
                    inner.register();
                } else {
                    inner.deactivate();
                }
            },
        );
    }

    fn register(self: &Rc<Self>) {
        if self.stopped.get() || !self.exports.borrow().is_empty() {
            return;
        }
        let Some(owner) = self.owner.borrow().clone() else {
            return;
        };
        if let Err(error) = self.export_objects() {
            log::warn!("Exporting tray: {error}");
            self.deactivate();
            return;
        }
        let generation = self.generation.get();
        let weak = Rc::downgrade(self);
        let Some(name) = self.connection.unique_name() else {
            log::warn!("Tray registration requires a session bus connection");
            self.deactivate();
            return;
        };
        self.connection.call(
            Some(&owner),
            WATCHER_PATH,
            WATCHER,
            "RegisterStatusNotifierItem",
            Some(&(name.as_str(),).to_variant()),
            Some(&<()>::static_variant_type()),
            gio::DBusCallFlags::NO_AUTO_START,
            2000,
            None::<&gio::Cancellable>,
            move |reply| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if inner.stopped.get() || inner.generation.get() != generation {
                    return;
                }
                match reply {
                    Ok(_) => {
                        inner.known.set(true);
                        if !inner.active.replace(true) {
                            (inner.callbacks.host_changed)(true);
                        }
                    }
                    Err(error) => {
                        log::debug!("Registering tray host: {error}");
                        inner.deactivate();
                    }
                }
            },
        );
    }

    fn export_objects(self: &Rc<Self>) -> Result<()> {
        let weak_method = Rc::downgrade(self);
        let weak_property = Rc::downgrade(self);
        let registration = self
            .connection
            .register_object(MENU_PATH, &self.menu_info)
            .method_call(move |_, _, _, _, method, params, invocation| {
                if let Some(inner) = weak_method.upgrade() {
                    inner.menu_call(method, &params, invocation);
                } else {
                    invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.Disconnected",
                        "Tray retired",
                    );
                }
            })
            .property(move |_, _, _, _, name| {
                weak_property
                    .upgrade()
                    .map(|inner| inner.menu_property(name))
                    .unwrap_or_else(|| "".to_variant())
            })
            .build()
            .map_err(bus_error)?;
        self.exports.borrow_mut().push(registration);
        let weak_method = Rc::downgrade(self);
        let weak_property = Rc::downgrade(self);
        let registration = self
            .connection
            .register_object(ITEM_PATH, &self.item_info)
            .method_call(move |_, _, _, _, method, _, invocation| {
                let Some(inner) = weak_method.upgrade() else {
                    invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.Disconnected",
                        "Tray retired",
                    );
                    return;
                };
                let item = match method {
                    "Activate" | "ContextMenu" => 1,
                    "SecondaryActivate" => 2,
                    _ => {
                        invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.UnknownMethod",
                            "Unknown tray method",
                        );
                        return;
                    }
                };
                invocation.return_value(None);
                inner.dispatch(item);
            })
            .property(move |_, _, _, _, name| {
                weak_property
                    .upgrade()
                    .map(|inner| inner.item_property(name))
                    .unwrap_or_else(|| "".to_variant())
            })
            .build()
            .map_err(bus_error)?;
        self.exports.borrow_mut().push(registration);
        Ok(())
    }

    fn dispatch(&self, item: i32) {
        if self.stopped.get() {
            return;
        }
        match item {
            1 => {
                let callback = self.callbacks.open.clone();
                glib::idle_add_local_once(move || callback());
            }
            2 => {
                let selected = {
                    let state = self.state.borrow();
                    if state.enabled { state.selected } else { None }
                };
                if let Some(unit) = selected {
                    let callback = self.callbacks.toggle_mute.clone();
                    glib::idle_add_local_once(move || callback(unit));
                }
            }
            4 => {
                let callback = self.callbacks.quit.clone();
                glib::idle_add_local_once(move || callback());
            }
            _ => {}
        }
    }

    fn emit(&self, path: &str, interface: &str, name: &str, parameters: Option<&Variant>) {
        if let Err(error) = self
            .connection
            .emit_signal(None, path, interface, name, parameters)
        {
            log::debug!("Tray {name}: {error}");
        }
    }

    fn rebuild_menu(&self) {
        let state = self.state.borrow();
        let item = |label: &str, enabled: bool, icon: &str| -> Properties {
            [
                ("label".into(), label.to_variant()),
                ("visible".into(), true.to_variant()),
                ("enabled".into(), enabled.to_variant()),
                ("icon-name".into(), icon.to_variant()),
            ]
            .into()
        };
        *self.menu.borrow_mut() = [
            (
                0,
                [("children-display".into(), "submenu".to_variant())].into(),
            ),
            (1, item("Open OpenWave", true, "openwave")),
            (
                2,
                item(
                    &state.label,
                    state.enabled,
                    &self.icons.resolve("microphone-sensitivity-muted-symbolic"),
                ),
            ),
            (
                3,
                [
                    ("type".into(), "separator".to_variant()),
                    ("visible".into(), true.to_variant()),
                ]
                .into(),
            ),
            (4, item("Quit", true, "application-exit-symbolic")),
        ]
        .into();
    }

    fn item_property(&self, name: &str) -> Variant {
        let state = self.state.borrow();
        match name {
            "Category" => "Hardware".to_variant(),
            "Id" => "openwave".to_variant(),
            "Title" => "OpenWave".to_variant(),
            "Status" => "Active".to_variant(),
            "IconName" => state.icon.to_variant(),
            "IconThemePath" => self.icons.theme_path(&state.icon).to_variant(),
            "ToolTip" => (
                "",
                Vec::<(i32, i32, Vec<u8>)>::new(),
                "OpenWave",
                state.tooltip.as_str(),
            )
                .to_variant(),
            "Menu" => self.menu_path.to_variant(),
            "ItemIsMenu" => false.to_variant(),
            _ => "".to_variant(),
        }
    }

    fn menu_property(&self, name: &str) -> Variant {
        match name {
            "Version" => 3_u32.to_variant(),
            "TextDirection" => "ltr".to_variant(),
            "Status" => "normal".to_variant(),
            "IconThemePath" => {
                let path = self.icons.theme_path("openwave");
                if path.is_empty() {
                    Vec::<String>::new().to_variant()
                } else {
                    vec![path].to_variant()
                }
            }
            _ => "".to_variant(),
        }
    }

    fn properties(&self, id: i32, requested: &[String]) -> Properties {
        self.menu
            .borrow()
            .get(&id)
            .map(|properties| {
                properties
                    .iter()
                    .filter(|(key, _)| requested.is_empty() || requested.contains(key))
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn layout(&self, id: i32, depth: i32, requested: &[String]) -> Variant {
        let children: Vec<Variant> = if id == 0 && depth != 0 {
            (1..=4)
                .map(|child| self.layout(child, 0, requested))
                .collect()
        } else {
            Vec::new()
        };
        (id, self.properties(id, requested), children).to_variant()
    }

    fn menu_call(&self, method: &str, params: &Variant, invocation: gio::DBusMethodInvocation) {
        let reply = match method {
            "GetLayout" => params
                .get::<(i32, i32, Vec<String>)>()
                .map(|(id, depth, requested)| {
                    Variant::tuple_from_iter([
                        1_u32.to_variant(),
                        self.layout(id, depth, &requested),
                    ])
                }),
            "GetGroupProperties" => {
                params
                    .get::<(Vec<i32>, Vec<String>)>()
                    .map(|(ids, requested)| {
                        let ids = if ids.is_empty() {
                            (0..=4).collect()
                        } else {
                            ids
                        };
                        (ids.into_iter()
                            .map(|id| (id, self.properties(id, &requested)))
                            .collect::<Vec<_>>(),)
                            .to_variant()
                    })
            }
            "GetProperty" => params.get::<(i32, String)>().map(|(id, property)| {
                let value = self
                    .menu
                    .borrow()
                    .get(&id)
                    .and_then(|properties| properties.get(&property))
                    .cloned()
                    .unwrap_or_else(|| "".to_variant());
                (value,).to_variant()
            }),
            "Event" => {
                if let Some((id, event, _, _)) = params.get::<(i32, String, Variant, u32)>() {
                    invocation.return_value(None);
                    if event == "clicked" {
                        self.dispatch(id);
                    }
                    return;
                }
                None
            }
            "AboutToShow" => params.get::<(i32,)>().map(|_| (false,).to_variant()),
            "AboutToShowGroup" => params.get::<(Vec<i32>,)>().map(|(ids,)| {
                let errors: Vec<i32> = ids.into_iter().filter(|id| !(0..=4).contains(id)).collect();
                (Vec::<i32>::new(), errors).to_variant()
            }),
            _ => {
                invocation.return_dbus_error(
                    "org.freedesktop.DBus.Error.UnknownMethod",
                    "Unknown menu method",
                );
                return;
            }
        };
        match reply {
            Some(reply) => invocation.return_value(Some(&reply)),
            None => invocation.return_dbus_error(
                "org.freedesktop.DBus.Error.InvalidArgs",
                "Invalid menu arguments",
            ),
        }
    }
}
