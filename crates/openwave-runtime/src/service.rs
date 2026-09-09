use crate::{
    installation::{self, InstallMethod},
    paths::{self, RuntimePaths},
    process::CommandRunner,
};
use openwave_core::model::{ErrorCode, OperationError, Result};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Output,
    sync::Arc,
    time::Duration,
};

pub const SYSTEMD_UNIT: &str = "openwave.service";
pub const RUNIT_SERVICE: &str = "wavexlr-audio";
const UNSUPPORTED: &str = "No supported init system detected. Configure the native openwave-daemon with your session's service manager.";

/// Explicit host boundary used by isolated fixtures as well as the real adapter.
pub trait HostCommands: Send + Sync {
    fn available(&self, program: &str) -> bool;
    fn run(&self, program: &str, args: &[String], timeout: Duration) -> Result<Output>;
    fn package_owner(&self, files: &[PathBuf]) -> Result<Option<InstallMethod>>;
    fn find_program(&self, program: &str) -> Option<PathBuf> {
        find_program(program)
    }
}
pub struct NativeCommands;
impl HostCommands for NativeCommands {
    fn available(&self, program: &str) -> bool {
        find_program(program).is_some()
    }
    fn run(&self, program: &str, args: &[String], timeout: Duration) -> Result<Output> {
        CommandRunner::default().run_status(program, args, timeout)
    }
    fn package_owner(&self, files: &[PathBuf]) -> Result<Option<InstallMethod>> {
        installation::package_owner(files)
    }
}
pub fn find_program(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|base| base.join(name))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

pub struct HostContext {
    pub home: PathBuf,
    pub config_home: PathBuf,
    pub data_home: PathBuf,
    pub udev_directory: PathBuf,
    pub runit_link: PathBuf,
    pub runit_definition: PathBuf,
    pub username: String,
    pub uid: u32,
    pub durable_bins: Vec<PathBuf>,
    pub sandboxed: bool,
    pub commands: Arc<dyn HostCommands>,
}
impl HostContext {
    pub fn discover() -> Result<Self> {
        let uid = rustix::process::getuid().as_raw();
        let passwd = fs::read_to_string("/etc/passwd")?;
        let username = passwd
            .lines()
            .find_map(|line| {
                let fields: Vec<_> = line.split(':').collect();
                (fields.len() >= 7 && fields[2].parse::<u32>().ok() == Some(uid))
                    .then(|| fields[0].to_owned())
            })
            .ok_or_else(|| {
                OperationError::new(ErrorCode::Unavailable, "Cannot resolve current login UID")
            })?;
        let home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| OperationError::new(ErrorCode::Unavailable, "HOME is unavailable"))?;
        Ok(Self {
            config_home: paths::xdg_config_home()?,
            data_home: paths::xdg_data_home()?,
            udev_directory: "/etc/udev/rules.d".into(),
            runit_link: PathBuf::from("/var/service").join(RUNIT_SERVICE),
            runit_definition: PathBuf::from("/etc/sv").join(RUNIT_SERVICE),
            username: username.clone(),
            uid,
            durable_bins: vec![
                home.join(".nix-profile/bin"),
                home.join(".local/state/nix/profile/bin"),
                PathBuf::from("/etc/profiles/per-user")
                    .join(username)
                    .join("bin"),
                "/run/current-system/sw/bin".into(),
            ],
            home,
            sandboxed: crate::setup::is_sandboxed(),
            commands: Arc::new(NativeCommands),
        })
    }
    pub fn require_native(&self) -> Result<()> {
        if self.sandboxed {
            Err(OperationError::new(
                ErrorCode::Unsupported,
                crate::setup::SANDBOX_GUIDANCE,
            ))
        } else {
            Ok(())
        }
    }
    pub fn command(&self, program: &str, args: &[&str], timeout: Duration) -> Result<Output> {
        let output = self.commands.run(
            program,
            &args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
            timeout,
        )?;
        if output.status.success() {
            Ok(output)
        } else {
            Err(OperationError::new(
                ErrorCode::Unavailable,
                format!(
                    "{program} failed ({}): {}{}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
            ))
        }
    }
}

