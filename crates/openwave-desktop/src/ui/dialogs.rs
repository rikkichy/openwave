use crate::{Submit, icons::Icons};
use adw::prelude::*;
use openwave_core::{model::*, routing::normalize_identity};
use openwave_runtime::controller::AppCommand;
use std::{cell::RefCell, collections::BTreeSet, rc::Rc, sync::Arc};

const SOURCE_ICONS: &[(&str, &str)] = &[
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
];
const MIX_ICONS: &[(&str, &str)] = &[
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
];

fn page(title: &str) -> (adw::NavigationPage, adw::HeaderBar, gtk::Box) {
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    toolbar.add_top_bar(&header);
    let scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();
    let clamp = adw::Clamp::builder()
        .maximum_size(440)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build();
    let body = gtk::Box::new(gtk::Orientation::Vertical, 16);
    clamp.set_child(Some(&body));
    scroll.set_child(Some(&clamp));
    toolbar.set_content(Some(&scroll));
    (adw::NavigationPage::new(&toolbar, title), header, body)
}
fn cancel(header: &adw::HeaderBar, dialog: &adw::Dialog) {
    let button = gtk::Button::with_label("Cancel");
    let weak = dialog.downgrade();
    button.connect_clicked(move |_| {
        if let Some(dialog) = weak.upgrade() {
            dialog.close();
        }
    });
    header.pack_start(&button);
}
fn hint(text: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(text)
        .wrap(true)
        .xalign(0.0)
        .build();
    label.add_css_class("dim-label");
    label.add_css_class("caption");
    label
}
fn entry(body: &gtk::Box, title: &str, field: &str, value: &str) -> adw::EntryRow {
    let group = adw::PreferencesGroup::builder().title(title).build();
    let row = adw::EntryRow::builder().title(field).text(value).build();
    group.add(&row);
    body.append(&group);
    row
}
fn icon_picker(
    body: &gtk::Box,
    icons: &Icons,
    current: &str,
    choices: &[(&str, &str)],
) -> Rc<RefCell<String>> {
    let selected = Rc::new(RefCell::new(current.to_owned()));
    let group = adw::PreferencesGroup::builder().title("Icon").build();
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .max_children_per_line(6)
        .min_children_per_line(4)
        .column_spacing(6)
        .row_spacing(6)
        .homogeneous(true)
        .build();
    flow.add_css_class("openwave-icon-picker");
    let mut names: Vec<String> = choices.iter().map(|(name, _)| (*name).into()).collect();
    if !names.iter().any(|name| name == current) {
        names.push(current.into());
    }
    for (index, name) in names.iter().enumerate() {
        let child = gtk::FlowBoxChild::new();
        child.set_child(Some(&icons.image(name, 28)));
        child.set_tooltip_text(Some(
            choices.get(index).map(|(_, tip)| *tip).unwrap_or("Current"),
        ));
        flow.insert(&child, -1);
        if name == current {
            flow.select_child(&child);
        }
    }
    let state = selected.clone();
    flow.connect_selected_children_changed(move |flow| {
        if let Some(child) = flow.selected_children().first() {
            if let Some(name) = names.get(child.index() as usize) {
                *state.borrow_mut() = name.clone();
            }
        }
    });
    group.add(&flow);
    body.append(&group);
    selected
}

