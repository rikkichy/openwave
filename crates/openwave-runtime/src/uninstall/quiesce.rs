use super::*;
use glib::variant::ToVariant;
use std::os::unix::ffi::OsStrExt;
use std::{
    collections::HashMap,
    io::Read,
    os::unix::fs::MetadataExt,
    process::{Command, Stdio},
    thread,
    time::Instant,
};
fn error(message: impl Into<String>) -> OperationError {
    OperationError::new(ErrorCode::Identity, message)
}
pub(super) fn request_app_stop(identity: &Path) -> Result<()> {
    let Some(address) = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").filter(|s| !s.is_empty())
    else {
        return Ok(());
    };
    let address = address
        .to_str()
        .ok_or_else(|| error("Session bus address is not UTF-8"))?;
    if address
        .split(';')
        .any(|a| !a.starts_with("unix:") && !a.starts_with("tcp:") && !a.starts_with("nonce-tcp:"))
    {
        return Err(error("Refusing autolaunch-capable session bus address"));
    }
    let connection = gio::DBusConnection::for_address_sync(
        address,
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
            | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None,
        gio::Cancellable::NONE,
    )
    .map_err(|e| {
        OperationError::unavailable(format!("Cannot inspect existing GUI session: {e}"))
    })?;
    let owner = || -> Result<Option<String>> {
        match connection.call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "GetNameOwner",
            Some(&("com.github.openwave",).to_variant()),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            2000,
            gio::Cancellable::NONE,
        ) {
            Ok(v) => v
                .get::<(String,)>()
                .map(|v| Some(v.0))
                .ok_or_else(|| error("Malformed existing GUI owner response")),
            Err(e)
                if gio::DBusError::remote_error(&e).as_deref()
                    == Some("org.freedesktop.DBus.Error.NameHasNoOwner") =>
            {
                Ok(None)
            }
            Err(e) => Err(OperationError::unavailable(format!(
                "Cannot determine existing GUI owner: {e}"
            ))),
        }
    };
    let Some(accepted) = owner()? else {
        return Ok(());
    };
    // A unique destination cannot activate a different replacement GUI.
    let parameters = (
        "prepare-uninstall",
        vec![service::text_path(identity)?.to_variant()],
        HashMap::<String, glib::Variant>::new(),
    )
        .to_variant();
    connection
        .call_sync(
            Some(&accepted),
            "/com/github/openwave",
            "org.gtk.Actions",
            "Activate",
            Some(&parameters),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            3000,
            gio::Cancellable::NONE,
        )
        .map_err(|e| {
            OperationError::unavailable(format!(
                "Close the matching OpenWave window and tray before removal: {e}"
            ))
        })?;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match owner()? {
            None => return Ok(()),
            Some(current) if current != accepted => {
                return Err(error(
                    "GUI owner changed during drain; close the new owner before retry",
                ));
            }
            _ => (),
        }
        if Instant::now() >= deadline {
            return Err(OperationError::new(
                ErrorCode::Busy,
                "OpenWave has not finished draining; no application files removed",
            ));
        }
        let reply = connection.call_sync(
            Some(&accepted),
            "/com/github/openwave",
            "org.gtk.Actions",
            "Describe",
            Some(&("prepare-uninstall",).to_variant()),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            2000,
            gio::Cancellable::NONE,
        );
        match reply {
            Ok(v) => {
                if v.n_children() != 1 {
                    return Err(error("Malformed prepare-uninstall action description"));
                }
                let description = v.child_value(0);
                if description.n_children() != 3 {
                    return Err(error("Malformed prepare-uninstall action description"));
                }
                let states = description.child_value(2);
                if states.n_children() != 1 {
                    return Err(error("Missing prepare-uninstall state"));
                }
                let state = states
                    .child_value(0)
                    .as_variant()
                    .and_then(|v| v.get::<String>())
                    .ok_or_else(|| error("Invalid prepare-uninstall state"))?;
                if let Some(problem) = state.strip_prefix("error:") {
                    return Err(OperationError::unavailable(problem));
                }
                if state != "stopping" && state != "idle" {
                    return Err(error("Unknown prepare-uninstall state"));
                }
            }
            Err(e) => {
                if owner()?.is_none() {
                    return Ok(());
                }
                return Err(OperationError::unavailable(format!(
                    "GUI drain status unavailable: {e}"
                )));
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}
fn missing(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::NotFound) || error.raw_os_error() == Some(3)
}
fn cmdline(path: &Path) -> Result<Vec<String>> {
    let bytes = fs::read(path)?;
    if bytes.len() > 1024 * 1024 {
        return Err(error("Process command line exceeds inspection bound"));
    }
    bytes
        .split(|b| *b == 0)
        .filter(|b| !b.is_empty())
        .map(|v| {
            String::from_utf8(v.to_vec()).map_err(|_| error("Process arguments are not UTF-8"))
        })
        .collect()
}
pub(super) fn python_interpreter(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let Some(version) = name.strip_prefix("python") else {
        return false;
    };
    version.is_empty()
        || (version.starts_with(|c: char| c.is_ascii_digit())
            && version
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())))
}
fn legacy_invocation(args: &[String], executable: &Path) -> bool {
    if args.len() < 3 || !python_interpreter(executable) {
        return false;
    }
    (args[1] == "-m"
        && matches!(
            args[2].as_str(),
            "wavexlr" | "wavexlr.daemon" | "wavexlr.probe" | "wavexlr.uninstall"
        ))
        || (args[1] == "-c"
            && matches!(
                args[2].trim(),
                "from wavexlr.daemon import main; main()"
                    | "from wavexlr.daemon import main;main()"
            ))
}
/// A procfs observation binds the PID to its birth time and executable inode.
/// Parent links are kernel observations, never caller-supplied exemption IDs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CallerIdentity {
    pid: u32,
    parent: u32,
    start: u64,
    uid: u32,
    executable: PathBuf,
    device: u64,
    inode: u64,
}
fn observe_process(pid: u32) -> Result<CallerIdentity> {
    let root = PathBuf::from(format!("/proc/{pid}"));
    let stat = fs::read_to_string(root.join("stat"))?;
    let (_, fields) = stat
        .rsplit_once(") ")
        .ok_or_else(|| error("Malformed process identity"))?;
    let fields: Vec<_> = fields.split_whitespace().collect();
    let parent = fields
        .get(1)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| error("Missing process parent"))?;
    let start = fields
        .get(19)
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| error("Missing process start identity"))?;
    let status = fs::read_to_string(root.join("status"))?;
    let uids: Vec<u32> = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .ok_or_else(|| error("Missing process credentials"))?
        .split_whitespace()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .map_err(|_| error("Malformed process credentials"))?;
    if uids.len() != 4 || uids.iter().any(|uid| *uid != uids[0]) {
        return Err(error("Process has transitional credentials"));
    }
    let executable = fs::read_link(root.join("exe"))?;
    let file = fs::File::open(root.join("exe"))?;
    let metadata = file.metadata()?;
    Ok(CallerIdentity {
        pid,
        parent,
        start,
        uid: uids[0],
        executable,
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

pub(super) fn initiating_caller(
    paths: &RuntimePaths,
    snapshot: &InstallationSnapshot,
    uid: u32,
) -> Result<Option<CallerIdentity>> {
    let mut chain = Vec::new();
    let mut pid = std::process::id();
    let caller = loop {
        if chain.len() == 32 || pid == 0 {
            return Err(error("Cannot authenticate bounded initiating ancestry"));
        }
        let observed = observe_process(pid)?;
        pid = observed.parent;
        chain.push(observed.clone());
        if observed.uid != 0 {
            // Only the nearest login-user ancestor may qualify. Shells,
            // another user's process, deleted binaries and other installs
            // never receive an exemption.
            if observed.uid != uid {
                return Err(error(
                    "Recorded login UID does not match initiating ancestry",
                ));
            }
            if snapshot.format != InstallationFormat::RustV2
                || observed.executable != paths.executable
            {
                break None;
            }
            let expected = fs::metadata(&paths.executable)?;
            if observed.device != expected.dev() || observed.inode != expected.ino() {
                return Err(error("Initiating executable identity changed"));
            }
            break Some(observed);
        }
        if pid == 0 {
            break None;
        }
    };
    for observed in &chain {
        if observe_process(observed.pid)? != *observed {
            return Err(error("Initiating ancestry changed during authentication"));
        }
    }
    Ok(caller)
}

fn process_owners(
    paths: &RuntimePaths,
    snapshot: Option<&InstallationSnapshot>,
    uid: u32,
    drained_parent: Option<&CallerIdentity>,
) -> Result<Vec<u32>> {
    let mut owners = Vec::new();
    let bins = ["openwave", "openwave-daemon", "openwave-probe"];
    let expected: Vec<PathBuf> = bins
        .into_iter()
        .map(|name| {
            paths
                .prefix
                .as_ref()
                .map(|p| p.join("bin").join(name))
                .unwrap_or_else(|| paths.executable.with_file_name(name))
        })
        .collect();
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let root = entry.path();
        let meta = match fs::metadata(&root) {
            Ok(m) => m,
            Err(e) if missing(&e) => continue,
            Err(e) => return Err(e.into()),
        };
        if meta.uid() != uid {
            continue;
        }
        let args = match cmdline(&root.join("cmdline")) {
            Ok(v) => v,
            Err(_) if !root.exists() => continue,
            Err(e) => return Err(e),
        };
        if args.is_empty() {
            continue;
        }
        let exe = match fs::read_link(root.join("exe")) {
            Ok(exe) => exe,
            Err(e) if missing(&e) && !root.exists() => continue,
            Err(e) => {
                return Err(error(format!(
                    "Cannot verify same-user PID {pid} executable: {e}"
                )));
            }
        };
        if let Some(caller) = drained_parent.filter(|caller| caller.pid == pid) {
            if observe_process(pid)? != *caller {
                return Err(error("Initiating caller identity changed before removal"));
            }
            continue;
        }
        // Readlink's deleted suffix is accepted only for an exact expected path;
        // it blocks deletion rather than authorizing action on that process.
        let exact = expected.iter().any(|path| {
            exe == *path
                || exe.as_os_str() == format!("{} (deleted)", path.display()).as_str()
                || fs::canonicalize(path).is_ok_and(|p| p == exe)
        });
        if exact {
            owners.push(pid);
            continue;
        }
        if let Some(snapshot) = snapshot.filter(|s| s.format != InstallationFormat::RustV2) {
            if legacy_invocation(&args, &exe) {
                let module = snapshot
                    .module_dir
                    .as_ref()
                    .ok_or_else(|| error("Missing legacy module identity"))?;
                let cwd = fs::read_link(root.join("cwd")).map_err(|e| {
                    error(format!(
                        "Cannot establish legacy PID {pid} working directory: {e}"
                    ))
                })?;
                if module.parent() == Some(cwd.as_path()) {
                    owners.push(pid);
                    continue;
                }
                // An exact Python module invocation with other/unknown import
                // paths is ambiguous, never proof that this install is idle.
                let environment = fs::read(root.join("environ")).map_err(|e| {
                    error(format!(
                        "Cannot establish legacy PID {pid} import identity: {e}"
                    ))
                })?;
                let pythonpath = environment
                    .split(|b| *b == 0)
                    .find_map(|p| p.strip_prefix(b"PYTHONPATH="));
                if pythonpath.is_some_and(|p| {
                    p.split(|b| *b == b':').any(|p| {
                        module
                            .parent()
                            .is_some_and(|m| m.as_os_str().as_bytes() == p)
                    })
                }) {
                    owners.push(pid);
                } else {
                    return Err(error(format!(
                        "Legacy PID {pid} import identity is ambiguous; close it before retirement"
                    )));
                }
            }
        }
    }
    Ok(owners)
}
pub(super) fn assert_stopped(
    paths: &RuntimePaths,
    snapshot: Option<&InstallationSnapshot>,
    uid: u32,
) -> Result<()> {
    assert_stopped_except_parent(paths, snapshot, uid, None)
}
pub(super) fn assert_stopped_except_parent(
    paths: &RuntimePaths,
    snapshot: Option<&InstallationSnapshot>,
    uid: u32,
    drained_parent: Option<&CallerIdentity>,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let owners = process_owners(paths, snapshot, uid, drained_parent)?;
        if owners.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(OperationError::new(
                ErrorCode::Busy,
                format!(
                    "Same-install workers remain at PID(s) {:?}; close them and retry. No process-name kill is performed",
                    owners
                ),
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}
fn legacy_daemon(args: &[String], workdir: &str, snapshot: &InstallationSnapshot) -> bool {
    if let [binary] = args {
        return Path::new(binary) == snapshot.prefix.join("bin/openwave-daemon");
    }
    let Some(module) = &snapshot.module_dir else {
        return false;
    };
    args.len() == 3
        && Path::new(&args[0]).is_absolute()
        && python_interpreter(Path::new(&args[0]))
        && module.parent() == Some(Path::new(workdir))
        && ((args[1] == "-m" && args[2] == "wavexlr.daemon")
            || (args[1] == "-c"
                && matches!(
                    args[2].trim(),
                    "from wavexlr.daemon import main; main()"
                        | "from wavexlr.daemon import main;main()"
                )))
}
fn legacy_service(snapshot: &InstallationSnapshot, host: &HostContext, stop: bool) -> Result<()> {
    host.require_native()?;
    if host.commands.available("systemctl") {
        let output=host.command("systemctl",&["--user","show",service::SYSTEMD_UNIT,"--property=LoadState,ActiveState,UnitFileState,FragmentPath,ExecStart,WorkingDirectory"],Duration::from_secs(5))?;
        let values = service::parse_show(
            std::str::from_utf8(&output.stdout)
                .map_err(|_| error("Non-UTF8 service properties"))?,
        )?;
        if values["LoadState"] == "not-found" {
            if host
                .config_home
                .join("systemd/user/openwave.service")
                .try_exists()?
            {
                return Err(error("Local legacy service has unknown effective identity"));
            }
            return Ok(());
        }
        if values["LoadState"] != "loaded" {
            return Err(error("Legacy service is not inspectable"));
        }
        let fragment = Path::new(&values["FragmentPath"]);
        if !fragment.is_absolute() {
            return Err(error("Missing legacy service fragment identity"));
        }
        let text = service::read_optional(&fs::canonicalize(fragment)?)?
            .ok_or_else(|| error("Legacy service fragment disappeared"))?;
        let ini = service::ini_section(&text, "Service")?;
        if ini
            .keys()
            .any(|key| key.starts_with("Exec") && key != "ExecStart")
        {
            return Err(error(
                "Legacy service contains unproven executable directives",
            ));
        }
        let args = service::split_words(
            ini.get("ExecStart")
                .ok_or_else(|| error("Missing legacy ExecStart"))?,
        )?;
        let workdir = ini
            .get("WorkingDirectory")
            .map(String::as_str)
            .unwrap_or("");
        if !legacy_daemon(&args, workdir, snapshot)
            || !legacy_daemon(
                &service::effective_args(&values["ExecStart"])?,
                &values["WorkingDirectory"],
                snapshot,
            )
        {
            return Err(error(
                "Capture service belongs to another or unproven installation; preserved",
            ));
        }
        if stop {
            host.command(
                "systemctl",
                &["--user", "stop", service::SYSTEMD_UNIT],
                Duration::from_secs(30),
            )?;
            let output = host.command(
                "systemctl",
                &[
                    "--user",
                    "show",
                    service::SYSTEMD_UNIT,
                    "--property=ActiveState",
                    "--value",
                ],
                Duration::from_secs(5),
            )?;
            if !matches!(
                std::str::from_utf8(&output.stdout).unwrap_or("").trim(),
                "inactive" | "failed"
            ) {
                return Err(OperationError::new(
                    ErrorCode::Busy,
                    "Legacy service did not stop",
                ));
            }
        }
    } else if host.commands.available("sv") {
        match fs::symlink_metadata(&host.runit_link) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
            Ok(_) => (),
        }
        let target = fs::canonicalize(&host.runit_link)?;
        if target != fs::canonicalize(&host.runit_definition)? {
            return Err(error("Foreign runit activation target"));
        }
        let script = service::read_optional(&target.join("run"))?
            .ok_or_else(|| error("Missing runit definition"))?;
        let mut commands = Vec::new();
        for line in script
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#') && *l != "exec 2>&1")
        {
            commands.push(service::split_words(line)?);
        }
        if commands.len() != 1 {
            return Err(error("Unproven runit script"));
        }
        let args = &commands[0];
        if args.len() != 5
            || args[..3] != ["exec", "chpst", "-u"]
            || (args[3] != host.username && args[3] != host.uid.to_string())
            || !legacy_daemon(&args[4..], "", snapshot)
        {
            return Err(error("Unproven legacy runit command/user"));
        }
        if stop {
            host.command(
                "sv",
                &["-w", "15", "down", service::text_path(&host.runit_link)?],
                Duration::from_secs(20),
            )?;
        }
    } else if host
        .config_home
        .join("systemd/user/openwave.service")
        .try_exists()?
        || host.runit_link.try_exists()?
    {
        return Err(OperationError::unavailable(
            "Cannot inspect the configured capture service; configure its manager before removal",
        ));
    }
    Ok(())
}
pub(super) fn remove_legacy_service(
    snapshot: &InstallationSnapshot,
    host: &HostContext,
) -> Result<()> {
    legacy_service(snapshot, host, true)?;
    if host.commands.available("systemctl") {
        let output=host.command("systemctl",&["--user","show",service::SYSTEMD_UNIT,"--property=LoadState,ActiveState,UnitFileState,FragmentPath,ExecStart,WorkingDirectory"],Duration::from_secs(5))?;
        let values = service::parse_show(
            std::str::from_utf8(&output.stdout)
                .map_err(|_| error("Non-UTF8 service properties"))?,
        )?;
        if values["LoadState"] == "not-found" {
            return Ok(());
        }
        if !matches!(values["ActiveState"].as_str(), "inactive" | "failed") {
            return Err(OperationError::new(
                ErrorCode::Busy,
                "Legacy capture service has not stopped",
            ));
        }
        let fragment = PathBuf::from(&values["FragmentPath"]);
        let local = host.config_home.join("systemd/user/openwave.service");
        if fragment != local
            || recovery::managed_path(&fragment)?
            || host.commands.package_owner(&[fragment.clone()])?.is_some()
        {
            return Ok(());
        }
        // Revalidate effective identity immediately before disabling the unit.
        legacy_service(snapshot, host, false)?;
        let text = service::read_optional(&fragment)?
            .ok_or_else(|| error("Legacy service fragment disappeared"))?;
        host.command(
            "systemctl",
            &["--user", "disable", service::SYSTEMD_UNIT],
            Duration::from_secs(30),
        )?;
        recovery::remove_unchanged(&fragment, text.as_bytes())?;
        host.command(
            "systemctl",
            &["--user", "daemon-reload"],
            Duration::from_secs(30),
        )?;
    } else if host.commands.available("sv") {
        let link = &host.runit_link;
        let meta = match fs::symlink_metadata(link) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if host
            .commands
            .package_owner(&[link.clone(), host.runit_definition.join("run")])?
            .is_some()
        {
            return Ok(());
        }
        if !meta.file_type().is_symlink()
            || fs::canonicalize(link)? != fs::canonicalize(&host.runit_definition)?
        {
            return Err(error("Runit activation identity changed"));
        }
        if meta.uid() != rustix::process::geteuid().as_raw() {
            return Err(error(
                "The administrator-owned runit activation remains; ask its administrator to remove the exact wavexlr-audio link",
            ));
        }
        let before = fs::read_link(link)?;
        if fs::symlink_metadata(link)?.ino() != meta.ino() || fs::read_link(link)? != before {
            return Err(error("Runit activation changed before removal"));
        }
        fs::remove_file(link)?;
    }
    Ok(())
}
pub(super) fn inspect_service(
    paths: &RuntimePaths,
    snapshot: Option<&InstallationSnapshot>,
    host: &HostContext,
) -> Result<()> {
    if let Some(snapshot) = snapshot.filter(|s| s.format != InstallationFormat::RustV2) {
        return legacy_service(snapshot, host, false);
    }
    if !host.commands.available("systemctl") && !host.commands.available("sv") {
        if host
            .config_home
            .join("systemd/user/openwave.service")
            .try_exists()?
            || host.runit_link.try_exists()?
        {
            return Err(error("Service manager unavailable for existing service"));
        }
        return Ok(());
    }
    service::status_with(paths, host)?;
    Ok(())
}
pub(super) fn stop_service(
    paths: &RuntimePaths,
    snapshot: Option<&InstallationSnapshot>,
    host: &HostContext,
) -> Result<()> {
    if let Some(snapshot) = snapshot.filter(|s| s.format != InstallationFormat::RustV2) {
        return legacy_service(snapshot, host, true);
    }
    inspect_service(paths, snapshot, host)?;
    if host.commands.available("systemctl") || host.commands.available("sv") {
        service::stop_owned_with(paths, host)?;
        if service::status_with(paths, host)?.running {
            return Err(OperationError::new(
                ErrorCode::Busy,
                "Capture service remains running",
            ));
        }
    }
    Ok(())
}
pub(super) fn as_login_user(helper: &Path, snapshot: &InstallationSnapshot) -> Result<()> {
    paths::trusted_for_root(helper)?;
    let uid = recovery::login_uid()?;
    let original = paths::original_user_environment(uid)?;
    let mut command = Command::new(helper);
    command
        .arg("quiesce-legacy")
        .arg("--prefix")
        .arg(&snapshot.prefix);
    if let Some(module) = &snapshot.module_dir {
        command.arg("--module-dir").arg(module);
    }
    command
        .arg("--login-uid")
        .arg(uid.to_string())
        .arg("--login-gid")
        .arg(original.gid.to_string());
    original.configure(&mut command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    crate::process::configure_supervised_child(&mut command)?;
    let mut child = command.spawn()?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| error("Missing quiescence error pipe"))?;
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stderr.take(1024 * 1024).read_to_end(&mut bytes);
        (result, bytes)
    });
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(OperationError::unavailable(
                "Login-user quiescence deadline expired",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    };
    let (_, bytes) = reader
        .join()
        .map_err(|_| error("Quiescence stderr reader failed"))?;
    if !status.success() {
        return Err(OperationError::unavailable(format!(
            "Login-user quiescence failed: {}",
            String::from_utf8_lossy(&bytes)
        )));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/fixtures/private_proc.rs"]
mod private_proc;

#[cfg(test)]
mod caller_tests {
    use super::*;

    fn target() -> (RuntimePaths, InstallationSnapshot) {
        let executable = std::env::current_exe().unwrap();
        let prefix = executable.parent().unwrap().to_owned();
        let paths = RuntimePaths {
            executable: executable.clone(),
            maintenance: executable,
            data: prefix.clone(),
            identity: prefix.clone(),
            prefix: None,
            source: None,
        };
        let snapshot = InstallationSnapshot {
            format: InstallationFormat::RustV2,
            method: InstallMethod::Manual,
            prefix,
            module_dir: None,
            receipt: None,
            files: vec![],
            directories: vec![],
            identities: vec![],
        };
        (paths, snapshot)
    }

    #[test]
    fn initiating_identity_requires_exact_executable_and_login_uid() {
        let uid = rustix::process::geteuid().as_raw();
        assert_ne!(uid, 0, "Use the ordinary-user isolated runner");
        let (mut paths, snapshot) = target();
        assert_eq!(
            initiating_caller(&paths, &snapshot, uid)
                .unwrap()
                .unwrap()
                .pid,
            std::process::id()
        );
        assert!(initiating_caller(&paths, &snapshot, uid + 1).is_err());
        paths.executable = paths.executable.with_file_name("other-install-openwave");
        assert!(initiating_caller(&paths, &snapshot, uid).unwrap().is_none());
    }

    #[test]
    fn owned_caller_fixture() {
        if std::env::var_os("OPENWAVE_CALLER_FIXTURE").is_some() {
            fs::write(
                std::env::var_os("OPENWAVE_CALLER_FIXTURE_READY").expect("fixture readiness path"),
                b"ready",
            )
            .unwrap();
            std::thread::sleep(Duration::from_secs(30));
        }
    }

    #[test]
    fn stale_start_identity_cannot_exempt_an_independent_owner() {
        if std::env::var_os("OPENWAVE_CALLER_IDENTITY_FIXTURE").is_none() {
            let temporary = tempfile::tempdir().unwrap();
            let output = super::private_proc::fixture_command(temporary.path())
                .args([
                    "--exact",
                    "uninstall::quiesce::caller_tests::stale_start_identity_cannot_exempt_an_independent_owner",
                    "--nocapture",
                ])
                .env("OPENWAVE_CALLER_IDENTITY_FIXTURE", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "caller identity fixture: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        super::private_proc::require_private_proc();
        struct Owned(std::process::Child);
        impl Drop for Owned {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let temporary = tempfile::tempdir().unwrap();
        let (mut paths, mut snapshot) = target();
        let executable = temporary.path().join("bin/openwave");
        fs::create_dir(executable.parent().unwrap()).unwrap();
        fs::copy(&paths.executable, &executable).unwrap();
        paths.executable = executable;
        paths.prefix = Some(temporary.path().to_owned());
        snapshot.prefix = temporary.path().to_owned();
        let caller_ready = temporary.path().join("caller-ready");
        let independent_ready = temporary.path().join("independent-ready");
        let child = Owned(
            Command::new(&paths.executable)
                .args([
                    "--exact",
                    "uninstall::quiesce::caller_tests::owned_caller_fixture",
                ])
                .env("OPENWAVE_CALLER_FIXTURE", "1")
                .env("OPENWAVE_CALLER_FIXTURE_READY", &caller_ready)
                .spawn()
                .unwrap(),
        );
        let independent = Owned(
            Command::new(&paths.executable)
                .args([
                    "--exact",
                    "uninstall::quiesce::caller_tests::owned_caller_fixture",
                ])
                .env("OPENWAVE_CALLER_FIXTURE", "1")
                .env("OPENWAVE_CALLER_FIXTURE_READY", &independent_ready)
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !caller_ready.is_file() || !independent_ready.is_file() {
            assert!(
                std::time::Instant::now() < deadline,
                "owned fixture startup deadline"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let uid = rustix::process::geteuid().as_raw();
        let identity = observe_process(child.0.id()).unwrap();
        let owners = process_owners(&paths, Some(&snapshot), uid, None).unwrap();
        assert!(owners.contains(&child.0.id()));
        assert!(owners.contains(&independent.0.id()));
        let mut stale = identity.clone();
        stale.start += 1;
        assert!(process_owners(&paths, Some(&snapshot), uid, Some(&stale)).is_err());
        assert!(
            !process_owners(&paths, Some(&snapshot), uid, Some(&identity))
                .unwrap()
                .contains(&child.0.id())
        );
        assert!(
            process_owners(&paths, Some(&snapshot), uid, Some(&identity))
                .unwrap()
                .contains(&independent.0.id())
        );
    }
}
