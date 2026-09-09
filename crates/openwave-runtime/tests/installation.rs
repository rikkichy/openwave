use openwave_runtime::{
    installation::{self, InstallMethod, InstallationFormat, InstallationSnapshot, NATIVE_PAYLOAD},
    paths::RuntimePaths,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};

fn put(path: &Path, bytes: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
fn payload(prefix: &Path) {
    for relative in NATIVE_PAYLOAD {
        let executable = relative.starts_with("bin/") || relative.starts_with("libexec/");
        put(
            &prefix.join(relative),
            if executable {
                b"\x7fELFowned native fixture"
            } else {
                b"owned asset fixture"
            },
            if executable { 0o755 } else { 0o644 },
        );
    }
}
struct Native {
    _temporary: tempfile::TempDir,
    stage: PathBuf,
    prefix: PathBuf,
    runtime: PathBuf,
    receipt: PathBuf,
}
impl Native {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let stage = temporary.path().join("stage");
        let runtime = PathBuf::from("/opt/openwave-test");
        let prefix = stage.join("opt/openwave-test");
        installation::check_install_target(&runtime, Some(&stage)).unwrap();
        payload(&prefix);
        let receipt =
            installation::record_install(&runtime, Some(&stage), InstallMethod::Manual).unwrap();
        Self {
            _temporary: temporary,
            stage,
            prefix,
            runtime,
            receipt,
        }
    }
    fn snapshot(&self) -> InstallationSnapshot {
        installation::inspect_prefix(&self.prefix, None)
            .unwrap()
            .snapshot
            .unwrap()
    }
    fn rewrite(&self, change: impl FnOnce(&mut Value)) {
        let mut value: Value = serde_json::from_slice(&fs::read(&self.receipt).unwrap()).unwrap();
        change(&mut value);
        fs::write(&self.receipt, serde_json::to_vec(&value).unwrap()).unwrap();
    }
}

#[test]
fn native_stage_records_runtime_paths_and_moves_as_one_tree() {
    let fixture = Native::new();
    let raw = fs::read_to_string(&fixture.receipt).unwrap();
    assert!(!raw.contains(fixture.stage.to_str().unwrap()));
    let value: Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(value["prefix"], fixture.runtime.to_str().unwrap());
    assert!(value.get("module_dir").is_none());
    let moved = fixture._temporary.path().join("moved");
    fs::rename(&fixture.stage, &moved).unwrap();
    let prefix = moved.join("opt/openwave-test");
    let inspected = installation::inspect_prefix(&prefix, None).unwrap();
    assert_eq!(inspected.canonical_identity, prefix.join("share/openwave"));
    let snapshot = inspected.snapshot.unwrap();
    assert!(snapshot.files.iter().all(|path| path.starts_with(&prefix)));
    assert_eq!(snapshot.format, InstallationFormat::RustV2);
    installation::validate_installation(&snapshot).unwrap();
    assert!(!snapshot.directories.contains(&prefix.join("bin")));
    assert!(!snapshot.directories.contains(&prefix.join("libexec")));
}

#[test]
fn accepted_missing_files_and_receipt_allow_bounded_retry_only() {
    let fixture = Native::new();
    let accepted = fixture.snapshot();
    fs::remove_file(fixture.prefix.join("bin/openwave-diag")).unwrap();
    assert_eq!(fixture.snapshot(), accepted);
    fs::remove_file(&fixture.receipt).unwrap();
    installation::validate_installation(&accepted).unwrap();
    let unrelated = fixture.prefix.join("share/openwave/my-private-data");
    put(&unrelated, b"preserve", 0o600);
    installation::remove_inventory(&accepted).unwrap();
    assert_eq!(fs::read(&unrelated).unwrap(), b"preserve");
    assert!(fixture.prefix.join("bin").is_dir());
    assert!(fixture.prefix.join("libexec").is_dir());
    assert!(fixture.prefix.join("share/openwave").is_dir());
}

