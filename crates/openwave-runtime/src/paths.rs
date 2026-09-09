//! Read-only layout discovery and opt-in, descriptor-owned process leases.
use glib::variant::ToVariant;
use openwave_core::{
    VERSION,
    model::{ErrorCode, OperationError, Result},
};
use rustix::fs::{FlockOperation, Mode, OFlags};
use sha2::{Digest, Sha256};
use std::{
    env,
    ffi::OsStr,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::OwnedFd,
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
};

mod privilege;
pub(crate) use privilege::original_user_environment;

pub(crate) fn user_account(uid: u32) -> Result<(String, u32, PathBuf)> {
    for line in fs::read_to_string("/etc/passwd")?.lines() {
        let fields: Vec<_> = line.split(':').collect();
        if fields.len() >= 7 && fields[2].parse::<u32>().ok() == Some(uid) {
            let gid = fields[3]
                .parse::<u32>()
                .map_err(|_| identity_error("Invalid login group"))?;
            let home = absolute(PathBuf::from(fields[5]))?;
            return Ok((fields[0].into(), gid, home));
        }
    }
    Err(identity_error("Original login account not found"))
}

fn canonical_lock_identity(identity: &Path, allow_missing: bool) -> Result<PathBuf> {
    absolute(identity.to_owned())?;
    let normalized: PathBuf = identity.components().collect();
    if normalized.as_os_str() != identity.as_os_str() {
        return Err(identity_error("Installation identity must be canonical"));
    }
    match fs::canonicalize(identity) {
        Ok(canonical) if canonical == identity && canonical.is_dir() => Ok(canonical),
        Ok(_) => Err(identity_error(
            "Installation identity changed or contains a symlink",
        )),
        Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
            let mut prefix = PathBuf::from("/");
            for component in identity.components() {
                let Component::Normal(name) = component else {
                    continue;
                };
                prefix.push(name);
                match fs::symlink_metadata(&prefix) {
                    Ok(info) if info.is_dir() && !info.file_type().is_symlink() => {}
                    Ok(_) => {
                        return Err(identity_error(
                            "Missing installation identity has an unsafe ancestor",
                        ));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(identity.to_owned())
        }
        Err(error) => Err(error.into()),
    }
}

fn os_error(error: rustix::io::Errno) -> OperationError {
    std::io::Error::from(error).into()
}
fn identity_error(message: impl Into<String>) -> OperationError {
    OperationError::new(ErrorCode::Identity, message)
}
fn absolute(path: PathBuf) -> Result<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(identity_error(format!(
            "Expected an absolute, normalized path: {}",
            path.display()
        )));
    }
    Ok(path)
}

/// Missing integration files are allowed; linked ancestry is never local
/// publication or deletion authority, even when the target has the same owner.
pub(crate) fn has_symlink_ancestor(path: &Path) -> Result<bool> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(identity_error(
            "Integration path must be absolute and normalized",
        ));
    }
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(info) if info.file_type().is_symlink() => return Ok(true),
            Ok(_) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}
fn nonempty_env(name: &str) -> Option<std::ffi::OsString> {
    env::var_os(name).filter(|value| !value.is_empty())
}
fn xdg(name: &str, fallback: &str) -> Result<PathBuf> {
    if let Some(value) = nonempty_env(name) {
        return absolute(PathBuf::from(value));
    }
    let home = nonempty_env("HOME")
        .ok_or_else(|| OperationError::unavailable("HOME is unset; cannot locate user state"))?;
    absolute(PathBuf::from(home).join(fallback))
}
pub fn xdg_config_home() -> Result<PathBuf> {
    xdg("XDG_CONFIG_HOME", ".config")
}
pub fn xdg_data_home() -> Result<PathBuf> {
    xdg("XDG_DATA_HOME", ".local/share")
}
pub fn xdg_state_home() -> Result<PathBuf> {
    xdg("XDG_STATE_HOME", ".local/state")
}
pub fn config_dir() -> Result<PathBuf> {
    Ok(xdg_config_home()?.join("openwave"))
}
pub fn data_dir() -> Result<PathBuf> {
    Ok(xdg_data_home()?.join("openwave"))
}
pub fn state_dir() -> Result<PathBuf> {
    Ok(xdg_state_home()?.join("openwave"))
}