pub fn show_error(parent: &gtk::Window, heading: &str, message: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(message)
        .build();
    dialog.add_response("ok", "OK");
    dialog.set_close_response("ok");
    dialog.present(Some(parent));
}
fn confirm(
    parent: &gtk::Window,
    heading: &str,
    body: &str,
    label: &str,
    command: AppCommand,
    submit: Submit,
) {
    let dialog = adw::AlertDialog::builder()
        .heading(heading)
        .body(body)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("confirm", label);
    dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response == "confirm" {
            submit(command.clone());
        }
    });
    dialog.present(Some(parent));
}
pub fn remove_source(
    parent: &gtk::Window,
    id: &SourceId,
    snapshot: Arc<AppSnapshot>,
    submit: Submit,
) {
    let Some(source) = snapshot.desired.sources.get(id) else {
        return;
    };
    if source.protected
        && snapshot
            .captures
            .iter()
            .any(|capture| capture.node_name == source.node_name)
    {
        show_error(
            parent,
            "Device still connected",
            "Disconnect the device before removing its protected source row.",
        );
        return;
    }
    let consequence = if source.protected {
        "If the device is plugged back in, the row is offered again."
    } else if source.kind == SourceKind::App {
        "The bound application itself is not affected."
    } else {
        "The capture device itself is not affected."
    };
    confirm(
        parent,
        "Remove source?",
        &format!(
            "This deletes “{}” and its mix levels. {consequence}",
            source.name
        ),
        "Remove",
        AppCommand::RemoveSource { source: id.clone() },
        submit,
    );
}
pub fn remove_mix(parent: &gtk::Window, id: &MixId, snapshot: Arc<AppSnapshot>, submit: Submit) {
    let Some(mix) = snapshot.desired.mixes.get(id) else {
        return;
    };
    if snapshot.desired.mixes.len() <= 1 {
        show_error(
            parent,
            "Cannot delete the last mix",
            "OpenWave needs at least one mix.",
        );
        return;
    }
    confirm(
        parent,
        "Delete mix?",
        &format!(
            "“{}” and its levels for every source are deleted, and the “{}” audio device disappears. Anything recording or listening to it — OBS, Discord — loses that input until it is pointed somewhere else.",
            mix.name, mix.description
        ),
        "Delete",
        AppCommand::RemoveMix { mix: id.clone() },
        submit,
    );
}

pub fn add_mix(
    parent: &gtk::Window,
    _snapshot: Arc<AppSnapshot>,
    icons: Rc<Icons>,
    submit: Submit,
) {
    mix_dialog(parent, None, icons, submit);
}
pub fn edit_mix(
    parent: &gtk::Window,
    id: &MixId,
    snapshot: Arc<AppSnapshot>,
    icons: Rc<Icons>,
    submit: Submit,
) {
    if let Some(mix) = snapshot.desired.mixes.get(id) {
        mix_dialog(parent, Some(mix.clone()), icons, submit);
    }
}
fn mix_dialog(parent: &gtk::Window, mix: Option<Mix>, icons: Rc<Icons>, submit: Submit) {
    let title = if mix.is_some() {
        "Rename Mix"
    } else {
        "Add Mix"
    };
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(460)
        .content_height(430)
        .build();
    let nav = adw::NavigationView::new();
    dialog.set_child(Some(&nav));
    let (page, header, body) = page(title);
    cancel(&header, &dialog);
    let name = entry(
        &body,
        "Name",
        "Mix name",
        mix.as_ref().map(|m| m.name.as_str()).unwrap_or(""),
    );
    body.append(&hint("The name is OpenWave's own label. The audio device other applications record from keeps the name it was created with, so renaming never breaks an OBS or Discord setup."));
    let icon = icon_picker(
        &body,
        &icons,
        mix.as_ref()
            .map(|m| m.icon_name.as_str())
            .unwrap_or("audio-speakers-symbolic"),
        MIX_ICONS,
    );
    let save = gtk::Button::with_label(if mix.is_some() { "Save" } else { "Add Mix" });
    save.add_css_class("suggested-action");
    save.set_sensitive(!name.text().trim().is_empty());
    header.pack_end(&save);
    let weak = save.downgrade();
    name.connect_changed(move |name| {
        if let Some(save) = weak.upgrade() {
            save.set_sensitive(!name.text().trim().is_empty());
        }
    });
    let weak = save.downgrade();
    name.connect_entry_activated(move |_| {
        if let Some(save) = weak.upgrade() {
            if save.is_sensitive() {
                save.emit_clicked();
            }
        }
    });
    let weak = dialog.downgrade();
    let commit: Rc<dyn Fn()> = Rc::new(move || {
        let text = name.text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        if let Some(mix) = &mix {
            submit(AppCommand::EditMix {
                mix: mix.id.clone(),
                changes: MixEdit {
                    name: Some(text),
                    icon_name: Some(icon.borrow().clone()),
                    ..Default::default()
                },
            });
        } else {
            let mut mix = Mix::new(text);
            mix.icon_name = icon.borrow().clone();
            submit(AppCommand::AddMix { mix });
        }
        if let Some(dialog) = weak.upgrade() {
            dialog.close();
        }
    });
    save.connect_clicked(move |_| commit());
    nav.push(&page);
    dialog.present(Some(parent));
}