#[test]
fn cancelled_inventory_never_mutates_and_can_be_retried() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let fixture = Native::new();
    let accepted = fixture.snapshot();
    let cancelled = AtomicBool::new(true);
    let error = installation::remove_inventory_cancellable(&accepted, &cancelled).unwrap_err();
    assert_eq!(error.code, openwave_core::model::ErrorCode::Cancelled);
    assert!(accepted.files.iter().all(|path| path.exists()));
    cancelled.store(false, Ordering::Release);
    installation::remove_inventory_cancellable(&accepted, &cancelled).unwrap();
    assert!(accepted.files.iter().all(|path| !path.exists()));
}

#[test]
fn changed_file_or_receipt_cannot_expand_an_accepted_plan() {
    let fixture = Native::new();
    let accepted = fixture.snapshot();
    let target = fixture.prefix.join("bin/openwave-diag");
    fs::write(&target, b"new file").unwrap();
    assert!(installation::validate_installation(&accepted).is_err());
    fixture.rewrite(|value| {
        value["sha256"]["/opt/openwave-test/bin/openwave-diag"] =
            installation::file_digest(&target).unwrap().into();
    });
    assert!(installation::validate_installation(&accepted).is_err());
    let changed = fixture.snapshot();
    assert_ne!(changed, accepted);
    assert!(installation::remove_inventory(&accepted).is_err());
    assert_eq!(fs::read(&target).unwrap(), b"new file");
}

#[test]
fn nofollow_rejects_file_directory_and_dangling_symlink_boundaries() {
    let fixture = Native::new();
    let accepted = fixture.snapshot();
    let target = fixture.prefix.join("bin/openwave-diag");
    let outside = fixture._temporary.path().join("outside");
    fs::rename(&target, &outside).unwrap();
    symlink(&outside, &target).unwrap();
    assert!(installation::validate_installation(&accepted).is_err());
    assert!(installation::remove_inventory(&accepted).is_err());
    assert!(outside.is_file());
    fs::remove_file(&target).unwrap();
    fs::rename(&outside, &target).unwrap();
    let icons = fixture.prefix.join("share/openwave/icons");
    fs::rename(&icons, &outside).unwrap();
    symlink(&outside, &icons).unwrap();
    assert!(installation::validate_installation(&accepted).is_err());
    fs::remove_file(&icons).unwrap();
    fs::rename(&outside, &icons).unwrap();
    fs::remove_file(&target).unwrap();
    symlink(fixture._temporary.path().join("absent"), &target).unwrap();
    assert!(installation::validate_installation(&accepted).is_err());
}

#[test]
fn removal_preserves_unrecorded_siblings_and_shared_roots() {
    let fixture = Native::new();
    let accepted = fixture.snapshot();
    let sibling = fixture.prefix.join("bin/not-openwave");
    let unknown = fixture.prefix.join("share/openwave/icons/custom.svg");
    put(&sibling, b"sibling", 0o755);
    put(&unknown, b"private icon", 0o644);
    installation::remove_inventory(&accepted).unwrap();
    assert_eq!(fs::read(&sibling).unwrap(), b"sibling");
    assert_eq!(fs::read(&unknown).unwrap(), b"private icon");
    assert!(!fixture.receipt.exists());
    assert!(!fixture.prefix.join("bin/openwave").exists());
    assert!(
        fixture
            .prefix
            .join("share/icons/hicolor/scalable/apps")
            .is_dir()
    );
}

#[test]
fn metadata_rejects_duplicate_keys_at_every_depth_and_oversize_input() {
    let fixture = Native::new();
    for malformed in [
        "{broken",
        "[]",
        "{\"schema\":2,\"schema\":2}",
        "{\"outer\":{\"same\":1,\"same\":2}}",
        "{\"outer\":[{\"same\":1,\"same\":2}]}",
    ] {
        fs::write(&fixture.receipt, malformed).unwrap();
        assert!(installation::read_metadata(&fixture.receipt).is_err());
        assert!(installation::inspect_prefix(&fixture.prefix, None).is_err());
    }
    fs::write(&fixture.receipt, vec![b' '; 4 * 1024 * 1024 + 1]).unwrap();
    assert!(installation::read_metadata(&fixture.receipt).is_err());
}