#[derive(Clone, Debug)]
pub struct RuntimePaths {
    pub executable: PathBuf,
    pub prefix: Option<PathBuf>,
    pub data: PathBuf,
    pub identity: PathBuf,
    pub maintenance: PathBuf,
    pub source: Option<PathBuf>,
}
fn executable_file(path: &Path) -> Result<PathBuf> {
    let info = fs::metadata(path)?;
    if !info.is_file() || info.mode() & 0o111 == 0 {
        return Err(identity_error(format!(
            "Not an executable file: {}",
            path.display()
        )));
    }
    Ok(fs::canonicalize(path)?)
}
fn version_witness(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut version = String::new();
    if file.take(128).read_to_string(&mut version).is_err() {
        return false;
    }
    version == VERSION || version == format!("{VERSION}\n")
}
fn relative(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(identity_error(
            "Asset must be a nonempty relative path without traversal",
        ));
    }
    Ok(path)
}
impl RuntimePaths {
    pub fn discover() -> Result<Self> {
        Self::for_executable(&env::current_exe()?)
    }
    pub fn for_executable(path: &Path) -> Result<Self> {
        let executable = executable_file(path)?;
        let parent = executable
            .parent()
            .ok_or_else(|| identity_error("Executable has no parent"))?;
        // An installed (including moved DESTDIR stage) layout wins over any
        // compiled source location. Receipt prefixes never redirect discovery.
        if matches!(
            parent.file_name().and_then(OsStr::to_str),
            Some("bin" | "libexec")
        ) {
            let prefix = parent
                .parent()
                .ok_or_else(|| identity_error("Installation has no prefix"))?;
            let data = prefix.join("share/openwave");
            if version_witness(&data.join("VERSION")) {
                let data = fs::canonicalize(data)?;
                if !data.starts_with(prefix) {
                    return Err(identity_error("Data directory escapes its installation"));
                }
                let maintenance = executable_file(&prefix.join("libexec/openwave-maintenance"))?;
                if !maintenance.starts_with(prefix) {
                    return Err(identity_error(
                        "Maintenance helper escapes its installation",
                    ));
                }
                return Ok(Self {
                    executable: executable.clone(),
                    prefix: Some(prefix.to_owned()),
                    identity: data.clone(),
                    data,
                    maintenance,
                    source: None,
                });
            }
            return Err(identity_error(
                "Installed executable has no matching same-prefix VERSION witness",
            ));
        }
        // Cargo integration tests live in target/<profile>/deps. Normal source
        // binaries and their private helper share target/<profile>.
        let binaries = if parent.file_name() == Some(OsStr::new("deps")) {
            parent
                .parent()
                .ok_or_else(|| identity_error("Invalid Cargo binary directory"))?
        } else {
            parent
        };
        if binaries
            .file_name()
            .and_then(OsStr::to_str)
            .is_some_and(|name| {
                name.starts_with("openwave-remove-") || name.starts_with("openwave-bootstrap-")
            })
        {
            return Err(identity_error(
                "Recovery/bootstrap helpers require their explicit accepted installation identity",
            ));
        }
        let compiled_root =
            fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).ok();
        let compiled_target = compiled_root.as_ref().and_then(|root| {
            let target = option_env!("CARGO_TARGET_DIR").filter(|value| !value.is_empty())?;
            let target = Path::new(target);
            fs::canonicalize(if target.is_absolute() {
                target.to_owned()
            } else {
                root.join(target)
            })
            .ok()
        });
        let mut candidates: Vec<PathBuf> =
            binaries.ancestors().take(8).map(Path::to_owned).collect();
        if let Some(root) = &compiled_root {
            if !candidates.contains(root) {
                candidates.push(root.clone());
            }
        }
        for source in candidates {
            if !version_witness(&source.join("VERSION"))
                || !source.join("Cargo.toml").is_file()
                || !source.join("crates/openwave-runtime/Cargo.toml").is_file()
            {
                continue;
            }
            let source = fs::canonicalize(source)?;
            // Only this build's exact compile-time target directory may sit
            // outside its verified source root. Runtime environment overrides
            // and unrelated binaries cannot borrow compiled source assets.
            let in_source_target = executable.starts_with(source.join("target"));
            let in_compiled_target = compiled_root.as_ref() == Some(&source)
                && compiled_target
                    .as_ref()
                    .is_some_and(|target| executable.starts_with(target));
            if !in_source_target && !in_compiled_target {
                continue;
            }
            let maintenance = executable_file(&binaries.join("openwave-maintenance"))?;
            if maintenance.parent() != Some(binaries) {
                return Err(identity_error("Source helper is not a sibling executable"));
            }
            return Ok(Self {
                executable,
                prefix: None,
                identity: source.clone(),
                data: source.clone(),
                maintenance,
                source: Some(source),
            });
        }
        Err(identity_error(
            "Cannot identify this native OpenWave build/install (matching VERSION, assets and same-install maintenance helper required)",
        ))
    }
    pub fn data_file(&self, name: &str) -> Result<PathBuf> {
        let rel = relative(name)?;
        let path = if self.source.is_some() && name == "style.css" {
            self.data.join("data/style.css")
        } else {
            self.data.join(rel)
        };
        let result = fs::canonicalize(path)?;
        if !result.starts_with(&self.data) || !result.is_file() {
            return Err(identity_error(
                "Asset escapes installation or is not a file",
            ));
        }
        Ok(result)
    }
    pub fn bin_file(&self, name: &str) -> Result<PathBuf> {
        if !matches!(
            name,
            "openwave"
                | "openwave-daemon"
                | "openwave-diag"
                | "openwave-probe"
                | "openwave-maintenance"
        ) {
            return Err(identity_error("Not an OpenWave executable name"));
        }
        if name == "openwave-maintenance" {
            return executable_file(&self.maintenance);
        }
        let base = if let Some(prefix) = &self.prefix {
            prefix.join("bin")
        } else {
            self.maintenance
                .parent()
                .expect("validated helper parent")
                .to_owned()
        };
        let path = executable_file(&base.join(name))?;
        let boundary = self.prefix.as_deref().unwrap_or(&base);
        if !path.starts_with(boundary) {
            return Err(identity_error("Executable escapes installation"));
        }
        Ok(path)
    }
}
pub fn maintenance_executable() -> Result<PathBuf> {
    Ok(RuntimePaths::discover()?.maintenance)
}

