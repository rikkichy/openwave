//! Release preparation only; none of these functions initialize runtime services.
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
pub type Result<T, E = ReleaseError> = std::result::Result<T, E>;

fn stable_version(value: &str) -> Result<()> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|p| {
            p.is_empty()
                || !p.bytes().all(|b| b.is_ascii_digit())
                || (p.len() > 1 && p.starts_with('0'))
        })
    {
        return Err(ReleaseError::Invalid(
            "VERSION must contain exactly one stable MAJOR.MINOR.PATCH version".into(),
        ));
    }
    Ok(())
}

pub fn validate_version(file: &Path, tag: Option<&str>) -> Result<String> {
    let raw = fs::read_to_string(file)?;
    let version = raw.strip_suffix('\n').unwrap_or(&raw);
    stable_version(version)?;
    if let Some(tag) = tag {
        if tag != format!("v{version}") {
            return Err(ReleaseError::Invalid(format!(
                "tag {tag:?} does not match VERSION ({version})"
            )));
        }
    }
    Ok(version.into())
}

pub fn render_aur(version: &str, sha256: &str, output: &Path) -> Result<()> {
    stable_version(version)?;
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ReleaseError::Invalid(
            "source checksum must be 64 hexadecimal digits".into(),
        ));
    }
    let fields = [
        ("pkgver=", format!("pkgver={version}")),
        (
            "source=",
            format!(
                "source=(\"https://github.com/rikkichy/openwave/releases/download/v{version}/openwave-{version}.tar.gz\")"
            ),
        ),
        (
            "sha256sums=",
            format!("sha256sums=('{}')", sha256.to_ascii_lowercase()),
        ),
        ("_srcdir=", format!("_srcdir=\"openwave-{version}\"")),
    ];
    let template = include_str!("../../../PKGBUILD");
    let mut counts = [0; 4];
    let mut recipe = String::with_capacity(template.len() + 128);
    for line in template.lines() {
        if let Some((index, (_, replacement))) = fields
            .iter()
            .enumerate()
            .find(|(_, (key, _))| line.starts_with(key))
        {
            counts[index] += 1;
            recipe.push_str(replacement);
        } else {
            recipe.push_str(line);
        }
        recipe.push('\n');
    }
    if counts != [1; 4] {
        return Err(ReleaseError::Invalid(
            "PKGBUILD must have exactly one pkgver, source, sha256sums and _srcdir assignment"
                .into(),
        ));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    file.write_all(recipe.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// Overlay Cargo's source replacement tables without discarding build, target,
/// registry, alias, or other source settings from the prepared source tree.
pub fn merge_vendor_config(existing: &str, emitted: &str) -> Result<String> {
    fn parse(text: &str) -> Result<toml::Table> {
        toml::from_str(text)
            .map_err(|e| ReleaseError::Invalid(format!("invalid Cargo configuration: {e}")))
    }
    fn merge(destination: &mut toml::Table, incoming: toml::Table) {
        for (key, value) in incoming {
            match (destination.get_mut(&key), value) {
                (Some(toml::Value::Table(current)), toml::Value::Table(next)) => {
                    merge(current, next)
                }
                (_, value) => {
                    destination.insert(key, value);
                }
            }
        }
    }
    let mut config = parse(existing)?;
    let vendor = parse(emitted)?;
    if vendor.len() != 1
        || !vendor
            .get("source")
            .is_some_and(|v| v.as_table().is_some_and(|t| !t.is_empty()))
    {
        return Err(ReleaseError::Invalid(
            "cargo vendor output must contain only nonempty source replacement tables".into(),
        ));
    }
    merge(&mut config, vendor);
    toml::to_string(&config)
        .map_err(|e| ReleaseError::Invalid(format!("cannot encode Cargo configuration: {e}")))
}