#[test]
fn malformed_paths_shared_roots_and_extra_native_keys_are_not_authority() {
    let fixture = Native::new();
    let original = fs::read(&fixture.receipt).unwrap();
    for (key, value) in [
        ("files", json!(["/etc/passwd"])),
        ("files", json!(["/opt/openwave-test/bin/../bin/openwave"])),
        ("directories", json!(["/opt/openwave-test/bin"])),
        ("module_dir", Value::Null),
    ] {
        fs::write(&fixture.receipt, &original).unwrap();
        fixture.rewrite(|metadata| metadata[key] = value);
        assert!(installation::inspect_prefix(&fixture.prefix, None).is_err());
    }
}

#[test]
fn pre_copy_check_refuses_unknown_and_legacy_conflicts_but_allows_verified_native_upgrade() {
    let temporary = tempfile::tempdir().unwrap();
    let prefix = temporary.path().join("prefix");
    installation::check_install_target(&prefix, None).unwrap();
    let launcher = prefix.join("bin/openwave");
    put(&launcher, b"unrecorded content", 0o755);
    assert!(installation::check_install_target(&prefix, None).is_err());
    assert_eq!(fs::read(&launcher).unwrap(), b"unrecorded content");
    let fixture = Native::new();
    installation::check_install_target(&fixture.runtime, Some(&fixture.stage)).unwrap();
    fs::write(fixture.prefix.join("share/openwave/VERSION"), b"changed").unwrap();
    assert!(installation::check_install_target(&fixture.runtime, Some(&fixture.stage)).is_err());
}

#[test]
fn managed_receipt_outranks_wrapped_hashes_and_never_exposes_deletion_inventory() {
    let fixture = Native::new();
    fixture.rewrite(|value| value["method"] = "deb".into());
    fs::write(fixture.prefix.join("bin/openwave"), b"manager wrapper").unwrap();
    let inspected = installation::inspect_prefix(&fixture.prefix, None).unwrap();
    assert_eq!(inspected.method, InstallMethod::Deb);
    assert!(inspected.snapshot.is_none());
    assert!(installation::check_install_target(&fixture.runtime, Some(&fixture.stage)).is_err());
    assert!(
        installation::snapshot_from_receipt(
            &fixture.receipt,
            &installation::file_digest(&fixture.receipt).unwrap(),
            &fixture.prefix
        )
        .is_err()
    );
    assert_eq!(
        installation::package_owner(&[PathBuf::from(
            "/nix/store/not-an-installed-fixture/bin/openwave"
        )])
        .unwrap(),
        Some(InstallMethod::Nix)
    );
}

#[test]
fn source_checkout_has_no_removal_inventory() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source");
    put(&source.join("Cargo.toml"), b"", 0o644);
    put(
        &source.join("crates/openwave-runtime/Cargo.toml"),
        b"",
        0o644,
    );
    let paths = RuntimePaths {
        executable: source.join("target/debug/openwave"),
        prefix: None,
        data: source.clone(),
        identity: source.clone(),
        maintenance: source.join("target/debug/openwave-maintenance"),
        source: Some(source),
    };
    let inspected = installation::inspect(&paths);
    assert_eq!(inspected.method, InstallMethod::Source);
    assert!(inspected.snapshot.is_none());
}

fn private_dirs(files: &[PathBuf], prefix: &Path, module: &Path) -> Vec<PathBuf> {
    let roots = [
        prefix.join("share/openwave"),
        prefix.join("share/doc/openwave"),
        prefix.join("share/licenses/openwave"),
        module.to_owned(),
    ];
    let mut dirs: BTreeSet<_> = roots.iter().cloned().collect();
    for path in files {
        for parent in path.ancestors().skip(1) {
            if roots.iter().any(|root| parent.starts_with(root)) {
                dirs.insert(parent.to_owned());
            }
        }
    }
    dirs.into_iter().collect()
}
fn legacy(prefix: &Path, module: &Path) {
    put(
        &prefix.join("bin/openwave"),
        b"#!/bin/sh\nexec python3 -m wavexlr \"$@\"\n",
        0o755,
    );
    put(
        &module.join("__init__.py"),
        b"# inert legacy fixture",
        0o644,
    );
    put(&module.join("__main__.py"), b"# never executed", 0o644);
    put(
        &prefix.join("share/applications/openwave.desktop"),
        b"[Desktop Entry]\nName=OpenWave\n",
        0o644,
    );
    fs::create_dir_all(prefix.join("share/openwave")).unwrap();
}