// Pin every directory component with openat(O_NOFOLLOW). User-owned ancestors
// may be 0755, but never writable by another UID. Root sticky /tmp is a safe
// ancestry boundary for private temporary fixtures, not a private lock root.
pub(crate) fn open_directory_for_uid(path: &Path, create: bool, uid: u32) -> Result<OwnedFd> {
    absolute(path.to_owned())?;
    let mut fd = rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(os_error)?;
    for component in path.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        let next = rustix::fs::openat(
            &fd,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        );
        fd = match next {
            Ok(next) => next,
            Err(rustix::io::Errno::NOENT) if create => {
                match rustix::fs::mkdirat(&fd, name, Mode::RWXU) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => (),
                    Err(error) => return Err(os_error(error)),
                }
                rustix::fs::openat(
                    &fd,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(os_error)?
            }
            Err(error) => return Err(os_error(error)),
        };
        let stat = rustix::fs::fstat(&fd).map_err(os_error)?;
        let sticky_root = stat.st_uid == 0 && stat.st_mode & 0o1000 != 0;
        if (stat.st_uid != uid && stat.st_uid != 0) || (stat.st_mode & 0o022 != 0 && !sticky_root) {
            return Err(identity_error(format!(
                "Untrusted writable directory ancestry: {}",
                path.display()
            )));
        }
    }
    Ok(fd)
}
fn private_directory(path: &Path, create: bool) -> Result<OwnedFd> {
    private_directory_for_uid(path, create, rustix::process::geteuid().as_raw())
}
fn private_directory_for_uid(path: &Path, create: bool, uid: u32) -> Result<OwnedFd> {
    let fd = open_directory_for_uid(path, create, uid)?;
    let stat = rustix::fs::fstat(&fd).map_err(os_error)?;
    if stat.st_uid != uid || stat.st_mode & 0o7777 != 0o700 {
        return Err(identity_error(format!(
            "Private directory must be UID {uid} mode 0700: {}",
            path.display()
        )));
    }
    Ok(fd)
}
/// Path selection is read-only. Only Lease constructors create directories.
pub fn runtime_private_dir() -> Result<PathBuf> {
    runtime_directory_for_uid(
        rustix::process::geteuid().as_raw(),
        nonempty_env("XDG_RUNTIME_DIR").as_deref(),
        xdg_state_home,
    )
}
fn runtime_directory_for_uid(
    uid: u32,
    runtime: Option<&OsStr>,
    state: impl FnOnce() -> Result<PathBuf>,
) -> Result<PathBuf> {
    if let Some(root) = runtime.filter(|value| !value.is_empty()).map(PathBuf::from) {
        if private_directory_for_uid(&root, false, uid).is_ok() {
            return Ok(root.join("openwave"));
        }
    }
    absolute(state()?.join("openwave/runtime"))
}

