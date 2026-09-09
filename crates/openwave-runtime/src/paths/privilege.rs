use super::*;
use std::{
    collections::HashMap,
    ffi::OsString,
    os::unix::{ffi::OsStringExt, fs::FileTypeExt},
    process::Command,
    sync::OnceLock,
};

const SELECTORS: [&str; 6] = [
    "HOME",
    "XDG_RUNTIME_DIR",
    "XDG_STATE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "DBUS_SESSION_BUS_ADDRESS",
];
const TOOL_PATH: &str = "/run/current-system/sw/bin:/usr/local/bin:/usr/bin:/bin";
type Environment = HashMap<String, OsString>;

fn parent_environment(uid: u32) -> Result<Option<Environment>> {
    let mut pid = rustix::process::getppid()
        .map(|pid| pid.as_raw_pid() as u32)
        .unwrap_or(0);
    for _ in 0..16 {
        if pid == 0 {
            break;
        }
        let base = PathBuf::from(format!("/proc/{pid}"));
        let status = match fs::read_to_string(base.join("status")) {
            Ok(status) => status,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        };
        let owner = status.lines().find_map(|line| {
            line.strip_prefix("Uid:")?
                .split_ascii_whitespace()
                .next()?
                .parse::<u32>()
                .ok()
        });
        if owner == Some(uid) {
            let mut bytes = Vec::new();
            File::open(base.join("environ"))?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 1024 * 1024 {
                return Err(identity_error(
                    "Original-user environment exceeds inspection bound",
                ));
            }
            let mut values = Environment::new();
            for entry in bytes.split(|byte| *byte == 0) {
                for key in SELECTORS {
                    if let Some(value) = entry
                        .strip_prefix(key.as_bytes())
                        .and_then(|rest| rest.strip_prefix(b"="))
                    {
                        if values
                            .insert(key.to_owned(), OsString::from_vec(value.to_vec()))
                            .is_some()
                        {
                            return Err(identity_error(
                                "Ambiguous original-user environment selector",
                            ));
                        }
                    }
                }
            }
            return Ok(Some(values));
        }
        pid = status
            .lines()
            .find_map(|line| line.strip_prefix("PPid:")?.trim().parse().ok())
            .unwrap_or(0);
    }
    Ok(None)
}