#[test]
fn explicit_legacy_identity_retains_historical_allowlist_without_adopting_unknown_files() {
    let temporary = tempfile::tempdir().unwrap();
    let prefix = temporary.path().join("legacy");
    let module = prefix.join("lib/python3.13/site-packages/wavexlr");
    legacy(&prefix, &module);
    let historic = prefix.join("share/icons/hicolor/symbolic/apps/openwave-muted-symbolic.svg");
    let unknown = module.join("unrelated.py");
    put(&historic, b"historical icon", 0o644);
    put(&unknown, b"unrelated", 0o644);
    let accepted = installation::inspect_prefix(&prefix, Some(&module))
        .unwrap()
        .snapshot
        .unwrap();
    assert_eq!(accepted.format, InstallationFormat::PythonLegacy);
    assert!(accepted.files.contains(&historic));
    assert!(!accepted.files.contains(&unknown));
    fs::remove_file(prefix.join("bin/openwave")).unwrap();
    installation::validate_installation(&accepted).unwrap();
    installation::remove_inventory(&accepted).unwrap();
    assert_eq!(fs::read(unknown).unwrap(), b"unrelated");
}

#[test]
fn external_interpreter_global_layout_requires_confirmed_old_uninstaller_guidance() {
    let temporary = tempfile::tempdir().unwrap();
    let prefix = temporary.path().join("legacy");
    let module = temporary.path().join("external/site-packages/wavexlr");
    legacy(&prefix, &module);
    let error = installation::inspect_prefix(&prefix, Some(&module))
        .unwrap_err()
        .to_string();
    assert!(error.contains("confirmed uninstaller"));
    assert!(module.join("__main__.py").exists());
}

#[test]
fn v1_external_module_moves_with_destdir_and_new_v2_blocks_old_snapshot() {
    let temporary = tempfile::tempdir().unwrap();
    let stage = temporary.path().join("stage");
    let runtime = Path::new("/opt/openwave-test");
    let runtime_module = Path::new("/opt/python-test/site-packages/wavexlr");
    let prefix = stage.join("opt/openwave-test");
    let module = stage.join("opt/python-test/site-packages/wavexlr");
    legacy(&prefix, &module);
    let receipt = runtime.join("share/openwave/install-manifest.json");
    let location = runtime_module.join("install-location.json");
    put(
        &stage.join(location.strip_prefix("/").unwrap()),
        b"{\"schema\":1,\"application\":\"openwave\",\"method\":\"manual\"}",
        0o644,
    );
    let mut files = vec![
        runtime.join("bin/openwave"),
        runtime.join("share/applications/openwave.desktop"),
        runtime_module.join("__init__.py"),
        runtime_module.join("__main__.py"),
        location,
        receipt.clone(),
    ];
    files.sort();
    let hashes: BTreeMap<_, _> = files
        .iter()
        .filter(|p| **p != receipt)
        .map(|path| {
            (
                path.to_str().unwrap().to_owned(),
                installation::file_digest(&stage.join(path.strip_prefix("/").unwrap())).unwrap(),
            )
        })
        .collect();
    let value = json!({"schema":1,"application":"openwave","method":"manual","prefix":runtime,"module_dir":runtime_module,"files":files,"directories":private_dirs(&files, runtime, runtime_module),"sha256":hashes});
    let actual_receipt = prefix.join("share/openwave/install-manifest.json");
    put(&actual_receipt, &serde_json::to_vec(&value).unwrap(), 0o644);
    let accepted = installation::inspect_prefix(&prefix, Some(&module))
        .unwrap()
        .snapshot
        .unwrap();
    assert_eq!(accepted.format, InstallationFormat::PythonV1);
    assert_eq!(accepted.module_dir.as_deref(), Some(module.as_path()));
    assert!(accepted.files.iter().all(|path| path.starts_with(&stage)));
    assert!(installation::check_install_target(runtime, Some(&stage)).is_err());
    fs::remove_file(&actual_receipt).unwrap();
    installation::validate_installation(&accepted).unwrap();
    payload(&prefix);
    installation::record_install(runtime, Some(&stage), InstallMethod::Manual).unwrap();
    assert!(installation::validate_installation(&accepted).is_err());
    assert!(installation::remove_inventory(&accepted).is_err());
    assert!(module.join("__main__.py").exists());
}

