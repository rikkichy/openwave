//! GTK owns presentation only. Runtime commands and completions delimit every mutation.
use crate::{
    Submit,
    actions::{ActionCallbacks, ActionRegistry},
    icons::Icons,
    tray::{Tray, TrayCallbacks},
    ui::{dialogs, matrix::MatrixView, sidebar::Sidebar, uninstall::UninstallDialog},
};
use adw::prelude::*;
use openwave_core::model::*;
use openwave_runtime::{
    controller::{AppCommand, RuntimeEvent, RuntimeHandle},
    paths::RuntimePaths,
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    thread::JoinHandle,
    time::Duration,
};

pub fn run(args: Vec<String>) -> i32 {
    if let Err(error) = openwave_runtime::process::require_user() {
        eprintln!("openwave: {error}");
        return 1;
    }
    let paths = match RuntimePaths::discover() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("openwave: {error}");
            return 1;
        }
    };
    let _ = env_logger::try_init();
    let application = adw::Application::builder()
        .application_id("com.github.openwave")
        .flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE)
        .build();
    application.add_main_option(
        "hide",
        glib::Char::from(b'\0'),
        glib::OptionFlags::NONE,
        glib::OptionArg::None,
        "Start hidden when a system tray host is available",
        None,
    );
    let state: Rc<RefCell<Option<Rc<AppUi>>>> = Rc::new(RefCell::new(None));
    let failed = Rc::new(Cell::new(false));
    let state_start = state.clone();
    let failed_start = failed.clone();
    application.connect_startup(move |application| {
        let owner = application
            .dbus_connection()
            .and_then(|connection| connection.unique_name())
            .map(|name| name.to_string());
        let (handle, events) = match RuntimeHandle::launch(paths.clone(), owner) {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("openwave: {error}");
                failed_start.set(true);
                application.quit();
                return;
            }
        };
        let ui = AppUi::new(application.clone(), paths.clone(), handle);
        *state_start.borrow_mut() = Some(ui.clone());
        let weak = Rc::downgrade(&ui);
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = events.recv().await {
                let Some(ui) = weak.upgrade() else {
                    break;
                };
                ui.event(event);
            }
        });
    });
    let state_command = state.clone();
    application.connect_command_line(move |application, command| {
        if let Some(ui) = state_command.borrow().as_ref() {
            let hide = command
                .options_dict()
                .lookup::<bool>("hide")
                .ok()
                .flatten()
                .unwrap_or(false);
            ui.start_hidden.set(hide && !ui.activated.get());
        }
        application.activate();
        glib::ExitCode::SUCCESS
    });
    let state_activate = state.clone();
    application.connect_activate(move |_| {
        if let Some(ui) = state_activate.borrow().as_ref() {
            ui.activate();
        }
    });
    let code: i32 = application.run_with_args(&args).into();
    // Application::shutdown must not join workers. Even unusual loop exits drain here,
    // after GTK has returned, and retain ownership of temporary inspection threads.
    if let Some(ui) = state.borrow_mut().take() {
        if let Some(tray) = ui.tray.borrow().as_ref() {
            tray.shutdown();
        }
        ui.tray_hold.borrow_mut().take();
        if ui.fatal.get() {
            failed.set(true);
        }
        if ui.handle.snapshot().lifecycle != Lifecycle::Stopped {
            let _ = ui.handle.submit(AppCommand::Shutdown);
        }
        let stopped = ui.handle.wait_stopped();
        for worker in ui.inspection_workers.borrow_mut().drain(..) {
            if worker.join().is_err() {
                eprintln!("openwave: removal inspection worker panicked");
                failed.set(true);
            }
        }
        if let Err(error) = stopped {
            eprintln!("openwave: {error}");
            failed.set(true);
        }
    }
    if failed.get() { 1 } else { code }
}

struct AppUi {
    application: adw::Application,
    paths: RuntimePaths,
    handle: RuntimeHandle,
    window: adw::ApplicationWindow,
    matrix: MatrixView,
    sidebar: Sidebar,
    split: adw::OverlaySplitView,
    title: adw::WindowTitle,
    warning: gtk::MenuButton,
    warning_text: gtk::Label,
    scene_button: gtk::MenuButton,
    status: gtk::Label,
    submit: Submit,
    registry: RefCell<Option<ActionRegistry>>,
    tray: RefCell<Option<Tray>>,
    tray_hold: RefCell<Option<gio::ApplicationHoldGuard>>,
    latest: RefCell<Arc<AppSnapshot>>,
    rendered_revision: Cell<Option<u64>>,
    render_pending: Cell<bool>,
    activated: Cell<bool>,
    main_presented: Cell<bool>,
    start_hidden: Cell<bool>,
    shutdown_requested: Cell<bool>,
    stopped: Cell<bool>,
    fatal: Cell<bool>,
    geometry_restored: Cell<bool>,
    prepare_command: Cell<Option<CommandId>>,
    uninstall_command: Cell<Option<CommandId>>,
    uninstall_result: Cell<bool>,
    uninstall: RefCell<Option<Rc<UninstallDialog>>>,
    inspection_workers: Rc<RefCell<Vec<JoinHandle<()>>>>,
    setup_phase: RefCell<Option<SetupPhase>>,
    setup_dialog: RefCell<Option<adw::AlertDialog>>,
    setup_generation: Cell<u64>,
    calibration: RefCell<Option<CalibrationSnapshot>>,
    calibration_dialog: RefCell<Option<adw::AlertDialog>>,
    calibration_generation: Cell<u64>,
}