pub(crate) fn text_path(path: &Path) -> Result<&str> {
    path.to_str()
        .filter(|s| !s.chars().any(|c| c.is_control()))
        .ok_or_else(|| OperationError::invalid("Path is not a printable UTF-8 path"))
}
pub(crate) fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(m) if !m.is_file() || m.file_type().is_symlink() => Err(OperationError::new(
            ErrorCode::Identity,
            format!("Refusing nonregular integration file {}", path.display()),
        )),
        Ok(m) if m.len() > 4 * 1024 * 1024 => {
            Err(OperationError::invalid("Integration file exceeds 4 MiB"))
        }
        Ok(_) => Ok(Some(fs::read_to_string(path)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
/// Same-directory publication; a failed write leaves the previous bytes intact.
pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<()> {
    if paths::has_symlink_ancestor(path)? {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Integration path has externally managed symlink ancestry; preserved",
        ));
    }
    read_optional(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| OperationError::invalid("Missing parent directory"))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".openwave-")
        .tempfile_in(parent)?;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|e| OperationError::from(e.error))?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

/// Decode quoted argument lists, rejecting unterminated escapes rather than guessing.
pub fn split_words(value: &str) -> Result<Vec<String>> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut started = false;
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' && quote != Some('\'') {
            word.push(
                chars
                    .next()
                    .ok_or_else(|| OperationError::invalid("Incomplete command escape"))?,
            );
            started = true;
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                word.push(c);
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
            started = true;
        } else if c.is_whitespace() {
            if started {
                result.push(std::mem::take(&mut word));
                started = false;
            }
        } else {
            word.push(c);
            started = true;
        }
    }
    if quote.is_some() {
        return Err(OperationError::invalid("Unterminated command quote"));
    }
    if started {
        result.push(word);
    }
    Ok(result)
}
fn daemon_path(paths: &RuntimePaths) -> PathBuf {
    paths
        .prefix
        .as_ref()
        .map(|p| p.join("bin/openwave-daemon"))
        .unwrap_or_else(|| paths.executable.with_file_name("openwave-daemon"))
}
fn same_path(a: &Path, b: &Path) -> bool {
    // Missing stale launchers are accepted only at their exact same-install path.
    a == b
        || fs::canonicalize(a)
            .ok()
            .zip(fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}
pub fn daemon_launcher(paths: &RuntimePaths, host: &HostContext) -> Result<PathBuf> {
    let expected = daemon_path(paths);
    let canonical = fs::canonicalize(&expected)?;
    if canonical.starts_with("/nix/store") || canonical.starts_with("/gnu/store") {
        for directory in &host.durable_bins {
            let candidate = directory.join("openwave-daemon");
            if same_path(&candidate, &expected)
                && fs::metadata(&candidate).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
            {
                return Ok(candidate);
            }
        }
    }
    let meta = fs::metadata(&expected)?;
    if !meta.is_file() || meta.mode() & 0o111 == 0 {
        return Err(OperationError::new(
            ErrorCode::Unavailable,
            "Build/install the same-install openwave-daemon executable first",
        ));
    }
    Ok(expected)
}
fn systemd_quote(value: &str) -> String {
    format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
            .replace('$', "$$")
    )
}
pub fn render_unit(paths: &RuntimePaths, host: &HostContext) -> Result<String> {
    let command = systemd_quote(text_path(&daemon_launcher(paths, host)?)?);
    Ok(format!(
        "[Unit]\nDescription=OpenWave Audio Manager\nWants=pipewire.service wireplumber.service\nBefore=pipewire.service wireplumber.service\nStartLimitIntervalSec=60\nStartLimitBurst=5\n\n[Service]\nType=simple\nExecStart={command}\nRestart=on-failure\nRestartSec=3\n\n[Install]\nWantedBy=default.target\n"
    ))
}
fn daemon_matches(args: &[String], workdir: &str, paths: &RuntimePaths) -> bool {
    let directory_ok = workdir.is_empty()
        || paths
            .source
            .as_ref()
            .is_some_and(|p| same_path(Path::new(workdir), p));
    directory_ok
        && matches!(args, [program] if Path::new(program).is_absolute() && same_path(Path::new(program), &daemon_path(paths)))
}
pub(crate) fn ini_section(text: &str, wanted: &str) -> Result<BTreeMap<String, String>> {
    let mut active = false;
    let mut result = BTreeMap::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with(['#', ';']))
    {
        if line.starts_with('[') {
            active = line == format!("[{wanted}]");
            continue;
        }
        if !active {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| OperationError::invalid("Malformed service directive"))?;
        if result
            .insert(key.trim().to_owned(), value.trim().to_owned())
            .is_some()
        {
            return Err(OperationError::invalid(
                "Ambiguous duplicate service directive",
            ));
        }
    }
    Ok(result)
}
pub(crate) fn parse_show(text: &str) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| OperationError::invalid("Malformed systemd observation"))?;
        if map.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(OperationError::invalid("Duplicate systemd observation"));
        }
    }
    // A missing unit has no service-specific properties: systemctl omits
    // ExecStart even with --all. Callers handle this state before reading them.
    if map
        .get("LoadState")
        .is_some_and(|state| state == "not-found")
    {
        return Ok(map);
    }
    for key in [
        "LoadState",
        "ActiveState",
        "UnitFileState",
        "FragmentPath",
        "ExecStart",
        "WorkingDirectory",
    ] {
        if !map.contains_key(key) {
            return Err(OperationError::new(
                ErrorCode::Unavailable,
                format!("Missing systemd observation {key}"),
            ));
        }
    }
    Ok(map)
}
pub(crate) fn effective_args(value: &str) -> Result<Vec<String>> {
    let inner = value
        .strip_prefix('{')
        .and_then(|v| v.strip_suffix('}'))
        .ok_or_else(|| OperationError::invalid("Unknown systemd ExecStart encoding"))?;
    if inner.contains(['{', '}']) {
        return Err(OperationError::invalid(
            "Multiple effective service commands",
        ));
    }
    let fields: BTreeMap<_, _> = inner
        .split(';')
        .filter_map(|s| s.trim().split_once('='))
        .collect();
    let path = fields
        .get("path")
        .ok_or_else(|| OperationError::invalid("Missing effective executable"))?
        .trim();
    let args = split_words(
        fields
            .get("argv[]")
            .ok_or_else(|| OperationError::invalid("Missing effective argv"))?
            .trim(),
    )?;
    if args.first().map(String::as_str) != Some(path) {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Effective executable differs from argv",
        ));
    }
    Ok(args)
}
#[derive(Debug, Clone)]
pub struct ServiceStatus {
    pub running: bool,
    pub failed: bool,
    pub message: String,
}
struct SystemdObservation {
    fragment: PathBuf,
    text: String,
    running: bool,
    failed: bool,
    enabled: bool,
    managed: bool,
}
fn observe_systemd(paths: &RuntimePaths, host: &HostContext) -> Result<Option<SystemdObservation>> {
    let output = host.command("systemctl", &["--user","show",SYSTEMD_UNIT,"--property=LoadState,ActiveState,UnitFileState,FragmentPath,ExecStart,WorkingDirectory"], Duration::from_secs(5))?;
    let values = parse_show(
        std::str::from_utf8(&output.stdout)
            .map_err(|_| OperationError::invalid("Non-UTF8 systemd observation"))?,
    )?;
    let local = host.config_home.join("systemd/user").join(SYSTEMD_UNIT);
    if values["LoadState"] == "not-found" {
        if fs::symlink_metadata(&local).is_ok() {
            return Err(OperationError::new(
                ErrorCode::Identity,
                "A local service exists but its effective ownership is unavailable; reload/inspect it before setup",
            ));
        }
        return Ok(None);
    }
    if values["LoadState"] != "loaded" {
        return Err(OperationError::new(
            ErrorCode::Unavailable,
            format!("Service cannot be inspected: {}", values["LoadState"]),
        ));
    }
    let fragment = PathBuf::from(&values["FragmentPath"]);
    if !fragment.is_absolute() {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Missing service fragment identity",
        ));
    }
    let canonical = fs::canonicalize(&fragment)?;
    let text = read_optional(&canonical)?
        .ok_or_else(|| OperationError::new(ErrorCode::Identity, "Service fragment disappeared"))?;
    let directives = ini_section(&text, "Service")?;
    if directives
        .keys()
        .any(|k| k.starts_with("Exec") && k != "ExecStart")
    {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Service has additional unproven executable directives",
        ));
    }
    let args = split_words(
        directives
            .get("ExecStart")
            .ok_or_else(|| OperationError::invalid("Service fragment lacks ExecStart"))?,
    )?
    .into_iter()
    .map(|s| s.replace("%%", "%").replace("$$", "$"))
    .collect::<Vec<_>>();
    let workdir = directives
        .get("WorkingDirectory")
        .map(String::as_str)
        .unwrap_or("");
    // User units default to HOME; systemd marks optional directories with '!'.
    // Only an unset fragment directory may inherit that default.
    let effective_workdir = values["WorkingDirectory"]
        .strip_prefix('!')
        .unwrap_or(&values["WorkingDirectory"]);
    let effective_workdir = if workdir.is_empty()
        && (effective_workdir == "~" || same_path(Path::new(effective_workdir), &host.home))
    {
        ""
    } else {
        effective_workdir
    };
    if !daemon_matches(&args, workdir, paths)
        || !daemon_matches(
            &effective_args(&values["ExecStart"])?,
            effective_workdir,
            paths,
        )
    {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Capture service belongs to another or unproven installation; preserved",
        ));
    }
    let managed = fragment != local
        || paths::has_symlink_ancestor(&local)?
        || host
            .commands
            .package_owner(&[fragment.clone(), canonical])?
            .is_some();
    Ok(Some(SystemdObservation {
        fragment,
        text,
        running: values["ActiveState"] == "active",
        failed: values["ActiveState"] == "failed",
        enabled: values["UnitFileState"] == "enabled",
        managed,
    }))
}
fn systemd(host: &HostContext) -> bool {
    host.commands.available("systemctl")
}
fn runit(host: &HostContext) -> bool {
    host.commands.available("sv") && host.runit_link.parent().is_some_and(Path::is_dir)
}
fn observe_runit(paths: &RuntimePaths, host: &HostContext) -> Result<Option<(PathBuf, bool)>> {
    if fs::symlink_metadata(&host.runit_link).is_err() {
        if !host.runit_link.try_exists()? {
            return Ok(None);
        }
    }
    let target = fs::canonicalize(&host.runit_link)?;
    if target != fs::canonicalize(&host.runit_definition)? {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Foreign runit service target; preserved",
        ));
    }
    let run = target.join("run");
    let script = read_optional(&run)?
        .ok_or_else(|| OperationError::new(ErrorCode::Identity, "Missing runit definition"))?;
    let mut commands = Vec::new();
    for line in script.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line == "exec 2>&1" {
            continue;
        }
        commands.push(split_words(line)?);
    }
    let valid = commands.len() == 1
        && commands[0].len() == 5
        && commands[0][..3] == ["exec", "chpst", "-u"]
        && (commands[0][3] == host.username || commands[0][3] == host.uid.to_string())
        && daemon_matches(&commands[0][4..], "", paths);
    if !valid {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Unproven runit command or user; preserved",
        ));
    }
    let managed = host
        .commands
        .package_owner(&[run, target.join("log/run"), host.runit_link.clone()])?
        .is_some();
    Ok(Some((target, managed)))
}
pub fn status_with(paths: &RuntimePaths, host: &HostContext) -> Result<ServiceStatus> {
    host.require_native()?;
    if systemd(host) {
        return match observe_systemd(paths, host)? {
            None => Ok(ServiceStatus {
                running: false,
                failed: false,
                message: "Capture service is not installed".into(),
            }),
            Some(v) => {
                let stale = !v.managed && (v.text != render_unit(paths, host)? || !v.enabled);
                Ok(ServiceStatus {
                    running: v.running,
                    failed: v.failed,
                    message: if stale {
                        "Capture service needs refresh"
                    } else if v.failed {
                        "Capture service failed"
                    } else if v.running {
                        "Capture service running"
                    } else {
                        "Capture service stopped"
                    }
                    .into(),
                })
            }
        };
    }
    if runit(host) {
        if observe_runit(paths, host)?.is_none() {
            return Ok(ServiceStatus {running:false,failed:false,message:"Administrator must configure wavexlr-audio; OpenWave does not install privileged runit services".into()});
        }
        let output = host.command(
            "sv",
            &["status", text_path(&host.runit_link)?],
            Duration::from_secs(5),
        )?;
        let value = std::str::from_utf8(&output.stdout)
            .map_err(|_| OperationError::invalid("Invalid runit status"))?;
        if !value.starts_with("run:") && !value.starts_with("down:") {
            return Err(OperationError::new(
                ErrorCode::Unavailable,
                "Unknown runit status",
            ));
        }
        return Ok(ServiceStatus {
            running: value.starts_with("run:"),
            failed: false,
            message: value.trim().into(),
        });
    }
    Err(OperationError::new(ErrorCode::Unsupported, UNSUPPORTED))
}
pub fn install_with(paths: &RuntimePaths, host: &HostContext) -> Result<()> {
    host.require_native()?;
    if !systemd(host) {
        return Err(OperationError::new(
            ErrorCode::Unsupported,
            if runit(host) {
                "Have an administrator configure wavexlr-audio with the same-install native daemon; GUI runit installation is unsupported"
            } else {
                UNSUPPORTED
            },
        ));
    }
    let current = observe_systemd(paths, host)?;
    let wanted = render_unit(paths, host)?;
    if let Some(v) = &current {
        if v.managed {
            return if v.running {
                Ok(())
            } else {
                Err(OperationError::new(
                    ErrorCode::Unsupported,
                    "Service is managed externally; start/repair it through its manager",
                ))
            };
        }
        if v.text == wanted && v.enabled && v.running && !v.failed {
            return Ok(());
        }
    }
    let path = host.config_home.join("systemd/user").join(SYSTEMD_UNIT);
    if host
        .commands
        .package_owner(std::slice::from_ref(&path))?
        .is_some()
    {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Package-owned service must not be overwritten",
        ));
    }
    atomic_write(&path, &wanted)?;
    host.command(
        "systemctl",
        &["--user", "daemon-reload"],
        Duration::from_secs(30),
    )?;
    // Loading a new fragment can expose an existing drop-in that was invisible
    // while the unit was absent. Never restart before rechecking effective ownership.
    let loaded = observe_systemd(paths, host)?.ok_or_else(|| {
        OperationError::new(ErrorCode::Unavailable, "New capture service was not loaded")
    })?;
    if loaded.fragment != path || loaded.text != wanted || loaded.managed {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Loaded capture service differs from the published owned definition",
        ));
    }
    for args in [
        vec!["--user", "enable", SYSTEMD_UNIT],
        vec!["--user", "reset-failed", SYSTEMD_UNIT],
        vec!["--user", "restart", SYSTEMD_UNIT],
    ] {
        host.command("systemctl", &args, Duration::from_secs(30))?;
    }
    Ok(())
}
pub fn stop_owned_with(paths: &RuntimePaths, host: &HostContext) -> Result<()> {
    host.require_native()?;
    if systemd(host) {
        if observe_systemd(paths, host)?.is_some() {
            host.command(
                "systemctl",
                &["--user", "stop", SYSTEMD_UNIT],
                Duration::from_secs(30),
            )?;
        }
    } else if runit(host) {
        if observe_runit(paths, host)?.is_some() {
            host.command(
                "sv",
                &["-w", "15", "down", text_path(&host.runit_link)?],
                Duration::from_secs(20),
            )?;
        }
    } else {
        return Err(OperationError::new(ErrorCode::Unsupported, UNSUPPORTED));
    }
    Ok(())
}
pub fn remove_owned_with(paths: &RuntimePaths, host: &HostContext) -> Result<()> {
    host.require_native()?;
    stop_owned_with(paths, host)?;
    if systemd(host) {
        if let Some(v) = observe_systemd(paths, host)? {
            if v.running {
                return Err(OperationError::new(
                    ErrorCode::Busy,
                    "Capture service is still running",
                ));
            }
            if v.managed {
                return Ok(());
            }
            host.command(
                "systemctl",
                &["--user", "disable", SYSTEMD_UNIT],
                Duration::from_secs(30),
            )?;
            if paths::has_symlink_ancestor(&v.fragment)?
                || read_optional(&v.fragment)?.as_deref() != Some(&v.text)
            {
                return Err(OperationError::new(
                    ErrorCode::Identity,
                    "Service fragment changed during removal",
                ));
            }
            fs::remove_file(&v.fragment)?;
            host.command(
                "systemctl",
                &["--user", "daemon-reload"],
                Duration::from_secs(30),
            )?;
        }
    } else if let Some((target, managed)) = observe_runit(paths, host)? {
        if managed {
            return Ok(());
        }
        // Admin scripts are not deletion authority for arbitrary log commands/files.
        // Remove the exact activation link; retain administrator-supplied definitions.
        if fs::symlink_metadata(&host.runit_link)?
            .file_type()
            .is_symlink()
            && fs::canonicalize(&host.runit_link)? == target
        {
            fs::remove_file(&host.runit_link)?;
        } else {
            return Err(OperationError::new(
                ErrorCode::Identity,
                "Runit activation is not the inspected symlink",
            ));
        }
    }
    Ok(())
}
pub fn status(paths: &RuntimePaths) -> Result<ServiceStatus> {
    status_with(paths, &HostContext::discover()?)
}
pub fn install(paths: &RuntimePaths) -> Result<()> {
    install_with(paths, &HostContext::discover()?)
}
pub fn stop_owned(paths: &RuntimePaths) -> Result<()> {
    stop_owned_with(paths, &HostContext::discover()?)
}
pub fn remove_owned(paths: &RuntimePaths) -> Result<()> {
    remove_owned_with(paths, &HostContext::discover()?)
}