#[test]
fn privileged_authority_is_derived_from_exact_receipt_hash_not_a_filename_list() {
    let fixture = Native::new();
    let accepted = fixture.snapshot();
    let hash = installation::file_digest(&fixture.receipt).unwrap();
    assert_eq!(
        installation::snapshot_from_receipt(&fixture.receipt, &hash, &fixture.prefix).unwrap(),
        accepted
    );
    assert!(
        installation::snapshot_from_receipt(&fixture.receipt, &"0".repeat(64), &fixture.prefix)
            .is_err()
    );
    assert!(
        installation::snapshot_from_receipt(&fixture.receipt, &hash, fixture._temporary.path())
            .is_err()
    );
    let mut expanded = accepted.clone();
    expanded.files.push(fixture.prefix.join("bin/unrelated"));
    expanded.files.sort();
    assert!(installation::validate_installation(&expanded).is_err());
    fs::write(&fixture.receipt, b"{}").unwrap();
    assert!(installation::snapshot_from_receipt(&fixture.receipt, &hash, &fixture.prefix).is_err());
}

#[test]
fn special_files_are_never_read_as_inventory_payload() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("directory");
    fs::create_dir(&directory).unwrap();
    assert!(installation::file_digest(&directory).is_err());
    let socket = temporary.path().join("socket");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    assert!(installation::file_digest(&socket).is_err());
}

#[test]
fn package_queries_fail_closed_and_elevated_queries_ignore_untrusted_path() {
    let fixture = Native::new();
    fs::write(
        fixture.prefix.join("bin/openwave"),
        b"package wrapper changed after install",
    )
    .unwrap();
    let tools = fixture._temporary.path().join("tools");
    let marker = fixture._temporary.path().join("queried");
    let query = tools.join("dpkg-query");
    for scenario in ["owned", "broken"] {
        let response = if scenario == "owned" {
            "printf 'openwave: owned fixture\\n'; exit 0"
        } else {
            "printf 'database is corrupt\\n' >&2; exit 2"
        };
        put(
            &query,
            format!(
                "#!/bin/sh\nprintf called > '{}'\n{response}\n",
                marker.display()
            )
            .as_bytes(),
            0o755,
        );
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "package_query_child", "--nocapture"])
            .env("OPENWAVE_INVENTORY_QUERY_CASE", scenario)
            .env("OPENWAVE_INVENTORY_PREFIX", &fixture.prefix)
            .env("OPENWAVE_INVENTORY_QUERY_MARKER", &marker)
            .env("PATH", &tools)
            .env_remove("FLATPAK_ID")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn package_query_child() {
    let Ok(scenario) = std::env::var("OPENWAVE_INVENTORY_QUERY_CASE") else {
        return;
    };
    let prefix = PathBuf::from(std::env::var_os("OPENWAVE_INVENTORY_PREFIX").unwrap());
    let result = installation::inspect_prefix(&prefix, None);
    if rustix::process::geteuid().is_root() {
        let marker = PathBuf::from(std::env::var_os("OPENWAVE_INVENTORY_QUERY_MARKER").unwrap());
        assert!(
            !marker.exists(),
            "Root executed a caller-PATH package query"
        );
    } else if scenario == "owned" {
        let inspected = result.unwrap();
        assert_eq!(inspected.method, InstallMethod::Deb);
        assert!(inspected.snapshot.is_none());
    } else {
        assert!(
            result.is_err(),
            "A broken database was treated as absent ownership"
        );
    }
}

