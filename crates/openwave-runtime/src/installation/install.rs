//! Privileged publication of a receipt-bound, fixed native payload. Source files
//! are untrusted data; none of their executable entrypoints are ever invoked.
use super::inventory::install_receipt;
use super::io::{
    absolute, digest_open, directory, errno, invalid, parse_metadata, read_bytes, regular,
    regular_at, same_file,
};
use super::{
    InstallationSnapshot, NATIVE_PAYLOAD, check_install_target, digest_hex, file_digest,
    package_owner, snapshot_from_receipt,
};
use openwave_core::model::Result;
use rustix::fs::{AtFlags, Mode, OFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, File, Metadata},
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::OwnedFd,
        unix::fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

const RECEIPT: &str = "share/openwave/install-manifest.json";
const FILE_LIMIT: u64 = 1024 * 1024 * 1024;
const AUTHORITY: &str = "install-authority.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Identity {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    len: u64,
    mtime: i64,
    mtime_nsec: i64,
    ctime: i64,
    ctime_nsec: i64,
}
impl Identity {
    fn of(info: &Metadata) -> Self {
        Self {
            dev: info.dev(),
            ino: info.ino(),
            mode: info.mode(),
            uid: info.uid(),
            gid: info.gid(),
            len: info.len(),
            mtime: info.mtime(),
            mtime_nsec: info.mtime_nsec(),
            ctime: info.ctime(),
            ctime_nsec: info.ctime_nsec(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    identity: Identity,
    digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    schema: u32,
    bootstrap: PathBuf,
    bootstrap_identity: (u64, u64),
    helper: Identity,
    stage: PathBuf,
    stage_identity: (u64, u64),
    accepted: InstallationSnapshot,
    sources: BTreeMap<PathBuf, Identity>,
    original: BTreeMap<PathBuf, Option<Target>>,
    directories: BTreeMap<PathBuf, (u64, u64)>,
}

fn directory_identity(fd: &OwnedFd) -> Result<(u64, u64)> {
    let info = rustix::fs::fstat(fd).map_err(errno)?;
    Ok((info.st_dev, info.st_ino))
}

struct Source {
    destination: PathBuf,
    file: File,
    identity: Metadata,
    digest: String,
    mode: u32,
}

fn final_mode(relative: &str) -> u32 {
    if relative.starts_with("bin/") || relative.starts_with("libexec/") {
        0o755
    } else {
        0o644
    }
}

fn trusted_directory(fd: &OwnedFd) -> Result<()> {
    let info = rustix::fs::fstat(fd).map_err(errno)?;
    if info.st_uid != 0 || info.st_mode & 0o022 != 0 {
        return Err(invalid(
            "Payload destination and every ancestor must be root-owned and not group/world writable",
        ));
    }
    Ok(())
}

fn required_directories(snapshot: &InstallationSnapshot) -> BTreeSet<PathBuf> {
    snapshot
        .files
        .iter()
        .flat_map(|path| path.ancestors().skip(1).map(Path::to_owned))
        .collect()
}

// No-follow traversal is used even during the read-only preflight. Missing
// directories are created only after the entire source and old target pass.
fn destination_directories(
    required: &BTreeSet<PathBuf>,
    create: bool,
) -> Result<BTreeMap<PathBuf, OwnedFd>> {
    let mut pinned = BTreeMap::new();
    for path in required {
        let opened = if path == Path::new("/") {
            Some(
                rustix::fs::open(
                    "/",
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(errno)?,
            )
        } else if let Some(parent) = pinned.get(
            path.parent()
                .ok_or_else(|| invalid("Missing destination parent"))?,
        ) {
            let name = path
                .file_name()
                .ok_or_else(|| invalid("Missing destination name"))?;
            match rustix::fs::openat(
                parent,
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => Some(fd),
                Err(rustix::io::Errno::NOENT) if create => {
                    rustix::fs::mkdirat(parent, name, Mode::from_raw_mode(0o755)).map_err(errno)?;
                    let fd = rustix::fs::openat(
                        parent,
                        name,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(errno)?;
                    rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o755)).map_err(errno)?;
                    rustix::fs::fsync(parent).map_err(errno)?;
                    Some(fd)
                }
                Err(rustix::io::Errno::NOENT) => None,
                Err(error) => return Err(errno(error)),
            }
        } else {
            None
        };
        if let Some(fd) = opened {
            trusted_directory(&fd)?;
            pinned.insert(path.clone(), fd);
        }
    }
    Ok(pinned)
}

fn parent_unchanged(path: &Path, pinned: &OwnedFd) -> Result<()> {
    let reopened =
        directory(path)?.ok_or_else(|| invalid("Payload destination directory disappeared"))?;
    trusted_directory(&reopened)?;
    let before = rustix::fs::fstat(pinned).map_err(errno)?;
    let after = rustix::fs::fstat(&reopened).map_err(errno)?;
    if before.st_dev != after.st_dev || before.st_ino != after.st_ino {
        return Err(invalid(
            "Payload destination directory changed before publication",
        ));
    }
    Ok(())
}

fn target(parent: &OwnedFd, name: &OsStr) -> Result<Option<Target>> {
    let Some(mut file) = regular_at(parent, name)? else {
        return Ok(None);
    };
    let before = file.metadata()?;
    if before.uid() != 0 || before.mode() & 0o022 != 0 {
        return Err(invalid(
            "Payload destination is not root-owned and nonwritable",
        ));
    }
    let digest = digest_open(&mut file)?;
    let reopened =
        regular_at(parent, name)?.ok_or_else(|| invalid("Payload destination disappeared"))?;
    if !same_file(&before, &reopened.metadata()?) {
        return Err(invalid("Payload destination identity changed"));
    }
    Ok(Some(Target {
        identity: Identity::of(&before),
        digest,
    }))
}

fn expected_target(parent: &OwnedFd, name: &OsStr, expected: Option<&Target>) -> Result<()> {
    if target(parent, name)?.as_ref() != expected {
        return Err(invalid(
            "Payload destination changed or contains unrecorded content",
        ));
    }
    Ok(())
}

fn publish(
    parent: &OwnedFd,
    path: &Path,
    mode: u32,
    expected: Option<&Target>,
    write: impl FnOnce(&mut File) -> Result<()>,
) -> Result<()> {
    let temporary_name = format!(".openwave-install-{}", uuid::Uuid::new_v4().simple());
    let fd = rustix::fs::openat(
        parent,
        temporary_name.as_str(),
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )
    .map_err(errno)?;
    let result = (|| {
        let mut temporary = File::from(fd);
        write(&mut temporary)?;
        temporary.set_permissions(fs::Permissions::from_mode(mode))?;
        temporary.sync_all()?;
        parent_unchanged(
            path.parent()
                .ok_or_else(|| invalid("Missing publication parent"))?,
            parent,
        )?;
        let name = path
            .file_name()
            .ok_or_else(|| invalid("Missing publication name"))?;
        expected_target(parent, name, expected)?;
        rustix::fs::renameat(parent, temporary_name.as_str(), parent, name).map_err(errno)?;
        rustix::fs::fsync(parent).map_err(errno)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(parent, temporary_name.as_str(), AtFlags::empty());
    }
    result
}

fn source_files(stage: &Path, snapshot: &InstallationSnapshot) -> Result<Vec<Source>> {
    let hashes: BTreeMap<_, _> = snapshot
        .identities
        .iter()
        .map(|(path, digest)| (path.as_path(), digest.as_str()))
        .collect();
    let mut sources = Vec::with_capacity(NATIVE_PAYLOAD.len());
    for relative in NATIVE_PAYLOAD {
        let destination = snapshot.prefix.join(relative);
        let path = stage.join(
            destination
                .strip_prefix("/")
                .map_err(|_| invalid("Invalid staged destination"))?,
        );
        let mut file = regular(&path)?
            .ok_or_else(|| invalid(format!("Missing staged payload: {}", path.display())))?;
        let identity = file.metadata()?;
        let mode = final_mode(relative);
        if identity.mode() & 0o7777 != mode {
            return Err(invalid(format!(
                "Staged payload does not have its final mode: {}",
                path.display()
            )));
        }
        if mode == 0o755 {
            let mut magic = [0; 4];
            file.read_exact(&mut magic)?;
            if magic != *b"\x7fELF" {
                return Err(invalid("Staged native binary is not ELF"));
            }
            file.seek(SeekFrom::Start(0))?;
        }
        let digest = digest_open(&mut file)?;
        if hashes.get(destination.as_path()).copied() != Some(digest.as_str()) {
            return Err(invalid(format!(
                "Staged payload digest changed: {}",
                path.display()
            )));
        }
        file.seek(SeekFrom::Start(0))?;
        if *relative == "share/openwave/VERSION" {
            if identity.len() > 128 {
                return Err(invalid("Staged VERSION is invalid"));
            }
            let mut version = String::new();
            (&mut file).take(129).read_to_string(&mut version)?;
            if version != openwave_core::VERSION
                && version != format!("{}\n", openwave_core::VERSION)
            {
                return Err(invalid(
                    "Staged VERSION does not match the trusted helper's compiled release",
                ));
            }
            file.seek(SeekFrom::Start(0))?;
        }
        if !same_file(&identity, &file.metadata()?) {
            return Err(invalid("Staged payload changed during preflight"));
        }
        let reopened = regular(&path)?.ok_or_else(|| invalid("Staged payload disappeared"))?;
        if !same_file(&identity, &reopened.metadata()?) {
            return Err(invalid("Staged payload identity changed"));
        }
        sources.push(Source {
            destination,
            file,
            identity,
            digest,
            mode,
        });
    }
    Ok(sources)
}

fn copy_source(source: &mut Source, destination: &mut File) -> Result<()> {
    if !same_file(&source.identity, &source.file.metadata()?) {
        return Err(invalid("Staged payload changed after preflight"));
    }
    source.file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    let mut total = 0u64;
    loop {
        let count = source.file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > source.identity.len() || total > FILE_LIMIT {
            return Err(invalid("Staged payload grew during copying"));
        }
        hash.update(&buffer[..count]);
        destination.write_all(&buffer[..count])?;
    }
    if total != source.identity.len()
        || !same_file(&source.identity, &source.file.metadata()?)
        || digest_hex(hash.finalize()) != source.digest
    {
        return Err(invalid(
            "Staged payload changed during copying; file was not published",
        ));
    }
    Ok(())
}

struct Pending {
    bootstrap: PathBuf,
    bootstrap_fd: OwnedFd,
    authority: Target,
    receipt: PathBuf,
    bytes: Vec<u8>,
    sources: Vec<Source>,
    pinned: BTreeMap<PathBuf, OwnedFd>,
    expected: BTreeMap<PathBuf, Option<Target>>,
}

fn prepare(
    stage: &Path,
    prefix: &Path,
    expected_sha256: &str,
    executable: &Path,
) -> Result<Pending> {
    if !rustix::process::geteuid().is_root() {
        return Err(invalid(
            "install-payload requires administrator authorization through a trusted bootstrap helper",
        ));
    }
    absolute(stage)?;
    absolute(prefix)?;
    crate::paths::trusted_for_root(executable)?;
    let bootstrap = executable
        .parent()
        .ok_or_else(|| invalid("Missing bootstrap directory"))?;
    let name = bootstrap.file_name().and_then(OsStr::to_str).unwrap_or("");
    if executable.file_name() != Some(OsStr::new("openwave-maintenance"))
        || bootstrap.parent() != Some(prefix.join("libexec").as_path())
        || !name
            .strip_prefix("openwave-bootstrap-")
            .is_some_and(|suffix| !suffix.is_empty())
    {
        return Err(invalid(
            "Payload installation requires this prefix's retained unique native bootstrap",
        ));
    }
    let bootstrap_fd = directory(bootstrap)?.ok_or_else(|| invalid("Bootstrap disappeared"))?;
    trusted_directory(&bootstrap_fd)?;
    if rustix::fs::fstat(&bootstrap_fd).map_err(errno)?.st_mode & 0o7777 != 0o755 {
        return Err(invalid("Bootstrap directory must have mode 0755"));
    }
    // The directory inode is the transaction identity and the lock. No second
    // lock file or orphan-record discovery is needed, including after a crash.
    rustix::fs::flock(
        &bootstrap_fd,
        rustix::fs::FlockOperation::NonBlockingLockExclusive,
    )
    .map_err(errno)?;
    let helper = Identity::of(
        &regular(executable)?
            .ok_or_else(|| invalid("Bootstrap helper disappeared"))?
            .metadata()?,
    );
    if stage.starts_with(prefix) || prefix.starts_with(stage) {
        return Err(invalid("Staging and destination trees must be separate"));
    }
    if expected_sha256.len() != 64
        || !expected_sha256
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(invalid(
            "Expected staged receipt digest must be 64 lowercase hexadecimal characters",
        ));
    }
    let stage_fd = directory(stage)?.ok_or_else(|| invalid("Stage disappeared"))?;
    let staged_receipt = stage
        .join(
            prefix
                .strip_prefix("/")
                .map_err(|_| invalid("Invalid prefix"))?,
        )
        .join(RECEIPT);
    let receipt_identity = regular(&staged_receipt)?
        .ok_or_else(|| invalid("Missing staged receipt"))?
        .metadata()?;
    let bytes = read_bytes(&staged_receipt)?;
    if digest_hex(Sha256::digest(&bytes)) != expected_sha256 {
        return Err(invalid("Staged receipt digest changed before installation"));
    }
    if !same_file(
        &receipt_identity,
        &regular(&staged_receipt)?
            .ok_or_else(|| invalid("Staged receipt disappeared"))?
            .metadata()?,
    ) {
        return Err(invalid("Staged receipt identity changed"));
    }
    let snapshot = install_receipt(&bytes, prefix)?;
    let sources = source_files(stage, &snapshot)?;
    let mut source_identities: BTreeMap<_, _> = sources
        .iter()
        .map(|source| (source.destination.clone(), Identity::of(&source.identity)))
        .collect();
    let receipt = prefix.join(RECEIPT);
    source_identities.insert(receipt.clone(), Identity::of(&receipt_identity));
    let required = required_directories(&snapshot);
    let mut pinned = destination_directories(&required, false)?;
    if package_owner(&snapshot.files)?.is_some() {
        return Err(invalid(
            "Package ownership overrides manual payload installation; use its package manager",
        ));
    }
    if directory(&prefix.join("share/openwave/site-packages/wavexlr"))?.is_some() {
        return Err(invalid(
            "Unresolved legacy module tree blocks native installation",
        ));
    }
    let authority_path = bootstrap.join(AUTHORITY);
    let existing_authority = target(&bootstrap_fd, OsStr::new(AUTHORITY))?;
    let authority = if let Some(record) = &existing_authority {
        let metadata = regular_at(&bootstrap_fd, OsStr::new(AUTHORITY))?
            .ok_or_else(|| invalid("Installation authority disappeared"))?
            .metadata()?;
        if metadata.mode() & 0o7777 != 0o600 || metadata.nlink() != 1 {
            return Err(invalid(
                "Installation authority must be a private root-created regular file",
            ));
        }
        let value = parse_metadata(&read_bytes(&authority_path)?)?;
        expected_target(&bootstrap_fd, OsStr::new(AUTHORITY), Some(record))?;
        let authority: Authority = serde_json::from_value(value)?;
        if authority.schema != 1
            || authority.bootstrap != bootstrap
            || authority.bootstrap_identity != directory_identity(&bootstrap_fd)?
            || authority.helper != helper
            || authority.stage != stage
            || authority.stage_identity != directory_identity(&stage_fd)?
            || authority.accepted != snapshot
            || authority.sources != source_identities
            || authority.original.keys().ne(snapshot.files.iter())
            || authority.directories.keys().ne(required.iter())
        {
            return Err(invalid(
                "Retained installation authority does not match this bootstrap, source or accepted payload",
            ));
        }
        for (path, identity) in &authority.directories {
            let fd = pinned
                .get(path)
                .ok_or_else(|| invalid("Recorded destination directory disappeared"))?;
            if directory_identity(fd)? != *identity {
                return Err(invalid("Recorded destination directory was replaced"));
            }
        }
        authority
    } else {
        // Ordinary install preflight is deliberately unchanged. Only this new,
        // durable root record can authorize partial publications on later runs.
        check_install_target(prefix, None)?;
        let old = if regular(&receipt)?.is_some() {
            Some(snapshot_from_receipt(
                &receipt,
                &file_digest(&receipt)?,
                prefix,
            )?)
        } else {
            None
        };
        pinned = destination_directories(&required, true)?;
        let mut original = BTreeMap::new();
        for path in &snapshot.files {
            let current = target(&pinned[path.parent().unwrap()], path.file_name().unwrap())?;
            if let Some(current) = &current {
                if old.as_ref().is_none_or(|old| {
                    !old.identities
                        .iter()
                        .any(|(p, h)| p == path && h == &current.digest)
                }) {
                    return Err(invalid("Destination changed after installation preflight"));
                }
            }
            original.insert(path.clone(), current);
        }
        let directories = pinned
            .iter()
            .map(|(path, fd)| Ok((path.clone(), directory_identity(fd)?)))
            .collect::<Result<_>>()?;
        let authority = Authority {
            schema: 1,
            bootstrap: bootstrap.to_owned(),
            bootstrap_identity: directory_identity(&bootstrap_fd)?,
            helper,
            stage: stage.to_owned(),
            stage_identity: directory_identity(&stage_fd)?,
            accepted: snapshot.clone(),
            sources: source_identities,
            original,
            directories,
        };
        let record = serde_json::to_vec(&authority)?;
        publish(&bootstrap_fd, &authority_path, 0o600, None, |temporary| {
            temporary.write_all(&record)?;
            Ok(())
        })?;
        authority
    };
    let mut expected = BTreeMap::new();
    for (path, hash) in &snapshot.identities {
        let current = target(&pinned[path.parent().unwrap()], path.file_name().unwrap())?;
        let original = authority
            .original
            .get(path)
            .ok_or_else(|| invalid("Missing original destination slot"))?;
        let mode = if path == &receipt {
            0o644
        } else {
            final_mode(path.strip_prefix(prefix).unwrap().to_str().unwrap())
        };
        // Missing originally-present files are not publication by this helper.
        // Old bytes require their original inode; new bytes require exact final
        // hashes/modes and stable opened-name identity, never a third hash.
        if current != *original
            && !current
                .as_ref()
                .is_some_and(|file| file.digest == *hash && file.identity.mode & 0o7777 == mode)
        {
            return Err(invalid(format!(
                "Destination differs from both original and accepted payload: {}",
                path.display()
            )));
        }
        expected.insert(path.clone(), current);
    }
    parent_unchanged(bootstrap, &bootstrap_fd)?;
    let authority = target(&bootstrap_fd, OsStr::new(AUTHORITY))?
        .ok_or_else(|| invalid("Installation authority disappeared"))?;
    if let Some(original) = existing_authority {
        if original != authority {
            return Err(invalid("Installation authority changed during preflight"));
        }
    }
    Ok(Pending {
        bootstrap: bootstrap.to_owned(),
        bootstrap_fd,
        authority,
        receipt,
        bytes,
        sources,
        pinned,
        expected,
    })
}

impl Pending {
    fn publish_source(&mut self, index: usize) -> Result<()> {
        let receipt_parent = &self.pinned[self.receipt.parent().unwrap()];
        expected_target(
            receipt_parent,
            OsStr::new("install-manifest.json"),
            self.expected[&self.receipt].as_ref(),
        )?;
        let source = &mut self.sources[index];
        let path = source.destination.clone();
        let parent = &self.pinned[path.parent().unwrap()];
        if self.expected[&path].as_ref().is_some_and(|current| {
            current.digest == source.digest && current.identity.mode & 0o7777 == source.mode
        }) {
            expected_target(
                parent,
                path.file_name().unwrap(),
                self.expected[&path].as_ref(),
            )?;
            return Ok(());
        }
        publish(
            parent,
            &path,
            source.mode,
            self.expected[&path].as_ref(),
            |temporary| copy_source(source, temporary),
        )?;
        self.expected
            .insert(path.clone(), target(parent, path.file_name().unwrap())?);
        Ok(())
    }

    fn verify_payload(&self) -> Result<()> {
        for source in &self.sources {
            let parent = &self.pinned[source.destination.parent().unwrap()];
            parent_unchanged(source.destination.parent().unwrap(), parent)?;
            let current = target(parent, source.destination.file_name().unwrap())?
                .ok_or_else(|| invalid("Published payload disappeared"))?;
            if current.identity.mode & 0o7777 != source.mode || current.digest != source.digest {
                return Err(invalid(
                    "Published payload changed before its final receipt",
                ));
            }
        }
        Ok(())
    }

    fn publish_receipt(&mut self) -> Result<()> {
        self.verify_payload()?;
        let parent = &self.pinned[self.receipt.parent().unwrap()];
        if self.expected[&self.receipt]
            .as_ref()
            .is_some_and(|current| {
                current.digest == digest_hex(Sha256::digest(&self.bytes))
                    && current.identity.mode & 0o7777 == 0o644
            })
        {
            expected_target(
                parent,
                self.receipt.file_name().unwrap(),
                self.expected[&self.receipt].as_ref(),
            )?;
            return Ok(());
        }
        publish(
            parent,
            &self.receipt,
            0o644,
            self.expected[&self.receipt].as_ref(),
            |temporary| {
                temporary.write_all(&self.bytes)?;
                Ok(())
            },
        )?;
        self.expected.insert(
            self.receipt.clone(),
            target(parent, self.receipt.file_name().unwrap())?,
        );
        Ok(())
    }

    fn finish(mut self) -> Result<()> {
        for index in 0..self.sources.len() {
            self.publish_source(index)?;
        }
        self.publish_receipt()?;
        self.verify_payload()?;
        let receipt_parent = &self.pinned[self.receipt.parent().unwrap()];
        parent_unchanged(self.receipt.parent().unwrap(), receipt_parent)?;
        expected_target(
            receipt_parent,
            self.receipt.file_name().unwrap(),
            self.expected[&self.receipt].as_ref(),
        )?;
        parent_unchanged(&self.bootstrap, &self.bootstrap_fd)?;
        expected_target(
            &self.bootstrap_fd,
            OsStr::new(AUTHORITY),
            Some(&self.authority),
        )?;
        rustix::fs::unlinkat(&self.bootstrap_fd, AUTHORITY, AtFlags::empty()).map_err(errno)?;
        rustix::fs::fsync(&self.bootstrap_fd).map_err(errno)?;
        Ok(())
    }
}

/// Publish a complete fixed native payload bound to its pre-elevation receipt.
/// Each file is atomic; a multi-file failure is partial, not rollback. Retaining
/// this exact trusted bootstrap and unchanged stage permits durable continuation.
pub fn install_payload(stage: &Path, prefix: &Path, expected_sha256: &str) -> Result<()> {
    prepare(stage, prefix, expected_sha256, &std::env::current_exe()?)?.finish()
        .map_err(|error| invalid(format!("Native payload installation is incomplete (no multi-file rollback): {error}. Preserve the trusted bootstrap and staged payload; resolve the reported conflict before continuing. Do not run the old uninstaller over partially installed native files.")))
}

#[cfg(test)]
#[path = "../../tests/fixtures/install_continuation.rs"]
mod tests;