pub fn save_scene(parent: &gtk::Window, submit: Submit) {
    let dialog = adw::AlertDialog::builder().heading("Save Scene As").body("Save the current source, mix and device levels. Effects and phantom power are not included.").build();
    let name = adw::EntryRow::builder().title("Scene name").build();
    dialog.set_extra_child(Some(&name));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_response_enabled("save", false);
    dialog.set_close_response("cancel");
    let weak = dialog.downgrade();
    name.connect_changed(move |name| {
        if let Some(dialog) = weak.upgrade() {
            dialog.set_response_enabled("save", !name.text().trim().is_empty());
        }
    });
    dialog.connect_response(None, move |_, response| {
        if response == "save" {
            let text = name.text().trim().to_owned();
            if !text.is_empty() {
                submit(AppCommand::SaveScene { name: text });
            }
        }
    });
    dialog.present(Some(parent));
}

pub fn add_source(
    parent: &gtk::Window,
    snapshot: Arc<AppSnapshot>,
    icons: Rc<Icons>,
    submit: Submit,
) {
    let dialog = adw::Dialog::builder()
        .title("Add Source")
        .content_width(480)
        .content_height(560)
        .build();
    let nav = adw::NavigationView::new();
    dialog.set_child(Some(&nav));
    let (page, header, body) = page("Add Source");
    cancel(&header, &dialog);
    body.append(&hint("What should this row carry into your mixes?"));
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");
    body.append(&list);
    for (kind, title, subtitle, icon) in [
        (
            SourceKind::App,
            "Application",
            "Follows every stream an app plays, now and later",
            "applications-multimedia-symbolic",
        ),
        (
            SourceKind::Device,
            "Capture Device",
            "A microphone or line input, such as a headset mic",
            "audio-input-microphone-symbolic",
        ),
    ] {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .activatable(true)
            .build();
        row.add_prefix(&icons.image(icon, 24));
        row.add_suffix(&icons.image("go-next-symbolic", 16));
        let weak_dialog = dialog.downgrade();
        let weak_nav = nav.downgrade();
        let (snapshot, icons, submit) = (snapshot.clone(), icons.clone(), submit.clone());
        row.connect_activated(move |_| {
            if let (Some(dialog), Some(nav)) = (weak_dialog.upgrade(), weak_nav.upgrade()) {
                source_picker(
                    &dialog,
                    &nav,
                    kind,
                    snapshot.clone(),
                    icons.clone(),
                    submit.clone(),
                );
            }
        });
        list.append(&row);
    }
    nav.push(&page);
    dialog.present(Some(parent));
}
pub fn edit_source(
    parent: &gtk::Window,
    id: &SourceId,
    snapshot: Arc<AppSnapshot>,
    icons: Rc<Icons>,
    submit: Submit,
) {
    let Some(source) = snapshot.desired.sources.get(id).cloned() else {
        return;
    };
    let dialog = adw::Dialog::builder()
        .title("Edit Source")
        .content_width(480)
        .content_height(560)
        .build();
    let nav = adw::NavigationView::new();
    dialog.set_child(Some(&nav));
    source_config(&dialog, &nav, source, true, snapshot, icons, submit);
    dialog.present(Some(parent));
}
fn source_picker(
    dialog: &adw::Dialog,
    nav: &adw::NavigationView,
    kind: SourceKind,
    snapshot: Arc<AppSnapshot>,
    icons: Rc<Icons>,
    submit: Submit,
) {
    let (page, header, body) = page(if kind == SourceKind::Device {
        "Pick Capture Device"
    } else {
        "Pick Application"
    });
    body.append(&hint(if kind == SourceKind::Device { "Pick a microphone or line input. OpenWave mixes it into each mix at the level you set." }
        else { "Pick an application that's currently playing audio, or enter one manually if it isn't running yet." }));
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .build();
    list.add_css_class("boxed-list");
    body.append(&list);
    let mut choices = Vec::new();
    if kind == SourceKind::Device {
        for capture in snapshot.captures.iter().filter(|c| {
            !c.node_name.starts_with("openwave_")
                && !c.node_name.ends_with(".monitor")
                && !snapshot
                    .desired
                    .sources
                    .values()
                    .any(|s| s.kind == SourceKind::Device && s.node_name == c.node_name)
        }) {
            let row = adw::ActionRow::builder()
                .title(&capture.name)
                .subtitle(&capture.node_name)
                .build();
            row.add_prefix(&icons.image("audio-input-microphone-symbolic", 24));
            list.append(&row);
            let mut source = Source::new(capture.name.clone(), kind);
            source.node_name = capture.node_name.clone();
            source.icon_name = "microphone-sensitivity-high-symbolic".into();
            choices.push(source);
        }
    } else {
        let bound: BTreeSet<String> = snapshot
            .desired
            .sources
            .values()
            .flat_map(|s| &s.match_app_names)
            .map(|n| normalize_identity(n))
            .collect();
        let names: BTreeSet<&str> = snapshot
            .streams
            .iter()
            .map(|s| s.app_name.as_str())
            .filter(|n| !n.is_empty() && !bound.contains(&normalize_identity(n)))
            .collect();
        for name in names {
            let row = adw::ActionRow::builder().title(name).build();
            row.add_prefix(&icons.image("applications-multimedia-symbolic", 24));
            list.append(&row);
            let mut source = Source::new(name.into(), kind);
            source.match_app_names.push(name.into());
            choices.push(source);
        }
        let row = adw::ActionRow::builder()
            .title("Enter manually…")
            .subtitle("Bind an application that isn't running yet")
            .build();
        row.add_prefix(&icons.image("document-edit-symbolic", 24));
        list.append(&row);
        choices.push(Source::new(String::new(), kind));
    }
    if choices.is_empty() {
        body.append(&hint("No other capture devices. Connect a headset or microphone, then open this dialog again."));
    }
    let next = gtk::Button::with_label("Next");
    next.add_css_class("suggested-action");
    next.set_sensitive(false);
    header.pack_end(&next);
    let weak = next.downgrade();
    list.connect_row_selected(move |_, row| {
        if let Some(next) = weak.upgrade() {
            next.set_sensitive(row.is_some());
        }
    });
    let weak_dialog = dialog.downgrade();
    let weak_nav = nav.downgrade();
    next.connect_clicked(move |_| {
        if let (Some(dialog), Some(nav), Some(row)) = (
            weak_dialog.upgrade(),
            weak_nav.upgrade(),
            list.selected_row(),
        ) {
            if let Some(source) = choices.get(row.index() as usize) {
                source_config(
                    &dialog,
                    &nav,
                    source.clone(),
                    false,
                    snapshot.clone(),
                    icons.clone(),
                    submit.clone(),
                );
            }
        }
    });
    nav.push(&page);
}