#[test]
fn legacy_wrapper_discovers_only_its_literal_module_location() {
    let temporary = tempfile::tempdir().unwrap();
    let prefix = temporary.path().join("legacy");
    let module = prefix.join("share/openwave/site-packages/wavexlr");
    legacy(&prefix, &module);
    let launcher = prefix.join("bin/openwave");
    let wrapper = "#!/bin/sh\nprefix=$(CDPATH= cd -- \"$(dirname -- \"$0\")/..\" && pwd)\nexport PYTHONPATH=\"$prefix/share/openwave/site-packages${PYTHONPATH:+:$PYTHONPATH}\"\nexec \"python3\" -m wavexlr \"$@\"\n";
    fs::write(&launcher, wrapper).unwrap();
    let inspected = installation::inspect_prefix(&prefix, None).unwrap();
    assert_eq!(inspected.canonical_identity, module);
    assert_eq!(
        inspected.snapshot.unwrap().format,
        InstallationFormat::PythonLegacy
    );
    fs::write(
        &launcher,
        wrapper.replace(
            "$prefix/share/openwave",
            "$prefix/$(untrusted-command)/openwave",
        ),
    )
    .unwrap();
    assert!(installation::inspect_prefix(&prefix, None).is_err());
    assert!(
        prefix
            .join("share/openwave/site-packages/wavexlr/__main__.py")
            .exists()
    );
}

