use super::dialogs;
use crate::{Submit, icons::Icons};
use adw::prelude::*;
use gtk::{gdk, glib};
use openwave_core::{
    effects::FxSettings,
    model::*,
    routing::{OutputDecision, claim_streams, eligible_output, resolve_output},
};
use openwave_runtime::controller::{AppCommand, EditTiming};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::Arc,
};

type Latest = Rc<RefCell<Arc<AppSnapshot>>>;

pub struct MatrixView {
    pub widget: gtk::Box,
    state: Rc<MatrixState>,
}
struct MatrixState {
    grid: gtk::Grid,
    icons: Rc<Icons>,
    submit: Submit,
    latest: Latest,
    updating: Rc<Cell<bool>>,
    rows: RefCell<HashMap<SourceId, SourceRow>>,
    headers: RefCell<HashMap<MixId, MixHeader>>,
    cells: RefCell<HashMap<(SourceId, MixId), Fader>>,
    source_order: RefCell<Vec<SourceId>>,
    mix_order: RefCell<Vec<MixId>>,
}
struct SourceRow {
    widget: gtk::Box,
    icon: gtk::Box,
    title: gtk::Label,
    group: gtk::Label,
    status: gtk::Label,
    switch: gtk::Button,
    remove: gtk::Button,
    leave: gtk::Button,
    fader: Fader,
    meter: gtk::LevelBar,
    fx: Option<FxControls>,
    meter_key: String,
}
struct MixHeader {
    icon: gtk::Box,
    title: gtk::Label,
    subtitle: gtk::Label,
    output: gtk::Label,
    fader: Fader,
    meter: gtk::LevelBar,
    output_list: gtk::ListBox,
    output_names: Rc<RefCell<Vec<String>>>,
    output_entries: RefCell<Vec<(String, String)>>,
    menu: gtk::MenuButton,
    remove: gtk::Button,
    delete_hint: gtk::Label,
    meter_key: String,
}
#[derive(Clone)]
struct Fader {
    widget: gtk::Box,
    scale: gtk::Scale,
    mute: gtk::ToggleButton,
    percent: gtk::Label,
    capture: bool,
}
impl Fader {
    fn new(capture: bool, label: &str) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let mute = gtk::ToggleButton::builder()
            .valign(gtk::Align::Center)
            .build();
        mute.add_css_class("flat");
        mute.add_css_class("circular");
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
        scale.set_draw_value(false);
        scale.set_round_digits(2);
        scale.set_hexpand(true);
        scale.set_valign(gtk::Align::Center);
        scale.add_css_class("openwave-mix-slider");
        scale.set_tooltip_text(Some(label));
        scale.update_property(&[gtk::accessible::Property::Label(label)]);
        let percent = gtk::Label::builder()
            .label("0%")
            .xalign(1.0)
            .width_chars(4)
            .build();
        for class in ["caption", "dim-label", "monospace"] {
            percent.add_css_class(class);
        }
        widget.append(&mute);
        widget.append(&scale);
        widget.append(&percent);
        Self {
            widget,
            scale,
            mute,
            percent,
            capture,
        }
    }
    fn render(&self, level: f64, muted: bool) {
        self.scale.set_value(level);
        self.mute.set_active(muted);
        self.percent.set_label(&format!("{:.0}%", level * 100.0));
        self.mute.set_icon_name(if self.capture {
            if muted {
                "microphone-sensitivity-muted-symbolic"
            } else {
                "audio-input-microphone-symbolic"
            }
        } else if muted {
            "audio-volume-muted-symbolic"
        } else {
            "audio-volume-high-symbolic"
        });
        self.mute
            .set_tooltip_text(Some(if muted { "Unmute" } else { "Mute" }));
        css(&self.widget, "openwave-muted", muted);
    }
}
fn css(widget: &impl IsA<gtk::Widget>, class: &str, enabled: bool) {
    if enabled {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}
fn reserve(widget: &impl IsA<gtk::Widget>, visible: bool) {
    widget.set_opacity(if visible { 1.0 } else { 0.0 });
    widget.set_sensitive(visible);
    widget.set_can_target(visible);
}
fn meter(width: i32, height: i32) -> gtk::LevelBar {
    let meter = gtk::LevelBar::for_interval(0.0, 1.0);
    meter.set_size_request(width, height);
    meter.set_valign(gtk::Align::Center);
    meter.add_css_class("openwave-level");
    meter.add_offset_value("low", 0.70);
    meter.add_offset_value("high", 0.90);
    meter.add_offset_value("full", 1.0);
    meter
}
fn label(text: &str, width: i32, class: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .width_chars(width)
        .max_width_chars(width)
        .build();
    if !class.is_empty() {
        label.add_css_class(class);
    }
    label
}
fn padding(widget: &impl IsA<gtk::Widget>, horizontal: i32, vertical: i32) {
    widget.set_margin_start(horizontal);
    widget.set_margin_end(horizontal);
    widget.set_margin_top(vertical);
    widget.set_margin_bottom(vertical);
}
fn button(icons: &Icons, icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .valign(gtk::Align::Center)
        .tooltip_text(tooltip)
        .build();
    button.set_child(Some(&icons.image(icon, 16)));
    button.add_css_class("flat");
    button.add_css_class("circular");
    button
}
fn menu_button(
    pop: &gtk::Popover,
    body: &gtk::Box,
    title: &str,
    callback: impl Fn() + 'static,
) -> gtk::Button {
    let button = gtk::Button::with_label(title);
    button.add_css_class("flat");
    let weak = pop.downgrade();
    let callback: Rc<dyn Fn()> = Rc::new(callback);
    button.connect_clicked(move |_| {
        if let Some(pop) = weak.upgrade() {
            pop.popdown();
        }
        let callback = callback.clone();
        glib::idle_add_local_once(move || callback());
    });
    body.append(&button);
    button
}
fn parent(widget: &impl IsA<gtk::Widget>) -> Option<gtk::Window> {
    widget.root()?.downcast::<gtk::Window>().ok()
}

impl MatrixView {
    pub fn new(icons: Rc<Icons>, submit: Submit) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 0);
        widget.add_css_class("openwave-matrix");
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hexpand(true)
            .build();
        widget.append(&scroll);
        let wrapper = gtk::Box::new(gtk::Orientation::Vertical, 10);
        scroll.set_child(Some(&wrapper));
        let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        padding(&add_row, 12, 12);
        wrapper.append(&add_row);
        let grid = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(6)
            .margin_start(12)
            .margin_end(12)
            .margin_top(12)
            .margin_bottom(12)
            .build();
        wrapper.append(&grid);
        let state = Rc::new(MatrixState {
            grid,
            icons,
            submit,
            latest: Rc::new(RefCell::new(Arc::new(AppSnapshot::default()))),
            updating: Rc::new(Cell::new(false)),
            rows: RefCell::new(HashMap::new()),
            headers: RefCell::new(HashMap::new()),
            cells: RefCell::new(HashMap::new()),
            source_order: RefCell::new(Vec::new()),
            mix_order: RefCell::new(Vec::new()),
        });
        for (title, class, source) in [
            ("+ Add Source", "openwave-add-source", true),
            ("+ Add Mix", "openwave-add-mix", false),
        ] {
            let button = gtk::Button::with_label(title);
            button.add_css_class(class);
            add_row.append(&button);
            let weak = Rc::downgrade(&state);
            button.connect_clicked(move |button| {
                if let (Some(state), Some(parent)) = (weak.upgrade(), parent(button)) {
                    let snapshot = state.latest.borrow().clone();
                    if source {
                        dialogs::add_source(
                            &parent,
                            snapshot,
                            state.icons.clone(),
                            state.submit.clone(),
                        );
                    } else {
                        dialogs::add_mix(
                            &parent,
                            snapshot,
                            state.icons.clone(),
                            state.submit.clone(),
                        );
                    }
                }
            });
        }
        Self { widget, state }
    }
    pub fn render(&self, snapshot: Arc<AppSnapshot>) {
        let previous = self.state.latest.replace(snapshot.clone());
        let desired_changed = !Arc::ptr_eq(&previous.desired, &snapshot.desired);
        let data_changed = desired_changed
            || !Arc::ptr_eq(&previous.pending_cells, &snapshot.pending_cells)
            || !Arc::ptr_eq(&previous.pending_fx, &snapshot.pending_fx)
            || !Arc::ptr_eq(&previous.captures, &snapshot.captures)
            || !Arc::ptr_eq(&previous.streams, &snapshot.streams)
            || !Arc::ptr_eq(&previous.outputs, &snapshot.outputs)
            || previous.default_output != snapshot.default_output;
        let topology_changed = !self
            .state
            .source_order
            .borrow()
            .iter()
            .eq(snapshot.desired.sources.keys())
            || !self
                .state
                .mix_order
                .borrow()
                .iter()
                .eq(snapshot.desired.mixes.keys());
        self.state.updating.set(true);
        if topology_changed {
            self.state.rebuild(&snapshot);
        }
        if data_changed || topology_changed {
            self.state.render_data(&snapshot, &previous);
        }
        self.widget.set_sensitive(matches!(
            snapshot.lifecycle,
            Lifecycle::Starting | Lifecycle::Running
        ));
        self.state.updating.set(false);
        if self.widget.is_mapped() {
            for row in self.state.rows.borrow().values() {
                row.meter.set_value(
                    snapshot
                        .meters
                        .get(&row.meter_key)
                        .copied()
                        .unwrap_or(0.0)
                        .clamp(0.0, 1.0),
                );
            }
            for header in self.state.headers.borrow().values() {
                let peak = snapshot
                    .meters
                    .get(&header.meter_key)
                    .copied()
                    .unwrap_or(0.0)
                    .clamp(0.0, 1.0)
                    .cbrt();
                header
                    .meter
                    .set_value(peak.max(header.meter.value() * 0.72));
            }
        }
    }
}
impl MatrixState {
    fn rebuild(self: &Rc<Self>, snapshot: &AppSnapshot) {
        while let Some(child) = self.grid.first_child() {
            self.grid.remove(&child);
        }
        self.rows.borrow_mut().clear();
        self.headers.borrow_mut().clear();
        self.cells.borrow_mut().clear();
        *self.source_order.borrow_mut() = snapshot.desired.sources.keys().cloned().collect();
        *self.mix_order.borrow_mut() = snapshot.desired.mixes.keys().cloned().collect();
        let corner = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        corner.set_size_request(400, 64);
        self.grid.attach(&corner, 0, 0, 1, 1);
        for (column, (id, mix)) in snapshot.desired.mixes.iter().enumerate() {
            let (widget, header) = self.build_header(mix);
            self.grid.attach(&widget, column as i32 + 1, 0, 1, 1);
            self.headers.borrow_mut().insert(id.clone(), header);
        }
        for (index, (id, source)) in snapshot.desired.sources.iter().enumerate() {
            let row = self.build_source(source);
            self.grid.attach(&row.widget, 0, index as i32 + 1, 1, 1);
            self.rows.borrow_mut().insert(id.clone(), row);
            for (column, mid) in snapshot.desired.mixes.keys().enumerate() {
                let cell = self.build_cell(id, mid);
                self.grid
                    .attach(&cell.widget, column as i32 + 1, index as i32 + 1, 1, 1);
                self.cells
                    .borrow_mut()
                    .insert((id.clone(), mid.clone()), cell);
            }
        }
    }
    fn build_cell(self: &Rc<Self>, source: &SourceId, mix: &MixId) -> Fader {
        let cell = Fader::new(false, &format!("{source} send to {mix}"));
        cell.widget.add_css_class("openwave-mix-cell");
        cell.widget.add_css_class("card");
        cell.widget.set_size_request(220, 64);
        padding(&cell.widget, 12, 10);
        cell.scale.set_widget_name(&format!("send-{source}-{mix}"));
        let (sid, mid, weak) = (source.clone(), mix.clone(), Rc::downgrade(self));
        let percent = cell.percent.downgrade();
        let mute = cell.mute.downgrade();
        cell.scale.connect_value_changed(move |scale| {
            if let Some(state) = weak.upgrade() {
                if state.updating.get() {
                    return;
                }
                let level = scale.value();
                state.updating.set(true);
                if let Some(percent) = percent.upgrade() {
                    percent.set_label(&format!("{:.0}%", level * 100.0));
                }
                if let Some(mute) = mute.upgrade() {
                    mute.set_active(level < 0.01);
                }
                state.updating.set(false);
                (state.submit)(AppCommand::SetCell {
                    source: sid.clone(),
                    mix: mid.clone(),
                    level,
                    muted: level < 0.01,
                    timing: EditTiming::Debounced,
                });
            }
        });
        let (sid, mid, weak) = (source.clone(), mix.clone(), Rc::downgrade(self));
        let scale = cell.scale.downgrade();
        cell.mute.connect_toggled(move |button| {
            if let (Some(state), Some(scale)) = (weak.upgrade(), scale.upgrade()) {
                if !state.updating.get() {
                    (state.submit)(AppCommand::SetCell {
                        source: sid.clone(),
                        mix: mid.clone(),
                        level: scale.value(),
                        muted: button.is_active(),
                        timing: EditTiming::Immediate,
                    });
                }
            }
        });
        cell
    }
    fn build_source(self: &Rc<Self>, source: &Source) -> SourceRow {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        widget.add_css_class("openwave-source-cell");
        widget.add_css_class("card");
        widget.set_size_request(400, 64);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
        padding(&content, 12, 10);
        content.set_hexpand(true);
        widget.append(&content);
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.append(&inner);
        let handle = self.icons.image("list-drag-handle-symbolic", 14);
        handle.add_css_class("dim-label");
        handle.set_tooltip_text(Some("Drag middle to group; edges to reorder"));
        inner.append(&handle);
        let icon = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        icon.append(&self.icons.image(&source.icon_name, 26));
        inner.append(&icon);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
        text.set_valign(gtk::Align::Center);
        text.set_hexpand(true);
        inner.append(&text);
        let group = label("", 0, "openwave-group-badge");
        group.set_max_width_chars(10);
        text.append(&group);
        let title = label(&source.name, 10, "heading");
        text.append(&title);
        let status = label("", 0, "dim-label");
        status.set_max_width_chars(10);
        status.add_css_class("caption");
        text.append(&status);
        let switch = button(
            &self.icons,
            "mail-send-receive-symbolic",
            "Switch to this source",
        );
        inner.append(&switch);
        let (sid, weak) = (source.id.clone(), Rc::downgrade(self));
        switch.connect_clicked(move |_| {
            if let Some(state) = weak.upgrade() {
                let snapshot = state.latest.borrow();
                if let Some(source) = snapshot.desired.sources.get(&sid) {
                    let command = if source.muted {
                        AppCommand::SetSourceMute {
                            source: sid.clone(),
                            muted: false,
                        }
                    } else {
                        AppCommand::SwitchGroup {
                            group: source.group.clone(),
                        }
                    };
                    (state.submit)(command);
                }
            }
        });
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        content.append(&controls);
        let fader = Fader::new(
            source.kind == SourceKind::Device,
            &format!("{} source trim", source.name),
        );
        fader.scale.set_size_request(110, -1);
        fader
            .scale
            .set_widget_name(&format!("source-{}-trim", source.id));
        fader.widget.set_hexpand(true);
        controls.append(&fader.widget);
        let (sid, weak) = (source.id.clone(), Rc::downgrade(self));
        let percent = fader.percent.downgrade();
        fader.scale.connect_value_changed(move |scale| {
            if let Some(state) = weak.upgrade() {
                if !state.updating.get() {
                    if let Some(percent) = percent.upgrade() {
                        percent.set_label(&format!("{:.0}%", scale.value() * 100.0));
                    }
                    (state.submit)(AppCommand::SetSourceLevel {
                        source: sid.clone(),
                        level: scale.value(),
                    });
                }
            }
        });
        let (sid, weak) = (source.id.clone(), Rc::downgrade(self));
        fader.mute.connect_toggled(move |_| {
            if let Some(state) = weak.upgrade() {
                if !state.updating.get() {
                    (state.submit)(AppCommand::ToggleSourceMute {
                        source: sid.clone(),
                    });
                }
            }
        });
        let meter = meter(56, 8);
        controls.append(&meter);
        let fx_button = gtk::MenuButton::builder()
            .label("FX")
            .valign(gtk::Align::Center)
            .tooltip_text("Effects: low cut, gate, compressor, EQ, delay")
            .build();
        fx_button.add_css_class("flat");
        controls.append(&fx_button);
        let fx = if source.kind == SourceKind::Device {
            let fx = FxControls::new(&source.id, self.updating.clone(), self.submit.clone());
            fx_button.set_popover(Some(&fx.popover));
            Some(fx)
        } else {
            reserve(&fx_button, false);
            None
        };
        let edit = button(&self.icons, "document-edit-symbolic", "Edit source");
        inner.append(&edit);
        let (sid, weak) = (source.id.clone(), Rc::downgrade(self));
        edit.connect_clicked(move |button| {
            if let (Some(state), Some(parent)) = (weak.upgrade(), parent(button)) {
                dialogs::edit_source(
                    &parent,
                    &sid,
                    state.latest.borrow().clone(),
                    state.icons.clone(),
                    state.submit.clone(),
                );
            }
        });
        let remove = button(&self.icons, "window-close-symbolic", "Remove source");
        inner.append(&remove);
        let (sid, weak) = (source.id.clone(), Rc::downgrade(self));
        remove.connect_clicked(move |button| {
            if let (Some(state), Some(parent)) = (weak.upgrade(), parent(button)) {
                dialogs::remove_source(
                    &parent,
                    &sid,
                    state.latest.borrow().clone(),
                    state.submit.clone(),
                );
            }
        });
        let order_menu = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text("Source order and group")
            .build();
        order_menu.add_css_class("flat");
        inner.append(&order_menu);
        let pop = gtk::Popover::new();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
        padding(&body, 8, 8);
        pop.set_child(Some(&body));
        order_menu.set_popover(Some(&pop));
        for (text, delta) in [("Move up", -1), ("Move down", 1)] {
            let (sid, weak) = (source.id.clone(), Rc::downgrade(self));
            menu_button(&pop, &body, text, move || {
                if let Some(state) = weak.upgrade() {
                    state.move_source(&sid, delta);
                }
            });
        }
        let (sid, weak) = (source.id.clone(), Rc::downgrade(self));
        let leave = menu_button(&pop, &body, "Leave group", move || {
            if let Some(state) = weak.upgrade() {
                (state.submit)(AppCommand::LeaveGroup {
                    source: sid.clone(),
                });
            }
        });
        self.drag_source(&widget, &source.id);
        SourceRow {
            widget,
            icon,
            title,
            group,
            status,
            switch,
            remove,
            leave,
            fader,
            meter,
            fx,
            meter_key: format!("src:{}", source.id),
        }
    }
    fn build_header(self: &Rc<Self>, mix: &Mix) -> (gtk::Box, MixHeader) {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        widget.add_css_class("openwave-mix-header");
        widget.add_css_class("card");
        widget.set_size_request(220, 78);
        let inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        padding(&inner, 10, 8);
        inner.set_hexpand(true);
        widget.append(&inner);
        let icon = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        icon.append(&self.icons.image(&mix.icon_name, 22));
        inner.append(&icon);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 1);
        text.set_valign(gtk::Align::Center);
        text.set_hexpand(true);
        inner.append(&text);
        let title = label(&mix.name, 14, "heading");
        text.append(&title);
        let subtitle = label(&mix.subtitle, 16, "caption");
        subtitle.add_css_class("dim-label");
        text.append(&subtitle);
        let output = label("", 16, "caption");
        output.add_css_class("dim-label");
        text.append(&output);
        let fader = Fader::new(false, &format!("{} master volume", mix.name));
        fader
            .scale
            .set_widget_name(&format!("mix-{}-master", mix.id));
        text.append(&fader.widget);
        let (mid, weak) = (mix.id.clone(), Rc::downgrade(self));
        let percent = fader.percent.downgrade();
        let mute = fader.mute.downgrade();
        fader.scale.connect_value_changed(move |scale| {
            if let (Some(state), Some(mute)) = (weak.upgrade(), mute.upgrade()) {
                if !state.updating.get() {
                    let muted = mute.is_active();
                    if let Some(percent) = percent.upgrade() {
                        percent.set_label(&format!("{:.0}%", scale.value() * 100.0));
                    }
                    (state.submit)(AppCommand::SetMaster {
                        mix: mid.clone(),
                        level: scale.value(),
                        muted,
                    });
                }
            }
        });
        let (mid, weak) = (mix.id.clone(), Rc::downgrade(self));
        let scale = fader.scale.downgrade();
        fader.mute.connect_toggled(move |button| {
            if let (Some(state), Some(scale)) = (weak.upgrade(), scale.upgrade()) {
                if !state.updating.get() {
                    let level = scale.value();
                    (state.submit)(AppCommand::SetMaster {
                        mix: mid.clone(),
                        level,
                        muted: button.is_active(),
                    });
                }
            }
        });
        let meter = meter(-1, 6);
        text.append(&meter);
        let menu = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text(format!("{} output, rename, delete", mix.name))
            .build();
        menu.set_widget_name(&format!("mix-{}-menu", mix.id));
        menu.add_css_class("flat");
        inner.append(&menu);
        menu.update_property(&[gtk::accessible::Property::Label(&format!(
            "{} output",
            mix.name
        ))]);
        let pop = gtk::Popover::new();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        padding(&body, 8, 8);
        body.set_size_request(272, -1);
        pop.set_child(Some(&body));
        menu.set_popover(Some(&pop));
        body.append(&label("Output", 20, "heading"));
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(260)
            .build();
        body.append(&scroll);
        let output_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        output_list.add_css_class("boxed-list");
        scroll.set_child(Some(&output_list));
        let output_names: Rc<RefCell<Vec<String>>> = Rc::default();
        let names = output_names.clone();
        let (mid, weak, popup) = (mix.id.clone(), Rc::downgrade(self), pop.downgrade());
        output_list.connect_row_selected(move |_, row| {
            if let (Some(state), Some(row)) = (weak.upgrade(), row) {
                if state.updating.get() {
                    return;
                }
                if let Some(choice) = names.borrow().get(row.index() as usize) {
                    if choice != state.latest.borrow().desired.matrix.output(&mid) {
                        (state.submit)(AppCommand::SetOutput {
                            mix: mid.clone(),
                            choice: choice.clone(),
                        });
                        if let Some(pop) = popup.upgrade() {
                            pop.popdown();
                        }
                    }
                }
            }
        });
        let published = gtk::Label::builder()
            .label(format!(
                "Published input: {}\nopenwave_capture_{}",
                mix.description, mix.id
            ))
            .wrap(true)
            .xalign(0.0)
            .selectable(true)
            .build();
        published.add_css_class("caption");
        published.add_css_class("dim-label");
        body.append(&published);
        body.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let (mid, weak) = (mix.id.clone(), Rc::downgrade(self));
        menu_button(&pop, &body, "Rename Mix…", move || {
            if let Some(state) = weak.upgrade() {
                if let Some(parent) = parent(&state.grid) {
                    dialogs::edit_mix(
                        &parent,
                        &mid,
                        state.latest.borrow().clone(),
                        state.icons.clone(),
                        state.submit.clone(),
                    );
                }
            }
        });
        for (title, delta) in [("Move left", -1), ("Move right", 1)] {
            let (mid, weak) = (mix.id.clone(), Rc::downgrade(self));
            menu_button(&pop, &body, title, move || {
                if let Some(state) = weak.upgrade() {
                    let mut order: Vec<_> = state
                        .latest
                        .borrow()
                        .desired
                        .mixes
                        .keys()
                        .cloned()
                        .collect();
                    if shift(&mut order, &mid, delta) {
                        (state.submit)(AppCommand::OrderMixes { order });
                    }
                }
            });
        }
        let (mid, weak) = (mix.id.clone(), Rc::downgrade(self));
        let remove = menu_button(&pop, &body, "Delete Mix", move || {
            if let Some(state) = weak.upgrade() {
                if let Some(parent) = parent(&state.grid) {
                    dialogs::remove_mix(
                        &parent,
                        &mid,
                        state.latest.borrow().clone(),
                        state.submit.clone(),
                    );
                }
            }
        });
        remove.add_css_class("error");
        let delete_hint = gtk::Label::builder()
            .label("OpenWave needs at least one mix.")
            .wrap(true)
            .xalign(0.0)
            .build();
        delete_hint.add_css_class("caption");
        delete_hint.add_css_class("dim-label");
        body.append(&delete_hint);
        (
            widget,
            MixHeader {
                icon,
                title,
                subtitle,
                output,
                fader,
                meter,
                output_list,
                output_names,
                output_entries: RefCell::new(Vec::new()),
                menu,
                remove,
                delete_hint,
                meter_key: format!("mix:{}", mix.id),
            },
        )
    }
    fn render_data(&self, snapshot: &AppSnapshot, previous: &AppSnapshot) {
        let claims = claim_streams(&snapshot.desired.sources, &snapshot.streams);
        for (id, row) in self.rows.borrow().iter() {
            if let Some(source) = snapshot.desired.sources.get(id) {
                row.title.set_label(&source.name);
                row.title.set_tooltip_text(Some(&source.name));
                let trim_label = format!("{} source trim", source.name);
                row.fader.scale.set_tooltip_text(Some(&trim_label));
                row.fader
                    .scale
                    .update_property(&[gtk::accessible::Property::Label(&trim_label)]);
                if previous
                    .desired
                    .sources
                    .get(id)
                    .is_some_and(|old| old.icon_name != source.icon_name)
                {
                    while let Some(child) = row.icon.first_child() {
                        row.icon.remove(&child);
                    }
                    row.icon.append(&self.icons.image(&source.icon_name, 26));
                }
                row.group.set_label(&source.group);
                row.group.set_visible(!source.group.is_empty());
                reserve(&row.switch, !source.group.is_empty());
                row.leave.set_sensitive(!source.group.is_empty());
                row.group.set_tooltip_text(Some(&format!(
                    "Only one source in “{}” is live at a time",
                    source.group
                )));
                row.fader.render(source.level, source.muted);
                css(&row.widget, "openwave-muted", source.muted);
                let available = source.kind != SourceKind::Device
                    || snapshot
                        .captures
                        .iter()
                        .any(|capture| capture.node_name == source.node_name);
                let waiting =
                    source.kind == SourceKind::App && claims.get(id).is_none_or(Vec::is_empty);
                row.status.set_label(if !available {
                    "Capture device not connected"
                } else if waiting {
                    "Waiting for audio"
                } else {
                    ""
                });
                row.status.set_visible(!available || waiting);
                css(&row.widget, "openwave-source-waiting", waiting);
                css(&row.title, "dim-label", !available);
                reserve(&row.remove, !source.protected || !available);
                row.remove.set_tooltip_text(Some(if source.protected {
                    "Remove disconnected device"
                } else {
                    "Remove source"
                }));
                if let Some(fx) = &row.fx {
                    fx.render(
                        snapshot
                            .pending_fx
                            .get(id)
                            .or(source.fx.as_ref())
                            .cloned()
                            .unwrap_or_default(),
                    );
                }
            }
        }
        for ((sid, mid), cell) in self.cells.borrow().iter() {
            let key = format!("{sid}.{mid}");
            let committed = snapshot.desired.matrix.cells.get(&key);
            if let Some(level) = snapshot.pending_cells.get(&key).or(committed) {
                cell.render(level.volume, level.muted || level.volume == 0.0);
            } else {
                cell.render(0.0, true);
            }
        }
        for (id, header) in self.headers.borrow().iter() {
            if let Some(mix) = snapshot.desired.mixes.get(id) {
                header.title.set_label(&mix.name);
                header.title.set_tooltip_text(Some(&mix.name));
                header.subtitle.set_label(&mix.subtitle);
                header.subtitle.set_visible(!mix.subtitle.is_empty());
                let master_label = format!("{} master volume", mix.name);
                header.fader.scale.set_tooltip_text(Some(&master_label));
                header
                    .fader
                    .scale
                    .update_property(&[gtk::accessible::Property::Label(&master_label)]);
                header
                    .menu
                    .update_property(&[gtk::accessible::Property::Label(&format!(
                        "{} output",
                        mix.name
                    ))]);
                header
                    .menu
                    .set_tooltip_text(Some(&format!("{} output, rename, delete", mix.name)));
                if previous
                    .desired
                    .mixes
                    .get(id)
                    .is_some_and(|old| old.icon_name != mix.icon_name)
                {
                    while let Some(child) = header.icon.first_child() {
                        header.icon.remove(&child);
                    }
                    header.icon.append(&self.icons.image(&mix.icon_name, 22));
                }
                let master = snapshot.desired.matrix.volumes.get(id);
                header.fader.render(
                    master.map(|m| m.volume).unwrap_or(1.0),
                    master.is_some_and(|m| m.muted),
                );
                header
                    .remove
                    .set_sensitive(snapshot.desired.mixes.len() > 1);
                header
                    .delete_hint
                    .set_visible(snapshot.desired.mixes.len() <= 1);
                let current = snapshot.desired.matrix.output(id);
                let resolved = resolve_output(
                    current,
                    &snapshot.outputs,
                    snapshot.default_output.as_deref(),
                );
                let description = match &resolved {
                    Ok(OutputDecision::Monitor(output)) => output.name.as_str(),
                    _ if current == "none" => "Not monitored",
                    _ => "No output",
                };
                let fed = snapshot.desired.sources.iter().any(|(sid, source)| {
                    let key = format!("{sid}.{id}");
                    !source.muted
                        && source.level > 0.0
                        && snapshot
                            .pending_cells
                            .get(&key)
                            .or(snapshot.desired.matrix.cells.get(&key))
                            .is_some_and(|cell| !cell.muted && cell.volume > 0.0)
                });
                header.output.set_label(if fed {
                    description
                } else {
                    "No sources routed"
                });
                header.output.set_tooltip_text(Some(if fed {
                    description
                } else {
                    "Every source is at zero or muted for this mix. Raise a slider in this column."
                }));
                let auto = if current == "auto" {
                    match &resolved {
                        Ok(OutputDecision::Monitor(output)) => {
                            format!("Automatic — {}", output.name)
                        }
                        _ => "Automatic".into(),
                    }
                } else {
                    "Automatic".into()
                };
                let mut choices = vec![
                    ("auto".to_owned(), auto),
                    ("none".to_owned(), "Not monitored".to_owned()),
                ];
                choices.extend(
                    snapshot
                        .outputs
                        .iter()
                        .filter(|output| eligible_output(output))
                        .map(|output| (output.node_name.clone(), output.name.clone())),
                );
                if !choices.iter().any(|(name, _)| name == current) {
                    choices.push((current.into(), format!("{current} (unavailable)")));
                }
                if *header.output_entries.borrow() != choices {
                    while let Some(child) = header.output_list.first_child() {
                        header.output_list.remove(&child);
                    }
                    *header.output_names.borrow_mut() =
                        choices.iter().map(|(name, _)| name.clone()).collect();
                    for (index, (name, title)) in choices.iter().enumerate() {
                        let row = gtk::ListBoxRow::new();
                        let label = label(title, 28, "");
                        padding(&label, 12, 8);
                        label.set_tooltip_text(Some(name));
                        row.set_child(Some(&label));
                        header.output_list.append(&row);
                        row.update_property(&[gtk::accessible::Property::Label(title)]);
                        row.set_widget_name(&format!("mix-{id}-output-{index}"));
                    }
                    *header.output_entries.borrow_mut() = choices;
                }
                if let Some(index) = header
                    .output_names
                    .borrow()
                    .iter()
                    .position(|name| name == current)
                {
                    header
                        .output_list
                        .select_row(header.output_list.row_at_index(index as i32).as_ref());
                }
            }
        }
    }
    fn move_source(&self, source: &SourceId, delta: i32) {
        let mut order: Vec<_> = self
            .latest
            .borrow()
            .desired
            .sources
            .keys()
            .cloned()
            .collect();
        if shift(&mut order, source, delta) {
            (self.submit)(AppCommand::OrderSources { order });
        }
    }
    fn drag_source(self: &Rc<Self>, widget: &gtk::Box, source: &SourceId) {
        let drag = gtk::DragSource::new();
        drag.set_actions(gdk::DragAction::MOVE);
        let id = source.to_string();
        drag.connect_prepare(move |_, _, _| Some(gdk::ContentProvider::for_value(&id.to_value())));
        let weak = widget.downgrade();
        drag.connect_drag_begin(move |_, drag| {
            if let Some(widget) = weak.upgrade() {
                let paintable = gtk::WidgetPaintable::new(Some(&widget));
                let picture = gtk::Picture::for_paintable(&paintable);
                picture.set_size_request(widget.width().max(320), widget.height().max(64));
                gtk::DragIcon::for_drag(drag).set_child(Some(&picture));
                widget.set_opacity(0.35);
            }
        });
        let weak = widget.downgrade();
        drag.connect_drag_end(move |_, _, _| {
            if let Some(widget) = weak.upgrade() {
                widget.set_opacity(1.0);
            }
        });
        let weak = widget.downgrade();
        drag.connect_drag_cancel(move |_, _, _| {
            if let Some(widget) = weak.upgrade() {
                widget.set_opacity(1.0);
            }
            false
        });
        widget.add_controller(drag);
        let drop = gtk::DropTarget::new(String::static_type(), gdk::DragAction::MOVE);
        let weak = widget.downgrade();
        drop.connect_motion(move |_, _, y| {
            if let Some(widget) = weak.upgrade() {
                let grouping = grouping(&widget, y);
                css(&widget, "openwave-drop-group", grouping);
                css(&widget, "openwave-drop-target", !grouping);
            }
            gdk::DragAction::MOVE
        });
        let weak = widget.downgrade();
        drop.connect_leave(move |_| {
            if let Some(widget) = weak.upgrade() {
                clear_drop(&widget);
            }
        });
        let (target, state, weak_widget) =
            (source.clone(), Rc::downgrade(self), widget.downgrade());
        drop.connect_drop(move |_, value, _, y| {
            let (Some(state), Some(widget), Ok(value)) = (
                state.upgrade(),
                weak_widget.upgrade(),
                value.get::<String>(),
            ) else {
                return false;
            };
            clear_drop(&widget);
            let Ok(dragged) = SourceId::new(value) else {
                return false;
            };
            if dragged == target || !state.latest.borrow().desired.sources.contains_key(&dragged) {
                return false;
            }
            let command = if grouping(&widget, y) {
                AppCommand::JoinGroup {
                    source: dragged,
                    target: target.clone(),
                }
            } else {
                let mut order: Vec<_> = state
                    .latest
                    .borrow()
                    .desired
                    .sources
                    .keys()
                    .cloned()
                    .collect();
                let Some(target_index) = order.iter().position(|id| id == &target) else {
                    return false;
                };
                order.retain(|id| id != &dragged);
                order.insert(target_index.min(order.len()), dragged);
                AppCommand::OrderSources { order }
            };
            let submit = state.submit.clone();
            glib::idle_add_local_once(move || submit(command));
            true
        });
        widget.add_controller(drop);
    }
}
fn shift<T: PartialEq>(order: &mut Vec<T>, id: &T, delta: i32) -> bool {
    let Some(index) = order.iter().position(|value| value == id) else {
        return false;
    };
    let destination = (index as i32 + delta).clamp(0, order.len() as i32 - 1) as usize;
    if index == destination {
        return false;
    }
    let value = order.remove(index);
    order.insert(destination, value);
    true
}
fn grouping(widget: &gtk::Box, y: f64) -> bool {
    let height = widget.height().max(64) as f64;
    y >= height * 0.28 && y <= height * 0.72
}
fn clear_drop(widget: &gtk::Box) {
    widget.remove_css_class("openwave-drop-group");
    widget.remove_css_class("openwave-drop-target");
}

