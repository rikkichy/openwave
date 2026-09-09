use crate::{
    installation::{self, InstallMethod, InstallationFormat},
    paths::{self, RuntimePaths},
    service::{self, HostContext},
};
use openwave_core::model::{ErrorCode, OperationError, Result};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{self, IsTerminal, Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// Desktop Entry Exec has two escape layers, separate from shell quoting.
pub fn exec_arg(value: &str) -> Result<String> {
    if value.chars().any(|c| c.is_control()) {
        return Err(OperationError::invalid(
            "Control characters are not allowed in desktop commands",
        ));
    }
    let mut escaped = String::new();
    for c in value.chars() {
        if c == '%' {
            escaped.push_str("%%");
        } else {
            if matches!(c, '\\' | '"' | '`' | '$') {
                escaped.push('\\');
            }
            escaped.push(c);
        }
    }
    Ok(format!("\"{}\"", escaped.replace('\\', "\\\\")))
}
pub(crate) fn decode_exec(value: &str) -> Result<Vec<String>> {
    let mut unescaped = String::new();
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            unescaped.push(match chars.next() {
                Some('\\') => '\\',
                Some('s') => ' ',
                Some('n') => '\n',
                Some('t') => '\t',
                Some('r') => '\r',
                _ => return Err(OperationError::invalid("Invalid Desktop Entry escape")),
            });
        } else if c == '%' {
            if chars.next() != Some('%') {
                return Err(OperationError::new(
                    ErrorCode::Identity,
                    "Unexpected Desktop Entry field code",
                ));
            }
            unescaped.push('%');
        } else {
            unescaped.push(c);
        }
    }
    service::split_words(&unescaped)
}
fn values(text: &str) -> Result<BTreeMap<String, String>> {
    let mut active = false;
    let mut seen = false;
    let mut map = BTreeMap::new();
    for line in text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
    {
        if line.starts_with('[') {
            active = line == "[Desktop Entry]";
            if active && seen {
                return Err(OperationError::invalid("Duplicate Desktop Entry section"));
            }
            seen |= active;
            continue;
        }
        if !active {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| OperationError::invalid("Malformed desktop entry"))?;
        if map.insert(key.to_owned(), value.to_owned()).is_some() {
            return Err(OperationError::invalid("Duplicate desktop entry key"));
        }
    }
    if !seen {
        return Err(OperationError::invalid("Missing Desktop Entry section"));
    }
    Ok(map)
}
pub fn autostart_state_at(path: &Path) -> Result<(bool, bool)> {
    let Some(text) = service::read_optional(path)? else {
        return Ok((false, false));
    };
    let entry = values(&text)?;
    let enabled = entry.get("Hidden").map(String::as_str) != Some("true")
        && entry.get("X-GNOME-Autostart-enabled").map(String::as_str) != Some("false");
    let args = decode_exec(entry.get("Exec").map(String::as_str).unwrap_or(""))?;
    Ok((enabled, args.iter().any(|a| a == "--hide")))
}
pub fn autostart_state() -> Result<(bool, bool)> {
    autostart_state_at(&paths::xdg_config_home()?.join("autostart/openwave.desktop"))
}
pub(crate) fn owned(text: &str, paths: &RuntimePaths, host: &HostContext) -> Result<bool> {
    let entry = values(text)?;
    if entry.get("Type").map(String::as_str) != Some("Application") {
        return Ok(false);
    }
    let args = decode_exec(entry.get("Exec").map(String::as_str).unwrap_or(""))?;
    if args.is_empty() || args.len() > 2 || (args.len() == 2 && args[1] != "--hide") {
        return Ok(false);
    }
    let target = if args[0] == "openwave" {
        let Some(target) = host.commands.find_program("openwave") else {
            return Ok(false);
        };
        target
    } else {
        let target = PathBuf::from(&args[0]);
        if !target.is_absolute() {
            return Ok(false);
        }
        // An exact same-install path may be stale; a bare PATH lookup may not.
        if target == paths.executable {
            return Ok(true);
        }
        target
    };
    Ok(launcher_targets(&target, &paths.executable))
}