struct Bindings {
    list: gtk::ListBox,
    names: RefCell<Vec<String>>,
    pending: adw::EntryRow,
    confirm: gtk::glib::WeakRef<gtk::Button>,
}
impl Bindings {
    fn valid(&self) {
        if let Some(confirm) = self.confirm.upgrade() {
            confirm.set_sensitive(
                !self.names.borrow().is_empty() || !self.pending.text().trim().is_empty(),
            );
        }
    }
    fn add(self: &Rc<Self>, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        if !self
            .names
            .borrow()
            .iter()
            .any(|name| normalize_identity(name) == normalize_identity(text))
        {
            self.names.borrow_mut().push(text.into());
        }
        self.rebuild();
    }
    fn rebuild(self: &Rc<Self>) {
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for name in self.names.borrow().iter() {
            let row = adw::ActionRow::builder().title(name).build();
            let remove = gtk::Button::builder()
                .icon_name("window-close-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text(format!("Stop matching {name}"))
                .build();
            remove.add_css_class("flat");
            let name = name.clone();
            let weak = Rc::downgrade(self);
            remove.connect_clicked(move |_| {
                if let Some(bindings) = weak.upgrade() {
                    bindings.names.borrow_mut().retain(|item| item != &name);
                    bindings.rebuild();
                }
            });
            row.add_suffix(&remove);
            self.list.append(&row);
        }
        self.valid();
    }
}
fn source_config(
    dialog: &adw::Dialog,
    nav: &adw::NavigationView,
    source: Source,
    editing: bool,
    snapshot: Arc<AppSnapshot>,
    icons: Rc<Icons>,
    submit: Submit,
) {
    let (page, header, body) = page(if editing {
        "Edit Source"
    } else {
        "Name and Icon"
    });
    if editing {
        cancel(&header, dialog);
    }
    let name = entry(&body, "Name", "Source name", &source.name);
    let save = gtk::Button::with_label(if editing { "Save" } else { "Add Source" });
    save.add_css_class("suggested-action");
    header.pack_end(&save);
    let bindings = if source.kind == SourceKind::App {
        let group = adw::PreferencesGroup::builder()
            .title("Applications")
            .description("Audio from any of these is gathered under this row's single fader.")
            .build();
        body.append(&group);
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build();
        list.add_css_class("boxed-list");
        group.add(&list);
        let pending = adw::EntryRow::builder().title("Add an application").build();
        group.add(&pending);
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text("Add this name")
            .build();
        add.add_css_class("flat");
        pending.add_suffix(&add);
        let bindings = Rc::new(Bindings {
            list,
            names: RefCell::new(source.match_app_names.clone()),
            pending: pending.clone(),
            confirm: save.downgrade(),
        });
        let weak = Rc::downgrade(&bindings);
        add.connect_clicked(move |_| {
            if let Some(b) = weak.upgrade() {
                let text = b.pending.text();
                b.add(&text);
                b.pending.set_text("");
            }
        });
        let weak = Rc::downgrade(&bindings);
        pending.connect_entry_activated(move |_| {
            if let Some(b) = weak.upgrade() {
                let text = b.pending.text();
                b.add(&text);
                b.pending.set_text("");
            }
        });
        let weak = Rc::downgrade(&bindings);
        pending.connect_changed(move |_| {
            if let Some(b) = weak.upgrade() {
                b.valid();
            }
        });
        let running = gtk::MenuButton::builder()
            .label("From running apps")
            .halign(gtk::Align::Start)
            .build();
        running.add_css_class("flat");
        group.add(&running);
        let pop = gtk::Popover::new();
        let choices = gtk::Box::new(gtk::Orientation::Vertical, 2);
        pop.set_child(Some(&choices));
        running.set_popover(Some(&pop));
        let names: BTreeSet<&str> = snapshot
            .streams
            .iter()
            .flat_map(|s| [s.app_name.as_str(), s.binary.as_str()])
            .filter(|s| !s.is_empty())
            .collect();
        if names.is_empty() {
            choices.append(&hint("Nothing is playing"));
        }
        for name in names {
            let button = gtk::Button::with_label(name);
            button.add_css_class("flat");
            let name = name.to_owned();
            let weak = Rc::downgrade(&bindings);
            let pop = pop.downgrade();
            button.connect_clicked(move |_| {
                if let Some(b) = weak.upgrade() {
                    b.add(&name);
                }
                if let Some(pop) = pop.upgrade() {
                    pop.popdown();
                }
            });
            choices.append(&button);
        }
        bindings.rebuild();
        Some(bindings)
    } else {
        let group = adw::PreferencesGroup::builder()
            .title("Capture Device")
            .build();
        group.add(
            &adw::ActionRow::builder()
                .title(&source.node_name)
                .subtitle("The capture device this row is bound to")
                .build(),
        );
        body.append(&group);
        save.set_sensitive(!name.text().trim().is_empty());
        let weak = save.downgrade();
        name.connect_changed(move |name| {
            if let Some(save) = weak.upgrade() {
                save.set_sensitive(!name.text().trim().is_empty());
            }
        });
        None
    };
    let group = entry(&body, "Group", "Group name", &source.group);
    body.append(&hint("Sources sharing a group are mutually exclusive: unmuting one mutes the others. Leave blank for none."));
    let icon = icon_picker(&body, &icons, &source.icon_name, SOURCE_ICONS);
    let weak = dialog.downgrade();
    save.connect_clicked(move |_| {
        let names = bindings.as_ref().map(|b| {
            let text = b.pending.text();
            b.add(&text);
            b.names.borrow().clone()
        });
        if names.as_ref().is_some_and(Vec::is_empty) {
            return;
        }
        let text = name.text().trim().to_owned();
        let text = if text.is_empty() {
            names
                .as_ref()
                .and_then(|names| names.first())
                .cloned()
                .unwrap_or_default()
        } else {
            text
        };
        if text.is_empty() {
            return;
        }
        if editing {
            submit(AppCommand::EditSource {
                source: source.id.clone(),
                changes: SourceEdit {
                    name: Some(text),
                    icon_name: Some(icon.borrow().clone()),
                    match_app_names: names,
                    group: Some(group.text().trim().into()),
                    ..Default::default()
                },
            });
        } else {
            let mut source = source.clone();
            source.name = text;
            source.icon_name = icon.borrow().clone();
            source.group = group.text().trim().into();
            if let Some(names) = names {
                source.match_app_names = names;
            }
            submit(AppCommand::AddSource { source });
        }
        if let Some(dialog) = weak.upgrade() {
            dialog.close();
        }
    });
    nav.push(&page);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_support::{Rig, descendants, icons};

    #[test]
    #[ignore = "requires the isolated installed GTK test runner"]
    fn manual_pending_binding_creates_named_application() {
        adw::init().expect("private GTK display");
        let rig = Rig::new(serde_json::json!({}), vec![]);
        let window = adw::Window::new();
        window.present();
        add_source(
            window.upcast_ref(),
            rig.snapshot(),
            icons(),
            rig.submitter(),
        );
        let dialog = window.visible_dialog().expect("Add Source dialog");
        let nav = dialog
            .child()
            .unwrap()
            .downcast::<adw::NavigationView>()
            .unwrap();
        let page = nav.visible_page().unwrap();
        let application = descendants::<adw::ActionRow>(&page)
            .into_iter()
            .find(|row| row.title() == "Application")
            .unwrap();
        application.emit_by_name::<()>("activated", &[]);
        let page = nav.visible_page().unwrap();
        let manual = descendants::<adw::ActionRow>(&page)
            .into_iter()
            .find(|row| row.title() == "Enter manually…")
            .unwrap();
        let list = manual.parent().unwrap().downcast::<gtk::ListBox>().unwrap();
        list.select_row(Some(&manual));
        let next = descendants::<gtk::Button>(&page)
            .into_iter()
            .find(|button| button.label().as_deref() == Some("Next"))
            .unwrap();
        assert!(next.is_sensitive());
        next.emit_clicked();
        let page = nav.visible_page().unwrap();
        let entries = descendants::<adw::EntryRow>(&page);
        let name = entries
            .iter()
            .find(|row| row.title() == "Source name")
            .unwrap();
        let pending = entries
            .iter()
            .find(|row| row.title() == "Add an application")
            .unwrap();
        assert!(name.text().is_empty());
        pending.set_text("  Player  ");
        let save = descendants::<gtk::Button>(&page)
            .into_iter()
            .find(|button| button.label().as_deref() == Some("Add Source"))
            .unwrap();
        assert!(
            save.is_sensitive(),
            "a pending binding supplies the default name"
        );
        save.emit_clicked();
        rig.finish_submissions();
        let snapshot = rig.snapshot();
        let source = snapshot
            .desired
            .sources
            .values()
            .find(|source| source.name == "Player")
            .unwrap();
        assert_eq!(source.kind, SourceKind::App);
        assert_eq!(source.match_app_names, ["Player"]);
        window.destroy();
    }
}
