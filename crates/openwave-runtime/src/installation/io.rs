use openwave_core::model::{ErrorCode, OperationError, Result};
use rustix::fs::{Mode, OFlags};
use serde::{
    Deserialize, Deserializer,
    de::{self, MapAccess, SeqAccess, Visitor},
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fmt,
    fs::{File, Metadata},
    io::Read,
    os::{fd::OwnedFd, unix::fs::MetadataExt},
    path::{Component, Path},
};

pub(super) const METADATA_LIMIT: u64 = 4 * 1024 * 1024;
pub(super) fn invalid(message: impl Into<String>) -> OperationError {
    OperationError::new(ErrorCode::Identity, message)
}
pub(super) fn errno(error: rustix::io::Errno) -> OperationError {
    std::io::Error::from(error).into()
}
pub(super) fn absolute(path: &Path) -> Result<()> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid("Inventory path is not UTF-8"))?;
    if !path.is_absolute()
        || path == Path::new("/")
        || text.ends_with('/')
        || text.contains("//")
        || text.split('/').any(|part| matches!(part, "." | ".."))
        || text.contains('\0')
    {
        return Err(invalid(format!(
            "Noncanonical inventory path: {}",
            path.display()
        )));
    }
    Ok(())
}

/// Each directory is opened relative to its already pinned parent. No component
/// may redirect through a symlink, including a final dangling symlink.
pub(super) fn directory(path: &Path) -> Result<Option<OwnedFd>> {
    if path != Path::new("/") {
        absolute(path)?;
    }
    let mut fd = rustix::fs::open(
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(errno)?;
    for part in path.components() {
        let Component::Normal(name) = part else {
            continue;
        };
        fd = match rustix::fs::openat(
            &fd,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(error) => return Err(errno(error)),
        };
    }
    Ok(Some(fd))
}
pub(super) fn regular(path: &Path) -> Result<Option<File>> {
    absolute(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Inventory file has no parent"))?;
    let Some(fd) = directory(parent)? else {
        return Ok(None);
    };
    regular_at(
        &fd,
        path.file_name()
            .ok_or_else(|| invalid("Inventory file has no name"))?,
    )
}
pub(super) fn regular_at(parent: &OwnedFd, name: &std::ffi::OsStr) -> Result<Option<File>> {
    let fd = match rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(errno(error)),
    };
    let file = File::from(fd);
    if !file.metadata()?.is_file() {
        return Err(invalid("Inventory target is not a regular file"));
    }
    Ok(Some(file))
}
pub(super) fn same_file(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.mode() == right.mode()
        && left.uid() == right.uid()
        && left.gid() == right.gid()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}
pub(super) fn digest_open(file: &mut File) -> Result<String> {
    digest_open_cancellable(file, &|| Ok(()))
}
pub(super) fn digest_open_cancellable(
    file: &mut File,
    check_cancel: &impl Fn() -> Result<()>,
) -> Result<String> {
    let before = file.metadata()?;
    // A bounded stream, not a whole-file allocation. Native release ELFs fit
    // comfortably below this cap; special files were rejected before reading.
    if before.len() > 1024 * 1024 * 1024 {
        return Err(invalid("Inventory file exceeds 1 GiB"));
    }
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    let mut total = 0u64;
    loop {
        check_cancel()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > before.len() {
            return Err(invalid("Inventory file changed while hashing"));
        }
        hash.update(&buffer[..count]);
    }
    if total != before.len() || !same_file(&before, &file.metadata()?) {
        return Err(invalid("Inventory file changed while hashing"));
    }
    Ok(super::digest_hex(hash.finalize()))
}
pub fn file_digest(path: &Path) -> Result<String> {
    let mut file = regular(path)?
        .ok_or_else(|| invalid(format!("Missing inventory file: {}", path.display())))?;
    digest_open(&mut file)
}
pub(super) fn read_bytes(path: &Path) -> Result<Vec<u8>> {
    let mut file =
        regular(path)?.ok_or_else(|| invalid(format!("Missing metadata: {}", path.display())))?;
    let before = file.metadata()?;
    if before.len() > METADATA_LIMIT {
        return Err(invalid("Installation metadata exceeds 4 MiB"));
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    (&mut file)
        .take(METADATA_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > METADATA_LIMIT || !same_file(&before, &file.metadata()?) {
        return Err(invalid("Metadata changed or exceeds 4 MiB"));
    }
    Ok(bytes)
}
struct Unique(Value);
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Strict;
        impl<'de> Visitor<'de> for Strict {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON without duplicate keys")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Unique, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| Unique(Value::Number(n)))
                    .ok_or_else(|| E::custom("Nonfinite number"))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Unique, E> {
                Ok(Unique(v.into()))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut values = Vec::new();
                while let Some(Unique(value)) = seq.next_element()? {
                    if values.len() == 20000 {
                        return Err(de::Error::custom("Metadata array exceeds 20000 entries"));
                    }
                    values.push(value);
                }
                Ok(Unique(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom(format!("Duplicate metadata key: {key}")));
                    }
                    if values.len() == 20000 {
                        return Err(de::Error::custom("Metadata object exceeds 20000 entries"));
                    }
                    let Unique(value) = map.next_value()?;
                    values.insert(key, value);
                }
                Ok(Unique(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Strict)
    }
}
pub(crate) fn parse_metadata(bytes: &[u8]) -> Result<Value> {
    if bytes.len() as u64 > METADATA_LIMIT {
        return Err(invalid("Installation metadata exceeds 4 MiB"));
    }
    // serde_json's default nesting limit is intentionally retained.
    let Unique(value) = serde_json::from_slice(bytes)?;
    if !value.is_object() {
        return Err(invalid("Installation metadata must be an object"));
    }
    Ok(value)
}
pub fn read_metadata(path: &Path) -> Result<Value> {
    parse_metadata(&read_bytes(path)?)
}