fn nix_wrapper_targets(bytes: &[u8], expected: &Path) -> bool {
    // This is only called after proving root-owned immutable same-package paths.
    // Inspect makeWrapper's final exec or makeBinaryWrapper's embedded build
    // command; neither a foreign ELF string nor an unknown wrapper is authority.
    if bytes.starts_with(b"\x7fELF") {
        let mut commands = bytes
            .split(|byte| *byte == 0)
            .filter_map(|part| std::str::from_utf8(part).ok())
            .filter_map(|part| {
                part.split_once("\nmakeCWrapper ")
                    .map(|(_, command)| command)
            });
        let Some(command) = commands.next() else {
            return false;
        };
        if commands.next().is_some() {
            return false;
        }
        let Some((command, _)) = command.split_once("\n# (Use `nix-shell -p makeBinaryWrapper`")
        else {
            return false;
        };
        let Ok(args) = service::split_words(&command.replace("\\\n", "")) else {
            return false;
        };
        if args.first().map(Path::new) != Some(expected) {
            return false;
        }
        let mut index = 1;
        while index < args.len() {
            let count = match args[index].as_str() {
                "--inherit-argv0" => 0,
                "--prefix" | "--suffix" => 3,
                "--set" | "--set-default" | "--prefix-each" | "--suffix-each" => 2,
                _ => return false,
            };
            index += count + 1;
            if index > args.len() {
                return false;
            }
        }
        return true;
    }
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    let Some(line) = text.lines().rev().find(|line| !line.trim().is_empty()) else {
        return false;
    };
    service::split_words(line.trim()).is_ok_and(|args| {
        args.len() == 5
            && args[0] == "exec"
            && args[1] == "-a"
            && args[2] == "$0"
            && Path::new(&args[3]) == expected
            && args[4] == "$@"
    })
}

fn launcher_targets(candidate: &Path, executable: &Path) -> bool {
    let (Ok(actual), Ok(expected)) = (fs::canonicalize(candidate), fs::canonicalize(executable))
    else {
        return false;
    };
    if actual == expected {
        return true;
    }
    let Some(bin) = expected.parent() else {
        return false;
    };
    if !expected.starts_with("/nix/store")
        || expected.file_name().and_then(|n| n.to_str()) != Some(".openwave-wrapped")
        || actual != bin.join("openwave")
    {
        return false;
    }
    // Use the same pinned file/directory checks as migration authority. The
    // root-owned sticky Nix store is not an unsafe writable package directory.
    let (Ok((file, wrapper)), Ok((_, target))) =
        (migration_file(&actual, 0), migration_file(&expected, 0))
    else {
        return false;
    };
    if wrapper.mode & 0o111 == 0 || target.mode & 0o111 == 0 {
        return false;
    }
    let mut bytes = Vec::new();
    file.take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .is_ok()
        && bytes.len() <= 4 * 1024 * 1024
        && nix_wrapper_targets(&bytes, &expected)
}

