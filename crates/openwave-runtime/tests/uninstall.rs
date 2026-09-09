use openwave_core::model::{ErrorCode, OperationError, Result};
use openwave_runtime::{
    installation::{self, InstallMethod, Installation, NATIVE_PAYLOAD},
    paths::RuntimePaths,
    service::{HostCommands, HostContext},
    uninstall::{self, UninstallPlan},
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::{
        fs::{MetadataExt, PermissionsExt, symlink},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output},
    sync::Arc,
    time::Duration,
};

fn put(path: &Path, bytes: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
// A nested proc mount may not expose kernel-global entries masked by the
// outer sandbox. Reuse that mount only after proving its PID/user ownership;
// the opt-in is not itself evidence and never grants a host fallback.
fn require_private_proc() {
    let uid = rustix::process::geteuid().as_raw();
    assert_ne!(uid, 0);
    let mapping = fs::read_to_string("/proc/self/uid_map").unwrap();
    let maps: Vec<_> = mapping.split_whitespace().collect();
    assert_eq!(maps.len(), 3);
    assert_eq!(maps[0].parse::<u32>().unwrap(), uid);
    assert_eq!(
        maps[2], "1",
        "Initial or broad user namespace is not a fixture"
    );
    for namespace in ["pid", "user"] {
        assert_eq!(
            fs::read_link(format!("/proc/self/ns/{namespace}")).unwrap(),
            fs::read_link(format!("/proc/1/ns/{namespace}")).unwrap(),
            "procfs must belong to the current private PID/user namespace"
        );
    }
    assert_eq!(fs::metadata("/proc/1").unwrap().uid(), uid);
    assert_eq!(
        fs::read_link("/proc/self").unwrap(),
        PathBuf::from(std::process::id().to_string())
    );
    for path in ["/sys", "/dev/snd", "/dev/bus/usb", "/run/user"] {
        assert!(!Path::new(path).exists(), "Host resource exposed: {path}");
    }
    if Path::new("/proc/asound").exists() {
        assert!(fs::read_dir("/proc/asound").unwrap().next().is_none());
        let mounts = fs::read_to_string("/proc/self/mountinfo").unwrap();
        assert!(
            mounts.lines().any(|line| {
                line.split_whitespace().nth(4) == Some("/proc/asound")
                    && line
                        .split_once(" - ")
                        .is_some_and(|(_, tail)| tail.starts_with("tmpfs "))
            }),
            "ALSA proc data must remain masked by a private tmpfs"
        );
    }
    for name in [
        "DBUS_SESSION_BUS_ADDRESS",
        "DBUS_SYSTEM_BUS_ADDRESS",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "PULSE_SERVER",
        "PIPEWIRE_REMOTE",
    ] {
        assert!(
            std::env::var_os(name).is_none(),
            "Unexpected session resource: {name}"
        );
    }
}

fn child(mode: &str) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    for name in ["home", "config", "data", "state", "runtime", "empty-bin"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let executable = std::env::current_exe().unwrap();
    let binaries = executable.parent().unwrap().parent().unwrap();
    let reuse = std::env::var_os("OPENWAVE_TEST_PRIVATE_PROC").is_some();
    let mut namespace = if reuse {
        require_private_proc();
        Command::new(&executable)
    } else {
        let bwrap = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("bwrap"))
            .find(|path| path.is_file())
            .expect("uninstall process-ownership fixtures require bubblewrap from nix develop");
        Command::new(bwrap)
    };
    if !reuse {
        namespace.args([
            "--unshare-all",
            "--as-pid-1",
            "--die-with-parent",
            "--new-session",
            "--ro-bind",
            "/",
            "/",
            "--proc",
            "/proc",
            "--dev",
            "/dev",
            "--tmpfs",
            "/tmp",
        ]);
        if Path::new("/proc/asound").exists() {
            namespace.args(["--tmpfs", "/proc/asound"]);
        }
        namespace
            .arg("--ro-bind")
            .arg(binaries)
            .arg(binaries)
            .arg("--bind")
            .arg(root)
            .arg(root)
            .args(["--chdir", "/"]);
        namespace.arg(&executable);
    }
    let result = namespace
        .args(["--exact", "removal_environment_fixture", "--nocapture"])
        .env("OPENWAVE_REMOVAL_FIXTURE", mode)
        .env("OPENWAVE_FIXTURE_ROOT", root)
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("PATH", root.join("empty-bin"))
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .env_remove("FLATPAK_ID")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "fixture {mode}: {}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}
struct Fixture {
    root: PathBuf,
    paths: RuntimePaths,
    plan: UninstallPlan,
}
impl Fixture {
    fn new(root: PathBuf) -> Self {
        let prefix = root.join("install");
        for relative in NATIVE_PAYLOAD {
            put(
                &prefix.join(relative),
                if relative.starts_with("bin/") || relative.starts_with("libexec/") {
                    b"\x7fELFinert native fixture"
                } else {
                    b"fixture asset"
                },
                if relative.starts_with("bin/") || relative.starts_with("libexec/") {
                    0o755
                } else {
                    0o644
                },
            );
        }
        let helper = prefix.join("libexec/openwave-maintenance");
        fs::copy(env!("CARGO_BIN_EXE_openwave-maintenance"), &helper).unwrap();
        fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
        installation::record_install(&prefix, None, InstallMethod::Manual).unwrap();
        let paths = RuntimePaths {
            executable: prefix.join("bin/openwave"),
            prefix: Some(prefix.clone()),
            data: prefix.join("share/openwave"),
            identity: prefix.join("share/openwave"),
            maintenance: helper,
            source: None,
        };
        let plan = uninstall::inspect(&paths);
        assert!(plan.blockers.is_empty(), "{:?}", plan.blockers);
        put(
            &root.join("config/openwave/scenes.json"),
            b"{\"scenes\":{}}",
            0o600,
        );
        put(&root.join("config/other-app/settings"), b"sibling", 0o600);
        put(
            &prefix.join("share/openwave/personal-note"),
            b"unrecorded",
            0o600,
        );
        Self { root, paths, plan }
    }
    fn bundle(&self) -> PathBuf {
        fs::read_dir(self.root.join("state/openwave-uninstall"))
            .unwrap()
            .map(|e| e.unwrap().path().join("plan.json"))
            .find(|p| p.is_file())
            .unwrap()
    }
    fn snapshot(&self) -> &openwave_runtime::installation::InstallationSnapshot {
        self.plan.installation.snapshot.as_ref().unwrap()
    }
}
#[derive(Clone)]
struct StopFailure {
    fragment: PathBuf,
    daemon: PathBuf,
}
impl HostCommands for StopFailure {
    fn available(&self, program: &str) -> bool {
        program == "systemctl"
    }
    fn package_owner(&self, _: &[PathBuf]) -> Result<Option<InstallMethod>> {
        Ok(None)
    }
    fn run(&self, program: &str, args: &[String], _: Duration) -> Result<Output> {
        assert_eq!(program, "systemctl");
        if args.iter().any(|s| s == "stop") {
            return Err(OperationError::new(
                ErrorCode::Unavailable,
                "injected stop failure",
            ));
        }
        assert!(args.iter().any(|s| s == "show"));
        Ok(Output {status:ExitStatus::from_raw(0),stdout:format!("LoadState=loaded\nActiveState=active\nUnitFileState=enabled\nFragmentPath={}\nExecStart={{ path={} ; argv[]={}; ignore_errors=no ; }}\nWorkingDirectory=\n",self.fragment.display(),self.daemon.display(),self.daemon.display()).into_bytes(),stderr:vec![]})
    }
}
#[test]
fn manual_removal_preserves_settings_and_unrecorded_content() {
    child("manual");
}
#[test]
fn settings_consent_does_not_expand_to_siblings() {
    child("settings");
}
#[test]
fn symlinked_settings_block_before_application_removal() {
    child("settings-link");
}
#[test]
fn changed_receipt_blocks_before_integration_cleanup() {
    child("changed");
}
#[test]
fn failed_service_stop_preserves_every_later_phase() {
    child("service-stop");
}
#[test]
fn copied_native_retry_survives_late_loss_of_installed_helper() {
    child("retry");
}
#[test]
fn recovery_rejects_duplicate_fields_and_changed_authority() {
    child("hostile-retry");
}
#[test]
fn manager_files_and_package_guidance_survive_native_cleanup() {
    child("manager");
}
#[test]
fn pre_freeze_preparation_is_idempotent_and_preserves_consent() {
    child("prepare");
}
#[test]
fn old_recovery_is_data_only_and_retirement_preserves_integration() {
    child("legacy-retry");
}
#[test]
fn historical_default_umask_plan_is_safe_data_with_strict_boundaries() {
    child("legacy-readable");
}
#[test]
fn missing_whole_inventory_can_finish_from_native_copy() {
    child("retry-empty");
}
#[test]
fn desktop_cleanup_resolves_only_the_exact_bare_launcher() {
    child("desktop");
}

#[test]
fn removal_environment_fixture() {
    let Ok(mode) = std::env::var("OPENWAVE_REMOVAL_FIXTURE") else {
        return;
    };
    assert!(
        !rustix::process::geteuid().is_root(),
        "User removal fixtures must run as an unprivileged user, never host root"
    );
    if std::env::var_os("OPENWAVE_TEST_PRIVATE_PROC").is_some() {
        require_private_proc();
    }
    let root = PathBuf::from(std::env::var_os("OPENWAVE_FIXTURE_ROOT").unwrap());
    let mut fixture = Fixture::new(root.clone());
    match mode.as_str() {
        "manual" | "settings" => {
            let result =
                uninstall::execute(&fixture.paths, &fixture.plan, mode == "settings", false);
            assert!(result.success, "{result:?}");
            assert!(result.app_removed);
            assert_eq!(
                root.join("config/openwave/scenes.json").exists(),
                mode != "settings"
            );
            assert_eq!(
                fs::read(root.join("config/other-app/settings")).unwrap(),
                b"sibling"
            );
            assert_eq!(
                fs::read(fixture.paths.data.join("personal-note")).unwrap(),
                b"unrecorded"
            );
            assert!(fixture.paths.prefix.as_ref().unwrap().join("bin").is_dir());
            assert!(
                fixture
                    .paths
                    .prefix
                    .as_ref()
                    .unwrap()
                    .join("libexec")
                    .is_dir()
            );
        }
        "settings-link" => {
            let settings = root.join("config/openwave");
            fs::rename(&settings, root.join("elsewhere")).unwrap();
            symlink(root.join("elsewhere"), &settings).unwrap();
            let result = uninstall::execute(&fixture.paths, &fixture.plan, true, false);
            assert!(!result.success);
            assert!(fixture.paths.executable.exists());
            assert!(root.join("elsewhere/scenes.json").exists());
        }
        "changed" => {
            let receipt = fixture.snapshot().receipt.as_ref().unwrap();
            let original = fs::read(receipt).unwrap();
            fs::write(receipt, [original, b" ".to_vec()].concat()).unwrap();
            let result = uninstall::execute(&fixture.paths, &fixture.plan, false, false);
            assert!(!result.success);
            assert!(result.removed.is_empty());
            assert!(fixture.paths.executable.exists());
            assert!(root.join("config/openwave/scenes.json").exists());
        }
        "service-stop" => {
            let fragment = root.join("config/systemd/user/openwave.service");
            let daemon = fixture
                .paths
                .prefix
                .as_ref()
                .unwrap()
                .join("bin/openwave-daemon");
            put(
                &fragment,
                format!("[Service]\nExecStart={}\n", daemon.display()).as_bytes(),
                0o600,
            );
            let mut host = HostContext::discover().unwrap();
            host.commands = Arc::new(StopFailure {
                fragment: fragment.clone(),
                daemon,
            });
            host.udev_directory = root.join("udev");
            let result = uninstall::execute_with(&fixture.paths, &fixture.plan, true, false, &host);
            assert!(!result.success);
            assert!(result.error.unwrap().contains("stop failure"));
            assert!(result.removed.is_empty());
            assert!(fragment.exists());
            assert!(fixture.paths.executable.exists());
            assert!(root.join("config/openwave/scenes.json").exists());
            assert!(fixture.bundle().exists());
        }
        "retry" => {
            uninstall::prepare(&fixture.paths, &fixture.plan, false).unwrap();
            let plan = fixture.bundle();
            let helper = plan.with_file_name("openwave-maintenance");
            // Simulate a late interruption after a subset was removed, including
            // the installed maintenance executable. No replacement authority.
            fs::remove_file(&fixture.paths.maintenance).unwrap();
            fs::remove_file(fixture.paths.data.join("style.css")).unwrap();
            let output = Command::new(&helper)
                .args(["resume-uninstall", "--plan"])
                .arg(&plan)
                .arg("--yes")
                .current_dir(root.join("home"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!fixture.paths.executable.exists());
            assert!(root.join("config/openwave/scenes.json").exists());
            assert!(!plan.exists());
        }
        "hostile-retry" => {
            uninstall::prepare(&fixture.paths, &fixture.plan, false).unwrap();
            let plan = fixture.bundle();
            let original = fs::read(&plan).unwrap();
            let mut text = String::from_utf8(original.clone()).unwrap();
            text.insert_str(1, "\"schema\":2,");
            fs::write(&plan, text).unwrap();
            assert!(!uninstall::resume_uninstall(&plan, true, false).success);
            assert!(fixture.paths.executable.exists());
            let mut value: Value = serde_json::from_slice(&original).unwrap();
            value["installation"]["files"]
                .as_array_mut()
                .unwrap()
                .push(json!(root.join("config/other-app/settings")));
            fs::write(&plan, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(!uninstall::resume_uninstall(&plan, true, false).success);
            assert!(root.join("config/other-app/settings").exists());
        }
        "manager" => {
            fixture.plan.installation = Installation {
                method: InstallMethod::Deb,
                snapshot: None,
                canonical_identity: fixture.paths.identity.clone(),
                guidance: "Use the package manager".into(),
                error: None,
            };
            let result = uninstall::execute(&fixture.paths, &fixture.plan, false, false);
            assert!(result.success, "{result:?}");
            assert!(!result.app_removed);
            assert_eq!(result.guidance, "Use the package manager");
            assert!(fixture.paths.executable.exists());
            assert!(root.join("config/openwave/scenes.json").exists());
        }
        "prepare" => {
            uninstall::prepare(&fixture.paths, &fixture.plan, false).unwrap();
            let first = fixture.bundle();
            let bytes = fs::read(&first).unwrap();
            uninstall::prepare(&fixture.paths, &fixture.plan, false).unwrap();
            assert_eq!(fs::read(&first).unwrap(), bytes);
            assert_eq!(
                fs::read_dir(root.join("state/openwave-uninstall"))
                    .unwrap()
                    .count(),
                1
            );
            let mut value: Value = serde_json::from_slice(&bytes).unwrap();
            value["delete_settings"] = json!(true);
            fs::write(&first, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(uninstall::prepare(&fixture.paths, &fixture.plan, false).is_err());
            assert!(fixture.paths.executable.exists());
            assert!(root.join("config/openwave/scenes.json").exists());
        }
        "retry-empty" => {
            uninstall::prepare(&fixture.paths, &fixture.plan, false).unwrap();
            let plan = fixture.bundle();
            let helper = plan.with_file_name("openwave-maintenance");
            fs::remove_file(fixture.paths.data.join("personal-note")).unwrap();
            installation::remove_inventory(fixture.snapshot()).unwrap();
            assert!(!fixture.paths.identity.exists());
            let output = Command::new(&helper)
                .arg("resume-uninstall")
                .arg("--plan")
                .arg(&plan)
                .arg("--yes")
                .current_dir(root.join("home"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!plan.exists());
            assert!(root.join("config/openwave/scenes.json").exists());
        }
        "desktop" => {
            symlink(&fixture.paths.executable, root.join("empty-bin/openwave")).unwrap();
            let owned = root.join("config/autostart/openwave-autostart.desktop");
            let foreign = root.join("data/applications/openwave.desktop");
            put(
                &owned,
                b"[Desktop Entry]\nType=Application\nExec=openwave --hide\n",
                0o600,
            );
            put(
                &foreign,
                b"[Desktop Entry]\nType=Application\nExec=/other/bin/openwave\n",
                0o600,
            );
            let result = uninstall::execute(&fixture.paths, &fixture.plan, false, false);
            assert!(result.success, "{result:?}");
            assert!(!owned.exists());
            assert!(foreign.exists());
        }
        "legacy-retry" | "legacy-readable" => {
            let prefix = root.join("legacy");
            let module = prefix.join("lib/python3.13/site-packages/wavexlr");
            put(
                &prefix.join("bin/openwave"),
                b"#!/bin/sh\nexec python3 -m wavexlr \"$@\"\n",
                0o755,
            );
            put(&module.join("__init__.py"), b"# inert legacy data", 0o644);
            put(&module.join("__main__.py"), b"# never executed", 0o644);
            put(
                &prefix.join("share/applications/openwave.desktop"),
                b"[Desktop Entry]\nType=Application\nName=OpenWave\nExec=openwave\n",
                0o644,
            );
            fs::create_dir_all(prefix.join("share/openwave")).unwrap();
            let snapshot = installation::inspect_prefix(&prefix, Some(&module))
                .unwrap()
                .snapshot
                .unwrap();
            let directory = root.join("state/openwave-uninstall/openwave-remove-old-fixture");
            fs::create_dir_all(&directory).unwrap();
            fs::set_permissions(
                directory.parent().unwrap(),
                fs::Permissions::from_mode(0o700),
            )
            .unwrap();
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            let old = json!({"schema":1,"delete_settings":false,"method":"manual","prefix":snapshot.prefix,"module_dir":snapshot.module_dir,"files":snapshot.files,"directories":snapshot.directories,"receipt":snapshot.receipt,"guidance":"","legacy":true,"identities":snapshot.identities});
            let plan = directory.join("plan.json");
            let accepted_mode = if mode == "legacy-readable" {
                0o644
            } else {
                0o600
            };
            let original = serde_json::to_vec(&old).unwrap();
            put(&plan, &original, accepted_mode);
            if mode == "legacy-readable" {
                for unsafe_mode in [0o664, 0o646, 0o666] {
                    fs::set_permissions(&plan, fs::Permissions::from_mode(unsafe_mode)).unwrap();
                    assert!(!uninstall::resume_retirement(&plan, true).success);
                    assert!(module.join("__main__.py").exists());
                    assert_eq!(fs::read(&plan).unwrap(), original);
                }
                fs::set_permissions(&plan, fs::Permissions::from_mode(accepted_mode)).unwrap();
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
                assert!(!uninstall::resume_retirement(&plan, true).success);
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
                let moved = directory.with_file_name("openwave-remove-moved");
                fs::rename(&directory, &moved).unwrap();
                symlink(&moved, &directory).unwrap();
                assert!(!uninstall::resume_retirement(&plan, true).success);
                fs::remove_file(&directory).unwrap();
                fs::rename(&moved, &directory).unwrap();
                let linked = directory.join("linked.json");
                fs::hard_link(&plan, &linked).unwrap();
                assert!(!uninstall::resume_retirement(&plan, true).success);
                fs::remove_file(&linked).unwrap();
                fs::rename(&plan, &linked).unwrap();
                symlink(&linked, &plan).unwrap();
                assert!(!uninstall::resume_retirement(&plan, true).success);
                fs::remove_file(&plan).unwrap();
                fs::rename(&linked, &plan).unwrap();
                put(&module.join("__main__.py"), b"# replaced authority", 0o644);
                assert!(!uninstall::resume_retirement(&plan, true).success);
                assert_eq!(fs::read(&plan).unwrap(), original);
                put(&module.join("__main__.py"), b"# never executed", 0o644);
            }
            put(
                &directory.join("retry.py"),
                b"raise RuntimeError('must never execute Python')",
                0o600,
            );
            let integration = root.join("config/autostart/openwave-autostart.desktop");
            put(&integration, b"legacy integration to preserve", 0o600);
            fs::remove_file(prefix.join("bin/openwave")).unwrap();
            let output = Command::new(env!("CARGO_BIN_EXE_openwave-maintenance"))
                .arg("resume-retirement")
                .arg("--plan")
                .arg(&plan)
                .arg("--yes")
                .current_dir(root.join("home"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!module.join("__main__.py").exists());
            assert!(fixture.paths.executable.exists());
            assert!(integration.exists());
            assert!(root.join("config/openwave/scenes.json").exists());
            assert!(
                directory.join("retry.py").exists(),
                "Historical executable content is never executed or recursively adopted"
            );
        }
        _ => panic!("unknown fixture"),
    }
}

#[test]
fn ordinary_privileged_entrypoints_refuse_without_root_trust() {
    let temporary = tempfile::tempdir().unwrap();
    let missing = temporary.path().join("authority.json");
    assert!(uninstall::resume_privileged(&missing).is_err());
    assert!(!missing.exists());
}

/// Run only inside a deliberately provisioned disposable user/root namespace.
/// The helper is copied into a trusted /opt prefix there, never installed on the
/// host. The namespace must map both UID0 and the nonroot fixture login UID.
#[test]
#[ignore = "requires disposable root/user namespace with trusted native maintenance and a mapped login UID"]
fn privileged_copy_survives_installed_helper_loss_without_user_plan_authority() {
    assert_eq!(
        std::env::var("OPENWAVE_DISPOSABLE_REMOVAL_PROOF").as_deref(),
        Ok("yes")
    );
    assert!(rustix::process::geteuid().is_root());
    let mapping = fs::read_to_string("/proc/self/uid_map").unwrap();
    assert!(
        !mapping
            .split_whitespace()
            .collect::<Vec<_>>()
            .eq(&["0", "0", "4294967295"]),
        "Refuse initial host user namespace"
    );
    let prefix = PathBuf::from(std::env::var("OPENWAVE_DISPOSABLE_REMOVAL_PREFIX").unwrap());
    assert!(
        prefix.starts_with("/opt/openwave-removal-fixture-")
            || prefix.parent() == Some(Path::new("/opt"))
                && prefix
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("openwave-removal-fixture-")
    );
    let helper = prefix.join("libexec/openwave-maintenance");
    let receipt = prefix.join("share/openwave/install-manifest.json");
    let digest = installation::file_digest(&receipt).unwrap();
    let prepared = Command::new(&helper)
        .args(["prepare-remove-files", "--prefix"])
        .arg(&prefix)
        .arg("--receipt")
        .arg(&receipt)
        .arg("--expected-sha256")
        .arg(&digest)
        .output()
        .unwrap();
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let authority = PathBuf::from(String::from_utf8(prepared.stdout).unwrap().trim());
    let copy = authority.with_file_name("openwave-maintenance");
    assert!(copy.exists());
    assert_eq!(
        fs::metadata(&authority).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let forged = prefix.join("user-plan.json");
    fs::write(
        &forged,
        b"{\"schema\":2,\"installation\":{\"files\":[\"/etc/passwd\"]}}",
    )
    .unwrap();
    let rejected = Command::new(&copy)
        .arg("resume-privileged")
        .arg("--transaction")
        .arg(&forged)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(receipt.exists());
    assert!(authority.exists());
    fs::remove_file(&helper).unwrap();
    let output = Command::new(&copy)
        .arg("resume-privileged")
        .arg("--transaction")
        .arg(&authority)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!receipt.exists());
    assert!(!authority.exists());
    assert!(!copy.exists());
}
