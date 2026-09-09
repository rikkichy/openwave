use super::io::{
    absolute, digest_open, directory, errno, invalid, parse_metadata, read_bytes, regular,
    regular_at, same_file,
};
use super::{
    InstallMethod, Installation, InstallationFormat, InstallationSnapshot, digest_hex,
    package_owner,
};
use openwave_core::model::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

const RECEIPT: &str = "share/openwave/install-manifest.json";
const MIGRATION: &str = "Use that existing installation's confirmed uninstaller, preserving settings, before installing native OpenWave. Do not guess interpreter sites or execute discovered Python to establish ownership.";
/// This list is shared with the deliberately enumerated Make payload, not with
/// destination-directory enumeration. Adding an asset requires changing both.
pub const NATIVE_PAYLOAD: &[&str] = &[
    "bin/openwave",
    "bin/openwave-daemon",
    "bin/openwave-diag",
    "bin/openwave-probe",
    "libexec/openwave-maintenance",
    "share/applications/openwave.desktop",
    "share/openwave/openwave-autostart.desktop",
    "share/openwave/style.css",
    "share/openwave/wireplumber/51-openwave-wave-xlr.conf",
    "share/openwave/pipewire/52-openwave-mixes.conf",
    "share/openwave/VERSION",
    "share/metainfo/com.github.openwave.metainfo.xml",
    "share/icons/hicolor/scalable/apps/openwave.svg",
    "share/icons/hicolor/scalable/status/openwave-white.svg",
    "share/icons/hicolor/scalable/status/openwave-black.svg",
    "share/icons/hicolor/scalable/status/openwave-red.svg",
    "share/openwave/icons/openwave.svg",
    "share/openwave/icons/openwave-white.svg",
    "share/openwave/icons/openwave-black.svg",
    "share/openwave/icons/openwave-red.svg",
    "share/doc/openwave/README.md",
    "share/doc/openwave/icons/openwave.svg",
    "share/doc/openwave/asset-attribution.txt",
    "share/licenses/openwave/LICENSE",
    "share/doc/openwave/docs/ARCHITECTURE.md",
    "share/doc/openwave/docs/hardware-support.md",
    "share/doc/openwave/docs/install-bazzite.md",
    "share/doc/openwave/docs/protocol.md",
    "share/doc/openwave/docs/troubleshooting.md",
];
const HISTORICAL: &[&str] = &[
    "share/icons/hicolor/symbolic/apps/openwave-symbolic.svg",
    "share/icons/hicolor/symbolic/apps/openwave-muted-symbolic.svg",
    "share/icons/hicolor/symbolic/apps/openwave-attention-symbolic.svg",
    "share/doc/openwave/openwave.svg",
    "share/openwave/icons/openwave-symbolic.svg",
    "share/openwave/icons/openwave-muted-symbolic.svg",
    "share/openwave/icons/openwave-attention-symbolic.svg",
];
const MODULES: &[&str] = &[
    "__init__",
    "__main__",
    "app",
    "audio",
    "calibrate",
    "child",
    "daemon",
    "desktop",
    "device",
    "diag",
    "effects",
    "health",
    "icons",
    "installation",
    "meter",
    "mixdialog",
    "mixer",
    "mixes",
    "mixmatrix",
    "paths",
    "probe",
    "profiles",
    "recovery",
    "scenes",
    "scheduler",
    "service",
    "setup",
    "sourcedialog",
    "sources",
    "tray",
    "uninstall",
    "uninstall_dialog",
];
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    schema: u64,
    application: String,
    method: InstallMethod,
    prefix: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    module_dir: Option<PathBuf>,
    files: Vec<PathBuf>,
    directories: Vec<PathBuf>,
    sha256: BTreeMap<PathBuf, String>,
}
fn manager(method: InstallMethod) -> bool {
    matches!(
        method,
        InstallMethod::Deb
            | InstallMethod::Rpm
            | InstallMethod::Arch
            | InstallMethod::Nix
            | InstallMethod::Flatpak
    )
}
fn guidance(method: InstallMethod) -> &'static str {
    match method {
        InstallMethod::Deb => "Remove the managed package with: sudo apt remove openwave",
        InstallMethod::Rpm => "Remove OpenWave with your RPM package manager (dnf or zypper).",
        InstallMethod::Arch => "Remove the managed package with: sudo pacman -R openwave",
        InstallMethod::Nix => {
            "Remove OpenWave from your Nix configuration or profile and rebuild/switch."
        }
        InstallMethod::Flatpak => {
            "On the host, run: flatpak uninstall com.github.openwave. Host audio integration is managed separately."
        }
        InstallMethod::Source => "This is a source checkout. Its files will not be deleted.",
        InstallMethod::Unknown => {
            "Installation ownership could not be established; no application files will be deleted."
        }
        InstallMethod::Manual => {
            "Only the accepted, unchanged OpenWave inventory will be removed; unrecorded files are preserved."
        }
    }
}
fn installation(
    method: InstallMethod,
    identity: PathBuf,
    snapshot: Option<InstallationSnapshot>,
) -> Installation {
    Installation {
        method,
        snapshot,
        canonical_identity: identity,
        guidance: guidance(method).into(),
        error: None,
    }
}
fn native_files(prefix: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = NATIVE_PAYLOAD
        .iter()
        .map(|name| prefix.join(name))
        .collect();
    files.push(prefix.join(RECEIPT));
    files.sort();
    files
}
fn legacy_files(prefix: &Path, module: &Path) -> BTreeSet<PathBuf> {
    NATIVE_PAYLOAD
        .iter()
        .filter(|name| {
            !matches!(
                **name,
                "libexec/openwave-maintenance" | "share/openwave/style.css"
            )
        })
        .chain(HISTORICAL)
        .map(|name| prefix.join(name))
        .chain(MODULES.iter().map(|name| module.join(format!("{name}.py"))))
        .chain([
            module.join("style.css"),
            module.join("install-location.json"),
            prefix.join(RECEIPT),
        ])
        .collect()
}
fn private_directories(files: &[PathBuf], prefix: &Path, module: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = vec![
        prefix.join("share/openwave"),
        prefix.join("share/doc/openwave"),
        prefix.join("share/licenses/openwave"),
    ];
    if let Some(module) = module {
        roots.push(module.to_owned());
    }
    let mut dirs: BTreeSet<_> = roots.iter().cloned().collect();
    for file in files {
        for parent in file.ancestors().skip(1) {
            if roots.iter().any(|root| parent.starts_with(root)) {
                dirs.insert(parent.to_owned());
            }
        }
    }
    dirs.into_iter().collect()
}
fn sorted_paths(paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() || paths.len() > 20000 || paths.windows(2).any(|p| p[0] >= p[1]) {
        return Err(invalid(
            "Inventory must be nonempty, sorted and unique, with at most 20000 paths",
        ));
    }
    for path in paths {
        absolute(path)?;
    }
    Ok(())
}
fn digest_valid(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn module_layout(prefix: &Path, module: &Path) -> Result<()> {
    absolute(module)?;
    if module.file_name().is_none_or(|name| name != "wavexlr")
        || prefix.starts_with(module)
        || module.starts_with(prefix.join("bin"))
        || module.starts_with(prefix.join("libexec"))
    {
        return Err(invalid("Invalid legacy OpenWave module boundary"));
    }
    Ok(())
}
fn shape(snapshot: &InstallationSnapshot) -> Result<()> {
    if snapshot.method != InstallMethod::Manual {
        return Err(invalid(
            "Only manual installation snapshots authorize removal",
        ));
    }
    absolute(&snapshot.prefix)?;
    sorted_paths(&snapshot.files)?;
    sorted_paths(&snapshot.directories)?;
    match (
        snapshot.format,
        snapshot.module_dir.as_deref(),
        snapshot.receipt.as_deref(),
    ) {
        (InstallationFormat::RustV2, None, Some(receipt))
            if receipt == snapshot.prefix.join(RECEIPT) =>
        {
            if snapshot.files != native_files(&snapshot.prefix) {
                return Err(invalid(
                    "Native inventory differs from the fixed complete payload",
                ));
            }
        }
        (InstallationFormat::PythonV1, Some(module), Some(receipt))
            if receipt == snapshot.prefix.join(RECEIPT) =>
        {
            module_layout(&snapshot.prefix, module)?;
            let required = [
                module.join("__init__.py"),
                module.join("__main__.py"),
                module.join("install-location.json"),
                receipt.to_owned(),
                snapshot.prefix.join("bin/openwave"),
            ];
            let allowed = legacy_files(&snapshot.prefix, module);
            if !required.iter().all(|p| snapshot.files.contains(p))
                || !snapshot.files.iter().all(|p| allowed.contains(p))
            {
                return Err(invalid("Invalid v1 historical inventory"));
            }
        }
        (InstallationFormat::PythonLegacy, Some(module), None) => {
            module_layout(&snapshot.prefix, module)?;
            let allowed = legacy_files(&snapshot.prefix, module);
            if ![
                module.join("__init__.py"),
                module.join("__main__.py"),
                snapshot.prefix.join("bin/openwave"),
            ]
            .iter()
            .all(|p| snapshot.files.contains(p))
                || !snapshot.files.iter().all(|p| allowed.contains(p))
                || snapshot
                    .files
                    .contains(&module.join("install-location.json"))
                || snapshot.files.contains(&snapshot.prefix.join(RECEIPT))
            {
                return Err(invalid("Invalid receipt-less historical inventory"));
            }
        }
        _ => {
            return Err(invalid(
                "Installation format/module/receipt identity mismatch",
            ));
        }
    }
    if snapshot.directories
        != private_directories(
            &snapshot.files,
            &snapshot.prefix,
            snapshot.module_dir.as_deref(),
        )
    {
        return Err(invalid(
            "Inventory contains missing or shared directory roots",
        ));
    }
    if snapshot.identities.len() != snapshot.files.len()
        || snapshot
            .identities
            .iter()
            .zip(&snapshot.files)
            .any(|((path, digest), expected)| path != expected || !digest_valid(digest))
    {
        return Err(invalid("Incomplete or malformed accepted file identities"));
    }
    Ok(())
}
fn relocate(path: &Path, recorded: &Path, actual: &Path) -> Result<PathBuf> {
    absolute(path)?;
    if let Ok(relative) = path.strip_prefix(recorded) {
        return Ok(actual.join(relative));
    }
    if recorded == actual {
        return Ok(path.to_owned());
    }
    // Split-prefix v1 installs can move only together under a complete DESTDIR.
    let suffix = recorded
        .strip_prefix("/")
        .map_err(|_| invalid("Invalid recorded prefix"))?;
    let mut stage = actual;
    for part in suffix.components().rev() {
        if stage.file_name() != Some(part.as_os_str()) {
            return Err(invalid("Cannot relocate split-prefix legacy installation"));
        }
        stage = stage
            .parent()
            .ok_or_else(|| invalid("Cannot determine legacy staging root"))?;
    }
    Ok(stage.join(
        path.strip_prefix("/")
            .map_err(|_| invalid("Invalid legacy path"))?,
    ))
}
fn decode_receipt(
    bytes: &[u8],
    actual_prefix: &Path,
    expected_module: Option<&Path>,
) -> Result<(InstallMethod, Option<InstallationSnapshot>)> {
    absolute(actual_prefix)?;
    let value = parse_metadata(bytes)?;
    let schema = value
        .get("schema")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| invalid("Invalid receipt schema"))?;
    if !matches!(schema, 1 | 2)
        || value.get("application").and_then(|v| v.as_str()) != Some("openwave")
    {
        return Err(invalid("Unrecognized installation receipt"));
    }
    if schema == 2 && value.get("module_dir").is_some() {
        return Err(invalid("Native receipt must not contain module_dir"));
    }
    let receipt: Receipt = serde_json::from_value(value)?;
    absolute(&receipt.prefix)?;
    if manager(receipt.method) {
        return Ok((receipt.method, None));
    }
    if receipt.method != InstallMethod::Manual {
        return Err(invalid("Unrecognized installation ownership"));
    }
    sorted_paths(&receipt.files)?;
    sorted_paths(&receipt.directories)?;
    let module = match (schema, receipt.module_dir) {
        (1, Some(module)) => {
            module_layout(&receipt.prefix, &module)?;
            Some(relocate(&module, &receipt.prefix, actual_prefix)?)
        }
        (2, None) => None,
        _ => return Err(invalid("Legacy receipt requires module_dir")),
    };
    if expected_module.is_some() && expected_module != module.as_deref() {
        return Err(invalid("Explicit module location disagrees with receipt"));
    }
    let map = |p: &Path| relocate(p, &receipt.prefix, actual_prefix);
    let files = receipt
        .files
        .iter()
        .map(|p| map(p))
        .collect::<Result<Vec<_>>>()?;
    let directories = receipt
        .directories
        .iter()
        .map(|p| map(p))
        .collect::<Result<Vec<_>>>()?;
    let receipt_path = actual_prefix.join(RECEIPT);
    let mut identities: BTreeMap<_, _> = receipt
        .sha256
        .into_iter()
        .map(|(p, h)| Ok((map(&p)?, h)))
        .collect::<Result<_>>()?;
    if identities.contains_key(&receipt_path) || identities.len() + 1 != files.len() {
        return Err(invalid("Receipt identities do not match inventory"));
    }
    identities.insert(receipt_path.clone(), digest_hex(Sha256::digest(bytes)));
    let snapshot = InstallationSnapshot {
        format: if schema == 2 {
            InstallationFormat::RustV2
        } else {
            InstallationFormat::PythonV1
        },
        method: receipt.method,
        prefix: actual_prefix.to_owned(),
        module_dir: module,
        receipt: Some(receipt_path),
        files,
        directories,
        identities: identities.into_iter().collect(),
    };
    shape(&snapshot)?;
    Ok((receipt.method, Some(snapshot)))
}