fn launcher(paths: &RuntimePaths, host: &HostContext) -> PathBuf {
    // Keep canonical installation identity for security, but publish a stable
    // profile path only while it demonstrably launches this exact executable.
    if fs::canonicalize(&paths.executable).is_ok() {
        for directory in &host.durable_bins {
            let candidate = directory.join("openwave");
            if candidate.is_absolute()
                && launcher_targets(&candidate, &paths.executable)
                && fs::metadata(&candidate)
                    .is_ok_and(|info| info.is_file() && info.permissions().mode() & 0o111 != 0)
            {
                return candidate;
            }
        }
    }
    paths.executable.clone()
}
fn render(
    paths: &RuntimePaths,
    host: &HostContext,
    autostart: bool,
    hidden: bool,
) -> Result<String> {
    let mut command = exec_arg(service::text_path(&launcher(paths, host))?)?;
    if hidden {
        command.push_str(" --hide");
    }
    let themed = host
        .data_home
        .join("icons/hicolor/scalable/apps/openwave.svg")
        .is_file()
        || paths.prefix.as_ref().is_some_and(|p| {
            p.join("share/icons/hicolor/scalable/apps/openwave.svg")
                .is_file()
        })
        || std::env::split_paths(
            &std::env::var_os("XDG_DATA_DIRS")
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/usr/local/share:/usr/share".into()),
        )
        .any(|p| p.join("icons/hicolor/scalable/apps/openwave.svg").is_file());
    Ok(format!(
        "[Desktop Entry]\nType=Application\nName=OpenWave\nComment=The audio mixing matrix for Linux\nExec={command}\nIcon={}\nCategories=AudioVideo;Audio;Mixer;\nTerminal=false\nStartupWMClass=com.github.openwave\nX-GNOME-UsesNotifications=true\n{}",
        if themed {
            "openwave"
        } else {
            "audio-input-microphone"
        },
        if autostart {
            "X-GNOME-Autostart-enabled=true\n"
        } else {
            ""
        }
    ))
}
fn guard(path: &Path, text: Option<&str>, paths: &RuntimePaths, host: &HostContext) -> Result<()> {
    if paths::has_symlink_ancestor(path)? {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Desktop integration has externally managed symlink ancestry; preserved",
        ));
    }
    if host.commands.package_owner(&[path.to_owned()])?.is_some() {
        return Err(OperationError::new(
            ErrorCode::Identity,
            "Desktop integration is package-managed; preserved",
        ));
    }
    if let Some(text) = text {
        if !owned(text, paths, host)? {
            return Err(OperationError::new(
                ErrorCode::Identity,
                "Desktop entry belongs to another or unproven installation; preserved. Before removing the previous installation, inspect with openwave --migrate-launchers-from /exact/previous/bin/openwave --dry-run, then explicitly confirm that migration. Missing previous installation authority cannot be reconstructed from an entry.",
            ));
        }
    }
    Ok(())
}
pub fn set_autostart_with(
    paths: &RuntimePaths,
    enabled: bool,
    hidden: bool,
    host: &HostContext,
) -> Result<(bool, bool)> {
    let path = host.config_home.join("autostart/openwave.desktop");
    let mutation = (|| -> Result<()> {
        let current = service::read_optional(&path)?;
        guard(&path, current.as_deref(), paths, host)?;
        if enabled {
            service::atomic_write(&path, &render(paths, host, true, hidden)?)?;
        } else if current.is_some() {
            guard(&path, current.as_deref(), paths, host)?;
            if service::read_optional(&path)? != current {
                return Err(OperationError::new(
                    ErrorCode::Identity,
                    "Autostart changed during update",
                ));
            }
            fs::remove_file(&path)?;
        }
        Ok(())
    })();
    match mutation {
        Ok(()) => autostart_state_at(&path),
        Err(error) => {
            let state = match autostart_state_at(&path) {
                Ok((enabled, hidden)) => format!("enabled={enabled}, hidden={hidden}"),
                Err(e) => format!("unavailable: {e}"),
            };
            Err(OperationError::new(
                error.code,
                format!("{error}; actual autostart state: {state}"),
            ))
        }
    }
}
pub fn set_autostart(paths: &RuntimePaths, enabled: bool, hidden: bool) -> Result<(bool, bool)> {
    set_autostart_with(paths, enabled, hidden, &HostContext::discover()?)
}
pub fn ensure_menu_entry_with(paths: &RuntimePaths, host: &HostContext) -> Result<bool> {
    let path = host.data_home.join("applications/openwave.desktop");
    let current = service::read_optional(&path)?;
    guard(&path, current.as_deref(), paths, host)?;
    let wanted = render(paths, host, false, false)?;
    if current.as_deref() == Some(&wanted) {
        return Ok(false);
    }
    service::atomic_write(&path, &wanted)?;
    Ok(true)
}
pub fn ensure_menu_entry(paths: &RuntimePaths) -> Result<bool> {
    ensure_menu_entry_with(paths, &HostContext::discover()?)
}
pub fn remove_owned_with(paths: &RuntimePaths, host: &HostContext) -> Result<()> {
    for path in [
        host.config_home.join("autostart/openwave.desktop"),
        host.data_home.join("applications/openwave.desktop"),
    ] {
        if paths::has_symlink_ancestor(&path)? {
            continue;
        }
        let Some(text) = service::read_optional(&path)? else {
            continue;
        };
        if !owned(&text, paths, host)? || host.commands.package_owner(&[path.clone()])?.is_some() {
            continue;
        }
        if paths::has_symlink_ancestor(&path)?
            || service::read_optional(&path)?.as_deref() != Some(&text)
        {
            return Err(OperationError::new(
                ErrorCode::Identity,
                "Desktop entry changed during removal",
            ));
        }
        fs::remove_file(path)?;
    }
    Ok(())
}
pub fn remove_owned(paths: &RuntimePaths) -> Result<()> {
    remove_owned_with(paths, &HostContext::discover()?)
}