#[test]
fn receipt_writer_requires_complete_native_payload_and_final_modes() {
    let temporary = tempfile::tempdir().unwrap();
    let prefix = temporary.path().join("prefix");
    payload(&prefix);
    let style = prefix.join("share/openwave/style.css");
    fs::remove_file(&style).unwrap();
    assert!(installation::record_install(&prefix, None, InstallMethod::Manual).is_err());
    put(&style, b"style", 0o666);
    assert!(installation::record_install(&prefix, None, InstallMethod::Manual).is_err());
    fs::set_permissions(&style, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(prefix.join("bin/openwave"), b"#!/bin/sh\n").unwrap();
    assert!(installation::record_install(&prefix, None, InstallMethod::Manual).is_err());
    assert!(!prefix.join("share/openwave/install-manifest.json").exists());
}

// These cases deliberately exercise the real root entrypoint, not a bypass of
// its trust checks. Run only in a disposable root/user namespace or container,
// never by elevating the test suite on the host.
struct RootPayload {
    _temporary: tempfile::TempDir,
    prefix: PathBuf,
    stage: PathBuf,
    staged_prefix: PathBuf,
    receipt: PathBuf,
    helper: PathBuf,
}

impl RootPayload {
    fn new() -> Self {
        assert_eq!(
            std::env::var("OPENWAVE_DISPOSABLE_ROOT").as_deref(),
            Ok("1"),
            "requires an explicitly disposable root filesystem"
        );
        assert!(rustix::process::geteuid().is_root());
        let temporary = tempfile::Builder::new()
            .prefix("openwave-install-fixture-")
            .tempdir_in("/")
            .unwrap();
        let prefix = temporary.path().join("prefix");
        let stage = temporary.path().join("stage");
        let staged_prefix = stage.join(prefix.strip_prefix("/").unwrap());
        payload(&staged_prefix);
        put(
            &staged_prefix.join("share/openwave/VERSION"),
            format!("{}\n", openwave_core::VERSION).as_bytes(),
            0o644,
        );
        let receipt =
            installation::record_install(&prefix, Some(&stage), InstallMethod::Manual).unwrap();
        // The untrusted stage is data even when it contains writable ancestry.
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o777)).unwrap();
        let helper = prefix.join("libexec/openwave-bootstrap-fixture/openwave-maintenance");
        fs::create_dir_all(helper.parent().unwrap()).unwrap();
        fs::set_permissions(helper.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::copy(env!("CARGO_BIN_EXE_openwave-maintenance"), &helper).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _temporary: temporary,
            prefix,
            stage,
            staged_prefix,
            receipt,
            helper,
        }
    }

    fn digest(&self) -> String {
        installation::file_digest(&self.receipt).unwrap()
    }

    fn invoke(&self, digest: &str) -> std::process::Output {
        std::process::Command::new(&self.helper)
            .arg("install-payload")
            .arg("--stage")
            .arg(&self.stage)
            .arg("--prefix")
            .arg(&self.prefix)
            .arg("--expected-sha256")
            .arg(digest)
            .env_remove("FLATPAK_ID")
            .env_remove("SNAP")
            .output()
            .unwrap()
    }

    fn reject_without_copying(&self, digest: &str) {
        let output = self.invoke(digest);
        assert!(!output.status.success(), "unexpected install success");
        assert!(
            installation::NATIVE_PAYLOAD
                .iter()
                .all(|file| !self.prefix.join(file).exists()),
            "preflight failure published payload"
        );
        assert!(
            !self
                .prefix
                .join("share/openwave/install-manifest.json")
                .exists(),
            "preflight failure published receipt"
        );
        assert!(self.helper.is_file(), "bootstrap must remain recoverable");
        assert!(self.receipt.is_file(), "stage must remain recoverable");
    }

    fn rewrite(&self, change: impl FnOnce(&mut Value)) {
        let mut value: Value = serde_json::from_slice(&fs::read(&self.receipt).unwrap()).unwrap();
        change(&mut value);
        fs::write(&self.receipt, serde_json::to_vec(&value).unwrap()).unwrap();
    }
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 inside a disposable root filesystem"]
fn root_payload_copies_data_without_executing_it_and_preserves_unrelated_files() {
    let fixture = RootPayload::new();
    let unrelated = fixture.prefix.join("share/openwave/unrecorded-user-file");
    put(&unrelated, b"preserve exactly", 0o600);
    let output = fixture.invoke(&fixture.digest());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Every staged ELF is intentionally invalid beyond its magic. Executing
    // any of them for --version or receipt generation would fail this install.
    for relative in NATIVE_PAYLOAD {
        let installed = fixture.prefix.join(relative);
        assert_eq!(
            fs::read(&installed).unwrap(),
            fs::read(fixture.staged_prefix.join(relative)).unwrap()
        );
        assert_eq!(
            fs::metadata(&installed).unwrap().permissions().mode() & 0o7777,
            if relative.starts_with("bin/") || relative.starts_with("libexec/") {
                0o755
            } else {
                0o644
            }
        );
    }
    for relative in [
        "bin",
        "libexec",
        "share/doc/openwave",
        "share/openwave/icons",
    ] {
        assert_eq!(
            fs::metadata(fixture.prefix.join(relative))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o755
        );
    }
    assert_eq!(fs::read(&unrelated).unwrap(), b"preserve exactly");
    let receipt = fixture.prefix.join("share/openwave/install-manifest.json");
    assert_eq!(
        fs::metadata(&receipt).unwrap().permissions().mode() & 0o7777,
        0o644
    );
    let snapshot = installation::snapshot_from_receipt(
        &receipt,
        &installation::file_digest(&receipt).unwrap(),
        &fixture.prefix,
    )
    .unwrap();
    assert_eq!(snapshot.prefix, fixture.prefix);
    assert!(!snapshot.files.contains(&unrelated));
    // A verified native replacement remains a supported upgrade, not adoption.
    let output = fixture.invoke(&fixture.digest());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(unrelated).unwrap(), b"preserve exactly");
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 inside a disposable root filesystem"]
fn root_payload_rejects_staged_tampering_and_invalid_authority_before_copying() {
    let fixture = RootPayload::new();
    let original_receipt = fs::read(&fixture.receipt).unwrap();
    let accepted = fixture.digest();
    fixture.reject_without_copying(&"0".repeat(64));
    fixture.reject_without_copying(&"A".repeat(64));
    fixture.reject_without_copying("1234");
    let asset = fixture.staged_prefix.join("share/openwave/style.css");
    let original_asset = fs::read(&asset).unwrap();
    fs::write(&asset, b"tampered after acceptance").unwrap();
    fixture.reject_without_copying(&accepted);
    fs::write(&asset, &original_asset).unwrap();
    fs::remove_file(&asset).unwrap();
    symlink(fixture.staged_prefix.join("share/openwave/VERSION"), &asset).unwrap();
    fixture.reject_without_copying(&accepted);
    fs::remove_file(&asset).unwrap();
    let socket = std::os::unix::net::UnixListener::bind(&asset).unwrap();
    fixture.reject_without_copying(&accepted);
    drop(socket);
    fs::remove_file(&asset).unwrap();
    put(&asset, &original_asset, 0o644);
    fs::set_permissions(&asset, fs::Permissions::from_mode(0o666)).unwrap();
    fixture.reject_without_copying(&accepted);
    fs::set_permissions(&asset, fs::Permissions::from_mode(0o644)).unwrap();
    let icons = fixture.staged_prefix.join("share/openwave/icons");
    let moved_icons = fixture.staged_prefix.join("share/openwave/other-icons");
    fs::rename(&icons, &moved_icons).unwrap();
    symlink(&moved_icons, &icons).unwrap();
    fixture.reject_without_copying(&accepted);
    fs::remove_file(&icons).unwrap();
    fs::rename(moved_icons, icons).unwrap();
    fixture.rewrite(|value| value["prefix"] = "/different-prefix".into());
    fixture.reject_without_copying(&accepted);
    fixture.reject_without_copying(&fixture.digest());
    fs::write(&fixture.receipt, &original_receipt).unwrap();
    fixture.rewrite(|value| {
        let extra = fixture
            .prefix
            .join("bin/unrelated")
            .to_string_lossy()
            .into_owned();
        value["files"]
            .as_array_mut()
            .unwrap()
            .push(extra.clone().into());
        value["files"]
            .as_array_mut()
            .unwrap()
            .sort_by_key(|v| v.as_str().unwrap().to_owned());
        value["sha256"][extra] = "0".repeat(64).into();
    });
    fixture.reject_without_copying(&fixture.digest());
    fs::write(&fixture.receipt, &original_receipt).unwrap();
    fixture.rewrite(|value| value["directories"] = json!([fixture.prefix.join("bin")]));
    fixture.reject_without_copying(&fixture.digest());
    fs::write(&fixture.receipt, &original_receipt).unwrap();
    fixture.rewrite(|value| value["method"] = "deb".into());
    fixture.reject_without_copying(&fixture.digest());
    fs::write(&fixture.receipt, &original_receipt).unwrap();
    let text = String::from_utf8(original_receipt.clone()).unwrap();
    fs::write(&fixture.receipt, text.replacen('{', "{\"schema\":2,", 1)).unwrap();
    fixture.reject_without_copying(&fixture.digest());
    fs::write(&fixture.receipt, &original_receipt).unwrap();
    put(
        &fixture.staged_prefix.join("share/openwave/VERSION"),
        b"999.0.0\n",
        0o644,
    );
    installation::record_install(&fixture.prefix, Some(&fixture.stage), InstallMethod::Manual)
        .unwrap();
    fixture.reject_without_copying(&fixture.digest());
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 inside a disposable root filesystem"]
fn root_payload_preserves_modified_unrecorded_and_nonmanual_destinations() {
    let fixture = RootPayload::new();
    let conflict = fixture.prefix.join("bin/openwave");
    put(&conflict, b"unrecorded destination", 0o755);
    assert!(!fixture.invoke(&fixture.digest()).status.success());
    assert_eq!(fs::read(&conflict).unwrap(), b"unrecorded destination");
    assert!(
        !fixture
            .prefix
            .join("share/openwave/install-manifest.json")
            .exists()
    );
    fs::remove_file(&conflict).unwrap();
    symlink(&fixture.helper, &conflict).unwrap();
    assert!(!fixture.invoke(&fixture.digest()).status.success());
    assert_eq!(fs::read_link(&conflict).unwrap(), fixture.helper);
    fs::remove_file(&conflict).unwrap();
    let output = fixture.invoke(&fixture.digest());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = fixture.prefix.join("share/openwave/install-manifest.json");
    let installed_receipt = fs::read(&receipt).unwrap();
    fs::write(&conflict, b"locally modified native binary").unwrap();
    assert!(!fixture.invoke(&fixture.digest()).status.success());
    assert_eq!(
        fs::read(&conflict).unwrap(),
        b"locally modified native binary"
    );
    assert_eq!(fs::read(&receipt).unwrap(), installed_receipt);
    fs::copy(fixture.staged_prefix.join("bin/openwave"), &conflict).unwrap();
    let mut value: Value = serde_json::from_slice(&installed_receipt).unwrap();
    value["method"] = "rpm".into();
    fs::write(&receipt, serde_json::to_vec(&value).unwrap()).unwrap();
    let managed = fs::read(&receipt).unwrap();
    assert!(!fixture.invoke(&fixture.digest()).status.success());
    assert_eq!(fs::read(&receipt).unwrap(), managed);
    assert!(fixture.helper.is_file());
    assert!(fixture.receipt.is_file());
}