/// Parse an untrusted staged receipt as source data, never deletion authority.
/// Unlike inspection, payload installation must not relocate its runtime prefix.
pub(super) fn install_receipt(bytes: &[u8], prefix: &Path) -> Result<InstallationSnapshot> {
    let value = parse_metadata(bytes)?;
    if value.get("schema").and_then(|v| v.as_u64()) != Some(2)
        || value.get("prefix").and_then(|v| v.as_str()) != prefix.to_str()
        || value.get("method").and_then(|v| v.as_str()) != Some("manual")
    {
        return Err(invalid(
            "Staged payload requires a schema 2 manual receipt for the exact destination prefix",
        ));
    }
    let (_, snapshot) = decode_receipt(bytes, prefix, None)?;
    snapshot.ok_or_else(|| invalid("Staged payload cannot have package-manager ownership"))
}
fn existing_hashes_checked(
    snapshot: &InstallationSnapshot,
    check_cancel: &impl Fn() -> Result<()>,
) -> Result<()> {
    for (path, digest) in &snapshot.identities {
        check_cancel()?;
        if let Some(mut file) = regular(path)? {
            if super::io::digest_open_cancellable(&mut file, check_cancel)? != *digest {
                return Err(invalid(format!(
                    "Installed file changed: {}",
                    path.display()
                )));
            }
        }
    }
    for path in &snapshot.directories {
        directory(path)?;
    }
    Ok(())
}
pub fn validate_installation(snapshot: &InstallationSnapshot) -> Result<()> {
    validate_installation_checked(snapshot, &|| Ok(()))
}
fn validate_installation_checked(
    snapshot: &InstallationSnapshot,
    check_cancel: &impl Fn() -> Result<()>,
) -> Result<()> {
    check_cancel()?;
    shape(snapshot)?;
    if package_owner(&snapshot.files)?.is_some() {
        return Err(invalid("A package manager owns installation files"));
    }
    let current_receipt = snapshot.prefix.join(RECEIPT);
    if regular(&current_receipt)?.is_some() {
        if snapshot.format == InstallationFormat::PythonLegacy {
            return Err(invalid("A new receipt blocks receipt-less legacy recovery"));
        }
        let (_, current) = decode_receipt(
            &read_bytes(&current_receipt)?,
            &snapshot.prefix,
            snapshot.module_dir.as_deref(),
        )?;
        if current.as_ref() != Some(snapshot) {
            return Err(invalid(
                "Receipt changed; the accepted removal plan cannot authorize this installation",
            ));
        }
    }
    if snapshot.format == InstallationFormat::PythonLegacy {
        if let Some(module) = &snapshot.module_dir {
            if regular(&module.join("install-location.json"))?.is_some() {
                return Err(invalid(
                    "Legacy recovery cannot override a new module locator",
                ));
            }
        }
    }
    existing_hashes_checked(snapshot, check_cancel)
}

