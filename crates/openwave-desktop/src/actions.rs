use gio::prelude::*;
use glib::{
    Variant,
    variant::{StaticVariantType, ToVariant},
};
use openwave_core::{
    model::{AppSnapshot, MixId, OperationError, Result, SourceId},
    routing::source_groups,
    scenes::SceneId,
};
use openwave_runtime::controller::{AppCommand, RuntimeHandle};
use std::{cell::Cell, rc::Rc};

pub struct ActionCallbacks {
    pub uninstall: Rc<dyn Fn()>,
    pub prepare_uninstall: Rc<dyn Fn(String)>,
}

pub struct ActionRegistry {
    snapshot: gio::SimpleAction,
    groups: gio::SimpleAction,
    prepare: gio::SimpleAction,
    revision: Cell<Option<u64>>,
}

impl ActionRegistry {
    pub fn register(
        app: &adw::Application,
        handle: RuntimeHandle,
        callbacks: ActionCallbacks,
    ) -> Self {
        for (name, signature) in [
            ("switch-group", String::static_variant_type()),
            ("set-source-level", <(String, f64)>::static_variant_type()),
            ("toggle-source-mute", String::static_variant_type()),
            (
                "set-cell-level",
                <(String, String, f64)>::static_variant_type(),
            ),
            (
                "toggle-cell-mute",
                <(String, String)>::static_variant_type(),
            ),
            ("apply-scene", String::static_variant_type()),
            ("save-scene", String::static_variant_type()),
            ("delete-scene", String::static_variant_type()),
            ("toggle-fx", <(String, String)>::static_variant_type()),
        ] {
            let action = gio::SimpleAction::new(name, Some(&signature));
            let runtime = handle.clone();
            action.connect_activate(move |_, parameter| {
                let result = parameter
                    .ok_or_else(|| OperationError::invalid("Missing action parameter"))
                    .and_then(|parameter| command(name, parameter));
                match result {
                    Ok(command) => {
                        if let Err(error) = runtime.submit(command) {
                            log::warn!("Remote {name}: {error}");
                        }
                    }
                    Err(error) => log::warn!("Remote {name}: {error}"),
                }
            });
            app.add_action(&action);
        }
        let snapshot = read_action(app, "snapshot", "{}".to_variant(), &handle);
        let groups = read_action(
            app,
            "source-groups",
            Vec::<String>::new().to_variant(),
            &handle,
        );
        read_action(app, "scenes", "{}".to_variant(), &handle);
        read_action(app, "levels", "{}".to_variant(), &handle);
        let uninstall = gio::SimpleAction::new("uninstall", None);
        uninstall.connect_activate(move |_, _| (callbacks.uninstall)());
        app.add_action(&uninstall);
        let prepare = gio::SimpleAction::new_stateful(
            "prepare-uninstall",
            Some(&String::static_variant_type()),
            &"idle".to_variant(),
        );
        prepare.connect_change_state(|_, _| {});
        prepare.connect_activate(move |_, parameter| {
            if let Some(identity) = parameter.and_then(|value| value.get::<String>()) {
                (callbacks.prepare_uninstall)(identity);
            }
        });
        app.add_action(&prepare);
        let registry = Self {
            snapshot,
            groups,
            prepare,
            revision: Cell::new(None),
        };
        registry.refresh(&handle.snapshot());
        registry
    }

    pub fn refresh(&self, snapshot: &AppSnapshot) {
        if self.revision.get() == Some(snapshot.revision) {
            return;
        }
        match snapshot.action_snapshot() {
            Ok(json) => {
                self.snapshot.set_state(&json.to_variant());
                self.groups
                    .set_state(&source_groups(&snapshot.desired.sources).to_variant());
                self.revision.set(Some(snapshot.revision));
            }
            Err(error) => log::warn!("Publishing remote snapshot: {error}"),
        }
    }

    pub fn set_uninstall_state(&self, state: &str) {
        self.prepare.set_state(&state.to_variant());
    }
}

fn read_action(
    app: &adw::Application,
    name: &'static str,
    initial: Variant,
    handle: &RuntimeHandle,
) -> gio::SimpleAction {
    let action = gio::SimpleAction::new_stateful(name, None, &initial);
    action.connect_change_state(|_, _| {});
    let handle = handle.clone();
    action.connect_activate(move |action, _| {
        let snapshot = handle.snapshot();
        if name == "source-groups" {
            action.set_state(&source_groups(&snapshot.desired.sources).to_variant());
            return;
        }
        let result = match name {
            "snapshot" => snapshot.action_snapshot(),
            "scenes" => snapshot.action_scenes(),
            "levels" => snapshot.action_levels(),
            _ => return,
        };
        match result {
            Ok(json) => action.set_state(&json.to_variant()),
            Err(error) => log::warn!("Reading remote {name}: {error}"),
        }
    });
    app.add_action(&action);
    action
}

fn parameter<T: glib::variant::FromVariant>(value: &Variant) -> Result<T> {
    value
        .get()
        .ok_or_else(|| OperationError::invalid("Wrong action parameter type"))
}

fn command(name: &str, value: &Variant) -> Result<AppCommand> {
    Ok(match name {
        "switch-group" => AppCommand::SwitchGroup {
            group: parameter(value)?,
        },
        "toggle-source-mute" => AppCommand::ToggleSourceMute {
            source: SourceId::new(parameter::<String>(value)?)?,
        },
        "set-source-level" => {
            let (source, level): (String, f64) = parameter(value)?;
            AppCommand::SetSourceLevel {
                source: SourceId::new(source)?,
                level,
            }
        }
        "set-cell-level" => {
            let (source, mix, level): (String, String, f64) = parameter(value)?;
            let source = SourceId::new(source)?;
            let mix = MixId::new(mix)?;
            // Resolve the current mute inside the controller, after any queued toggle.
            AppCommand::SetCellLevel { source, mix, level }
        }
        "toggle-cell-mute" => {
            let (source, mix): (String, String) = parameter(value)?;
            AppCommand::ToggleCellMute {
                source: SourceId::new(source)?,
                mix: MixId::new(mix)?,
            }
        }
        "toggle-fx" => {
            let (source, effect): (String, String) = parameter(value)?;
            AppCommand::ToggleFx {
                source: SourceId::new(source)?,
                effect,
            }
        }
        "save-scene" => AppCommand::SaveScene {
            name: parameter(value)?,
        },
        "apply-scene" => AppCommand::ApplyScene {
            scene: SceneId::new(parameter::<String>(value)?)?,
        },
        "delete-scene" => AppCommand::DeleteScene {
            scene: SceneId::new(parameter::<String>(value)?)?,
        },
        _ => return Err(OperationError::invalid("Unknown remote action")),
    })
}
