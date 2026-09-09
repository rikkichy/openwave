use super::*;
use rustix::fs::{AtFlags, Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, PermissionsExt},
        },
    },
    path::Component,
    sync::{LazyLock, Mutex},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct UserRecord {
    pub schema: u32,
    pub delete_settings: bool,
    pub installation: InstallationSnapshot,
    pub privileged_transaction: Option<PathBuf>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RootRecord {
    schema: u32,
    uid: u32,
    installation: InstallationSnapshot,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OldRecord {
    schema: u32,
    delete_settings: bool,
    method: InstallMethod,
    prefix: PathBuf,
    module_dir: PathBuf,
    files: Vec<PathBuf>,
    directories: Vec<PathBuf>,
    receipt: Option<PathBuf>,
    guidance: String,
    legacy: bool,
    identities: Vec<(PathBuf, String)>,
}
pub(super) struct UserRecovery {
    pub record: UserRecord,
    path: PathBuf,
}
fn invalid(message: impl Into<String>) -> OperationError {
    OperationError::new(ErrorCode::Identity, message)
}
pub(super) fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}
fn normal(path: &Path) -> Result<()> {
    if !path.is_absolute()
        || path
            .components()
            .any(|p| !matches!(p, Component::RootDir | Component::Normal(_)))
        || path.as_os_str().as_bytes().contains(&0)
    {
        return Err(invalid("Recovery paths must be canonical absolute paths"));
    }
    Ok(())
}
/// Missing leaves are acceptable on retry; every existing ancestor is pinned
/// without following a symlink before any mutation below it.
fn directory(path: &Path, create: bool, uid: u32, private: bool) -> Result<File> {
    normal(path)?;
    let mut fd = File::from(
        rustix::fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    let mut current = PathBuf::from("/");
    for part in path.components().filter_map(|c| {
        if let Component::Normal(n) = c {
            Some(n)
        } else {
            None
        }
    }) {
        current.push(part);
        if create {
            match rustix::fs::mkdirat(&fd, part, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
                Ok(()) => (),
                Err(e) if e == rustix::io::Errno::EXIST => (),
                Err(e) => return Err(io::Error::from(e).into()),
            }
        }
        fd = File::from(
            rustix::fs::openat(
                &fd,
                part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?,
        );
        let meta = fd.metadata()?;
        let sticky_root = meta.uid() == 0 && meta.mode() & 0o1000 != 0;
        if (meta.uid() != uid && meta.uid() != 0) || (meta.mode() & 0o022 != 0 && !sticky_root) {
            return Err(invalid(format!(
                "Untrusted recovery ancestor: {}",
                current.display()
            )));
        }
    }
    let meta = fd.metadata()?;
    if private && (meta.uid() != uid || meta.mode() & 0o077 != 0) {
        return Err(invalid(
            "Recovery directory is not private to the expected UID",
        ));
    }
    Ok(fd)
}
fn read_regular(path: &Path, uid: Option<u32>, private: bool) -> Result<File> {
    open_regular(path, uid, private, true)
}
fn open_regular(path: &Path, uid: Option<u32>, private: bool, single_link: bool) -> Result<File> {
    let parent = path.parent().ok_or_else(|| invalid("Missing parent"))?;
    let dir = directory(
        parent,
        false,
        uid.unwrap_or(rustix::process::geteuid().as_raw()),
        false,
    )?;
    let fd = rustix::fs::openat(
        &dir,
        path.file_name()
            .ok_or_else(|| invalid("Missing filename"))?,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(io::Error::from)?;
    let file = File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file()
        || (single_link && meta.nlink() != 1)
        || uid.is_some_and(|uid| uid != meta.uid())
        || (private && meta.mode() & 0o077 != 0)
    {
        return Err(invalid(
            "Recovery file has unsafe type, ownership, links, or mode",
        ));
    }
    Ok(file)
}
fn digest_file(file: &mut File) -> Result<String> {
    let mut digest = Sha256::new();
    let mut bytes = [0u8; 65536];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        digest.update(&bytes[..n]);
    }
    Ok(installation::digest_hex(digest.finalize()))
}
fn copy_helper(source: &Path, dir: &File) -> Result<()> {
    // Cargo legitimately hardlinks bin/ and deps/ executables. Copying their
    // pinned read-only descriptor is not authority to unlink either source.
    let mut input = open_regular(source, None, false, false)?;
    let metadata = input.metadata()?;
    if metadata.mode() & 0o111 == 0 {
        return Err(invalid("Recovery helper is not executable"));
    }
    let out = rustix::fs::openat(
        dir,
        "openwave-maintenance",
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR | Mode::XUSR,
    )
    .map_err(io::Error::from)?;
    let mut output = File::from(out);
    io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    use std::io::{Seek, SeekFrom};
    input.seek(SeekFrom::Start(0))?;
    let expected = digest_file(&mut input)?;
    let mut copied = File::from(
        rustix::fs::openat(
            dir,
            "openwave-maintenance",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    if digest_file(&mut copied)? != expected {
        return Err(invalid("Recovery helper copy digest mismatch"));
    }
    Ok(())
}
fn write_record<T: Serialize>(dir: &File, name: &str, record: &T, replace: bool) -> Result<()> {
    let bytes = serde_json::to_vec(record)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(invalid("Recovery record exceeds 4 MiB"));
    }
    let temporary = format!(".record-{}", uuid::Uuid::new_v4().simple());
    let fd = rustix::fs::openat(
        dir,
        &temporary,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(io::Error::from)?;
    let mut file = File::from(fd);
    file.write_all(&bytes)?;
    file.sync_all()?;
    if !replace && rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
        let _ = rustix::fs::unlinkat(dir, &temporary, AtFlags::empty());
        return Err(invalid("Recovery record already exists"));
    }
    rustix::fs::renameat(dir, &temporary, dir, name).map_err(io::Error::from)?;
    dir.sync_all()?;
    Ok(())
}
fn user_base() -> Result<PathBuf> {
    Ok(paths::xdg_state_home()?.join("openwave-uninstall"))
}
pub(super) fn prepare_user(
    paths: &RuntimePaths,
    snapshot: &InstallationSnapshot,
    delete_settings: bool,
) -> Result<UserRecovery> {
    installation::validate_installation(snapshot)?;
    let uid = rustix::process::geteuid().as_raw();
    let base = user_base()?;
    let parent = directory(&base, true, uid, true)?;
    let name = format!("openwave-remove-{}", uuid::Uuid::new_v4().simple());
    rustix::fs::mkdirat(&parent, &name, Mode::RUSR | Mode::WUSR | Mode::XUSR)
        .map_err(io::Error::from)?;
    let path = base.join(name).join("plan.json");
    let dir = directory(
        path.parent()
            .ok_or_else(|| invalid("Missing bundle directory"))?,
        false,
        uid,
        true,
    )?;
    copy_helper(&paths.maintenance, &dir)?;
    let record = UserRecord {
        schema: 2,
        delete_settings,
        installation: snapshot.clone(),
        privileged_transaction: None,
    };
    write_record(&dir, "plan.json", &record, false)?;
    Ok(UserRecovery { record, path })
}
static PREPARED: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
fn preparation_key(snapshot: &InstallationSnapshot, delete_settings: bool) -> Result<String> {
    Ok(installation::digest_hex(Sha256::digest(
        serde_json::to_vec(&(snapshot, delete_settings))?,
    )))
}
pub(super) fn prepared(
    paths: &RuntimePaths,
    snapshot: &InstallationSnapshot,
    delete_settings: bool,
) -> Result<UserRecovery> {
    let key = preparation_key(snapshot, delete_settings)?;
    let mut records = PREPARED
        .lock()
        .map_err(|_| invalid("Removal preparation state is poisoned; no mutation authorized"))?;
    if let Some(path) = records.get(&key) {
        let bundle = load_user(path)?;
        if bundle.record.installation != *snapshot
            || bundle.record.delete_settings != delete_settings
        {
            return Err(invalid("Prepared removal authority changed"));
        }
        return Ok(bundle);
    }
    let bundle = prepare_user(paths, snapshot, delete_settings)?;
    records.insert(key, bundle.path.clone());
    Ok(bundle)
}
impl UserRecovery {
    pub(super) fn helper(&self) -> PathBuf {
        self.path.with_file_name("openwave-maintenance")
    }
    pub(super) fn command(&self) -> String {
        format!(
            "{} resume-uninstall --plan {} --yes",
            quote(&self.helper()),
            quote(&self.path)
        )
    }
    pub(super) fn retirement_command(&self) -> String {
        format!(
            "{} resume-retirement --plan {} --yes",
            quote(&self.helper()),
            quote(&self.path)
        )
    }
    pub(super) fn ensure_native_helper(&self) -> Result<()> {
        if self.helper().try_exists()? {
            let _ = read_regular(
                &self.helper(),
                Some(rustix::process::geteuid().as_raw()),
                true,
            )?;
            return Ok(());
        }
        let current = std::env::current_exe()?;
        if current.file_name().and_then(|n| n.to_str()) != Some("openwave-maintenance") {
            return Err(invalid(
                "Historical recovery requires the native maintenance executable",
            ));
        }
        let dir = directory(
            self.path
                .parent()
                .ok_or_else(|| invalid("Missing recovery directory"))?,
            false,
            rustix::process::geteuid().as_raw(),
            true,
        )?;
        copy_helper(&current, &dir)?;
        self.persist()
    }
    pub(super) fn persist(&self) -> Result<()> {
        let dir = directory(
            self.path
                .parent()
                .ok_or_else(|| invalid("Missing recovery directory"))?,
            false,
            rustix::process::geteuid().as_raw(),
            true,
        )?;
        write_record(&dir, "plan.json", &self.record, true)
    }
    pub(super) fn set_transaction(&mut self, path: PathBuf) -> Result<()> {
        self.record.privileged_transaction = Some(path);
        self.persist()
    }
    pub(super) fn cleanup(self) -> Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("Missing recovery directory"))?;
        let dir = directory(parent, false, rustix::process::geteuid().as_raw(), true)?;
        // Never recursively delete a recovery directory: preserve added content.
        for name in ["plan.json", "openwave-maintenance"] {
            let file = read_regular(
                &parent.join(name),
                Some(rustix::process::geteuid().as_raw()),
                true,
            )?;
            let meta = file.metadata()?;
            let now = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io::Error::from)?;
            if now.st_dev != meta.dev() || now.st_ino != meta.ino() {
                return Err(invalid("Recovery file changed before cleanup"));
            }
            rustix::fs::unlinkat(&dir, name, AtFlags::empty()).map_err(io::Error::from)?;
        }
        match fs::remove_dir(parent) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => (),
            Err(e) => return Err(e.into()),
        }
        Ok(())
    }
}
pub(super) fn load_user(path: &Path) -> Result<UserRecovery> {
    normal(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Missing recovery directory"))?;
    if path.file_name().and_then(|s| s.to_str()) != Some("plan.json")
        || !parent
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.starts_with("openwave-remove-"))
        || parent.parent() != Some(user_base()?.as_path())
    {
        return Err(invalid("Unrecognized recovery bundle location"));
    }
    let dir = directory(parent, false, rustix::process::geteuid().as_raw(), true)?;
    // Schema-1 Python writers used the default 0644 mode inside a private
    // bundle. Read inert JSON through the same pinned no-follow descriptor;
    // never reopen the pathname after checking its authority.
    let mut file = read_regular(path, Some(rustix::process::geteuid().as_raw()), false)?;
    let before = file.metadata()?;
    if before.mode() & 0o022 != 0 || before.len() > 4 * 1024 * 1024 {
        return Err(invalid("Recovery plan is writable by others or too large"));
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    let now = file.metadata()?;
    let named = rustix::fs::statat(&dir, "plan.json", AtFlags::SYMLINK_NOFOLLOW)
        .map_err(io::Error::from)?;
    if before.dev() != named.st_dev
        || before.ino() != named.st_ino
        || before.len() != now.len()
        || before.mtime() != now.mtime()
        || before.mtime_nsec() != now.mtime_nsec()
        || before.ctime() != now.ctime()
        || before.ctime_nsec() != now.ctime_nsec()
    {
        return Err(invalid("Recovery plan changed during admission"));
    }
    let value = installation::parse_metadata(&bytes)?;
    let historical = value.get("schema").and_then(serde_json::Value::as_u64) == Some(1);
    if !historical && before.mode() & 0o077 != 0 {
        return Err(invalid("Native recovery plan is not private"));
    }
    let record = if historical {
        let old: OldRecord = serde_json::from_value(value)?;
        if old.schema != 1
            || old.method != InstallMethod::Manual
            || old.legacy != old.receipt.is_none()
        {
            return Err(invalid("Invalid historical recovery record"));
        }
        let _ = old.guidance;
        UserRecord {
            schema: 2,
            delete_settings: old.delete_settings,
            privileged_transaction: None,
            installation: InstallationSnapshot {
                format: if old.legacy {
                    InstallationFormat::PythonLegacy
                } else {
                    InstallationFormat::PythonV1
                },
                method: old.method,
                prefix: old.prefix,
                module_dir: Some(old.module_dir),
                receipt: old.receipt,
                files: old.files,
                directories: old.directories,
                identities: old.identities,
            },
        }
    } else {
        serde_json::from_value(value)?
    };
    if record.schema != 2 || record.installation.method != InstallMethod::Manual {
        return Err(invalid("Invalid native recovery schema/method"));
    }
    installation::validate_installation(&record.installation)?;
    if let Some(transaction) = &record.privileged_transaction {
        check_transaction_path(transaction, Some(&record.installation.prefix))?;
    }
    Ok(UserRecovery {
        record,
        path: path.to_owned(),
    })
}
pub(super) fn managed_path(path: &Path) -> Result<bool> {
    normal(path)?;
    paths::has_symlink_ancestor(path)
}
pub(super) fn remove_unchanged(path: &Path, expected: &[u8]) -> Result<()> {
    let uid = rustix::process::geteuid().as_raw();
    let dir = directory(
        path.parent().ok_or_else(|| invalid("Missing parent"))?,
        false,
        uid,
        false,
    )?;
    let mut file = read_regular(path, Some(uid), false)?;
    let meta = file.metadata()?;
    let mut actual = Vec::new();
    Read::by_ref(&mut file)
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut actual)?;
    if actual != expected {
        return Err(invalid("Owned integration changed before removal"));
    }
    let name = path
        .file_name()
        .ok_or_else(|| invalid("Missing filename"))?;
    let now = rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
    if now.st_dev != meta.dev() || now.st_ino != meta.ino() {
        return Err(invalid("Owned integration identity changed"));
    }
    rustix::fs::unlinkat(&dir, name, AtFlags::empty()).map_err(io::Error::from)?;
    dir.sync_all()?;
    Ok(())
}
pub(super) fn check_settings(path: &Path) -> Result<()> {
    if !path.try_exists()? && fs::symlink_metadata(path).is_err() {
        return Ok(());
    }
    if managed_path(path)? {
        return Err(invalid(
            "Settings are externally managed/symlinked; preserved",
        ));
    }
    let uid = rustix::process::geteuid().as_raw();
    directory(path, false, uid, false)?;
    fn inspect(path: &Path, uid: u32, count: &mut usize, depth: usize) -> Result<()> {
        if depth > 64 {
            return Err(invalid("Settings tree exceeds safe traversal depth"));
        }
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            *count += 1;
            if *count > 20_000 {
                return Err(invalid("Settings tree exceeds removal bound"));
            }
            let m = fs::symlink_metadata(entry.path())?;
            if m.uid() != uid
                || m.file_type().is_symlink()
                || (!m.is_dir() && !m.is_file())
                || (m.is_file() && m.nlink() != 1)
            {
                return Err(invalid(
                    "Settings contain externally owned, linked or special content; preserved",
                ));
            }
            if m.is_dir() {
                inspect(&entry.path(), uid, count, depth + 1)?;
            }
        }
        Ok(())
    }
    inspect(path, uid, &mut 0, 0)
}
pub(super) fn remove_settings(path: &Path) -> Result<()> {
    check_settings(path)?;
    if !path.try_exists()? {
        return Ok(());
    }
    fn remove(
        path: &Path,
        expected: Option<(u64, u64)>,
        count: &mut usize,
        depth: usize,
    ) -> Result<()> {
        if depth > 64 {
            return Err(invalid("Settings tree changed beyond safe traversal depth"));
        }
        let uid = rustix::process::geteuid().as_raw();
        let dir = directory(path, false, uid, false)?;
        let observed = dir.metadata()?;
        if expected.is_some_and(|(dev, ino)| dev != observed.dev() || ino != observed.ino()) {
            return Err(invalid("Settings directory changed before traversal"));
        }
        for entry in fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))? {
            let entry = entry?;
            let name = entry.file_name();
            *count += 1;
            if *count > 20_000 {
                return Err(invalid("Settings tree changed beyond removal bound"));
            }
            let before = rustix::fs::statat(&dir, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io::Error::from)?;
            if before.st_uid != uid {
                return Err(invalid("Settings ownership changed"));
            }
            let kind = rustix::fs::FileType::from_raw_mode(before.st_mode);
            if kind == rustix::fs::FileType::Directory {
                remove(
                    &path.join(&name),
                    Some((before.st_dev, before.st_ino)),
                    count,
                    depth + 1,
                )?;
            } else if kind == rustix::fs::FileType::RegularFile && before.st_nlink == 1 {
                let fd = rustix::fs::openat(
                    &dir,
                    &name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                    Mode::empty(),
                )
                .map_err(io::Error::from)?;
                let opened = rustix::fs::fstat(&fd).map_err(io::Error::from)?;
                let now = rustix::fs::statat(&dir, &name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(io::Error::from)?;
                if before.st_ino != opened.st_ino
                    || before.st_dev != opened.st_dev
                    || opened.st_ino != now.st_ino
                    || opened.st_dev != now.st_dev
                {
                    return Err(invalid("Settings identity changed"));
                }
                rustix::fs::unlinkat(&dir, &name, AtFlags::empty()).map_err(io::Error::from)?;
            } else {
                return Err(invalid(
                    "Settings type changed; remaining content preserved",
                ));
            }
        }
        let parent = directory(
            path.parent()
                .ok_or_else(|| invalid("Missing settings parent"))?,
            false,
            uid,
            false,
        )?;
        let name = path
            .file_name()
            .ok_or_else(|| invalid("Missing settings name"))?;
        let opened = dir.metadata()?;
        let now = rustix::fs::statat(&parent, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(io::Error::from)?;
        if now.st_ino != opened.ino() || now.st_dev != opened.dev() {
            return Err(invalid("Settings directory changed"));
        }
        rustix::fs::unlinkat(&parent, name, AtFlags::REMOVEDIR).map_err(io::Error::from)?;
        Ok(())
    }
    remove(path, None, &mut 0, 0)
}
pub(super) fn needs_root(snapshot: &InstallationSnapshot) -> Result<bool> {
    for file in &snapshot.files {
        if fs::symlink_metadata(file).is_err_and(|e| e.kind() == io::ErrorKind::NotFound) {
            continue;
        }
        let parent = file
            .parent()
            .ok_or_else(|| invalid("Missing inventory parent"))?;
        match rustix::fs::access(
            parent,
            rustix::fs::Access::WRITE_OK | rustix::fs::Access::EXEC_OK,
        ) {
            Ok(()) => (),
            Err(e) if e == rustix::io::Errno::ACCESS => return Ok(true),
            Err(e) => return Err(io::Error::from(e).into()),
        }
    }
    Ok(false)
}
fn own_root_helper() -> Result<PathBuf> {
    if !rustix::process::geteuid().is_root() {
        return Err(invalid(
            "This operation requires administrator authorization",
        ));
    }
    let executable = std::env::current_exe()?;
    paths::trusted_for_root(&executable)?;
    if executable.file_name().and_then(|s| s.to_str()) != Some("openwave-maintenance") {
        return Err(invalid(
            "Privileged operation requires the private native maintenance executable",
        ));
    }
    Ok(executable)
}
pub(super) fn login_uid() -> Result<u32> {
    for name in ["PKEXEC_UID", "SUDO_UID"] {
        if let Ok(value) = std::env::var(name) {
            let uid = value
                .parse::<u32>()
                .map_err(|_| invalid("Invalid original login UID"))?;
            if uid != 0 {
                return Ok(uid);
            }
        }
    }
    if let Ok(user) = std::env::var("DOAS_USER") {
        for line in fs::read_to_string("/etc/passwd")?.lines() {
            let fields: Vec<_> = line.split(':').collect();
            if fields.len() >= 7 && fields[0] == user {
                if let Ok(uid) = fields[2].parse::<u32>() {
                    if uid != 0 {
                        return Ok(uid);
                    }
                }
            }
        }
        return Err(invalid("Original doas login account is unavailable"));
    }
    Err(invalid(
        "Original non-root login UID is required; use pkexec or sudo from your login session",
    ))
}
pub(super) fn verify_bootstrap(helper: &Path, snapshot: &InstallationSnapshot) -> Result<()> {
    own_root_helper()?;
    let parent = helper
        .parent()
        .ok_or_else(|| invalid("Missing bootstrap directory"))?;
    if parent.parent() != Some(snapshot.prefix.join("libexec").as_path())
        || !parent
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.starts_with("openwave-bootstrap-"))
        || snapshot.files.iter().any(|p| p.starts_with(parent))
    {
        return Err(invalid(
            "Legacy privileged retirement requires a trusted fresh bootstrap outside the old inventory",
        ));
    }
    Ok(())
}
pub(super) fn prepare_root(snapshot: &InstallationSnapshot) -> Result<PathBuf> {
    let helper = own_root_helper()?;
    let uid = login_uid()?;
    installation::validate_installation(snapshot)?;
    if snapshot.format != InstallationFormat::RustV2 {
        verify_bootstrap(&helper, snapshot)?;
    }
    let libexec = snapshot.prefix.join("libexec");
    // Root trust applies to the prefix as well as the copied executable.
    let parent = directory(&libexec, false, 0, false)?;
    for ancestor in libexec.ancestors() {
        let m = fs::symlink_metadata(ancestor)?;
        if m.uid() != 0 || m.mode() & 0o022 != 0 || !m.is_dir() {
            return Err(invalid("Privileged transaction ancestry is untrusted"));
        }
    }
    let name = format!("openwave-remove-{}", uuid::Uuid::new_v4().simple());
    rustix::fs::mkdirat(&parent, &name, Mode::from_raw_mode(0o755)).map_err(io::Error::from)?;
    let root = libexec.join(name);
    let dir = directory(&root, false, 0, false)?;
    copy_helper(&helper, &dir)?;
    let executable = File::from(
        rustix::fs::openat(
            &dir,
            "openwave-maintenance",
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    executable.set_permissions(fs::Permissions::from_mode(0o755))?;
    write_record(
        &dir,
        "authority.json",
        &RootRecord {
            schema: 2,
            uid,
            installation: snapshot.clone(),
        },
        false,
    )?;
    parent.sync_all()?;
    Ok(root.join("authority.json"))
}
fn check_transaction_path(path: &Path, prefix: Option<&Path>) -> Result<()> {
    normal(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Missing transaction parent"))?;
    if path.file_name().and_then(|n| n.to_str()) != Some("authority.json")
        || !parent
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("openwave-remove-"))
        || parent
            .parent()
            .and_then(Path::file_name)
            .and_then(|n| n.to_str())
            != Some("libexec")
        || prefix.is_some_and(|p| parent.parent() != Some(p.join("libexec").as_path()))
    {
        return Err(invalid("Unrecognized privileged transaction path"));
    }
    paths::trusted_for_root(&parent.join("openwave-maintenance"))
}
pub(super) fn trusted_helper(paths: &RuntimePaths) -> Result<PathBuf> {
    if paths::trusted_for_root(&paths.maintenance).is_ok() {
        return Ok(paths.maintenance.clone());
    }
    if let Some(prefix) = &paths.prefix {
        let installed = prefix.join("libexec/openwave-maintenance");
        paths::trusted_for_root(&installed)?;
        return Ok(installed);
    }
    Err(invalid(
        "No same-install root-trusted helper is available; ask an administrator to install the native helper or exact USB rules",
    ))
}
pub(super) fn request_privileged(
    paths: &RuntimePaths,
    snapshot: &InstallationSnapshot,
    runner: &crate::process::CommandRunner,
) -> Result<PathBuf> {
    let helper = trusted_helper(paths)?;
    let receipt = snapshot.receipt.as_ref().ok_or_else(|| {
        invalid(
            "Receipt-less privileged retirement requires the installer-owned bootstrap procedure",
        )
    })?;
    let digest = snapshot
        .identities
        .iter()
        .find(|(p, _)| p == receipt)
        .map(|(_, h)| h.as_str())
        .ok_or_else(|| invalid("Missing accepted receipt hash"))?;
    let output = runner.run_privileged_status(
        "pkexec",
        &[
            service::text_path(&helper)?.into(),
            "prepare-remove-files".into(),
            "--receipt".into(),
            service::text_path(receipt)?.into(),
            "--expected-sha256".into(),
            digest.into(),
            "--prefix".into(),
            service::text_path(&snapshot.prefix)?.into(),
        ],
        Duration::from_secs(120),
    )?;
    if !output.status.success() {
        return Err(OperationError::unavailable(format!(
            "Administrator preparation did not complete: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| invalid("Non-UTF8 transaction response"))?
        .trim();
    if text.lines().count() != 1 {
        return Err(invalid("Malformed privileged transaction response"));
    }
    let path = PathBuf::from(text);
    check_transaction_path(&path, Some(&snapshot.prefix))?;
    Ok(path)
}
pub(super) fn invoke_transaction(
    path: &Path,
    runner: &crate::process::CommandRunner,
) -> Result<()> {
    check_transaction_path(path, None)?;
    let helper = path.with_file_name("openwave-maintenance");
    let output = runner.run_privileged_status(
        "pkexec",
        &[
            service::text_path(&helper)?.into(),
            "resume-privileged".into(),
            "--transaction".into(),
            service::text_path(path)?.into(),
        ],
        Duration::from_secs(120),
    )?;
    if !output.status.success() {
        return Err(OperationError::unavailable(format!(
            "Privileged removal incomplete: {}\nRetry: pkexec {} resume-privileged --transaction {}",
            String::from_utf8_lossy(&output.stderr),
            quote(&helper),
            quote(path)
        )));
    }
    Ok(())
}
pub(super) fn resume_root(path: &Path) -> Result<()> {
    own_root_helper()?;
    check_transaction_path(path, None)?;
    let _opened = read_regular(path, Some(0), true)?;
    let record: RootRecord = serde_json::from_value(installation::read_metadata(path)?)?;
    if record.schema != 2
        || record.uid == 0
        || record.uid != login_uid()?
        || record.installation.method != InstallMethod::Manual
    {
        return Err(invalid("Privileged authority schema/login UID mismatch"));
    }
    check_transaction_path(path, Some(&record.installation.prefix))?;
    installation::validate_installation(&record.installation)?;
    let target = target_paths(
        &record.installation,
        path.with_file_name("openwave-maintenance"),
    );
    let _lease =
        paths::Lease::installation_exclusive_for_uid(&identity(&record.installation), record.uid)?;
    let caller = quiesce::initiating_caller(&target, &record.installation, record.uid)?;
    quiesce::assert_stopped_except_parent(
        &target,
        Some(&record.installation),
        record.uid,
        caller.as_ref(),
    )?;
    installation::remove_inventory(&record.installation).map_err(|e| {
        OperationError::unavailable(format!(
            "{e}\nRetry: pkexec {} resume-privileged --transaction {}",
            quote(&path.with_file_name("openwave-maintenance")),
            quote(path)
        ))
    })?;
    // The root-created copy and authority are the only extra deletion targets.
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Missing transaction directory"))?;
    let dir = directory(parent, false, 0, false)?;
    for name in ["authority.json", "openwave-maintenance"] {
        let file = read_regular(&parent.join(name), Some(0), name == "authority.json")?;
        let m = file.metadata()?;
        let now =
            rustix::fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
        if now.st_ino != m.ino() || now.st_dev != m.dev() {
            return Err(invalid("Root transaction changed before cleanup"));
        }
        rustix::fs::unlinkat(&dir, name, AtFlags::empty()).map_err(io::Error::from)?;
    }
    match fs::remove_dir(parent) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => (),
        Err(e) => return Err(e.into()),
    }
    Ok(())
}