fn migration_error(message: impl Into<String>) -> OperationError {
    OperationError::new(ErrorCode::Identity, message)
}

#[derive(Debug, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl FileIdentity {
    fn of(info: &fs::Metadata) -> Self {
        Self {
            device: info.dev(),
            inode: info.ino(),
            mode: info.mode(),
            uid: info.uid(),
            gid: info.gid(),
            size: info.len(),
            modified: (info.mtime(), info.mtime_nsec()),
            changed: (info.ctime(), info.ctime_nsec()),
        }
    }
}

fn migration_file(path: &Path, uid: u32) -> Result<(File, FileIdentity)> {
    let parent = paths::open_directory_for_uid(
        path.parent()
            .ok_or_else(|| migration_error("Missing file parent"))?,
        false,
        uid,
    )?;
    let fd = rustix::fs::openat(
        &parent,
        path.file_name()
            .ok_or_else(|| migration_error("Missing file name"))?,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let file = File::from(fd);
    let info = file.metadata()?;
    // Nix may deduplicate immutable authority files. Only mutable user entries
    // and manual-install authority require a single link; neither is a store path.
    let immutable_store = path.starts_with("/nix/store") && info.uid() == 0;
    if !info.is_file()
        || (info.nlink() != 1 && !immutable_store)
        || info.mode() & 0o022 != 0
        || (info.uid() != uid && info.uid() != 0)
    {
        return Err(migration_error(format!(
            "Untrusted or linked file: {}",
            path.display()
        )));
    }
    Ok((file, FileIdentity::of(&info)))
}

#[derive(Debug, PartialEq, Eq)]
struct LauncherAuthority {
    installation: installation::Installation,
    files: Vec<(PathBuf, FileIdentity, String)>,
}

fn launcher_authority(executable: &Path, uid: u32) -> Result<LauncherAuthority> {
    if !executable.is_absolute()
        || fs::canonicalize(executable)? != executable
        || !matches!(
            executable.file_name().and_then(|n| n.to_str()),
            Some("openwave" | ".openwave-wrapped")
        )
        || executable
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            != Some("bin")
    {
        return Err(migration_error(
            "Select the exact existing canonical native bin/openwave (or Nix bin/.openwave-wrapped) executable recorded in the old entry, not a profile alias",
        ));
    }
    let prefix = executable
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| migration_error("Missing native prefix"))?;
    let install = installation::inspect_prefix(prefix, None)?;
    let receipt = prefix.join("share/openwave/install-manifest.json");
    let metadata = installation::read_metadata(&receipt)?;
    if metadata["schema"] != 2 || metadata["application"] != "openwave" {
        return Err(migration_error(
            "Launcher migration requires an existing native installation receipt; do not remove the previous installation first",
        ));
    }
    match install.method {
        InstallMethod::Manual
            if install.snapshot.as_ref().is_some_and(|s| {
                s.format == InstallationFormat::RustV2 && s.files.contains(&executable.to_owned())
            }) =>
        {
            ()
        }
        InstallMethod::Nix
            if prefix.starts_with("/nix/store")
                && metadata["method"] == "nix"
                && metadata["prefix"].as_str() == prefix.to_str() => {}
        _ => {
            return Err(migration_error(
                "No validated native manual/Nix installation authority. Keep the previous installation and use its package manager's launcher guidance.",
            ));
        }
    }
    let authority_uid = if install.method == InstallMethod::Nix {
        0
    } else {
        uid
    };
    let mut files = Vec::new();
    let mut authority_paths = vec![
        executable.to_owned(),
        receipt,
        prefix.join("share/openwave/VERSION"),
        prefix.join("libexec/openwave-maintenance"),
    ];
    if executable.file_name().and_then(|n| n.to_str()) == Some(".openwave-wrapped") {
        let wrapper = prefix.join("bin/openwave");
        if !launcher_targets(&wrapper, executable) {
            return Err(migration_error(
                "Nix wrapper does not prove the exact recorded native executable",
            ));
        }
        authority_paths.push(wrapper);
    }
    for path in authority_paths {
        let (_, identity) = migration_file(&path, authority_uid)?;
        if (path == executable || path.ends_with("libexec/openwave-maintenance"))
            && identity.mode & 0o111 == 0
        {
            return Err(migration_error("Native launcher/helper is not executable"));
        }
        let digest = installation::file_digest(&path)?;
        if migration_file(&path, authority_uid)?.1 != identity {
            return Err(migration_error(
                "Installation authority changed during inspection",
            ));
        }
        files.push((path, identity, digest));
    }
    Ok(LauncherAuthority {
        installation: install,
        files,
    })
}