struct FxControls {
    popover: gtk::Popover,
    value: Rc<RefCell<FxSettings>>,
    lowcut: gtk::DropDown,
    gate: gtk::Switch,
    comp: gtk::Switch,
    mono: gtk::Switch,
    scales: Vec<gtk::Scale>,
    delay: gtk::SpinButton,
}
impl FxControls {
    fn new(source: &SourceId, updating: Rc<Cell<bool>>, submit: Submit) -> Self {
        let popover = gtk::Popover::new();
        let body = gtk::Box::new(gtk::Orientation::Vertical, 8);
        padding(&body, 12, 12);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(600)
            .build();
        scroll.set_child(Some(&body));
        popover.set_child(Some(&scroll));
        let (sid, callback) = (source.clone(), submit.clone());
        menu_button(&popover, &body, "Auto-calibrate microphone", move || {
            callback(AppCommand::StartCalibration {
                source: sid.clone(),
            })
        });
        let value = Rc::new(RefCell::new(FxSettings::default()));
        let lowcut = gtk::DropDown::from_strings(&["Off", "80 Hz", "120 Hz"]);
        fx_row(&body, "Low cut", &lowcut);
        let (sid, guard, settings, callback) = (
            source.clone(),
            updating.clone(),
            value.clone(),
            submit.clone(),
        );
        lowcut.connect_selected_notify(move |dropdown| {
            if !guard.get() {
                settings.borrow_mut().lowcut = match dropdown.selected() {
                    1 => 80,
                    2 => 120,
                    _ => 0,
                };
                callback(AppCommand::SetFx {
                    source: sid.clone(),
                    settings: settings.borrow().clone(),
                    timing: EditTiming::Debounced,
                });
            }
        });
        let mut switches = Vec::new();
        for (index, title) in [(0, "Gate"), (1, "Comp"), (2, "Mono")] {
            let switch = gtk::Switch::builder()
                .halign(gtk::Align::Start)
                .valign(gtk::Align::Center)
                .build();
            let (sid, guard, settings, callback) = (
                source.clone(),
                updating.clone(),
                value.clone(),
                submit.clone(),
            );
            switch.connect_active_notify(move |switch| {
                if !guard.get() {
                    match index {
                        0 => settings.borrow_mut().gate = switch.is_active(),
                        1 => settings.borrow_mut().comp = switch.is_active(),
                        _ => settings.borrow_mut().mono = switch.is_active(),
                    }
                    callback(AppCommand::SetFx {
                        source: sid.clone(),
                        settings: settings.borrow().clone(),
                        timing: EditTiming::Debounced,
                    });
                }
            });
            switches.push((title, switch));
        }
        let gate = switches[0].1.clone();
        let comp = switches[1].1.clone();
        let mono = switches[2].1.clone();
        let mut scales = Vec::new();
        for (index, title, low, high, step) in [
            (0, "Gate dB", -70.0, -20.0, 1.0),
            (1, "Comp dB", -30.0, 0.0, 1.0),
            (2, "Ratio", 1.0, 10.0, 0.5),
            (3, "Low dB", -12.0, 12.0, 1.0),
            (4, "Mid dB", -12.0, 12.0, 1.0),
            (5, "High dB", -12.0, 12.0, 1.0),
        ] {
            if index == 0 {
                fx_row(&body, "Gate", &gate);
            }
            if index == 1 {
                fx_row(&body, "Comp", &comp);
            }
            let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, low, high, step);
            scale.set_digits(if index == 2 { 1 } else { 0 });
            scale.set_hexpand(true);
            scale.set_size_request(160, -1);
            if index >= 3 {
                scale.add_mark(0.0, gtk::PositionType::Bottom, None);
            }
            fx_row(&body, title, &scale);
            let (sid, guard, settings, callback) = (
                source.clone(),
                updating.clone(),
                value.clone(),
                submit.clone(),
            );
            scale.connect_value_changed(move |scale| {
                if !guard.get() {
                    let mut settings = settings.borrow_mut();
                    match index {
                        0 => settings.gate_thresh = scale.value(),
                        1 => settings.comp_thresh = scale.value(),
                        2 => settings.comp_ratio = scale.value(),
                        3 => settings.eq_low = scale.value(),
                        4 => settings.eq_mid = scale.value(),
                        _ => settings.eq_high = scale.value(),
                    }
                    callback(AppCommand::SetFx {
                        source: sid.clone(),
                        settings: settings.clone(),
                        timing: EditTiming::Debounced,
                    });
                }
            });
            scales.push(scale);
        }
        let gate_scale = scales[0].downgrade();
        gate.connect_active_notify(move |switch| {
            if let Some(scale) = gate_scale.upgrade() {
                scale.set_sensitive(switch.is_active());
            }
        });
        let comp_scales = [scales[1].downgrade(), scales[2].downgrade()];
        comp.connect_active_notify(move |switch| {
            for scale in &comp_scales {
                if let Some(scale) = scale.upgrade() {
                    scale.set_sensitive(switch.is_active());
                }
            }
        });
        let delay = gtk::SpinButton::with_range(0.0, 500.0, 5.0);
        fx_row(&body, "Delay ms", &delay);
        fx_row(&body, "Mono", &mono);
        let (sid, guard, settings) = (source.clone(), updating, value.clone());
        delay.connect_value_changed(move |delay| {
            if !guard.get() {
                settings.borrow_mut().delay_ms = delay.value();
                submit(AppCommand::SetFx {
                    source: sid.clone(),
                    settings: settings.borrow().clone(),
                    timing: EditTiming::Debounced,
                });
            }
        });
        Self {
            popover,
            value,
            lowcut,
            gate,
            comp,
            mono,
            scales,
            delay,
        }
    }
    fn render(&self, settings: FxSettings) {
        self.lowcut.set_selected(match settings.lowcut {
            80 => 1,
            120 => 2,
            _ => 0,
        });
        self.gate.set_active(settings.gate);
        self.comp.set_active(settings.comp);
        self.mono.set_active(settings.mono);
        for (scale, value) in self.scales.iter().zip([
            settings.gate_thresh,
            settings.comp_thresh,
            settings.comp_ratio,
            settings.eq_low,
            settings.eq_mid,
            settings.eq_high,
        ]) {
            scale.set_value(value);
        }
        self.scales[0].set_sensitive(settings.gate);
        self.scales[1].set_sensitive(settings.comp);
        self.scales[2].set_sensitive(settings.comp);
        self.delay.set_value(settings.delay_ms);
        *self.value.borrow_mut() = settings;
    }
}
fn fx_row(body: &gtk::Box, title: &str, control: &impl IsA<gtk::Widget>) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.append(&label(title, 9, ""));
    row.append(control);
    body.append(&row);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_support::{Rig, icons};

    #[test]
    #[ignore = "requires the isolated installed GTK test runner"]
    fn paired_master_edits_preserve_level_and_mute() {
        adw::init().expect("private GTK display");
        for mute_first in [false, true] {
            let rig = Rig::new(
                serde_json::json!({
                    "volumes": {"personal": {"volume": 1.0, "muted": false}}
                }),
                vec![],
            );
            let view = MatrixView::new(icons(), rig.submitter());
            view.render(rig.snapshot());
            let personal = MixId::new("personal").unwrap();
            let fader = view
                .state
                .headers
                .borrow()
                .get(&personal)
                .unwrap()
                .fader
                .clone();
            assert_eq!((fader.scale.value(), fader.mute.is_active()), (1.0, false));
            if mute_first {
                fader.mute.set_active(true);
                fader.scale.set_value(0.25);
            } else {
                fader.scale.set_value(0.25);
                fader.mute.set_active(true);
            }
            rig.finish_submissions();
            let snapshot = rig.snapshot();
            let master = snapshot.desired.matrix.volumes.get(&personal).unwrap();
            assert_eq!(
                (master.volume, master.muted),
                (0.25, true),
                "mute_first={mute_first}"
            );
        }
    }
}