/// A descriptor is the lease. Never unlink its name: doing so would let a new
/// process create a second inode and bypass a still-held exclusion lock.
#[derive(Debug)]
pub struct Lease {
    _file: File,
    pub path: PathBuf,
}
impl Lease {
    pub fn installation_shared(identity: &Path) -> Result<Self> {
        Self::installation(identity, false)
    }
    pub fn installation_exclusive(identity: &Path) -> Result<Self> {
        Self::installation(identity, true)
    }
    fn installation(identity: &Path, exclusive: bool) -> Result<Self> {
        Self::acquire(&installation_lock_name(identity, exclusive)?, exclusive)
    }
    pub fn installation_exclusive_for_uid(identity: &Path, uid: u32) -> Result<Self> {
        privilege::installation_exclusive(identity, uid)
    }
    pub fn vendor_control(allowed_unique_bus_owner: Option<&str>) -> Result<Self> {
        let lease = Self::acquire("vendor-control.lock", true)?;
        check_gui_owner(allowed_unique_bus_owner)?;
        Ok(lease)
    }
    pub fn capture_daemon() -> Result<Self> {
        Self::acquire("capture-daemon.lock", true)
    }
    fn acquire(name: &str, exclusive: bool) -> Result<Self> {
        let directory = runtime_private_dir()?;
        let dir = private_directory(&directory, true)?;
        let fd = rustix::fs::openat(
            &dir,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(os_error)?;
        let stat = rustix::fs::fstat(&fd).map_err(os_error)?;
        if stat.st_uid != rustix::process::geteuid().as_raw()
            || stat.st_mode & 0o7777 != 0o600
            || rustix::fs::FileType::from_raw_mode(stat.st_mode)
                != rustix::fs::FileType::RegularFile
            || stat.st_nlink != 1
        {
            return Err(identity_error(
                "Lease file must be a private current-UID regular file with one link",
            ));
        }
        let mut file = File::from(fd);
        let lock = if exclusive {
            FlockOperation::NonBlockingLockExclusive
        } else {
            FlockOperation::NonBlockingLockShared
        };
        if let Err(error) = rustix::fs::flock(&file, lock) {
            if error == rustix::io::Errno::WOULDBLOCK {
                let mut owner = String::new();
                let _ = (&mut file).take(64).read_to_string(&mut owner);
                return Err(OperationError::new(
                    ErrorCode::Busy,
                    format!(
                        "{} is busy{}",
                        name,
                        if owner.trim().is_empty() {
                            String::new()
                        } else {
                            format!(" (owner PID {})", owner.trim())
                        }
                    ),
                ));
            }
            return Err(os_error(error));
        }
        if exclusive {
            file.set_len(0)?;
            file.seek(SeekFrom::Start(0))?;
            writeln!(file, "{}", std::process::id())?;
            file.flush()?;
        }
        Ok(Self {
            _file: file,
            path: directory.join(name),
        })
    }
}

fn installation_lock_name(identity: &Path, allow_missing: bool) -> Result<String> {
    let canonical = canonical_lock_identity(identity, allow_missing)?;
    Ok(format!(
        "install-{}.lock",
        crate::installation::digest_hex(Sha256::digest(canonical.as_os_str().as_bytes()))
    ))
}

fn check_gui_owner(allowed: Option<&str>) -> Result<()> {
    if allowed.is_some_and(|name| !name.starts_with(':')) {
        return Err(OperationError::invalid(
            "Only an existing unique session-bus owner can be allowed",
        ));
    }
    let address = nonempty_env("DBUS_SESSION_BUS_ADDRESS").ok_or_else(|| {
        OperationError::unavailable("An existing desktop session-bus address is required to verify vendor-control ownership")
    })?;
    let address = address
        .to_str()
        .ok_or_else(|| identity_error("Session bus address is not UTF-8"))?;
    if address.split(';').any(|entry| {
        !entry.starts_with("unix:")
            && !entry.starts_with("tcp:")
            && !entry.starts_with("nonce-tcp:")
    }) {
        return Err(OperationError::unavailable(
            "Refusing a session-bus address that could autolaunch another process",
        ));
    }
    let connection = gio::DBusConnection::for_address_sync(
        address,
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
            | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None,
        gio::Cancellable::NONE,
    )
    .map_err(|error| {
        OperationError::unavailable(format!("Cannot establish existing GUI ownership: {error}"))
    })?;
    let reply = connection.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        "GetNameOwner",
        Some(&("com.github.openwave",).to_variant()),
        None,
        gio::DBusCallFlags::NO_AUTO_START,
        2000,
        gio::Cancellable::NONE,
    );
    match reply {
        Ok(reply) => {
            let (owner,) = reply
                .get::<(String,)>()
                .ok_or_else(|| identity_error("Malformed GUI owner response"))?;
            if allowed != Some(owner.as_str()) {
                return Err(OperationError::new(
                    ErrorCode::Busy,
                    "Another OpenWave GUI owns vendor control; close it before opening USB",
                ));
            }
            Ok(())
        }
        Err(error)
            if gio::DBusError::remote_error(&error).as_deref()
                == Some("org.freedesktop.DBus.Error.NameHasNoOwner") =>
        {
            Ok(())
        }
        Err(error) => Err(OperationError::unavailable(format!(
            "Cannot verify GUI ownership: {error}"
        ))),
    }
}

/// Validate the actual executable and every ancestor without following any
/// symlink. This is necessary preflight, not a substitute for elevated helper
/// revalidation and pinned descriptor deletion authority.
pub fn trusted_for_root(path: &Path) -> Result<()> {
    absolute(path.to_owned())?;
    for ancestor in path.ancestors() {
        let info = fs::symlink_metadata(ancestor)?;
        if info.file_type().is_symlink()
            || info.uid() != 0
            || info.mode() & 0o022 != 0
            || (ancestor == path && (!info.is_file() || info.mode() & 0o111 == 0))
            || (ancestor != path && !info.is_dir())
        {
            return Err(identity_error(format!(
                "Elevated helper requires a root-owned, nonwritable executable and ancestors: {}",
                ancestor.display()
            )));
        }
    }
    Ok(())
}