impl AppUi {
    fn new(application: adw::Application, paths: RuntimePaths, handle: RuntimeHandle) -> Rc<Self> {
        if let Ok(css) = paths.data_file("style.css") {
            let provider = gtk::CssProvider::new();
            provider.load_from_path(css);
            if let Some(display) = gtk::gdk::Display::default() {
                gtk::style_context_add_provider_for_display(
                    &display,
                    &provider,
                    gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
                );
            }
        }
        let icons = Rc::new(Icons::new(paths.clone()));
        let snapshot = handle.snapshot();
        let ui = Rc::new_cyclic(|weak: &std::rc::Weak<Self>| {
            let target = weak.clone();
            let submit: Submit = Rc::new(move |command| {
                if let Some(ui) = target.upgrade() {
                    ui.submit_command(command);
                }
            });
            let window = adw::ApplicationWindow::builder()
                .application(&application)
                .title("OpenWave")
                .default_width(snapshot.preferences.width.max(820))
                .default_height(snapshot.preferences.height.max(480))
                .build();
            window.set_size_request(820, 480);
            if snapshot.preferences.maximized {
                window.maximize();
            }
            let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let header = adw::HeaderBar::new();
            let title = adw::WindowTitle::new("OpenWave", "Disconnected");
            header.set_title_widget(Some(&title));
            let warning = gtk::MenuButton::builder()
                .icon_name("dialog-warning-symbolic")
                .visible(false)
                .tooltip_text("Connection, service and routing status")
                .build();
            let warning_text = gtk::Label::builder()
                .xalign(0.0)
                .wrap(true)
                .selectable(true)
                .max_width_chars(48)
                .margin_top(12)
                .margin_bottom(12)
                .margin_start(12)
                .margin_end(12)
                .build();
            let popover = gtk::Popover::new();
            popover.set_child(Some(&warning_text));
            warning.set_popover(Some(&popover));
            header.pack_start(&warning);
            let scene_button = gtk::MenuButton::builder().label("Scenes").build();
            header.pack_start(&scene_button);
            let menu = gio::Menu::new();
            menu.append(Some("Settings"), Some("win.settings"));
            menu.append(Some("Uninstall OpenWave…"), Some("app.uninstall"));
            let menu_button = gtk::MenuButton::builder()
                .icon_name("open-menu-symbolic")
                .menu_model(&menu)
                .tooltip_text("Application menu")
                .build();
            header.pack_end(&menu_button);
            let reconnect = gtk::Button::builder()
                .icon_name("view-refresh-symbolic")
                .tooltip_text("Reconnect")
                .build();
            let send = submit.clone();
            reconnect.connect_clicked(move |_| send(AppCommand::Reconnect));
            header.pack_end(&reconnect);
            let toggle = gtk::ToggleButton::builder()
                .icon_name("sidebar-show-symbolic")
                .tooltip_text("Toggle device panel")
                .build();
            header.pack_end(&toggle);
            content.append(&header);
            let status = gtk::Label::builder()
                .xalign(0.0)
                .wrap(true)
                .visible(false)
                .margin_start(12)
                .margin_end(12)
                .build();
            status.add_css_class("dim-label");
            content.append(&status);
            let split = adw::OverlaySplitView::builder()
                .sidebar_position(gtk::PackType::End)
                .min_sidebar_width(320.0)
                .max_sidebar_width(420.0)
                .sidebar_width_fraction(0.30)
                .vexpand(true)
                .show_sidebar(false)
                .build();
            toggle
                .bind_property("active", &split, "show-sidebar")
                .bidirectional()
                .sync_create()
                .build();
            if let Ok(condition) = adw::BreakpointCondition::parse("max-width: 900sp") {
                let breakpoint = adw::Breakpoint::new(condition);
                breakpoint.add_setter(&split, "collapsed", Some(&true.to_value()));
                window.add_breakpoint(breakpoint);
            }
            let matrix = MatrixView::new(icons.clone(), submit.clone());
            let sidebar = Sidebar::new(icons.clone(), submit.clone());
            split.set_content(Some(&matrix.widget));
            split.set_sidebar(Some(&sidebar.widget));
            content.append(&split);
            window.set_content(Some(&content));
            Self {
                application: application.clone(),
                paths: paths.clone(),
                handle: handle.clone(),
                window,
                matrix,
                sidebar,
                split,
                title,
                warning,
                warning_text,
                scene_button,
                status,
                submit,
                registry: RefCell::new(None),
                tray: RefCell::new(None),
                tray_hold: RefCell::new(None),
                latest: RefCell::new(snapshot.clone()),
                rendered_revision: Cell::new(None),
                render_pending: Cell::new(false),
                activated: Cell::new(false),
                main_presented: Cell::new(false),
                start_hidden: Cell::new(false),
                shutdown_requested: Cell::new(false),
                stopped: Cell::new(false),
                prepare_command: Cell::new(None),
                fatal: Cell::new(false),
                geometry_restored: Cell::new(false),
                uninstall_command: Cell::new(None),
                uninstall_result: Cell::new(false),
                uninstall: RefCell::new(None),
                inspection_workers: Rc::new(RefCell::new(Vec::new())),
                setup_phase: RefCell::new(None),
                setup_dialog: RefCell::new(None),
                setup_generation: Cell::new(0),
                calibration: RefCell::new(None),
                calibration_dialog: RefCell::new(None),
                calibration_generation: Cell::new(0),
            }
        });
        let weak = Rc::downgrade(&ui);
        ui.window.connect_close_request(move |_| {
            if let Some(ui) = weak.upgrade() {
                if let Some(dialog) = ui
                    .uninstall
                    .borrow()
                    .as_ref()
                    .filter(|dialog| !dialog.is_closed())
                {
                    dialog.present();
                    return glib::Propagation::Stop;
                }
                if ui.tray_active()
                    && ui.latest.borrow().setup_phase == SetupPhase::Ready
                    && !ui.shutdown_requested.get()
                    && matches!(
                        ui.handle.snapshot().lifecycle,
                        Lifecycle::Starting | Lifecycle::Running
                    )
                {
                    ui.save_geometry();
                    ui.window.set_visible(false);
                } else {
                    ui.request_quit();
                }
            }
            glib::Propagation::Stop
        });
        let settings = gio::SimpleAction::new("settings", None);
        let weak = Rc::downgrade(&ui);
        settings.connect_activate(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                ui.present_main_window(false);
                ui.split.set_show_sidebar(true);
                ui.sidebar.render(ui.handle.snapshot());
                ui.sidebar.focus_settings();
            }
        });
        ui.window.add_action(&settings);
        let save = gio::SimpleAction::new("save-scene-as", None);
        let weak = Rc::downgrade(&ui);
        save.connect_activate(move |_, _| {
            if let Some(ui) = weak.upgrade() {
                if !ui.shutdown_requested.get() {
                    dialogs::save_scene(ui.window.upcast_ref(), ui.submit.clone());
                }
            }
        });
        ui.window.add_action(&save);
        let uninstall_target = Rc::downgrade(&ui);
        let prepare_target = Rc::downgrade(&ui);
        *ui.registry.borrow_mut() = Some(ActionRegistry::register(
            &application,
            handle,
            ActionCallbacks {
                uninstall: Rc::new(move || {
                    if let Some(ui) = uninstall_target.upgrade() {
                        ui.show_uninstall();
                    }
                }),
                prepare_uninstall: Rc::new(move |identity| {
                    if let Some(ui) = prepare_target.upgrade() {
                        ui.prepare_uninstall(identity);
                    }
                }),
            },
        ));
        if let Some(connection) = application.dbus_connection() {
            let open = Rc::downgrade(&ui);
            let toggle = Rc::downgrade(&ui);
            let quit = Rc::downgrade(&ui);
            let host = Rc::downgrade(&ui);
            match Tray::new(
                connection,
                icons,
                TrayCallbacks {
                    open: Rc::new(move || {
                        if let Some(ui) = open.upgrade() {
                            ui.activate();
                        }
                    }),
                    toggle_mute: Rc::new(move |unit| {
                        if let Some(ui) = toggle.upgrade() {
                            ui.submit_command(AppCommand::ToggleDeviceMute { unit });
                        }
                    }),
                    quit: Rc::new(move || {
                        if let Some(ui) = quit.upgrade() {
                            ui.request_quit();
                        }
                    }),
                    host_changed: Rc::new(move |active| {
                        if let Some(ui) = host.upgrade() {
                            ui.host_changed(active);
                        }
                    }),
                },
            ) {
                Ok(tray) => {
                    let active = tray.host_active();
                    *ui.tray.borrow_mut() = Some(tray);
                    ui.host_changed(active);
                }
                Err(error) => log::warn!("Tray unavailable: {error}"),
            }
        }
        ui
    }

    fn activate(self: &Rc<Self>) {
        if self.stopped.get() {
            return;
        }
        if let Some(dialog) = self
            .uninstall
            .borrow()
            .as_ref()
            .filter(|dialog| !dialog.is_closed())
        {
            dialog.present();
            return;
        }
        let first = !self.activated.replace(true);
        if !first {
            self.start_hidden.set(false);
        }
        let snapshot = self.handle.snapshot();
        *self.latest.borrow_mut() = Arc::clone(&snapshot);
        if snapshot.setup_phase == SetupPhase::Ready {
            self.present_main_window(first && self.start_hidden.get());
        } else {
            self.window.present();
            self.render_setup(&snapshot.setup_phase);
        }
        self.refresh(snapshot);
    }

    /// Ordinary activation and first-run Continue converge here: no alternate window lifecycle.
    fn present_main_window(self: &Rc<Self>, hide_requested: bool) {
        if self.latest.borrow().setup_phase != SetupPhase::Ready {
            self.window.present();
            return;
        }
        self.main_presented.set(true);
        self.render_widgets();
        self.start_hidden.set(hide_requested);
        if hide_requested && self.tray_active() {
            self.window.set_visible(false);
            self.start_hidden.set(false);
        } else {
            self.window.present();
            if self.tray_known() {
                self.start_hidden.set(false);
            }
        }
    }

    fn tray_active(&self) -> bool {
        self.tray.borrow().as_ref().is_some_and(Tray::host_active)
    }
    fn tray_known(&self) -> bool {
        self.tray.borrow().as_ref().is_none_or(Tray::host_known)
    }
    fn host_changed(self: &Rc<Self>, active: bool) {
        if active && !self.stopped.get() {
            if self.tray_hold.borrow().is_none() {
                *self.tray_hold.borrow_mut() = Some(self.application.hold());
            }
            if self.start_hidden.get()
                && self.main_presented.get()
                && self.latest.borrow().setup_phase == SetupPhase::Ready
            {
                self.window.set_visible(false);
                self.start_hidden.set(false);
            }
        } else {
            self.start_hidden.set(false);
            // Present before releasing the last application hold.
            if self.activated.get() && !self.stopped.get() && !self.window.is_visible() {
                self.window.present();
                self.render_widgets();
            }
            self.tray_hold.borrow_mut().take();
        }
    }
    fn submit_command(self: &Rc<Self>, command: AppCommand) {
        if let Err(error) = self.handle.submit(command) {
            self.error("Operation was not accepted", &error.to_string());
        }
    }
    fn save_geometry(&self) {
        let snapshot = self.handle.snapshot();
        if !matches!(snapshot.lifecycle, Lifecycle::Running | Lifecycle::Starting) {
            return;
        }
        let maximized = self.window.is_maximized();
        let changes = PreferencesEdit {
            width: (!maximized).then(|| self.window.width().max(820)),
            height: (!maximized).then(|| self.window.height().max(480)),
            maximized: Some(maximized),
            ..Default::default()
        };
        if let Err(error) = self.handle.submit(AppCommand::SetPreferences { changes }) {
            log::warn!("Window geometry was not saved: {error}");
        }
    }
    fn request_quit(self: &Rc<Self>) {
        if let Some(dialog) = self
            .uninstall
            .borrow()
            .as_ref()
            .filter(|dialog| dialog.is_busy())
        {
            dialog.present();
            return;
        }
        if self.stopped.get() {
            self.application.quit();
            return;
        }
        if self.shutdown_requested.replace(true) {
            return;
        }
        self.save_geometry();
        match self.handle.submit(AppCommand::Shutdown) {
            Ok(_) => {
                self.set_status("Stopping OpenWave; waiting for owned device and audio workers…")
            }
            Err(error) => {
                self.shutdown_requested.set(false);
                self.error("Cannot stop OpenWave", &error.to_string());
            }
        }
    }
    fn prepare_uninstall(self: &Rc<Self>, identity: String) {
        if identity != self.paths.identity.to_string_lossy() {
            if let Some(registry) = self.registry.borrow().as_ref() {
                registry.set_uninstall_state(
                    "error:Installation identity does not match this running OpenWave",
                );
            }
            return;
        }
        if let Some(dialog) = self
            .uninstall
            .borrow()
            .as_ref()
            .filter(|dialog| !dialog.is_closed())
        {
            if let Some(registry) = self.registry.borrow().as_ref() {
                registry.set_uninstall_state(
                    "error:An interactive removal dialog is open; finish or cancel it before preparing removal",
                );
            }
            dialog.present();
            return;
        }
        self.save_geometry();
        match self.handle.submit(AppCommand::PrepareUninstall {
            canonical_identity: identity,
        }) {
            Ok(id) => {
                self.prepare_command.set(Some(id));
                self.shutdown_requested.set(true);
                if let Some(registry) = self.registry.borrow().as_ref() {
                    registry.set_uninstall_state("stopping");
                }
            }
            Err(error) => {
                if let Some(registry) = self.registry.borrow().as_ref() {
                    registry.set_uninstall_state(&format!("error:{error}"));
                }
            }
        }
    }
    fn show_uninstall(self: &Rc<Self>) {
        if let Some(dialog) = self
            .uninstall
            .borrow()
            .as_ref()
            .filter(|dialog| !dialog.is_closed())
        {
            dialog.present();
            return;
        }
        if self.shutdown_requested.get() {
            return;
        }
        self.setup_generation
            .set(self.setup_generation.get().wrapping_add(1));
        if let Some(dialog) = self.setup_dialog.borrow_mut().take() {
            dialog.close();
        }
        let confirm = Rc::downgrade(self);
        let closed = Rc::downgrade(self);
        let dialog = UninstallDialog::new(
            self.window.upcast_ref(),
            self.paths.clone(),
            Rc::new(move |plan, delete_settings| {
                let Some(ui) = confirm.upgrade() else {
                    return Err("OpenWave is no longer available".into());
                };
                ui.save_geometry();
                let id = ui
                    .handle
                    .submit(AppCommand::ConfirmUninstall {
                        plan,
                        delete_settings,
                    })
                    .map_err(|error| error.to_string())?;
                ui.uninstall_command.set(Some(id));
                ui.uninstall_result.set(false);
                ui.shutdown_requested.set(true);
                Ok(())
            }),
            Rc::new(move |confirmed| {
                if let Some(ui) = closed.upgrade() {
                    if ui.stopped.get() {
                        ui.application.quit();
                    } else if confirmed {
                        ui.shutdown_requested.set(false);
                        ui.request_quit();
                    } else {
                        *ui.setup_phase.borrow_mut() = None;
                        ui.render_setup(&ui.handle.snapshot().setup_phase);
                    }
                }
            }),
            self.inspection_workers.clone(),
        );
        *self.uninstall.borrow_mut() = Some(dialog);
    }

    fn event(self: &Rc<Self>, event: RuntimeEvent) {
        match event {
            RuntimeEvent::SnapshotChanged => self.refresh(self.handle.snapshot()),
            RuntimeEvent::UninstallFinished(result) => {
                self.uninstall_result.set(true);
                if let Some(dialog) = self.uninstall.borrow().as_ref() {
                    dialog.completed(&result);
                }
            }
            RuntimeEvent::CommandFinished { id, result } => {
                if self.uninstall_command.get() == Some(id) {
                    if let CommandOutcome::Rejected(error) = &result {
                        if !self.uninstall_result.get() {
                            if let Some(dialog) = self.uninstall.borrow().as_ref() {
                                dialog.failed(&error.to_string());
                            }
                        }
                    }
                    self.uninstall_command.set(None);
                } else if self.prepare_command.get() == Some(id) {
                    if let CommandOutcome::Rejected(error) = &result {
                        if let Some(registry) = self.registry.borrow().as_ref() {
                            registry.set_uninstall_state(&format!("error:{error}"));
                        }
                    }
                } else {
                    match result {
                        CommandOutcome::Rejected(error) => {
                            if !self.handle.snapshot().scene_pending
                                && self.status.text()
                                    == "Applying scene; waiting for all device operations…"
                            {
                                self.set_status("Scene could not be completed.");
                            }
                            self.error("Operation could not be completed", &error.to_string());
                        }
                        CommandOutcome::SceneFinished {
                            skipped, failed, ..
                        } => {
                            if skipped.is_empty() && failed.is_empty() {
                                self.set_status("Scene applied.");
                            } else {
                                self.set_status("Scene finished with skipped or failed targets.");
                                let mut details = String::new();
                                for (label, issues) in [("Skipped", skipped), ("Failed", failed)] {
                                    for issue in issues {
                                        details.push_str(&format!(
                                            "{label} — {}: {}\n",
                                            issue.target, issue.message
                                        ));
                                    }
                                }
                                self.error("Scene partially applied", details.trim());
                            }
                        }
                        CommandOutcome::Applied { .. } | CommandOutcome::Cancelled => {}
                    }
                }
                self.refresh(self.handle.snapshot());
            }
            RuntimeEvent::ShutdownFinished(result) => match result {
                Ok(()) => {
                    self.stopped.set(true);
                    if let Some(tray) = self.tray.borrow().as_ref() {
                        tray.shutdown();
                    }
                    self.tray_hold.borrow_mut().take();
                    if self
                        .uninstall
                        .borrow()
                        .as_ref()
                        .is_some_and(|dialog| !dialog.is_closed())
                    {
                        self.window.set_visible(false);
                    } else {
                        self.application.quit();
                    }
                }
                Err(error) => {
                    self.shutdown_requested.set(false);
                    if self.handle.snapshot().lifecycle == Lifecycle::Stopped {
                        self.fatal.set(true);
                        self.stopped.set(true);
                        self.setup_generation
                            .set(self.setup_generation.get().wrapping_add(1));
                        if let Some(dialog) = self.setup_dialog.borrow_mut().take() {
                            dialog.close();
                        }
                        self.error("OpenWave could not start", &error.to_string());
                        return;
                    }
                    if let Some(registry) = self.registry.borrow().as_ref() {
                        if self.prepare_command.get().is_some() {
                            registry.set_uninstall_state(&format!("error:{error}"));
                        }
                    }
                    if let Some(dialog) = self
                        .uninstall
                        .borrow()
                        .as_ref()
                        .filter(|dialog| !dialog.is_closed())
                    {
                        dialog.failed(&error.to_string());
                    } else {
                        self.error("Shutdown incomplete", &format!("{error}\n\nMutations remain frozen. Close the window again to retry stopping owned workers."));
                    }
                }
            },
        }
    }
    fn refresh(self: &Rc<Self>, snapshot: Arc<AppSnapshot>) {
        if let Some(registry) = self.registry.borrow().as_ref() {
            registry.refresh(&snapshot);
        }
        if let Some(tray) = self.tray.borrow().as_ref() {
            tray.update(&snapshot);
        }
        *self.latest.borrow_mut() = snapshot.clone();
        if snapshot.revision > 0 && !self.geometry_restored.replace(true) {
            self.window.set_default_size(
                snapshot.preferences.width.max(820),
                snapshot.preferences.height.max(480),
            );
            if snapshot.preferences.maximized {
                self.window.maximize();
            } else {
                self.window.unmaximize();
            }
        }
        if self.activated.get() {
            self.render_setup(&snapshot.setup_phase);
            self.render_calibration(snapshot.calibration.as_ref());
        }
        if snapshot.scene_pending {
            self.set_status("Applying scene; waiting for all device operations…");
        }
        if !self.render_pending.replace(true) {
            let weak = Rc::downgrade(self);
            glib::timeout_add_local_once(Duration::from_millis(50), move || {
                if let Some(ui) = weak.upgrade() {
                    ui.render_pending.set(false);
                    if ui.window.is_visible() {
                        ui.render_widgets();
                    }
                }
            });
        }
    }
    fn render_widgets(&self) {
        let snapshot = self.latest.borrow().clone();
        self.matrix.render(snapshot.clone());
        self.sidebar.render(snapshot.clone());
        let active = matches!(snapshot.lifecycle, Lifecycle::Starting | Lifecycle::Running)
            && snapshot.setup_phase == SetupPhase::Ready;
        self.matrix.widget.set_sensitive(active);
        self.sidebar.widget.set_sensitive(active);
        let subtitle = snapshot
            .selected_unit
            .and_then(|id| snapshot.units.iter().find(|unit| unit.id == id))
            .map(|unit| {
                let name = unit.id.profile.profile().display_name;
                if snapshot.units.len() > 1 {
                    format!("{name} · {} connected devices", snapshot.units.len())
                } else {
                    format!("{name} · Connected")
                }
            })
            .unwrap_or_else(|| "Disconnected".to_string());
        if self.title.subtitle() != subtitle {
            self.title.set_subtitle(&subtitle);
        }
        let mut warnings = snapshot.service_status.clone();
        for issue in snapshot
            .errors
            .iter()
            .chain(snapshot.units.iter().flat_map(|unit| unit.errors.iter()))
        {
            if !warnings.is_empty() {
                warnings.push_str("\n\n");
            }
            warnings.push_str(&format!("{}: {}", issue.target, issue.message));
        }
        if self.warning_text.text() != warnings {
            self.warning_text.set_label(&warnings);
        }
        self.warning.set_visible(!warnings.is_empty());
        if self.rendered_revision.replace(Some(snapshot.revision)) != Some(snapshot.revision) {
            let menu = gio::Menu::new();
            let recall = gio::Menu::new();
            let delete = gio::Menu::new();
            let mut scenes: Vec<_> = snapshot.desired.scenes.iter().collect();
            scenes.sort_by_key(|(_, scene)| scene.name.to_lowercase());
            for (id, scene) in scenes {
                for (section, action) in
                    [(&recall, "app.apply-scene"), (&delete, "app.delete-scene")]
                {
                    let item = gio::MenuItem::new(Some(&scene.name), None);
                    item.set_action_and_target_value(Some(action), Some(&id.as_str().to_variant()));
                    section.append_item(&item);
                }
            }
            if recall.n_items() > 0 {
                menu.append_section(None, &recall);
            }
            let manage = gio::Menu::new();
            manage.append(Some("Save current as…"), Some("win.save-scene-as"));
            if delete.n_items() > 0 {
                manage.append_submenu(Some("Delete scene"), &delete);
            }
            menu.append_section(None, &manage);
            self.scene_button.set_menu_model(Some(&menu));
        }
    }
    fn set_status(&self, text: &str) {
        self.status.set_label(text);
        self.status.set_visible(!text.is_empty());
    }
    fn error(&self, heading: &str, message: &str) {
        self.window.present();
        dialogs::show_error(self.window.upcast_ref(), heading, message);
    }

    fn render_setup(self: &Rc<Self>, phase: &SetupPhase) {
        if self.stopped.get()
            || self.shutdown_requested.get()
            || self
                .uninstall
                .borrow()
                .as_ref()
                .is_some_and(|dialog| !dialog.is_closed())
        {
            return;
        }
        if self.setup_phase.borrow().as_ref() == Some(phase) {
            return;
        }
        *self.setup_phase.borrow_mut() = Some(phase.clone());
        let generation = self.setup_generation.get().wrapping_add(1);
        self.setup_generation.set(generation);
        if let Some(dialog) = self.setup_dialog.borrow_mut().take() {
            dialog.close();
        }
        if phase == &SetupPhase::Ready {
            if !self.main_presented.get() {
                self.present_main_window(self.start_hidden.get());
            }
            return;
        }
        let (heading, body) = match phase {
            SetupPhase::Checking => ("Checking OpenWave setup", "Inspecting USB permissions, audio configuration and the capture service. No device controls are opened during setup inspection.".to_string()),
            SetupPhase::Required => ("First-Time Setup", "OpenWave needs to configure USB permissions and install the audio service.\n\nYou may be prompted for your password.".to_string()),
            SetupPhase::Running => ("Setting Up OpenWave", "Configuring USB permissions, user audio rules, mixes and the capture service. This window remains responsive.".to_string()),
            SetupPhase::Replug(message) => ("Setup Complete", format!("{message}\n\nPlease replug your Elgato Wave device, then click Continue.")),
            SetupPhase::Failed(message) => ("Setup Failed", message.clone()),
            SetupPhase::Starting => ("Starting OpenWave", "Opening the device and audio workers. No host setup changes are made.".to_string()),
            SetupPhase::ActivationFailed(message) => ("OpenWave could not start", format!("{message}\n\nResolve the problem, then retry starting OpenWave.")),
            SetupPhase::Ready => return,
        };
        let dialog = adw::AlertDialog::builder()
            .heading(heading)
            .body(&body)
            .build();
        dialog.add_response(
            "cancel",
            if matches!(phase, SetupPhase::Failed(_)) {
                "Close"
            } else {
                "Cancel"
            },
        );
        dialog.set_close_response("cancel");
        if !matches!(
            phase,
            SetupPhase::Checking | SetupPhase::Running | SetupPhase::Starting
        ) {
            dialog.add_response("uninstall", "Uninstall OpenWave…");
        }
        match phase {
            SetupPhase::Required | SetupPhase::Failed(_) => {
                dialog.add_response(
                    "setup",
                    if phase == &SetupPhase::Required {
                        "Set Up"
                    } else {
                        "Retry Setup"
                    },
                );
                dialog.set_response_appearance("setup", adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("setup"));
            }
            SetupPhase::Replug(_) | SetupPhase::ActivationFailed(_) => {
                dialog.add_response(
                    "continue",
                    if matches!(phase, SetupPhase::ActivationFailed(_)) {
                        "Retry"
                    } else {
                        "Continue"
                    },
                );
                dialog.set_response_appearance("continue", adw::ResponseAppearance::Suggested);
                dialog.set_default_response(Some("continue"));
            }
            SetupPhase::Checking | SetupPhase::Running | SetupPhase::Starting => {
                let spinner = gtk::Spinner::new();
                spinner.start();
                dialog.set_extra_child(Some(&spinner));
            }
            SetupPhase::Ready => {}
        }
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.setup_generation.get() != generation {
                return;
            }
            match response {
                "setup" => ui.submit_command(AppCommand::RunSetup),
                "continue" => {
                    // Snapshot notifications may coalesce Starting and an identical failure.
                    // The dismissed dialog is no longer a presentation of that failure.
                    *ui.setup_phase.borrow_mut() = None;
                    ui.submit_command(AppCommand::ContinueSetup);
                }
                "uninstall" => ui.show_uninstall(),
                _ => ui.request_quit(),
            }
        });
        self.window.present();
        dialog.present(Some(&self.window));
        *self.setup_dialog.borrow_mut() = Some(dialog);
    }

    fn render_calibration(self: &Rc<Self>, calibration: Option<&CalibrationSnapshot>) {
        if self.calibration.borrow().as_ref() == calibration {
            return;
        }
        *self.calibration.borrow_mut() = calibration.cloned();
        let generation = self.calibration_generation.get().wrapping_add(1);
        self.calibration_generation.set(generation);
        if let Some(dialog) = self.calibration_dialog.borrow_mut().take() {
            dialog.close();
        }
        let Some(calibration) = calibration else {
            return;
        };
        if self.shutdown_requested.get() {
            return;
        }
        let (heading, body) = match &calibration.phase {
            CalibrationPhase::NoiseReady => ("Measure room noise", "Stay quiet for three seconds after pressing Record. Only the raw microphone is measured. Current effects and hardware gain stay unchanged; proposed settings require explicit confirmation.".to_string()),
            CalibrationPhase::RecordingNoise => ("Recording room noise", "Please stay quiet.".to_string()),
            CalibrationPhase::SpeechReady => ("Measure speech", "Speak normally for five seconds after pressing Record.".to_string()),
            CalibrationPhase::RecordingSpeech => ("Recording speech", "Speak normally.".to_string()),
            CalibrationPhase::Review { proposal, summary } => ("Review calibration", format!("{summary}\n\nGate: {:.1} dB\nCompressor: {:.1} dB, {:.1}:1\nLow cut: {} Hz\nHigh shelf: {:+.0} dB{}", proposal.gate_thresh, proposal.comp_thresh, proposal.comp_ratio, proposal.lowcut, proposal.eq_high, if proposal.mono { "\nMono: enabled" } else { "" })),
            CalibrationPhase::Expired(message) => ("Calibration expired", format!("{message}\n\nNo proposed settings were applied. Repeat the measurements with the current input.")),
        };
        let dialog = adw::AlertDialog::builder()
            .heading(heading)
            .body(&body)
            .build();
        dialog.add_response(
            "cancel",
            if matches!(calibration.phase, CalibrationPhase::Review { .. }) {
                "Keep current settings"
            } else {
                "Cancel"
            },
        );
        dialog.set_close_response("cancel");
        dialog.set_default_response(Some("cancel"));
        match calibration.phase {
            CalibrationPhase::NoiseReady | CalibrationPhase::SpeechReady => {
                dialog.add_response("record", "Record");
                dialog.set_response_appearance("record", adw::ResponseAppearance::Suggested);
            }
            CalibrationPhase::Review { .. } => {
                dialog.add_response("apply", "Apply");
                dialog.set_response_appearance("apply", adw::ResponseAppearance::Suggested);
            }
            CalibrationPhase::RecordingNoise | CalibrationPhase::RecordingSpeech => {
                let spinner = gtk::Spinner::new();
                spinner.start();
                dialog.set_extra_child(Some(&spinner));
            }
            CalibrationPhase::Expired(_) => {}
        }
        let token = calibration.token.clone();
        let phase = calibration.phase.clone();
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.calibration_generation.get() != generation {
                return;
            }
            let command = match (response, &phase) {
                ("record", CalibrationPhase::NoiseReady) => AppCommand::RecordNoise {
                    token: token.clone(),
                },
                ("record", CalibrationPhase::SpeechReady) => AppCommand::RecordSpeech {
                    token: token.clone(),
                },
                ("apply", CalibrationPhase::Review { .. }) => AppCommand::AcceptCalibration {
                    token: token.clone(),
                },
                _ => AppCommand::CancelCalibration {
                    token: token.clone(),
                },
            };
            ui.submit_command(command);
        });
        self.window.present();
        dialog.present(Some(&self.window));
        *self.calibration_dialog.borrow_mut() = Some(dialog);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_support::Rig;
    use std::time::Instant;

    fn fixture() -> (Rig, Rc<AppUi>) {
        adw::init().expect("private GTK display");
        let rig = Rig::new(serde_json::json!({}), vec![]);
        let application = adw::Application::builder()
            .application_id("com.github.openwave.WidgetTest")
            .build();
        application.register(None::<&gio::Cancellable>).unwrap();
        let ui = AppUi::new(application, rig.paths(), rig.handle());
        ui.window.present();
        (rig, ui)
    }

    fn until(rig: &Rig, ui: &Rc<AppUi>, condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            for event in rig.events() {
                ui.event(event);
            }
            while glib::MainContext::default().pending() {
                glib::MainContext::default().iteration(false);
            }
            rig.snapshot();
            if condition() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "GTK runtime observation deadline"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    #[ignore = "requires the isolated installed GTK test runner"]
    fn open_removal_dialog_reports_immediate_prepare_conflict() {
        let (rig, ui) = fixture();
        ui.application.activate_action("uninstall", None);
        let dialog = ui.uninstall.borrow().as_ref().unwrap().clone();
        assert!(!dialog.is_closed() && !dialog.is_busy());
        let identity = ui.paths.identity.to_string_lossy().to_string().to_variant();
        ui.application
            .activate_action("prepare-uninstall", Some(&identity));
        let state = ui
            .application
            .action_state("prepare-uninstall")
            .unwrap()
            .get::<String>()
            .unwrap();
        assert!(
            state.starts_with("error:"),
            "public caller needs an immediate conflict, got {state}"
        );
        assert!(!ui.shutdown_requested.get());
        assert_eq!(
            rig.shutdown_attempts(),
            0,
            "read-only confirmation must not freeze the runtime"
        );
        assert!(dialog.window.is_visible());
        dialog.window.close();
        until(&rig, &ui, || dialog.is_closed());
        // Cancellation resolves the conflict; the same public request can then finish.
        ui.application
            .activate_action("prepare-uninstall", Some(&identity));
        until(&rig, &ui, || ui.stopped.get());
        assert_eq!(rig.shutdown_attempts(), 1);
        until(&rig, &ui, || {
            ui.inspection_workers
                .borrow()
                .iter()
                .all(JoinHandle::is_finished)
        });
        for worker in ui.inspection_workers.borrow_mut().drain(..) {
            worker.join().expect("read-only inspection worker");
        }
        ui.window.destroy();
    }

    #[test]
    #[ignore = "requires the isolated installed GTK test runner"]
    fn close_retries_failed_shutdown_with_active_tray_host() {
        let (rig, ui) = fixture();
        let bus = ui
            .application
            .dbus_connection()
            .expect("private session bus");
        let xml = gio::DBusNodeInfo::for_xml(
            r#"<node>
            <interface name="org.kde.StatusNotifierWatcher">
                <method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
                <property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
                <property name="ProtocolVersion" type="i" access="read"/>
            </interface>
        </node>"#,
        )
        .unwrap();
        let interface = xml
            .lookup_interface("org.kde.StatusNotifierWatcher")
            .unwrap();
        let registration = bus
            .register_object("/StatusNotifierWatcher", &interface)
            .method_call(|_, _, _, _, method, parameters, invocation| {
                if method == "RegisterStatusNotifierItem" && parameters.get::<(String,)>().is_some()
                {
                    invocation.return_value(None);
                } else {
                    invocation.return_dbus_error(
                        "org.freedesktop.DBus.Error.InvalidArgs",
                        "Unexpected fixture request",
                    );
                }
            })
            .property(|_, _, _, _, name| match name {
                "IsStatusNotifierHostRegistered" => true.to_variant(),
                "ProtocolVersion" => 0_i32.to_variant(),
                _ => unreachable!("declared watcher properties only"),
            })
            .build()
            .unwrap();
        let owner = gio::bus_own_name_on_connection(
            &bus,
            "org.kde.StatusNotifierWatcher",
            gio::BusNameOwnerFlags::DO_NOT_QUEUE,
            |_, _| {},
            |_, _| {},
        );
        until(&rig, &ui, || ui.tray_active());
        assert_eq!(ui.latest.borrow().setup_phase, SetupPhase::Ready);
        rig.fail_first_shutdown();
        ui.request_quit();
        until(&rig, &ui, || {
            rig.shutdown_attempts() == 1
                && !ui.shutdown_requested.get()
                && ui.window.visible_dialog().is_some()
        });
        assert!(!ui.stopped.get());
        until(&rig, &ui, || {
            if let Some(error) = ui.window.visible_dialog() {
                error.close();
                false
            } else {
                true
            }
        });
        assert!(!matches!(
            rig.snapshot().lifecycle,
            Lifecycle::Starting | Lifecycle::Running
        ));
        assert!(
            ui.tray_active(),
            "the tray host must remain present across the failure"
        );
        ui.window.close();
        until(&rig, &ui, || ui.stopped.get());
        assert_eq!(
            rig.shutdown_attempts(),
            2,
            "Close must retry rather than hide frozen ownership"
        );
        ui.window.destroy();
        gio::bus_unown_name(owner);
        bus.unregister_object(registration).unwrap();
    }
}
