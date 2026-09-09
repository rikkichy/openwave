//! Exact installation inventories. Destructive callers must own the exclusion
//! lease; privileged callers independently derive authority from trusted records.
mod install;
mod inventory;
mod io;
pub use install::install_payload;
pub use inventory::remove_inventory_cancellable;
pub use inventory::{
    NATIVE_PAYLOAD, check_install_target, inspect, inspect_prefix, record_install,
    remove_inventory, snapshot_from_receipt, validate_installation,
};
pub(crate) use io::parse_metadata;
pub use io::{file_digest, read_metadata};

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(crate) fn digest_hex(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(char::from(HEX[(byte >> 4) as usize]));
        result.push(char::from(HEX[(byte & 15) as usize]));
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InstallationFormat {
    #[serde(rename = "rust-v2")]
    RustV2,
    #[serde(rename = "python-v1")]
    PythonV1,
    #[serde(rename = "python-legacy")]
    PythonLegacy,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstallMethod {
    Manual,
    Deb,
    Rpm,
    Arch,
    Nix,
    Flatpak,
    Source,
    Unknown,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationSnapshot {
    pub format: InstallationFormat,
    pub method: InstallMethod,
    pub prefix: PathBuf,
    pub module_dir: Option<PathBuf>,
    pub receipt: Option<PathBuf>,
    pub files: Vec<PathBuf>,
    pub directories: Vec<PathBuf>,
    pub identities: Vec<(PathBuf, String)>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    pub method: InstallMethod,
    pub snapshot: Option<InstallationSnapshot>,
    pub canonical_identity: PathBuf,
    pub guidance: String,
    pub error: Option<String>,
}

/// Package databases outrank receipts, including when a wrapper changed hashes.
/// An unavailable database is not evidence that files are manually owned.
pub fn package_owner(paths: &[PathBuf]) -> openwave_core::model::Result<Option<InstallMethod>> {
    use openwave_core::model::{ErrorCode, OperationError};
    use std::{collections::BTreeSet, fs, time::Duration};
    if crate::setup::is_sandboxed() {
        return Ok(Some(InstallMethod::Flatpak));
    }
    for path in paths {
        if path.starts_with("/nix/store") {
            return Ok(Some(InstallMethod::Nix));
        }
        match fs::canonicalize(path) {
            Ok(resolved) if resolved.starts_with("/nix/store") => {
                return Ok(Some(InstallMethod::Nix));
            }
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => {
                return Err(OperationError::new(
                    ErrorCode::Unavailable,
                    format!("Cannot establish package path identity: {e}"),
                ));
            }
        }
    }
    let probes: BTreeSet<_> = paths.iter().collect();
    let runner = crate::process::CommandRunner::default();
    for (method, program, flags, absent) in [
        (
            InstallMethod::Deb,
            "dpkg-query",
            vec!["-S"],
            "no path found matching pattern",
        ),
        (
            InstallMethod::Rpm,
            "rpm",
            vec!["-qf", "--"],
            "is not owned by any package",
        ),
        (
            InstallMethod::Arch,
            "pacman",
            vec!["-Qo", "--"],
            "no package owns",
        ),
    ] {
        let elevated = rustix::process::geteuid().is_root();
        // A root request cannot hide a database by replacing PATH, or execute a
        // query tool found in an administrator caller's writable directory.
        let binary = if elevated {
            [
                "/usr/bin",
                "/usr/sbin",
                "/bin",
                "/sbin",
                "/run/current-system/sw/bin",
            ]
            .iter()
            .map(|base| std::path::Path::new(base).join(program))
            .find(|path| fs::metadata(path).is_ok_and(|m| m.is_file()))
        } else {
            crate::service::find_program(program)
        };
        let Some(binary) = binary else {
            let databases: &[&str] = match method {
                InstallMethod::Deb => &["/var/lib/dpkg/status"],
                InstallMethod::Rpm => &["/var/lib/rpm", "/usr/lib/sysimage/rpm"],
                InstallMethod::Arch => &["/var/lib/pacman/local"],
                _ => &[],
            };
            for database in databases {
                match fs::symlink_metadata(database) {
                    Ok(_) => {
                        return Err(OperationError::unavailable(format!(
                            "Package database exists but {program} is unavailable"
                        )));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                    Err(e) => return Err(e.into()),
                }
            }
            continue;
        };
        let binary = fs::canonicalize(binary)?;
        if elevated {
            crate::paths::trusted_for_root(&binary)?;
        }
        for path in &probes {
            let mut args: Vec<String> = flags.iter().map(|s| (*s).to_owned()).collect();
            args.push(crate::service::text_path(path)?.to_owned());
            // Clear DPKG_ROOT/RPM_CONFIGDIR/LD_* before spawn, not through an
            // env wrapper whose own dynamic loader would inherit those values.
            let output = runner.run_status_clean(
                crate::service::text_path(&binary)?,
                &args,
                Duration::from_secs(5),
            )?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            if output.status.success() {
                if !stdout.trim().is_empty() {
                    return Ok(Some(method));
                }
                return Err(OperationError::new(
                    ErrorCode::Unavailable,
                    format!("{program} returned an empty ownership result"),
                ));
            }
            let diagnostic = format!("{stderr}{stdout}").to_lowercase();
            if output.status.code() != Some(1) || !diagnostic.contains(absent) {
                return Err(OperationError::new(
                    ErrorCode::Unavailable,
                    format!("Package ownership query failed: {program}: {diagnostic}"),
                ));
            }
        }
    }
    Ok(None)
}