#[derive(Debug, PartialEq, Eq)]
struct LauncherEntry {
    path: PathBuf,
    identity: FileIdentity,
    parent: FileIdentity,
    before: String,
    after: String,
}

/// Read-only, bounded handoff plan. Private fields prevent callers from inventing
/// authority or changing the executable to which consent applies.
#[derive(Debug, PartialEq, Eq)]
pub struct LauncherMigration {
    previous: PathBuf,
    current: PathBuf,
    launcher: PathBuf,
    previous_authority: LauncherAuthority,
    current_authority: LauncherAuthority,
    entries: Vec<LauncherEntry>,
}
impl LauncherMigration {
    pub fn describe(&self) -> String {
        let mut text = format!(
            "Previous executable: {}\nCurrent executable: {}\nStable launcher: {}\nOnly these user entries will be rewritten (enabled/hidden intent preserved):",
            self.previous.display(),
            self.current.display(),
            self.launcher.display()
        );
        for entry in &self.entries {
            text.push_str(&format!("\n  {}", entry.path.display()));
        }
        if self.entries.is_empty() {
            text.push_str("\n  None");
        }
        text.push_str("\nNo installation files will be executed, removed or modified.");
        text
    }
}

/// Inspect only the two application-owned user launcher locations.
pub fn inspect_launcher_migration_with(
    paths: &RuntimePaths,
    previous: &Path,
    host: &HostContext,
) -> Result<LauncherMigration> {
    crate::process::require_user()?;
    let uid = rustix::process::geteuid().as_raw();
    if host.sandboxed || uid != host.uid {
        return Err(migration_error(
            "Launcher migration requires the ordinary native login user",
        ));
    }
    let previous_authority = launcher_authority(previous, uid)?;
    let current_authority = launcher_authority(&paths.executable, uid)?;
    if previous == paths.executable {
        return Err(migration_error(
            "Previous and current installations must be distinct",
        ));
    }
    let stable = launcher(paths, host);
    if stable == paths.executable {
        return Err(migration_error(
            "No proven stable current launcher. Select this installation in ~/.nix-profile or the per-user/system Nix profile before migrating; a versioned store path on PATH is not a stable profile",
        ));
    }
    let mut entries = Vec::new();
    for path in [
        host.data_home.join("applications/openwave.desktop"),
        host.config_home.join("autostart/openwave.desktop"),
    ] {
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
            Ok(_) => (),
        }
        if host.commands.package_owner(&[path.clone()])?.is_some() {
            return Err(migration_error(format!(
                "Package-managed entry preserved: {}",
                path.display()
            )));
        }
        let (file, identity) = migration_file(&path, uid)?;
        if identity.uid != uid || identity.size > 4 * 1024 * 1024 {
            return Err(migration_error(
                "Launcher must be a bounded ordinary-user-owned file",
            ));
        }
        let mut before = String::new();
        file.take(4 * 1024 * 1024 + 1).read_to_string(&mut before)?;
        if before.len() > 4 * 1024 * 1024 || migration_file(&path, uid)?.1 != identity {
            return Err(migration_error("Launcher changed during inspection"));
        }
        let entry = values(&before)?;
        let args = decode_exec(entry.get("Exec").map(String::as_str).unwrap_or(""))?;
        // Unlike ordinary same-install adoption, handoff accepts only the
        // canonical application-generated shape and the explicitly selected path.
        let canonical = [
            ("Type", "Application"),
            ("Name", "OpenWave"),
            ("Comment", "The audio mixing matrix for Linux"),
            ("Categories", "AudioVideo;Audio;Mixer;"),
            ("Terminal", "false"),
            ("StartupWMClass", "com.github.openwave"),
            ("X-GNOME-UsesNotifications", "true"),
        ];
        if !canonical
            .iter()
            .all(|(key, value)| entry.get(*key).map(String::as_str) == Some(*value))
            || !matches!(
                entry.get("Icon").map(String::as_str),
                Some("openwave" | "audio-input-microphone")
            )
            || entry.keys().any(|key| {
                !canonical.iter().any(|(k, _)| *k == key.as_str())
                    && !matches!(
                        key.as_str(),
                        "Exec" | "Icon" | "Hidden" | "X-GNOME-Autostart-enabled"
                    )
            })
            || ["Hidden", "X-GNOME-Autostart-enabled"]
                .iter()
                .any(|key| entry.get(*key).is_some_and(|v| v != "true" && v != "false"))
            || args.is_empty()
            || args.len() > 2
            || (args.len() == 2 && args[1] != "--hide")
            || before
                .lines()
                .filter(|line| line.trim().starts_with('['))
                .count()
                != 1
            || before
                .lines()
                .filter(|line| line.starts_with("Exec="))
                .count()
                != 1
        {
            return Err(migration_error(format!(
                "Foreign or noncanonical launcher preserved: {}",
                path.display()
            )));
        }
        if Path::new(&args[0]) != previous {
            // A partly completed handoff may already have published this exact
            // durable launcher. Leave it untouched and migrate only the remainder.
            if Path::new(&args[0]) == stable && owned(&before, paths, host)? {
                continue;
            }
            return Err(migration_error(format!(
                "Launcher does not belong to the selected previous executable; preserved: {}",
                path.display()
            )));
        }
        let command = format!(
            "Exec={}{}",
            exec_arg(service::text_path(&stable)?)?,
            if args.len() == 2 { " --hide" } else { "" }
        );
        let after = before
            .split_inclusive('\n')
            .map(|line| {
                if line.starts_with("Exec=") {
                    format!("{command}{}", if line.ends_with('\n') { "\n" } else { "" })
                } else {
                    line.to_owned()
                }
            })
            .collect();
        let parent = FileIdentity::of(&fs::metadata(path.parent().unwrap())?);
        entries.push(LauncherEntry {
            path,
            identity,
            parent,
            before,
            after,
        });
    }
    Ok(LauncherMigration {
        previous: previous.to_owned(),
        current: paths.executable.clone(),
        launcher: stable,
        previous_authority,
        current_authority,
        entries,
    })
}