// Validate existing ancestry without creating state as root. The dropped user
// alone may create missing descendants; the exclusion opener requires 0700.
fn selector_directory(path: &Path, uid: u32, require_owner: bool) -> Result<()> {
    absolute(path.to_owned())?;
    let mut ancestor = path;
    loop {
        match fs::symlink_metadata(ancestor) {
            Ok(_) => {
                let fd = open_directory_for_uid(ancestor, false, uid)?;
                if require_owner && rustix::fs::fstat(&fd).map_err(os_error)?.st_uid != uid {
                    return Err(identity_error("Original-user HOME has a different owner"));
                }
                return Ok(());
            }
            Err(error) if !require_owner && error.kind() == std::io::ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .ok_or_else(|| identity_error("User selector has no existing ancestor"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub(crate) struct OriginalUserEnvironment {
    uid: u32,
    pub(crate) gid: u32,
    values: Environment,
    directory: PathBuf,
}
impl OriginalUserEnvironment {
    pub(crate) fn configure(&self, command: &mut Command) {
        command
            .env_clear()
            .envs(&self.values)
            .env("PATH", TOOL_PATH);
    }
}

fn derive_environment(
    uid: u32,
    gid: u32,
    account_home: &Path,
    supplied: &Environment,
    direct: bool,
) -> Result<OriginalUserEnvironment> {
    let value = |key: &str| supplied.get(key).filter(|value| !value.is_empty());
    let mut home = value("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| account_home.to_owned());
    // An administrator's HOME is not the original user's HOME. A supplied
    // caller HOME, unlike this direct-root fallback, must validate as-is.
    if direct && selector_directory(&home, uid, true).is_err() {
        home = account_home.to_owned();
    }
    selector_directory(&home, uid, true)?;
    let mut values = Environment::from([("HOME".into(), home.clone().into_os_string())]);
    for (key, fallback) in [
        ("XDG_STATE_HOME", ".local/state"),
        ("XDG_CONFIG_HOME", ".config"),
        ("XDG_DATA_HOME", ".local/share"),
    ] {
        let path = value(key)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(fallback));
        selector_directory(&path, uid, false)?;
        values.insert(key.into(), path.into_os_string());
    }
    let runtime = value("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            (direct
                && !supplied.contains_key("XDG_RUNTIME_DIR")
                && value("XDG_STATE_HOME").is_none())
            .then(|| PathBuf::from(format!("/run/user/{uid}")))
        })
        .filter(|path| private_directory_for_uid(path, false, uid).is_ok());
    if let Some(runtime) = &runtime {
        values.insert("XDG_RUNTIME_DIR".into(), runtime.clone().into_os_string());
    }
    let state = PathBuf::from(&values["XDG_STATE_HOME"]);
    let directory =
        runtime_directory_for_uid(uid, runtime.as_deref().map(Path::as_os_str), || Ok(state))?;
    // Validate any existing leaf, while leaving creation to the dropped user.
    match fs::symlink_metadata(&directory) {
        Ok(_) => {
            private_directory_for_uid(&directory, false, uid)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            selector_directory(&directory, uid, false)?;
        }
        Err(error) => return Err(error.into()),
    }
    if let Some(address) = value("DBUS_SESSION_BUS_ADDRESS") {
        let text = address
            .to_str()
            .ok_or_else(|| identity_error("Session bus address is not UTF-8"))?;
        if text.split(';').any(|entry| {
            !entry.starts_with("unix:")
                && !entry.starts_with("tcp:")
                && !entry.starts_with("nonce-tcp:")
        }) {
            return Err(identity_error(
                "Refusing autolaunch-capable session bus address",
            ));
        }
        values.insert("DBUS_SESSION_BUS_ADDRESS".into(), address.clone());
    } else if let Some(runtime) = runtime {
        let bus = runtime.join("bus");
        match fs::symlink_metadata(&bus) {
            Ok(info) if info.file_type().is_socket() && info.uid() == uid => {
                let path = bus
                    .to_str()
                    .ok_or_else(|| identity_error("Session bus path is not UTF-8"))?;
                // D-Bus address values require escaping, including comma/semicolon.
                let mut address = String::from("unix:path=");
                const HEX: &[u8; 16] = b"0123456789abcdef";
                for byte in path.bytes() {
                    if byte.is_ascii_alphanumeric() || b"/_-.\\".contains(&byte) {
                        address.push(byte as char);
                    } else {
                        address.push('%');
                        address.push(HEX[(byte >> 4) as usize] as char);
                        address.push(HEX[(byte & 15) as usize] as char);
                    }
                }
                values.insert("DBUS_SESSION_BUS_ADDRESS".into(), address.into());
            }
            Ok(_) => {
                return Err(identity_error(
                    "Original-user session bus is not an owned socket",
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(OriginalUserEnvironment {
        uid,
        gid,
        values,
        directory,
    })
}

pub(crate) fn original_user_environment(uid: u32) -> Result<&'static OriginalUserEnvironment> {
    static ORIGINAL: OnceLock<OriginalUserEnvironment> = OnceLock::new();
    if uid == 0 {
        return Err(identity_error("Original login UID must be non-root"));
    }
    if ORIGINAL.get().is_none() {
        let (_, gid, home) = user_account(uid)?;
        let parent = parent_environment(uid)?;
        let direct = parent.is_none();
        let values = parent.unwrap_or_else(|| {
            SELECTORS
                .into_iter()
                .filter_map(|key| env::var_os(key).map(|value| (key.into(), value)))
                .collect()
        });
        let selected = derive_environment(uid, gid, &home, &values, direct)?;
        let _ = ORIGINAL.set(selected);
    }
    let original = ORIGINAL
        .get()
        .ok_or_else(|| identity_error("Original-user environment unavailable"))?;
    if original.uid != uid {
        return Err(identity_error("Original-user UID changed during operation"));
    }
    Ok(original)
}

pub(super) fn installation_exclusive(identity: &Path, uid: u32) -> Result<Lease> {
    if !rustix::process::geteuid().is_root() || uid == 0 {
        return Err(identity_error(
            "Privileged exclusion requires root and the original non-root login UID",
        ));
    }
    let original = original_user_environment(uid)?;
    open_existing_exclusive(identity, uid, &original.directory)
}

fn open_existing_exclusive(identity: &Path, uid: u32, directory: &Path) -> Result<Lease> {
    let name = installation_lock_name(identity, true)?;
    let parent = private_directory_for_uid(directory, false, uid)?;
    let fd = rustix::fs::openat(
        &parent,
        name.as_str(),
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(os_error)?;
    let stat = rustix::fs::fstat(&fd).map_err(os_error)?;
    if stat.st_uid != uid
        || stat.st_mode & 0o7777 != 0o600
        || stat.st_nlink != 1
        || rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile
    {
        return Err(identity_error(
            "Original-user lease is not a private single-link regular file",
        ));
    }
    let mut file = File::from(fd);
    rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive).map_err(|error| {
        if error == rustix::io::Errno::WOULDBLOCK {
            OperationError::new(
                ErrorCode::Busy,
                "An original-user OpenWave owner still holds the installation lease",
            )
        } else {
            os_error(error)
        }
    })?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(Lease {
        _file: file,
        path: directory.join(name),
    })
}

#[cfg(test)]
mod tests;
