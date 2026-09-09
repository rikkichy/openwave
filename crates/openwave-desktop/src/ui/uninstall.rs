//! Inspection is read-only; only the explicit destructive button admits removal.
use adw::prelude::*;
use openwave_runtime::{
    paths::RuntimePaths,
    uninstall::{self, UninstallPlan, UninstallResult},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    thread::JoinHandle,
};

type Confirm = Rc<dyn Fn(Arc<UninstallPlan>, bool) -> Result<(), String>>;

pub struct UninstallDialog {
    pub window: adw::Window,
    heading: gtk::Label,
    details: gtk::Label,
    settings: gtk::CheckButton,
    status: gtk::Label,
    accept: gtk::Button,
    cancel: gtk::Button,
    plan: RefCell<Option<Arc<UninstallPlan>>>,
    busy: Cell<bool>,
    confirmed: Cell<bool>,
    delete_settings: Cell<bool>,
    closed: Cell<bool>,
}

impl UninstallDialog {
    pub fn new(
        parent: &gtk::Window,
        paths: RuntimePaths,
        confirm: Confirm,
        on_close: Rc<dyn Fn(bool)>,
        workers: Rc<RefCell<Vec<JoinHandle<()>>>>,
    ) -> Rc<Self> {
        let window = adw::Window::builder()
            .title("Uninstall OpenWave")
            .transient_for(parent)
            .modal(true)
            .default_width(620)
            .default_height(520)
            .build();
        if let Some(application) = parent.application() {
            window.set_application(Some(&application));
        }
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        let heading = gtk::Label::builder()
            .label("Inspecting installation…")
            .xalign(0.0)
            .build();
        heading.add_css_class("title-2");
        content.append(&heading);
        let details = gtk::Label::builder()
            .xalign(0.0)
            .yalign(0.0)
            .wrap(true)
            .selectable(true)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&details)
            .build();
        content.append(&scroll);
        let settings = gtk::CheckButton::with_label("Delete settings and saved scenes");
        settings.set_sensitive(false);
        content.append(&settings);
        let status = gtk::Label::builder().xalign(0.0).wrap(true).build();
        content.append(&status);
        let buttons = gtk::Box::builder()
            .spacing(12)
            .halign(gtk::Align::End)
            .build();
        let cancel = gtk::Button::with_label("Cancel");
        let accept = gtk::Button::with_label("Uninstall");
        accept.add_css_class("destructive-action");
        accept.set_sensitive(false);
        buttons.append(&cancel);
        buttons.append(&accept);
        content.append(&buttons);
        window.set_content(Some(&content));
        window.set_default_widget(Some(&cancel));
        let dialog = Rc::new(Self {
            window,
            heading,
            details,
            settings,
            status,
            accept,
            cancel,
            plan: RefCell::new(None),
            busy: Cell::new(false),
            confirmed: Cell::new(false),
            delete_settings: Cell::new(false),
            closed: Cell::new(false),
        });
        let weak = Rc::downgrade(&dialog);
        dialog.cancel.connect_clicked(move |_| {
            if let Some(dialog) = weak.upgrade() {
                dialog.window.close();
            }
        });
        let weak = Rc::downgrade(&dialog);
        dialog.window.connect_close_request(move |_| {
            let Some(dialog) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            if dialog.busy.get() {
                return glib::Propagation::Stop;
            }
            dialog.closed.set(true);
            on_close(dialog.confirmed.get());
            glib::Propagation::Proceed
        });
        let weak = Rc::downgrade(&dialog);
        dialog.accept.connect_clicked(move |_| {
            let Some(dialog) = weak.upgrade() else {
                return;
            };
            if dialog.busy.get() {
                return;
            }
            let Some(plan) = dialog.plan.borrow().clone() else {
                return;
            };
            if !plan.can_execute() {
                return;
            }
            if !dialog.confirmed.get() {
                dialog.delete_settings.set(dialog.settings.is_active());
            }
            dialog.busy.set(true);
            dialog.accept.set_sensitive(false);
            dialog.cancel.set_sensitive(false);
            dialog.settings.set_sensitive(false);
            dialog.heading.set_label("Stopping OpenWave…");
            dialog.status.set_label(
                "Waiting for device and audio workers. This window will remain responsive.",
            );
            match confirm(plan, dialog.delete_settings.get()) {
                Ok(()) => dialog.confirmed.set(true),
                Err(error) => dialog.failed(&error),
            }
        });
        let (sender, receiver) = async_channel::bounded(1);
        match std::thread::Builder::new()
            .name("openwave-removal-inspect".into())
            .spawn(move || {
                let plan = uninstall::inspect(&paths);
                let description = uninstall::describe(&plan);
                let _ = sender.send_blocking((plan, description));
            }) {
            Ok(worker) => workers.borrow_mut().push(worker),
            Err(error) => dialog.inspection_failed(&error.to_string()),
        }
        let weak = Rc::downgrade(&dialog);
        glib::MainContext::default().spawn_local(async move {
            let result = receiver.recv().await;
            let Some(dialog) = weak.upgrade() else {
                return;
            };
            if dialog.closed.get() {
                return;
            }
            match result {
                Ok((plan, description)) => {
                    dialog.heading.set_label(if plan.remove_application() {
                        "Uninstall OpenWave?"
                    } else {
                        "Prepare OpenWave removal?"
                    });
                    dialog.accept.set_label(if plan.remove_application() {
                        "Uninstall"
                    } else {
                        "Prepare removal"
                    });
                    dialog.details.set_label(&description);
                    dialog.accept.set_sensitive(plan.can_execute());
                    dialog.settings.set_sensitive(plan.can_execute());
                    dialog.status.set_label(if plan.can_execute() {
                        "Settings and saved scenes are preserved unless selected above."
                    } else {
                        "Removal is blocked. Follow the guidance above."
                    });
                    *dialog.plan.borrow_mut() = Some(Arc::new(plan));
                }
                Err(error) => dialog.inspection_failed(&format!(
                    "Installation inspection did not complete: {error}"
                )),
            }
        });
        dialog.window.present();
        dialog.cancel.grab_focus();
        dialog
    }

    pub fn present(&self) {
        self.window.present();
    }
    pub fn is_closed(&self) -> bool {
        self.closed.get()
    }
    pub fn is_busy(&self) -> bool {
        self.busy.get()
    }
    fn inspection_failed(&self, message: &str) {
        self.heading.set_label("Cannot inspect this installation");
        self.details.set_label(message);
        self.settings.set_sensitive(false);
        self.accept.set_sensitive(false);
        self.cancel.set_label("Close");
    }
    pub fn failed(&self, message: &str) {
        self.busy.set(false);
        self.heading.set_label("Removal did not complete");
        self.details.set_label(message);
        self.status.set_label("Routing may be stopped. Retry the remaining steps, or close OpenWave. Restart OpenWave to resume normal operation if it is still installed.");
        self.accept.set_label("Retry");
        self.accept.set_sensitive(true);
        self.cancel.set_label(if self.confirmed.get() {
            "Close OpenWave"
        } else {
            "Cancel"
        });
        self.cancel.set_sensitive(true);
        self.cancel.grab_focus();
    }
    pub fn completed(&self, result: &UninstallResult) {
        let mut details = result.removed.join("\n");
        if !result.guidance.is_empty() {
            details.push_str("\n\n");
            details.push_str(&result.guidance);
        }
        if !result.success {
            self.failed(&format!(
                "{}\n\nAlready completed:\n{}",
                result
                    .error
                    .as_deref()
                    .unwrap_or("Some removal steps did not complete."),
                details
            ));
            return;
        }
        self.busy.set(false);
        self.heading.set_label(if result.app_removed {
            "OpenWave uninstalled"
        } else {
            "Removal preparation complete"
        });
        if !result.app_removed {
            details.push_str("\n\nThe application has not been removed.");
        }
        self.details.set_label(details.trim());
        self.status.set_label(if self.delete_settings.get() {
            "Settings deletion was requested."
        } else {
            "Your settings and saved scenes were preserved."
        });
        self.accept.set_visible(false);
        self.cancel.set_label("Close OpenWave");
        self.cancel.set_sensitive(true);
        self.cancel.grab_focus();
    }
}
