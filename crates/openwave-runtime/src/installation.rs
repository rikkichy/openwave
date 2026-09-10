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
            vec!["-qa", "--queryformat", RPM_OWNERSHIP_FORMAT],
            "",
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
        if method == InstallMethod::Rpm {
            // Query headers once, not the live filesystem: -qf and --path both
            // report ENOENT for missing unowned files on supported RPM versions.
            let args: Vec<String> = flags.iter().map(|s| (*s).to_owned()).collect();
            let output = runner.run_status_clean(
                crate::service::text_path(&binary)?,
                &args,
                Duration::from_secs(5),
            )?;
            if rpm_query_owner(&output, paths)? {
                return Ok(Some(method));
            }
            continue;
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

const RPM_OWNERSHIP_FORMAT: &str = concat!(
    "%|FILENAMES?{[%{FILENAMES:shescape}\\n]}:{}|",
    "%|PROVIDENAME?{[P%{PROVIDENAME:shescape}\\n]}:{}|",
);

// RPM's shescape queryformat protects embedded newlines and quotes without
// executing a shell. Parse only that fixed encoding; malformed/truncated output
// and diagnostics never establish that a path is manually owned.
fn rpm_query_owner(
    output: &std::process::Output,
    paths: &[PathBuf],
) -> openwave_core::model::Result<bool> {
    use std::os::unix::ffi::OsStrExt;
    let failed = || {
        openwave_core::model::OperationError::unavailable(format!(
            "Package ownership query failed: rpm: {}{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout),
        ))
    };
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(failed());
    }
    let mut remaining = output.stdout.as_slice();
    let mut path = Vec::new();
    let mut owned = false;
    while !remaining.is_empty() {
        // -qf also consults PROVIDENAME after its filename lookup. Preserve
        // absolute-path provides, while ordinary capabilities are not paths.
        let provided = remaining.starts_with(b"P");
        if provided {
            remaining = &remaining[1..];
        }
        remaining = remaining.strip_prefix(b"'").ok_or_else(failed)?;
        path.clear();
        loop {
            let quote = remaining
                .iter()
                .position(|byte| *byte == b'\'')
                .ok_or_else(failed)?;
            path.extend_from_slice(&remaining[..quote]);
            remaining = &remaining[quote..];
            if let Some(rest) = remaining.strip_prefix(b"'\\''") {
                path.push(b'\'');
                remaining = rest;
            } else {
                remaining = remaining.strip_prefix(b"'\n").ok_or_else(failed)?;
                break;
            }
        }
        if (!provided && !path.starts_with(b"/")) || path.is_empty() || path.contains(&0) {
            return Err(failed());
        }
        owned |= paths
            .iter()
            .any(|probe| probe.as_os_str().as_bytes() == path);
    }
    Ok(owned)
}

#[cfg(test)]
mod package_query_tests {
    use super::*;
    use std::{os::unix::process::ExitStatusExt, process::Output};

    #[test]
    fn rpm_database_results_distinguish_absence_ownership_and_failure() {
        let output = |status, stdout: &[u8], stderr: &[u8]| Output {
            status: std::process::ExitStatus::from_raw(status << 8),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        };
        let paths = [PathBuf::from("/missing")];
        assert!(!rpm_query_owner(&output(0, b"", b""), &paths).unwrap());
        assert!(!rpm_query_owner(&output(0, b"'/unrelated'\n", b""), &paths).unwrap());
        assert!(rpm_query_owner(&output(0, b"'/missing'\n", b""), &paths).unwrap());
        assert!(!rpm_query_owner(&output(0, b"P'other-capability'\n", b""), &paths).unwrap());
        assert!(rpm_query_owner(&output(0, b"P'/missing'\n", b""), &paths).unwrap());
        for result in [
            output(1, b"", b""),
            output(1, b"", b"error: file /missing: No such file or directory\n"),
            output(1, b"file /missing is not owned by any package\n", b""),
            output(0, b"", b"error: cannot open Packages index\n"),
            output(0, b"'/missing'\n", b"error: damaged package header\n"),
            output(0, b"'/missing'\ntruncated", b""),
            output(0, b"'/missing", b""),
            output(0, b"'relative'\n", b""),
            output(0, b"\xff", b""),
        ] {
            assert!(rpm_query_owner(&result, &paths).is_err());
        }
    }