pub fn inspect(paths: &crate::paths::RuntimePaths) -> Installation {
    let result = (|| {
        if let Some(owner) = package_owner(&[paths.executable.clone(), paths.identity.clone()])? {
            return Ok(installation(owner, paths.identity.clone(), None));
        }
        if let Some(source) = &paths.source {
            absolute(source)?;
            directory(source)?;
            if regular(&source.join("Cargo.toml"))?.is_none()
                || regular(&source.join("crates/openwave-runtime/Cargo.toml"))?.is_none()
            {
                return Err(invalid("Source checkout witnesses are missing"));
            }
            return Ok(installation(InstallMethod::Source, source.clone(), None));
        }
        inspect_prefix(
            paths
                .prefix
                .as_deref()
                .ok_or_else(|| invalid("Installation prefix is unknown"))?,
            None,
        )
    })();
    result.unwrap_or_else(|error: openwave_core::model::OperationError| Installation {
        error: Some(error.to_string()),
        ..installation(InstallMethod::Unknown, paths.identity.clone(), None)
    })
}
pub fn inspect_prefix(prefix: &Path, module_dir: Option<&Path>) -> Result<Installation> {
    absolute(prefix)?;
    if let Some(module) = module_dir {
        module_layout(prefix, module)?;
    }
    let receipt = prefix.join(RECEIPT);
    let mut probes = vec![
        prefix.join("bin/openwave"),
        receipt.clone(),
        prefix.join("libexec/openwave-maintenance"),
    ];
    if let Some(module) = module_dir {
        probes.extend([
            module.join("__init__.py"),
            module.join("__main__.py"),
            module.join("install-location.json"),
        ]);
    }
    if let Some(owner) = package_owner(&probes)? {
        return Ok(installation(
            owner,
            module_dir
                .map(Path::to_owned)
                .unwrap_or_else(|| prefix.join("share/openwave")),
            None,
        ));
    }
    if regular(&receipt)?.is_some() {
        let (method, snapshot) = decode_receipt(&read_bytes(&receipt)?, prefix, module_dir)?;
        let identity = snapshot
            .as_ref()
            .and_then(|s| s.module_dir.clone())
            .unwrap_or_else(|| prefix.join("share/openwave"));
        if let Some(snapshot) = &snapshot {
            validate_installation(snapshot)?;
        }
        return Ok(installation(method, identity, snapshot));
    }
    inspect_legacy(prefix, module_dir)
}
fn python_name(text: &str) -> bool {
    let name = Path::new(text)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("");
    name == "python"
        || name == "python3"
        || name
            .strip_prefix("python3.")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}
