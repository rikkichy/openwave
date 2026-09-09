//! Confirmed removal. Inspection never acquires leases or starts a service.
use crate::{
    installation::{self, InstallMethod, Installation, InstallationFormat, InstallationSnapshot},
    paths::{self, RuntimePaths},
    service::{self, HostContext},
};
use openwave_core::model::{ErrorCode, OperationError, Result};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
mod quiesce;
mod recovery;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallPlan {
    pub installation: Installation,
    pub actions: Vec<String>,
    pub warnings: Vec<String>,
    pub blockers: Vec<String>,
}
impl UninstallPlan {
    pub fn can_execute(&self) -> bool {
        self.blockers.is_empty() && self.installation.method != InstallMethod::Flatpak
    }
    pub fn remove_application(&self) -> bool {
        self.blockers.is_empty() && self.installation.method == InstallMethod::Manual
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallResult {
    pub success: bool,
    pub removed: Vec<String>,
    pub error: Option<String>,
    pub guidance: String,
    pub app_removed: bool,
}
fn failure(error: impl ToString) -> UninstallResult {
    UninstallResult {
        success: false,
        removed: vec![],
        error: Some(error.to_string()),
        guidance: String::new(),
        app_removed: false,
    }
}
fn identity(snapshot: &InstallationSnapshot) -> PathBuf {
    snapshot
        .module_dir
        .clone()
        .unwrap_or_else(|| snapshot.prefix.join("share/openwave"))
}
fn target_paths(snapshot: &InstallationSnapshot, helper: PathBuf) -> RuntimePaths {
    let data = snapshot.prefix.join("share/openwave");
    RuntimePaths {
        executable: snapshot.prefix.join("bin/openwave"),
        prefix: Some(snapshot.prefix.clone()),
        data,
        identity: identity(snapshot),
        maintenance: helper,
        source: None,
    }
}
fn plan_for(installation: Installation) -> UninstallPlan {
    let mut plan = UninstallPlan {
        installation,
        actions: vec![],
        warnings: vec![],
        blockers: vec![],
    };
    if let Some(error) = &plan.installation.error {
        plan.blockers.push(error.clone());
    }
    match plan.installation.method {
        InstallMethod::Unknown => plan
            .blockers
            .push("Installation identity is unknown; no cleanup is authorized".into()),
        InstallMethod::Flatpak => plan.warnings.push(crate::setup::SANDBOX_GUIDANCE.into()),
        _ => {
            plan.actions.extend(
                [
                    "Ask this installation's running GUI to drain and exit",
                    "Verify and stop only this installation's capture service",
                    "Prove workers have stopped and exclude new owners",
                    "Remove eligible owned desktop/audio integration and USB rules",
                ]
                .map(str::to_owned),
            );
            if plan.installation.method == InstallMethod::Manual {
                if let Some(snapshot) = &plan.installation.snapshot {
                    if let Err(error) = installation::validate_installation(snapshot) {
                        plan.blockers.push(error.to_string());
                    }
                    plan.actions.push(format!(
                        "Remove {} recorded application files from {}",
                        snapshot.files.len(),
                        snapshot.prefix.display()
                    ));
                } else {
                    plan.blockers
                        .push("Manual installation has no validated removal inventory".into());
                }
            } else {
                plan.warnings.push("Application files and package-owned integration remain under their manager's control".into());
            }
        }
    }
    plan
}
pub fn inspect(paths: &RuntimePaths) -> UninstallPlan {
    let mut plan = plan_for(installation::inspect(paths));
    if plan.can_execute() {
        match HostContext::discover().and_then(|host| {
            quiesce::inspect_service(paths, plan.installation.snapshot.as_ref(), &host)
        }) {
            Ok(()) => (),
            Err(error) => plan.blockers.push(error.to_string()),
        }
    }
    plan
}
pub fn describe(plan: &UninstallPlan) -> String {
    let mut text = format!(
        "Installation: {:?}\nLocation: {}\n",
        plan.installation.method,
        plan.installation.canonical_identity.display()
    );
    for action in &plan.actions {
        text.push_str(&format!("\n- {action}"));
    }
    text.push_str("\n\nSettings and saved scenes are kept unless separately selected for deletion.\nShared dependencies, including PipeWire and GTK, are never removed.\n");
    for warning in &plan.warnings {
        text.push_str(&format!("\nNote: {warning}"));
    }
    for blocker in &plan.blockers {
        text.push_str(&format!("\nCannot continue: {blocker}"));
    }
    if !plan.installation.guidance.is_empty() {
        text.push_str(&format!("\n{}", plan.installation.guidance));
    }
    text
}
fn require_user() -> Result<()> {
    if rustix::process::geteuid().is_root() {
        return Err(OperationError::invalid(
            "Run ordinary removal as your login user; administrator permission is requested only for narrow trusted operations",
        ));
    }
    Ok(())
}
fn app_gone(snapshot: &InstallationSnapshot) -> bool {
    !snapshot.files.is_empty()
        && snapshot
            .files
            .iter()
            .all(|p| matches!(fs::symlink_metadata(p),Err(e) if e.kind()==io::ErrorKind::NotFound))
}

/// Confirmed pre-freeze preparation. The durable copy and any privileged
/// authority exist before the controller gives up its live runtime.
pub fn prepare(paths: &RuntimePaths, plan: &UninstallPlan, delete_settings: bool) -> Result<()> {
    prepare_with_runner(
        paths,
        plan,
        delete_settings,
        &crate::process::CommandRunner::default(),
    )
}
fn prepare_with_runner(
    paths: &RuntimePaths,
    plan: &UninstallPlan,
    delete_settings: bool,
    runner: &crate::process::CommandRunner,
) -> Result<()> {
    require_user()?;
    if !plan.can_execute() {
        return Err(OperationError::invalid(plan.blockers.join("; ")));
    }
    if delete_settings {
        recovery::check_settings(&paths::config_dir()?)?;
    }
    if !plan.remove_application() {
        return Ok(());
    }
    let snapshot = plan
        .installation
        .snapshot
        .as_ref()
        .ok_or_else(|| OperationError::invalid("Missing removal inventory"))?;
    installation::validate_installation(snapshot)?;
    let mut bundle = recovery::prepared(paths, snapshot, delete_settings)?;
    let outcome = (|| -> Result<()> {
        bundle.ensure_native_helper()?;
        // This creates only the same-user lease inode, never an exclusive
        // pre-freeze lease while the GUI still holds its shared ownership.
        let _lease = paths::Lease::installation_shared(&identity(snapshot))?;
        if bundle.record.privileged_transaction.is_none() && recovery::needs_root(snapshot)? {
            let transaction = recovery::request_privileged(paths, snapshot, runner)?;
            bundle.set_transaction(transaction)?;
        }
        Ok(())
    })();
    outcome.map_err(|error| {
        OperationError::new(
            error.code,
            format!("{error}\nRetry without a checkout:\n{}", bundle.command()),
        )
    })
}

/// The controller calls this only after freezing, draining, and releasing its
/// shared installation lease. It never saves state after this call begins.
pub fn execute(
    paths: &RuntimePaths,
    plan: &UninstallPlan,
    delete_settings: bool,
    stop_running_app: bool,
) -> UninstallResult {
    execute_inner(
        paths,
        plan,
        delete_settings,
        stop_running_app,
        None,
        None,
        None,
    )
}
/// Host-boundary variant, matching setup/service's isolated-fixture adapter.
pub fn execute_with(
    paths: &RuntimePaths,
    plan: &UninstallPlan,
    delete_settings: bool,
    stop_running_app: bool,
    host: &HostContext,
) -> UninstallResult {
    execute_inner(
        paths,
        plan,
        delete_settings,
        stop_running_app,
        None,
        None,
        Some(host),
    )
}
fn execute_inner(
    paths: &RuntimePaths,
    plan: &UninstallPlan,
    delete_settings: bool,
    stop_running_app: bool,
    existing: Option<recovery::UserRecovery>,
    interrupted: Option<&Arc<AtomicBool>>,
    host: Option<&HostContext>,
) -> UninstallResult {
    let cancel = interrupted.cloned().unwrap_or_default();
    let runner = crate::process::CommandRunner::new(cancel.clone());
    let mut result = UninstallResult {
        success: false,
        removed: vec![],
        error: None,
        guidance: plan.installation.guidance.clone(),
        app_removed: false,
    };
    let mut bundle = existing;
    let outcome = (|| -> Result<()> {
        check_cancel(interrupted)?;
        require_user()?;
        if !plan.can_execute() {
            return Err(OperationError::invalid(plan.blockers.join("; ")));
        }
        let discovered;
        let host = match host {
            Some(host) => host,
            None => {
                discovered = HostContext::discover()?;
                &discovered
            }
        };
        host.require_native()?;
        let snapshot = plan.installation.snapshot.as_ref();
        if plan.remove_application() {
            let snapshot =
                snapshot.ok_or_else(|| OperationError::invalid("Missing removal authority"))?;
            installation::validate_installation(snapshot)?;
            if bundle.is_none() {
                prepare_with_runner(paths, plan, delete_settings, &runner)?;
                bundle = Some(recovery::prepared(paths, snapshot, delete_settings)?);
            }
            if let Some(bundle) = bundle.as_mut() {
                bundle.ensure_native_helper()?;
                if bundle.record.privileged_transaction.is_none() && recovery::needs_root(snapshot)?
                {
                    bundle
                        .set_transaction(recovery::request_privileged(paths, snapshot, &runner)?)?;
                }
            }
        }
        check_cancel(interrupted)?;
        if stop_running_app {
            quiesce::request_app_stop(&plan.installation.canonical_identity)?;
        }
        check_cancel(interrupted)?;
        quiesce::stop_service(paths, snapshot, &host)?;
        result
            .removed
            .push("Owned capture service stopped (if present)".into());
        quiesce::assert_stopped(paths, snapshot, rustix::process::geteuid().as_raw())?;
        let mut exclusion = Some(paths::Lease::installation_exclusive(
            &plan.installation.canonical_identity,
        )?);
        quiesce::assert_stopped(paths, snapshot, rustix::process::geteuid().as_raw())?;
        check_cancel(interrupted)?;
        // Fail before deleting integration if consent names a managed/symlink tree.
        if delete_settings {
            recovery::check_settings(&host.config_home.join("openwave"))?;
        }
        if let Some(legacy) = snapshot.filter(|s| s.format != InstallationFormat::RustV2) {
            quiesce::remove_legacy_service(legacy, host)?;
        } else if host.commands.available("systemctl") || host.commands.available("sv") {
            service::remove_owned_with(paths, host)?;
        }
        // Manager-owned application files and USB rules are never manual authority.
        remove_desktop(paths, snapshot, host)?;
        remove_user_audio(paths, &host)?;
        result.removed.push("Eligible owned desktop and audio integration removed; foreign/manager entries preserved".into());
        check_cancel(interrupted)?;
        remove_usb_rules(paths, &host, bundle.as_ref())?;
        result
            .removed
            .push("Eligible OpenWave USB rules removed; manager rules preserved".into());
        check_cancel(interrupted)?;
        if delete_settings {
            recovery::remove_settings(&host.config_home.join("openwave"))?;
            result
                .removed
                .push("Settings and saved scenes removed by explicit consent".into());
        }
        check_cancel(interrupted)?;
        if plan.remove_application() {
            let snapshot =
                snapshot.ok_or_else(|| OperationError::invalid("Missing removal authority"))?;
            installation::validate_installation(snapshot)?;
            let lease = exclusion
                .take()
                .ok_or_else(|| OperationError::invalid("Installation exclusion was lost"))?;
            remove_application(paths, snapshot, bundle.as_mut(), lease, &cancel)?;
            result.app_removed = app_gone(snapshot);
            if !result.app_removed {
                return Err(OperationError::unavailable(
                    "Recorded application files remain",
                ));
            }
            result.removed.push(
                "Recorded OpenWave application files removed; unrecorded contents preserved".into(),
            );
        }
        check_cancel(interrupted)?;
        Ok(())
    })();
    if let Some(snapshot) = &plan.installation.snapshot {
        result.app_removed = app_gone(snapshot);
    }
    match outcome {
        Ok(()) => {
            result.success = true;
            if let Some(bundle) = bundle {
                if let Err(error) = bundle.cleanup() {
                    result.guidance.push_str(&format!(
                        "\nRemoval completed; recovery cleanup retained: {error}"
                    ));
                }
            }
        }
        Err(error) => {
            result.error = Some(error.to_string());
            if let Some(bundle) = bundle {
                result.guidance.push_str(&format!(
                    "\nRetry without a checkout:\n{}",
                    bundle.command()
                ));
            }
        }
    }
    result
}
fn check_cancel(flag: Option<&Arc<AtomicBool>>) -> Result<()> {
    if flag.is_some_and(|v| v.load(Ordering::Acquire)) {
        Err(OperationError::new(
            ErrorCode::Cancelled,
            "Removal interrupted; completed phases are not rolled back",
        ))
    } else {
        Ok(())
    }
}
fn remove_application(
    paths: &RuntimePaths,
    snapshot: &InstallationSnapshot,
    bundle: Option<&mut recovery::UserRecovery>,
    exclusion: paths::Lease,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    let runner = crate::process::CommandRunner::new(cancel.clone());
    if let Some(bundle) = bundle {
        if let Some(transaction) = &bundle.record.privileged_transaction {
            // No mutation during handoff: root acquires the same lock and
            // repeats process/authority checks before the first unlink.
            drop(exclusion);
            return recovery::invoke_transaction(transaction, &runner);
        }
        if recovery::needs_root(snapshot)? {
            let transaction = recovery::request_privileged(paths, snapshot, &runner)?;
            // The root transaction path is written before requesting deletion.
            bundle.set_transaction(transaction.clone())?;
            drop(exclusion);
            return recovery::invoke_transaction(&transaction, &runner);
        }
    } else if recovery::needs_root(snapshot)? {
        return Err(OperationError::invalid(
            "Privileged removal requires a prepared recovery transaction",
        ));
    }
    installation::remove_inventory_cancellable(snapshot, cancel)?;
    Ok(())
}
fn remove_desktop(
    paths: &RuntimePaths,
    snapshot: Option<&InstallationSnapshot>,
    host: &HostContext,
) -> Result<()> {
    for path in [
        host.config_home.join("autostart/openwave.desktop"),
        host.config_home
            .join("autostart/openwave-autostart.desktop"),
        host.data_home.join("applications/openwave.desktop"),
    ] {
        if recovery::managed_path(&path)? || host.commands.package_owner(&[path.clone()])?.is_some()
        {
            continue;
        }
        let Some(text) = service::read_optional(&path)? else {
            continue;
        };
        let native = crate::desktop::owned(&text, paths, host).unwrap_or(false);
        let legacy=(||->Result<bool> {
            let values=service::ini_section(&text,"Desktop Entry")?;
            if values.get("Type").map(String::as_str)!=Some("Application") {return Ok(false);}
            let args=crate::desktop::decode_exec(values.get("Exec").map(String::as_str).unwrap_or(""))?;
            let Some(snapshot)=snapshot.filter(|s|s.format!=InstallationFormat::RustV2) else {return Ok(false);};
            let working=values.get("Path").map(Path::new);
            Ok(matches!(args.as_slice(),[program,module,app] if Path::new(program).is_absolute() && quiesce::python_interpreter(Path::new(program)) && module=="-m" && app=="wavexlr")
                && snapshot.module_dir.as_ref().and_then(|p|p.parent())==working)
        })().unwrap_or(false);
        if native || legacy {
            recovery::remove_unchanged(&path, text.as_bytes())?;
        }
    }
    Ok(())
}
fn remove_user_audio(paths: &RuntimePaths, host: &HostContext) -> Result<()> {
    let wp = host
        .config_home
        .join("wireplumber/wireplumber.conf.d")
        .join(crate::setup::WIREPLUMBER_NAME);
    let mixes = host
        .config_home
        .join("pipewire/pipewire.conf.d")
        .join(crate::setup::MIXES_NAME);
    for path in [wp, mixes] {
        if recovery::managed_path(&path)? || host.commands.package_owner(&[path.clone()])?.is_some()
        {
            continue;
        }
        let Some(text) = service::read_optional(&path)? else {
            continue;
        };
        let expected =
            if path.file_name().and_then(|s| s.to_str()) == Some(crate::setup::WIREPLUMBER_NAME) {
                let asset = paths
                    .data
                    .join("wireplumber")
                    .join(crate::setup::WIREPLUMBER_NAME);
                match service::read_optional(&asset)? {
                    Some(text) => text,
                    None => continue,
                }
            } else {
                let definitions = host.config_home.join("openwave/mixdefs.json");
                let mixes = match service::read_optional(&definitions)? {
                    Some(definitions) => {
                        openwave_core::model::normalize_mixes(serde_json::from_str(&definitions)?)?
                    }
                    None => openwave_core::model::default_mixes(),
                };
                crate::setup::render_mixes_conf(&mixes)?
            };
        if text == expected {
            recovery::remove_unchanged(&path, text.as_bytes())?;
        }
    }
    Ok(())
}
fn remove_usb_rules(
    paths: &RuntimePaths,
    host: &HostContext,
    bundle: Option<&recovery::UserRecovery>,
) -> Result<()> {
    let mut eligible = false;
    for name in ["99-openwave.rules", "99-wavexlr.rules"] {
        let path = host.udev_directory.join(name);
        if !recovery::managed_path(&path)?
            && service::read_optional(&path)?.is_some()
            && host.commands.package_owner(&[path])?.is_none()
        {
            eligible = true;
        }
    }
    if !eligible {
        return Ok(());
    }
    let helper = match bundle.and_then(|b| b.record.privileged_transaction.as_ref()) {
        Some(transaction) => transaction.with_file_name("openwave-maintenance"),
        None => recovery::trusted_helper(paths)?,
    };
    paths::trusted_for_root(&helper).map_err(|e| OperationError::invalid(format!("{e}. Ask an administrator to remove the exact owned OpenWave USB rules; no user-writable helper can elevate")))?;
    host.command(
        "pkexec",
        &[service::text_path(&helper)?, "remove-udev"],
        Duration::from_secs(120),
    )?;
    Ok(())
}

pub fn files_only(prefix: &Path, destdir: Option<&Path>) -> Result<()> {
    let staged = if let Some(stage) = destdir {
        if !stage.is_absolute() || stage == Path::new("/") {
            return Err(OperationError::invalid(
                "DESTDIR must be an absolute separate staging directory",
            ));
        }
        stage.join(
            prefix
                .strip_prefix("/")
                .map_err(|_| OperationError::invalid("PREFIX must be absolute"))?,
        )
    } else {
        prefix.to_owned()
    };
    let install = installation::inspect_prefix(&staged, None)?;
    let snapshot = install.snapshot.ok_or_else(|| {
        OperationError::invalid("Files-only removal requires a manual bounded inventory")
    })?;
    if install.method != InstallMethod::Manual {
        return Err(OperationError::invalid(
            "Use the installation's package manager",
        ));
    }
    installation::validate_installation(&snapshot)?;
    let paths = target_paths(&snapshot, std::env::current_exe()?);
    let _lease = if destdir.is_none() {
        require_user()?;
        quiesce::assert_stopped(&paths, Some(&snapshot), rustix::process::geteuid().as_raw())?;
        Some(paths::Lease::installation_exclusive(&identity(&snapshot))?)
    } else {
        None
    };
    installation::remove_inventory(&snapshot)?;
    Ok(())
}
pub fn resume_uninstall(
    plan_path: &Path,
    confirmed: bool,
    delete_settings: bool,
) -> UninstallResult {
    let loaded = (|| -> Result<_> {
        require_user()?;
        recovery::load_user(plan_path)
    })();
    let mut bundle = match loaded {
        Ok(bundle) => bundle,
        Err(error) => return failure(error),
    };
    if !confirmed {
        return failure("Confirmation required: resume-uninstall --plan PATH --yes");
    }
    if let Err(error) = bundle.ensure_native_helper() {
        return failure(error);
    }
    if delete_settings && !bundle.record.delete_settings {
        bundle.record.delete_settings = true;
        if let Err(error) = bundle.persist() {
            return failure(error);
        }
    }
    let snapshot = bundle.record.installation.clone();
    let paths = target_paths(&snapshot, bundle.helper());
    let install = Installation {
        method: InstallMethod::Manual,
        snapshot: Some(snapshot.clone()),
        canonical_identity: identity(&snapshot),
        guidance: String::new(),
        error: None,
    };
    execute_inner(
        &paths,
        &plan_for(install),
        bundle.record.delete_settings,
        true,
        Some(bundle),
        None,
        None,
    )
}
pub fn prepare_files_privileged(
    receipt: &Path,
    expected_sha256: &str,
    prefix: &Path,
) -> Result<PathBuf> {
    let snapshot = installation::snapshot_from_receipt(receipt, expected_sha256, prefix)?;
    recovery::prepare_root(&snapshot)
}
pub fn remove_files_privileged(receipt: &Path, expected_sha256: &str, prefix: &Path) -> Result<()> {
    let transaction = prepare_files_privileged(receipt, expected_sha256, prefix)?;
    recovery::resume_root(&transaction)
}
pub fn resume_privileged(transaction: &Path) -> Result<()> {
    recovery::resume_root(transaction)
}
/// Narrow login-user subprocess used by a trusted privileged legacy bootstrap.
pub fn quiesce_legacy(prefix: &Path, module_dir: Option<&Path>) -> Result<()> {
    require_user()?;
    let install = installation::inspect_prefix(prefix, module_dir)?;
    let snapshot = install
        .snapshot
        .ok_or_else(|| OperationError::invalid("Legacy inventory unavailable"))?;
    if snapshot.format == InstallationFormat::RustV2 {
        return Err(OperationError::invalid("Not a legacy installation"));
    }
    let target = target_paths(&snapshot, std::env::current_exe()?);
    quiesce::request_app_stop(&identity(&snapshot))?;
    quiesce::stop_service(&target, Some(&snapshot), &HostContext::discover()?)?;
    // Establish the current-user lock inode before root acquires it after this
    // bounded subprocess exits; never create a root-owned user lock.
    let _lease = paths::Lease::installation_exclusive(&identity(&snapshot))?;
    quiesce::assert_stopped(
        &target,
        Some(&snapshot),
        rustix::process::geteuid().as_raw(),
    )
}
pub fn retire_legacy(
    prefix: &Path,
    module_dir: Option<&Path>,
    dry_run: bool,
    confirmed: bool,
) -> UninstallResult {
    let mut result = failure("Legacy retirement did not complete");
    let mut accepted = None;
    let outcome = (|| -> Result<()> {
        let install = installation::inspect_prefix(prefix, module_dir)?;
        if let Some(error) = install.error {
            return Err(OperationError::invalid(error));
        }
        let snapshot = install.snapshot.ok_or_else(|| OperationError::invalid("Use the existing installation's confirmed uninstaller, preserving settings, before native installation"))?;
        if snapshot.format == InstallationFormat::RustV2 || snapshot.method != InstallMethod::Manual
        {
            return Err(OperationError::invalid(
                "Retirement accepts only validated manual Python-v1/legacy installations",
            ));
        }
        installation::validate_installation(&snapshot)?;
        accepted = Some(snapshot.clone());
        result.guidance = format!(
            "Retire only {} legacy files from {}; preserve settings and user integration",
            snapshot.files.len(),
            snapshot.prefix.display()
        );
        if dry_run {
            return Ok(());
        }
        if !confirmed {
            return Err(OperationError::invalid(
                "Explicit confirmation required: retire-legacy --yes",
            ));
        }
        let helper = std::env::current_exe()?;
        let target = target_paths(&snapshot, helper.clone());
        if rustix::process::geteuid().is_root() {
            recovery::verify_bootstrap(&helper, &snapshot)?;
            let transaction = recovery::prepare_root(&snapshot)?;
            result.guidance.push_str(&format!(
                "\nPrivileged retry:\npkexec {} resume-privileged --transaction {}",
                recovery::quote(&transaction.with_file_name("openwave-maintenance")),
                recovery::quote(&transaction)
            ));
            quiesce::as_login_user(&helper, &snapshot)?;
            recovery::resume_root(&transaction)?;
        } else {
            let bundle = recovery::prepare_user(&target, &snapshot, false)?;
            result.guidance.push_str(&format!(
                "\nRetry retirement without a checkout:\n{}",
                bundle.retirement_command()
            ));
            quiesce_legacy(prefix, module_dir)?;
            let lease = paths::Lease::installation_exclusive(&identity(&snapshot))?;
            let mut bundle = bundle;
            remove_application(
                &target,
                &snapshot,
                Some(&mut bundle),
                lease,
                &Arc::default(),
            )?;
            bundle.cleanup()?;
        }
        result.app_removed = app_gone(&snapshot);
        if !result.app_removed {
            return Err(OperationError::unavailable(
                "Legacy application files remain",
            ));
        }
        result.removed.push(
            "Validated legacy application files retired; settings and integration preserved".into(),
        );
        Ok(())
    })();
    if !dry_run {
        if let Some(snapshot) = accepted {
            result.app_removed = app_gone(&snapshot);
        }
    }
    match outcome {
        Ok(()) => {
            result.success = true;
            result.error = None;
            if !dry_run {
                result.guidance="Legacy application files retired. Settings and user integration were preserved; install the already prepared native payload next.".into();
            }
        }
        Err(error) => result.error = Some(error.to_string()),
    }
    result
}

pub fn resume_retirement(plan_path: &Path, confirmed: bool) -> UninstallResult {
    let outcome = (|| -> Result<UninstallResult> {
        require_user()?;
        let mut bundle = recovery::load_user(plan_path)?;
        if !confirmed {
            return Err(OperationError::invalid(
                "Confirmation required: resume-retirement --plan PATH --yes",
            ));
        }
        bundle.ensure_native_helper()?;
        let snapshot = bundle.record.installation.clone();
        if snapshot.format == InstallationFormat::RustV2 || bundle.record.delete_settings {
            return Err(OperationError::invalid(
                "Retirement retry requires legacy authority without settings deletion consent",
            ));
        }
        let target = target_paths(&snapshot, bundle.helper());
        let mut result = UninstallResult {
            success: false,
            removed: vec![],
            error: None,
            guidance: format!("Retry retirement:\n{}", bundle.retirement_command()),
            app_removed: app_gone(&snapshot),
        };
        let operation = (|| -> Result<()> {
            quiesce::request_app_stop(&identity(&snapshot))?;
            quiesce::stop_service(&target, Some(&snapshot), &HostContext::discover()?)?;
            quiesce::assert_stopped(
                &target,
                Some(&snapshot),
                rustix::process::geteuid().as_raw(),
            )?;
            let lease = paths::Lease::installation_exclusive(&identity(&snapshot))?;
            remove_application(
                &target,
                &snapshot,
                Some(&mut bundle),
                lease,
                &Arc::default(),
            )?;
            result.app_removed = app_gone(&snapshot);
            bundle.cleanup()?;
            Ok(())
        })();
        result.app_removed = app_gone(&snapshot);
        match operation {
            Ok(()) => {
                result.success = true;
                result.guidance =
                    "Legacy inventory retired; all settings and integration preserved".into();
            }
            Err(error) => result.error = Some(error.to_string()),
        }
        Ok(result)
    })();
    outcome.unwrap_or_else(failure)
}
pub fn cli(paths: &RuntimePaths, delete_settings: bool, yes: bool, dry_run: bool) -> i32 {
    let plan = inspect(paths);
    println!("{}", describe(&plan));
    if delete_settings {
        println!("\nAlso delete this user's OpenWave settings and saved scenes.");
    }
    if dry_run {
        return i32::from(!plan.blockers.is_empty());
    }
    if !plan.can_execute() {
        return i32::from(!plan.blockers.is_empty());
    }
    let interrupted = Arc::new(AtomicBool::new(false));
    let signal = match signal_hook::flag::register(signal_hook::consts::SIGINT, interrupted.clone())
    {
        Ok(signal) => signal,
        Err(error) => {
            eprintln!("Cannot arrange safe interrupt handling: {error}");
            return 1;
        }
    };
    let status = (|| {
        if !yes {
            if !io::stdin().is_terminal() {
                eprintln!("Run interactively or pass --yes to confirm. No changes made.");
                return 2;
            }
            print!("Continue with the displayed removal plan? [y/N] ");
            if io::stdout().flush().is_err() {
                return 1;
            }
            let stdin = io::stdin();
            let mut descriptors = [rustix::event::PollFd::new(
                &stdin,
                rustix::event::PollFlags::IN,
            )];
            let timeout = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 100_000_000,
            };
            loop {
                if interrupted.load(Ordering::Acquire) {
                    return 130;
                }
                match rustix::event::poll(&mut descriptors, Some(&timeout)) {
                    Ok(0) => continue,
                    Ok(_) => break,
                    Err(error) if error == rustix::io::Errno::INTR => continue,
                    Err(_) => return 1,
                }
            }
            let mut answer = String::new();
            if stdin.read_line(&mut answer).is_err() {
                return if interrupted.load(Ordering::Acquire) {
                    130
                } else {
                    1
                };
            }
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                println!("Cancelled; no changes made.");
                return 0;
            }
        }
        let result = execute_inner(
            paths,
            &plan,
            delete_settings,
            true,
            None,
            Some(&interrupted),
            None,
        );
        for phase in &result.removed {
            println!("{phase}");
        }
        if let Some(error) = &result.error {
            eprintln!("Removal incomplete: {error}");
        }
        if !result.guidance.is_empty() {
            println!("{}", result.guidance);
        }
        if interrupted.load(Ordering::Acquire) {
            130
        } else {
            i32::from(!result.success)
        }
    })();
    signal_hook::low_level::unregister(signal);
    status
}