    #[test]
    fn rpm_header_paths_preserve_literal_quotes_newlines_and_pattern_characters() {
        let path = PathBuf::from("/missing/a'\n[?]*$\\");
        let output = Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"'/missing/a'\\''\n[?]*$\\'\n".to_vec(),
            stderr: Vec::new(),
        };
        assert!(rpm_query_owner(&output, std::slice::from_ref(&path)).unwrap());
        assert!(!rpm_query_owner(&output, &[PathBuf::from("/missing/a")]).unwrap());
    }

    #[test]
    #[ignore = "requires rpm and rpmbuild; uses only a disposable private RPM database"]
    fn real_rpm_database_preserves_missing_file_ownership() {
        use std::{fs, process::Command};
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let top = root.join("build");
        for directory in ["BUILD", "BUILDROOT", "RPMS", "SOURCES", "SPECS", "SRPMS"] {
            fs::create_dir_all(top.join(directory)).unwrap();
        }
        let owned = root.join("missing-owned");
        let unowned = root.join("missing-unowned");
        let provided = root.join("missing-provided");
        let spec = top.join("SPECS/proof.spec");
        fs::write(
            &spec,
            format!(
                "Name: openwave-ownership-proof\n\
                 Version: 1\n\
                 Release: 1\n\
                 Summary: Disposable ownership proof\n\
                 License: MIT\n\
                 BuildArch: noarch\n\
                 Provides: {}\n\
                 %description\n\
                 Disposable ownership proof.\n\
                 %files\n\
                 %attr(0644,root,root) %ghost {}\n",
                provided.display(),
                owned.display(),
            ),
        )
        .unwrap();
        let command = |program: &str, args: &[&str]| {
            Command::new(program)
                .args(args)
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("HOME", root)
                .env("LC_ALL", "C")
                .current_dir(root)
                .output()
                .expect("explicit real-RPM regression requires rpm and rpmbuild")
        };
        let success = |output: Output| {
            assert!(
                output.status.success(),
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            output
        };
        success(command(
            "rpmbuild",
            &[
                "-bb",
                "--define",
                &format!("_topdir {}", top.display()),
                spec.to_str().unwrap(),
            ],
        ));
        let package = top.join("RPMS/noarch/openwave-ownership-proof-1-1.noarch.rpm");
        let database = root.join("rpmdb");
        let db = database.to_str().unwrap();
        success(command("rpm", &["--dbpath", db, "--initdb"]));
        let query = || {
            command(
                "rpm",
                &["--dbpath", db, "-qa", "--queryformat", RPM_OWNERSHIP_FORMAT],
            )
        };
        assert!(!rpm_query_owner(&query(), std::slice::from_ref(&owned)).unwrap());
        success(command(
            "rpm",
            &[
                "--dbpath",
                db,
                "-i",
                "--justdb",
                "--nodeps",
                "--noscripts",
                "--notriggers",
                "--noplugins",
                package.to_str().unwrap(),
            ],
        ));
        assert!(!owned.exists() && !unowned.exists() && !provided.exists());
        let output = query();
        assert!(rpm_query_owner(&output, std::slice::from_ref(&owned)).unwrap());
        assert!(rpm_query_owner(&output, std::slice::from_ref(&provided)).unwrap());
        assert!(!rpm_query_owner(&output, &[unowned]).unwrap());
        let bad_database = root.join("not-a-database-directory");
        fs::write(&bad_database, b"not an RPM database directory").unwrap();
        let error = command(
            "rpm",
            &[
                "--dbpath",
                bad_database.to_str().unwrap(),
                "-qa",
                "--queryformat",
                RPM_OWNERSHIP_FORMAT,
            ],
        );
        assert!(rpm_query_owner(&error, &[owned]).is_err());
    }
}