fn wrapper_module(prefix: &Path, text: &str) -> Result<Option<PathBuf>> {
    let lines: Vec<_> = text.lines().collect();
    if lines.len() == 2 && lines[0] == "#!/bin/sh" {
        let interpreter = lines[1]
            .strip_prefix("exec ")
            .and_then(|s| s.strip_suffix(" -m wavexlr \"$@\""));
        if interpreter.is_some_and(|s| {
            python_name(s)
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
        }) {
            return Ok(None);
        }
    }
    if lines.len() == 4
        && lines[0] == "#!/bin/sh"
        && lines[1] == "prefix=$(CDPATH= cd -- \"$(dirname -- \"$0\")/..\" && pwd)"
    {
        let interpreter = lines[3]
            .strip_prefix("exec \"")
            .and_then(|s| s.strip_suffix("\" -m wavexlr \"$@\""));
        if !interpreter.is_some_and(|s| python_name(s) && !s.contains(['$', '`', '\\', '"'])) {
            return Err(invalid("Unrecognized legacy interpreter wrapper"));
        }
        let site = lines[2]
            .strip_prefix("export PYTHONPATH=\"")
            .and_then(|s| s.strip_suffix("${PYTHONPATH:+:$PYTHONPATH}\""))
            .ok_or_else(|| invalid("Unrecognized legacy PYTHONPATH wrapper"))?;
        let module = if let Some(relative) = site.strip_prefix("$prefix/") {
            if relative.is_empty()
                || relative.contains("//")
                || relative.contains(['$', '`', '\\', '"'])
                || !Path::new(relative)
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)))
            {
                return Err(invalid("Unsafe legacy relative module path"));
            }
            prefix.join(relative).join("wavexlr")
        } else if site.starts_with('/') && !site.contains(['$', '`', '\\', '"']) {
            Path::new(site).join("wavexlr")
        } else {
            return Err(invalid("Unrecognized legacy module path"));
        };
        absolute(&module)?;
        return Ok(Some(module));
    }
    Err(invalid(format!(
        "Unrecognized legacy launcher. {MIGRATION}"
    )))
}
fn inspect_legacy(prefix: &Path, explicit: Option<&Path>) -> Result<Installation> {
    let launcher = prefix.join("bin/openwave");
    let text = String::from_utf8(read_bytes(&launcher)?)
        .map_err(|_| invalid("Legacy launcher is not UTF-8"))?;
    let wrapper = wrapper_module(prefix, &text)?;
    let plain_wrapper = wrapper.is_none();
    let module = match (wrapper, explicit) {
        (Some(module), Some(expected)) if module != expected => {
            return Err(invalid("Legacy wrapper and explicit module disagree"));
        }
        (Some(module), _) => module,
        (None, Some(module))
            if module.starts_with(prefix)
                && module
                    .parent()
                    .and_then(Path::file_name)
                    .is_some_and(|name| name == "site-packages" || name == "dist-packages") =>
        {
            module.to_owned()
        }
        _ => {
            return Err(invalid(format!(
                "Legacy module identity is ambiguous. {MIGRATION}"
            )));
        }
    };
    if plain_wrapper {
        let interpreter = text
            .lines()
            .nth(1)
            .and_then(|line| line.strip_prefix("exec "))
            .and_then(|line| line.strip_suffix(" -m wavexlr \"$@\""))
            .ok_or_else(|| invalid("Legacy interpreter token disappeared"))?;
        let interpreter = Path::new(interpreter);
        if interpreter.is_absolute()
            && !interpreter
                .parent()
                .and_then(Path::parent)
                .is_some_and(|root| module.starts_with(root))
        {
            return Err(invalid(format!(
                "External interpreter ownership is not established. {MIGRATION}"
            )));
        }
    }
    module_layout(prefix, &module)?;
    // An explicit module disambiguates location, not multiple coherent wrappers.
    for ancestor in module
        .ancestors()
        .skip(1)
        .filter(|p| *p != prefix && *p != Path::new("/"))
    {
        if let Some(_) = regular(&ancestor.join("bin/openwave"))? {
            if let Ok(bytes) = read_bytes(&ancestor.join("bin/openwave")) {
                if let Ok(other) = std::str::from_utf8(&bytes) {
                    if wrapper_module(ancestor, other).is_ok() {
                        return Err(invalid(format!(
                            "Multiple legacy launcher prefixes. {MIGRATION}"
                        )));
                    }
                }
            }
        }
    }
    if regular(&prefix.join("share/applications/openwave.desktop"))?.is_none()
        || directory(&prefix.join("share/openwave"))?.is_none()
    {
        return Err(invalid("Legacy desktop/data witnesses are missing"));
    }
    if regular(&module.join("install-location.json"))?.is_some() {
        return Err(invalid(
            "Legacy locator exists without its required receipt",
        ));
    }
    let mut files = Vec::new();
    let mut identities = Vec::new();
    for path in legacy_files(prefix, &module) {
        if let Some(mut file) = regular(&path)? {
            identities.push((path.clone(), digest_open(&mut file)?));
            files.push(path);
        }
    }
    let snapshot = InstallationSnapshot {
        format: InstallationFormat::PythonLegacy,
        method: InstallMethod::Manual,
        prefix: prefix.to_owned(),
        module_dir: Some(module.clone()),
        receipt: None,
        directories: private_directories(&files, prefix, Some(&module)),
        files,
        identities,
    };
    validate_installation(&snapshot)?;
    Ok(installation(InstallMethod::Manual, module, Some(snapshot)))
}
fn staged_prefix(prefix: &Path, destdir: Option<&Path>) -> Result<PathBuf> {
    absolute(prefix)?;
    match destdir.filter(|p| !p.as_os_str().is_empty()) {
        Some(stage) => {
            absolute(stage)?;
            Ok(stage.join(
                prefix
                    .strip_prefix("/")
                    .map_err(|_| invalid("Invalid prefix"))?,
            ))
        }
        None => Ok(prefix.to_owned()),
    }
}
pub fn check_install_target(prefix: &Path, destdir: Option<&Path>) -> Result<()> {
    let actual = staged_prefix(prefix, destdir)?;
    let files = native_files(&actual);
    let existing = files
        .iter()
        .filter_map(|path| match regular(path) {
            Ok(Some(_)) => Some(Ok(path.clone())),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<Result<Vec<_>>>()?;
    // Empty package build destinations carry no old ownership to retire. This
    // also permits a fresh Nix /nix/store output and Flatpak /app build tree.
    if !existing.is_empty() {
        if let Some(owner) = package_owner(&existing)? {
            return Err(invalid(format!(
                "Install target is owned by {owner:?}; use its package manager"
            )));
        }
    }
    let accepted = if regular(&actual.join(RECEIPT))?.is_some() {
        let installation = inspect_prefix(&actual, None)?;
        let snapshot = installation
            .snapshot
            .ok_or_else(|| invalid("Cannot overwrite a manager-owned installation"))?;
        if snapshot.format != InstallationFormat::RustV2 {
            return Err(invalid(format!(
                "Retire the validated legacy installation before copying native files. {MIGRATION}"
            )));
        }
        Some(snapshot)
    } else {
        None
    };
    for file in files {
        if regular(&file)?.is_some()
            && accepted
                .as_ref()
                .is_none_or(|snapshot| !snapshot.files.contains(&file))
        {
            return Err(invalid(format!(
                "Unrecorded destination conflict: {}",
                file.display()
            )));
        }
    }
    // Reject the known historical private module tree even if its launcher was
    // removed. External interpreter locations are deliberately not guessed.
    if directory(&actual.join("share/openwave/site-packages/wavexlr"))?.is_some() {
        return Err(invalid(format!(
            "Unresolved legacy module tree. {MIGRATION}"
        )));
    }
    Ok(())
}
pub fn record_install(
    prefix: &Path,
    destdir: Option<&Path>,
    method: InstallMethod,
) -> Result<PathBuf> {
    if method != InstallMethod::Manual && !manager(method) {
        return Err(invalid("Unsupported installation method"));
    }
    let actual = staged_prefix(prefix, destdir)?;
    let files = native_files(prefix);
    let mut hashes = BTreeMap::new();
    for name in NATIVE_PAYLOAD {
        let path = actual.join(name);
        let mut file = regular(&path)?
            .ok_or_else(|| invalid(format!("Missing installed payload: {}", path.display())))?;
        let info = file.metadata()?;
        let executable = name.starts_with("bin/") || name.starts_with("libexec/");
        if info.mode() & 0o7777 != if executable { 0o755 } else { 0o644 } {
            return Err(invalid(format!(
                "Payload has not received final install mode: {}",
                path.display()
            )));
        }
        if executable {
            use std::io::{Read, Seek, SeekFrom};
            let mut magic = [0; 4];
            file.read_exact(&mut magic)?;
            file.seek(SeekFrom::Start(0))?;
            if magic != *b"\x7fELF" {
                return Err(invalid("Native payload binary is not ELF"));
            }
        }
        hashes.insert(prefix.join(name), digest_open(&mut file)?);
    }
    let destination = actual.join(RECEIPT);
    // Refuse adoption of an old format even when a caller skipped preflight.
    if regular(&destination)?.is_some() {
        let old = parse_metadata(&read_bytes(&destination)?)?;
        if old.get("schema").and_then(|v| v.as_u64()) != Some(2)
            || old.get("method").and_then(|v| v.as_str()) != Some("manual")
        {
            return Err(invalid(
                "Existing receipt is not a native manual upgrade target",
            ));
        }
    }
    let receipt = Receipt {
        schema: 2,
        application: "openwave".into(),
        method,
        prefix: prefix.to_owned(),
        module_dir: None,
        directories: private_directories(&files, prefix, None),
        files,
        sha256: hashes,
    };
    let mut bytes = serde_json::to_vec_pretty(&receipt)?;
    bytes.push(b'\n');
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("Receipt has no parent"))?;
    let parent_fd = directory(parent)?.ok_or_else(|| invalid("Receipt directory is missing"))?;
    let temporary_name = format!(".install-manifest-{}", uuid::Uuid::new_v4().simple());
    let fd = rustix::fs::openat(
        &parent_fd,
        temporary_name.as_str(),
        rustix::fs::OFlags::WRONLY
            | rustix::fs::OFlags::CREATE
            | rustix::fs::OFlags::EXCL
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .map_err(errno)?;
    let publication = (|| -> Result<()> {
        let mut temporary = File::from(fd);
        temporary.set_permissions(fs::Permissions::from_mode(0o644))?;
        temporary.write_all(&bytes)?;
        temporary.sync_all()?;
        let reopened = directory(parent)?.ok_or_else(|| invalid("Receipt parent disappeared"))?;
        let old = rustix::fs::fstat(&parent_fd).map_err(errno)?;
        let new = rustix::fs::fstat(&reopened).map_err(errno)?;
        if old.st_dev != new.st_dev || old.st_ino != new.st_ino {
            return Err(invalid("Receipt parent changed before publication"));
        }
        regular_at(&parent_fd, std::ffi::OsStr::new("install-manifest.json"))?;
        rustix::fs::renameat(
            &parent_fd,
            temporary_name.as_str(),
            &parent_fd,
            "install-manifest.json",
        )
        .map_err(errno)?;
        rustix::fs::fsync(&parent_fd).map_err(errno)?;
        Ok(())
    })();
    if publication.is_err() {
        let _ = rustix::fs::unlinkat(
            &parent_fd,
            temporary_name.as_str(),
            rustix::fs::AtFlags::empty(),
        );
    }
    publication?;
    Ok(destination)
}

fn root_authority_boundary(path: &Path, require_file: bool) -> Result<()> {
    if !rustix::process::geteuid().is_root() {
        return Ok(());
    }
    for ancestor in path.ancestors() {
        let metadata = match fs::symlink_metadata(ancestor) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !require_file => continue,
            Err(error) => return Err(error.into()),
        };
        let sticky_root = ancestor != path
            && metadata.is_dir()
            && metadata.uid() == 0
            && metadata.mode() & 0o1000 != 0;
        if metadata.uid() != 0
            || (metadata.mode() & 0o022 != 0 && !sticky_root)
            || metadata.file_type().is_symlink()
            || (!metadata.is_dir() && !(ancestor == path && metadata.is_file()))
        {
            return Err(invalid(format!(
                "Elevated removal requires root-owned, nonwritable authority and ancestry: {}",
                ancestor.display()
            )));
        }
    }
    Ok(())
}

/// Root entrypoints derive their entire inventory here, never from a caller's
/// filename list. Caller must separately prove its own executable/root context.
pub fn snapshot_from_receipt(
    receipt: &Path,
    expected_sha256: &str,
    prefix: &Path,
) -> Result<InstallationSnapshot> {
    absolute(prefix)?;
    if receipt != prefix.join(RECEIPT) || !digest_valid(expected_sha256) {
        return Err(invalid("Receipt authority path/hash mismatch"));
    }
    root_authority_boundary(receipt, true)?;
    let bytes = read_bytes(receipt)?;
    if digest_hex(Sha256::digest(&bytes)) != expected_sha256 {
        return Err(invalid("Accepted receipt digest changed"));
    }
    if package_owner(&[receipt.to_owned(), prefix.join("bin/openwave")])?.is_some() {
        return Err(invalid("Package ownership overrides receipt authority"));
    }
    let (_, snapshot) = decode_receipt(&bytes, prefix, None)?;
    let snapshot =
        snapshot.ok_or_else(|| invalid("Managed receipt cannot grant manual removal authority"))?;
    validate_installation(&snapshot)?;
    Ok(snapshot)
}

fn receipt_unchanged(snapshot: &InstallationSnapshot) -> Result<()> {
    let path = snapshot.prefix.join(RECEIPT);
    if let Some(mut current) = regular(&path)? {
        let accepted = snapshot
            .identities
            .binary_search_by(|(entry, _)| entry.cmp(&path))
            .ok()
            .map(|index| &snapshot.identities[index].1)
            .ok_or_else(|| invalid("A new receipt appeared during legacy removal"))?;
        if digest_open(&mut current)? != *accepted {
            return Err(invalid("Receipt changed during removal"));
        }
    }
    if snapshot.format == InstallationFormat::PythonLegacy {
        if let Some(module) = &snapshot.module_dir {
            if regular(&module.join("install-location.json"))?.is_some() {
                return Err(invalid("A locator appeared during legacy removal"));
            }
        }
    }
    Ok(())
}

/// The caller holds the installation's exclusive lease and has drained workers.
/// An open descriptor pins each parent; a final opened-name identity comparison
/// precedes unlinkat. This is not a root entrypoint accepting arbitrary lists.
pub fn remove_inventory(snapshot: &InstallationSnapshot) -> Result<Vec<PathBuf>> {
    remove_inventory_checked(snapshot, &|| Ok(()))
}

pub fn remove_inventory_cancellable(
    snapshot: &InstallationSnapshot,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<Vec<PathBuf>> {
    remove_inventory_checked(snapshot, &|| {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            Err(openwave_core::model::OperationError::new(
                openwave_core::model::ErrorCode::Cancelled,
                "Inventory removal interrupted; removed files are not rolled back; retain recovery authority",
            ))
        } else {
            Ok(())
        }
    })
}

fn remove_inventory_checked(
    snapshot: &InstallationSnapshot,
    check_cancel: &impl Fn() -> Result<()>,
) -> Result<Vec<PathBuf>> {
    check_cancel()?;
    validate_installation_checked(snapshot, check_cancel)?;
    check_cancel()?;
    for path in &snapshot.files {
        root_authority_boundary(path, false)?;
    }
    let mut removed = Vec::new();
    // Keep the receipt until all payload files have been removed, preserving
    // independently rederivable authority across an ordinary partial failure.
    let mut files: Vec<_> = snapshot.identities.iter().collect();
    files.sort_by_key(|(path, _)| snapshot.receipt.as_ref() == Some(path));
    for (path, expected) in files {
        check_cancel()?;
        let Some(parent) = directory(
            path.parent()
                .ok_or_else(|| invalid("File parent missing"))?,
        )?
        else {
            continue;
        };
        let name = path
            .file_name()
            .ok_or_else(|| invalid("File name missing"))?;
        let Some(mut file) = regular_at(&parent, name)? else {
            continue;
        };
        let before = file.metadata()?;
        if super::io::digest_open_cancellable(&mut file, check_cancel)? != *expected {
            return Err(invalid(format!(
                "File changed before removal: {}",
                path.display()
            )));
        }
        receipt_unchanged(snapshot)?;
        let current =
            regular_at(&parent, name)?.ok_or_else(|| invalid("File disappeared during removal"))?;
        if !same_file(&before, &file.metadata()?) || !same_file(&before, &current.metadata()?) {
            return Err(invalid("File identity changed immediately before unlink"));
        }
        let reopened = directory(path.parent().ok_or_else(|| invalid("Missing parent"))?)?
            .ok_or_else(|| invalid("Parent disappeared during removal"))?;
        let left = rustix::fs::fstat(&parent).map_err(errno)?;
        let right = rustix::fs::fstat(&reopened).map_err(errno)?;
        if left.st_dev != right.st_dev || left.st_ino != right.st_ino {
            return Err(invalid("Parent identity changed before unlink"));
        }
        check_cancel()?;
        rustix::fs::unlinkat(&parent, name, rustix::fs::AtFlags::empty()).map_err(errno)?;
        removed.push(path.clone());
    }
    let mut directories: Vec<_> = snapshot.directories.iter().collect();
    directories.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for path in directories {
        check_cancel()?;
        receipt_unchanged(snapshot)?;
        let Some(parent) = directory(
            path.parent()
                .ok_or_else(|| invalid("Directory parent missing"))?,
        )?
        else {
            continue;
        };
        let Some(target) = directory(path)? else {
            continue;
        };
        let name = path
            .file_name()
            .ok_or_else(|| invalid("Directory name missing"))?;
        let reopened = rustix::fs::openat(
            &parent,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )
        .map_err(errno)?;
        let before = rustix::fs::fstat(&target).map_err(errno)?;
        let now = rustix::fs::fstat(&reopened).map_err(errno)?;
        if before.st_dev != now.st_dev || before.st_ino != now.st_ino {
            return Err(invalid("Directory changed before removal"));
        }
        check_cancel()?;
        match rustix::fs::unlinkat(&parent, name, rustix::fs::AtFlags::REMOVEDIR) {
            Ok(()) => removed.push(path.clone()),
            Err(
                rustix::io::Errno::NOTEMPTY | rustix::io::Errno::EXIST | rustix::io::Errno::NOENT,
            ) => (),
            Err(error) => return Err(errno(error)),
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[test]
    fn interruption_after_first_unlink_retains_authority_and_retry_finishes() {
        let temporary = tempfile::tempdir().unwrap();
        let prefix = temporary.path().join("installation");
        for relative in NATIVE_PAYLOAD {
            let path = prefix.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                if relative.starts_with("bin/") || relative.starts_with("libexec/") {
                    b"\x7fELFinert fixture".as_slice()
                } else {
                    b"inert asset".as_slice()
                },
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                &path,
                std::fs::Permissions::from_mode(
                    if relative.starts_with("bin/") || relative.starts_with("libexec/") {
                        0o755
                    } else {
                        0o644
                    },
                ),
            )
            .unwrap();
        }
        record_install(&prefix, None, InstallMethod::Manual).unwrap();
        let snapshot = inspect_prefix(&prefix, None).unwrap().snapshot.unwrap();
        let first = snapshot
            .identities
            .iter()
            .find(|(p, _)| Some(p) != snapshot.receipt.as_ref())
            .unwrap()
            .0
            .clone();
        let interruption = remove_inventory_checked(&snapshot, &|| {
            if !first.exists() {
                return Err(openwave_core::model::OperationError::new(
                    openwave_core::model::ErrorCode::Cancelled,
                    "fixture interrupted after unlink",
                ));
            }
            Ok(())
        })
        .unwrap_err();
        assert_eq!(
            interruption.code,
            openwave_core::model::ErrorCode::Cancelled
        );
        assert!(!first.exists());
        assert!(snapshot.receipt.as_ref().unwrap().exists());
        assert!(
            snapshot
                .files
                .iter()
                .filter(|p| *p != &first)
                .all(|p| p.exists())
        );
        remove_inventory(&snapshot).unwrap();
        assert!(snapshot.files.iter().all(|p| !p.exists()));
    }
}