pub fn apply_launcher_migration_with(
    paths: &RuntimePaths,
    plan: &LauncherMigration,
    confirmed: bool,
    host: &HostContext,
) -> Result<usize> {
    if !confirmed {
        return Err(migration_error(
            "Explicit consent to the inspected previous executable is required; no changes made",
        ));
    }
    if inspect_launcher_migration_with(paths, &plan.previous, host)? != *plan {
        return Err(migration_error(
            "Launcher entries or installation authority changed after inspection; inspect again",
        ));
    }
    for entry in &plan.entries {
        let uid = rustix::process::geteuid().as_raw();
        let parent = paths::open_directory_for_uid(entry.path.parent().unwrap(), false, uid)?;
        let temporary_name = format!(".openwave-migrate-{}", uuid::Uuid::new_v4().simple());
        let fd = rustix::fs::openat(
            &parent,
            temporary_name.as_str(),
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::CREATE
                | rustix::fs::OFlags::EXCL
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .map_err(io::Error::from)?;
        let result = (|| -> Result<()> {
            let mut temporary = File::from(fd);
            temporary.set_permissions(fs::Permissions::from_mode(entry.identity.mode & 0o777))?;
            temporary.write_all(entry.after.as_bytes())?;
            temporary.sync_all()?;
            // Recheck authority and exact opened target immediately before the
            // descriptor-relative rename. Never follow a replaced parent link.
            if launcher_authority(&plan.previous, uid)? != plan.previous_authority
                || launcher_authority(&paths.executable, uid)? != plan.current_authority
                || launcher(paths, host) != plan.launcher
                || host
                    .commands
                    .package_owner(&[entry.path.clone()])?
                    .is_some()
                || migration_file(&entry.path, uid)?.1 != entry.identity
            {
                return Err(migration_error(
                    "Migration authority changed before publication",
                ));
            }
            let (file, identity) = migration_file(&entry.path, uid)?;
            let mut bytes = String::new();
            file.take(4 * 1024 * 1024 + 1).read_to_string(&mut bytes)?;
            if identity != entry.identity || bytes != entry.before {
                return Err(migration_error("Launcher bytes changed before publication"));
            }
            let reopened = paths::open_directory_for_uid(entry.path.parent().unwrap(), false, uid)?;
            let old = rustix::fs::fstat(&parent).map_err(io::Error::from)?;
            let new = rustix::fs::fstat(&reopened).map_err(io::Error::from)?;
            if old.st_dev != new.st_dev
                || old.st_ino != new.st_ino
                || new.st_dev != entry.parent.device
                || new.st_ino != entry.parent.inode
                || new.st_mode != entry.parent.mode
                || new.st_uid != entry.parent.uid
                || new.st_gid != entry.parent.gid
            {
                return Err(migration_error(
                    "Launcher parent changed before publication",
                ));
            }
            rustix::fs::renameat(
                &parent,
                temporary_name.as_str(),
                &parent,
                entry.path.file_name().unwrap(),
            )
            .map_err(io::Error::from)?;
            rustix::fs::fsync(&parent).map_err(io::Error::from)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(
                &parent,
                temporary_name.as_str(),
                rustix::fs::AtFlags::empty(),
            );
        }
        result?;
    }
    Ok(plan.entries.len())
}

pub fn migrate_launchers_cli(
    paths: &RuntimePaths,
    previous: &Path,
    yes: bool,
    dry_run: bool,
) -> i32 {
    let outcome = (|| -> Result<()> {
        let host = HostContext::discover()?;
        let plan = inspect_launcher_migration_with(paths, previous, &host)?;
        println!("{}", plan.describe());
        if dry_run || plan.entries.is_empty() {
            return Ok(());
        }
        if !yes {
            if !io::stdin().is_terminal() {
                return Err(migration_error(
                    "Run interactively or pass --yes to consent to this previous executable. No changes made.",
                ));
            }
            print!("Migrate the displayed launchers from this previous executable? [y/N] ");
            io::stdout().flush()?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                println!("Cancelled; no changes made.");
                return Ok(());
            }
        }
        let count = apply_launcher_migration_with(paths, &plan, true, &host)?;
        println!("Migrated {count} launcher(s).");
        Ok(())
    })();
    match outcome {
        Ok(()) => 0,
        Err(error) => {
            eprintln!(
                "openwave: {error}\nKeep the previous installation available until migration succeeds; missing authority is not inferred from launcher text. If an update partly completed, inspect each displayed launcher before retrying."
            );
            1
        }
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn nix_wrapper_proof_requires_exact_final_exec_without_added_arguments() {
        let expected = Path::new("/nix/store/example-openwave/bin/.openwave-wrapped");
        assert!(nix_wrapper_targets(b"#!/bin/bash\nexport PATH=/private/bin\nexec -a \"$0\" \"/nix/store/example-openwave/bin/.openwave-wrapped\" \"$@\"\n", expected));
        for text in [
            "exec -a \"$0\" \"/nix/store/other-openwave/bin/.openwave-wrapped\" \"$@\"",
            "exec -a \"$0\" \"/nix/store/example-openwave/bin/.openwave-wrapped\" --uninstall \"$@\"",
            "exec -a \"$0\" \"/nix/store/example-openwave/bin/.openwave-wrapped\" \"$@\"\nexec /foreign/openwave",
        ] {
            assert!(!nix_wrapper_targets(text.as_bytes(), expected));
        }
    }

    #[test]
    fn binary_wrapper_proof_refuses_retargeting_and_added_flags() {
        let expected = Path::new("/nix/store/example-openwave/bin/.openwave-wrapped");
        let metadata = b"\x7fELF\0# The C-code for this binary wrapper has been generated using the following command:\nmakeCWrapper '/nix/store/example-openwave/bin/.openwave-wrapped' \\\n --inherit-argv0 \\\n --prefix 'PATH' ':' '/private/bin'\n# (Use `nix-shell -p makeBinaryWrapper` to get access to makeCWrapper in your shell)\n\0";
        assert!(nix_wrapper_targets(metadata, expected));
        assert!(!nix_wrapper_targets(
            metadata,
            Path::new("/nix/store/other/bin/.openwave-wrapped")
        ));
        let altered = String::from_utf8_lossy(metadata)
            .replace("--inherit-argv0", "--add-flags '--uninstall'");
        assert!(!nix_wrapper_targets(altered.as_bytes(), expected));
    }
}
