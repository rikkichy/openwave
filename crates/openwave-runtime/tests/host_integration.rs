use openwave_core::model::{ErrorCode, OperationError, Result, default_mixes};
use openwave_runtime::{
    desktop,
    installation::InstallMethod,
    paths::RuntimePaths,
    service::{self, HostCommands, HostContext},
    setup,
};
use std::{
    collections::HashSet,
    fs,
    os::unix::{
        fs::{MetadataExt, PermissionsExt, symlink},
        process::ExitStatusExt,
    },
    path::{Path, PathBuf},
    process::{ExitStatus, Output},
    sync::{Arc, Mutex},
    time::Duration,
};

fn fixture_lock<T>(lock: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    lock.lock().map_err(|_| {
        OperationError::new(ErrorCode::Unavailable, "Fixture observation lock poisoned")
    })
}

#[derive(Default)]
struct Commands {
    available: HashSet<String>,
    show: Mutex<String>,
    reload_show: Mutex<String>,
    calls: Mutex<Vec<(String, Vec<String>)>>,
    fail: Mutex<Option<String>>,
    owner: Mutex<Option<InstallMethod>>,
    package_error: Mutex<bool>,
    runit_status: Mutex<String>,
    setup_files: Mutex<Vec<PathBuf>>,
    launcher: Mutex<Option<PathBuf>>,
}
fn output(code: i32, text: &str) -> Output {
    Output {
        status: ExitStatus::from_raw(code << 8),
        stdout: text.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}
impl HostCommands for Commands {
    fn available(&self, program: &str) -> bool {
        self.available.contains(program)
    }
    fn find_program(&self, program: &str) -> Option<PathBuf> {
        (program == "openwave")
            .then(|| {
                fixture_lock(&self.launcher)
                    .expect("fixture synchronization failed")
                    .clone()
            })
            .flatten()
    }
    fn run(&self, program: &str, args: &[String], _: Duration) -> Result<Output> {
        fixture_lock(&self.calls)?.push((program.into(), args.to_vec()));
        if fixture_lock(&self.fail)?
            .as_ref()
            .is_some_and(|s| args.iter().any(|a| a == s))
        {
            return Ok(output(1, "injected command failure"));
        }
        if program == "systemctl" && args.iter().any(|a| a == "show") {
            return Ok(output(0, &fixture_lock(&self.show)?));
        }
        if program == "systemctl" && args.iter().any(|a| a == "daemon-reload") {
            *fixture_lock(&self.show)? = fixture_lock(&self.reload_show)?.clone();
        }
        if program == "systemctl" && args.iter().any(|a| a == "stop") {
            let mut show = fixture_lock(&self.show)?;
            *show = show.replace("ActiveState=active", "ActiveState=inactive");
        }
        if program == "systemctl" && args.iter().any(|a| a == "restart") {
            for path in fixture_lock(&self.setup_files)?.iter() {
                assert!(
                    path.is_file(),
                    "service started before setup prerequisite {}",
                    path.display()
                );
            }
        }
        if program == "sv" && args.first().map(String::as_str) == Some("status") {
            return Ok(output(0, &fixture_lock(&self.runit_status)?));
        }
        Ok(output(0, ""))
    }
    fn package_owner(&self, _: &[PathBuf]) -> Result<Option<InstallMethod>> {
        if *fixture_lock(&self.package_error)? {
            return Err(OperationError::new(
                ErrorCode::Unavailable,
                "package database unavailable",
            ));
        }
        Ok(*fixture_lock(&self.owner)?)
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    paths: RuntimePaths,
    host: HostContext,
    commands: Arc<Commands>,
}
impl Fixture {
    fn new(backend: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let prefix = root.join("install");
        fs::create_dir_all(prefix.join("bin")).unwrap();
        fs::create_dir_all(prefix.join("share/openwave/wireplumber")).unwrap();
        for name in ["openwave", "openwave-daemon"] {
            let file = prefix.join("bin").join(name);
            fs::write(&file, b"fixture executable, never launched").unwrap();
            fs::set_permissions(file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::write(
            prefix
                .join("share/openwave/wireplumber")
                .join(setup::WIREPLUMBER_NAME),
            "monitor.alsa.rules = []\n",
        )
        .unwrap();
        let paths = RuntimePaths {
            executable: prefix.join("bin/openwave"),
            prefix: Some(prefix.clone()),
            data: prefix.join("share/openwave"),
            identity: prefix.join("share/openwave"),
            maintenance: prefix.join("libexec/openwave-maintenance"),
            source: None,
        };
        let commands = Arc::new(Commands {
            available: HashSet::from([backend.into()]),
            ..Commands::default()
        });
        let host = HostContext {
            home: root.join("home"),
            config_home: root.join("config"),
            data_home: root.join("data"),
            udev_directory: root.join("udev"),
            runit_link: root.join("var/service/wavexlr-audio"),
            runit_definition: root.join("etc/sv/wavexlr-audio"),
            username: "fixture".into(),
            uid: 1000,
            durable_bins: vec![],
            sandboxed: false,
            commands: commands.clone(),
        };
        fs::create_dir_all(&host.udev_directory).unwrap();
        fs::create_dir_all(host.runit_link.parent().unwrap()).unwrap();
        let fixture = Self {
            _temp: temp,
            paths,
            host,
            commands,
        };
        fixture.observe(
            &fixture.unit(),
            &prefix.join("bin/openwave-daemon"),
            &format!("!{}", fixture.host.home.display()),
        );
        *fixture_lock(&fixture.commands.reload_show).expect("fixture synchronization failed") =
            fixture_lock(&fixture.commands.show)
                .expect("fixture synchronization failed")
                .clone();
        fixture.missing();
        fixture
    }
    fn unit(&self) -> PathBuf {
        self.host.config_home.join("systemd/user/openwave.service")
    }
    fn missing(&self) {
        // systemctl omits ExecStart for an absent unit, including with --all.
        *fixture_lock(&self.commands.show).expect("fixture synchronization failed") = "LoadState=not-found\nActiveState=inactive\nUnitFileState=\nFragmentPath=\nWorkingDirectory=\n".into();
    }
    fn observe(&self, fragment: &Path, program: &Path, workdir: &str) {
        *fixture_lock(&self.commands.show).expect("fixture synchronization failed") = format!(
            "LoadState=loaded\nActiveState=active\nUnitFileState=enabled\nFragmentPath={}\nExecStart={{ path={} ; argv[]=\"{}\" ; ignore_errors=no ; pid=0 ; }}\nWorkingDirectory={workdir}\n",
            fragment.display(),
            program.display(),
            program.display()
        );
    }
    fn installed(&self, stale: bool) {
        let mut text = service::render_unit(&self.paths, &self.host).unwrap();
        if stale {
            text = text.replace("Before=pipewire.service wireplumber.service\n", "");
        }
        fs::create_dir_all(self.unit().parent().unwrap()).unwrap();
        fs::write(self.unit(), text).unwrap();
        self.observe(
            &self.unit(),
            &self
                .paths
                .prefix
                .as_ref()
                .unwrap()
                .join("bin/openwave-daemon"),
            &format!("!{}", self.host.home.display()),
        );
    }
    fn no_mutating_commands(&self) -> bool {
        fixture_lock(&self.commands.calls)
            .expect("fixture synchronization failed")
            .iter()
            .all(|(_, args)| args.iter().any(|a| a == "show" || a == "status"))
    }
}

#[test]
fn proven_stale_service_refreshes_and_reports_command_failures() {
    let f = Fixture::new("systemctl");
    f.installed(true);
    assert!(
        service::status_with(&f.paths, &f.host)
            .unwrap()
            .message
            .contains("refresh")
    );
    service::install_with(&f.paths, &f.host).unwrap();
    let status = service::status_with(&f.paths, &f.host).unwrap();
    assert!(status.running);
    assert!(!status.message.contains("refresh"));
    let calls = fixture_lock(&f.commands.calls).expect("fixture synchronization failed");
    let mutations: Vec<_> = calls
        .iter()
        .filter(|(_, a)| !a.iter().any(|s| s == "show"))
        .map(|(_, a)| a[1].as_str())
        .collect();
    assert_eq!(
        mutations,
        ["daemon-reload", "enable", "reset-failed", "restart"]
    );
    drop(calls);
    f.installed(true);
    *fixture_lock(&f.commands.fail).expect("fixture synchronization failed") =
        Some("reset-failed".into());
    let error = service::install_with(&f.paths, &f.host).unwrap_err();
    assert!(error.message.contains("injected command failure"));
}
#[test]
fn loaded_service_without_effective_command_blocks_mutation() {
    let f = Fixture::new("systemctl");
    f.installed(true);
    let saved = fs::read(f.unit()).unwrap();
    {
        let mut show = fixture_lock(&f.commands.show).expect("fixture synchronization failed");
        *show = show
            .lines()
            .filter(|line| !line.starts_with("ExecStart="))
            .collect::<Vec<_>>()
            .join("\n");
    }
    assert_eq!(
        service::install_with(&f.paths, &f.host).unwrap_err().code,
        ErrorCode::Unavailable
    );
    assert!(service::stop_owned_with(&f.paths, &f.host).is_err());
    assert!(service::remove_owned_with(&f.paths, &f.host).is_err());
    assert_eq!(fs::read(f.unit()).unwrap(), saved);
    assert!(f.no_mutating_commands());
}
#[test]
fn effective_foreign_command_or_workdir_prevents_refresh_stop_and_removal() {
    let f = Fixture::new("systemctl");
    f.installed(true);
    let saved = fs::read(f.unit()).unwrap();
    f.observe(
        &f.unit(),
        Path::new("/another/install/bin/openwave-daemon"),
        "",
    );
    assert_eq!(
        service::install_with(&f.paths, &f.host).unwrap_err().code,
        ErrorCode::Identity
    );
    assert!(service::remove_owned_with(&f.paths, &f.host).is_err());
    f.observe(
        &f.unit(),
        &f.paths.prefix.as_ref().unwrap().join("bin/openwave-daemon"),
        "!/another/checkout",
    );
    assert!(service::stop_owned_with(&f.paths, &f.host).is_err());
    assert_eq!(fs::read(f.unit()).unwrap(), saved);
    assert!(f.no_mutating_commands());
}
#[test]
fn default_home_cannot_override_explicit_source_working_directory() {
    let mut f = Fixture::new("systemctl");
    f.paths.source = f.paths.prefix.take();
    let text = service::render_unit(&f.paths, &f.host).unwrap().replace(
        "Type=simple\n",
        &format!(
            "Type=simple\nWorkingDirectory={}\n",
            f.paths.source.as_ref().unwrap().display()
        ),
    );
    fs::create_dir_all(f.unit().parent().unwrap()).unwrap();
    fs::write(f.unit(), &text).unwrap();
    f.observe(
        &f.unit(),
        &f.paths.executable.with_file_name("openwave-daemon"),
        &format!("!{}", f.host.home.display()),
    );
    assert_eq!(
        service::install_with(&f.paths, &f.host).unwrap_err().code,
        ErrorCode::Identity
    );
    assert_eq!(fs::read_to_string(f.unit()).unwrap(), text);
    assert!(f.no_mutating_commands());
}
#[test]
fn matching_effective_command_cannot_authorize_a_foreign_fragment() {
    let f = Fixture::new("systemctl");
    f.installed(false);
    fs::write(f.unit(), "[Service]\nExecStart=/foreign/daemon\n").unwrap();
    assert!(service::stop_owned_with(&f.paths, &f.host).is_err());
    assert!(f.no_mutating_commands());
}
#[test]
fn unmanaged_name_without_effective_identity_is_not_stale_authority() {
    let f = Fixture::new("systemctl");
    f.installed(true);
    f.missing();
    let bytes = fs::read(f.unit()).unwrap();
    assert!(service::install_with(&f.paths, &f.host).is_err());
    assert_eq!(fs::read(f.unit()).unwrap(), bytes);
    assert!(f.no_mutating_commands());
}
#[test]
fn package_owned_service_is_stopped_but_not_deleted() {
    let f = Fixture::new("systemctl");
    f.installed(false);
    *fixture_lock(&f.commands.owner).expect("fixture synchronization failed") =
        Some(InstallMethod::Deb);
    let before = fs::read(f.unit()).unwrap();
    service::remove_owned_with(&f.paths, &f.host).unwrap();
    assert_eq!(fs::read(f.unit()).unwrap(), before);
    assert!(
        fixture_lock(&f.commands.calls)
            .expect("fixture synchronization failed")
            .iter()
            .any(|(_, a)| a.iter().any(|s| s == "stop"))
    );
    assert!(
        !fixture_lock(&f.commands.calls)
            .expect("fixture synchronization failed")
            .iter()
            .any(|(_, a)| a.iter().any(|s| s == "disable"))
    );
}
#[test]
fn package_query_failure_blocks_service_mutation() {
    let f = Fixture::new("systemctl");
    f.installed(true);
    *fixture_lock(&f.commands.package_error).expect("fixture synchronization failed") = true;
    assert!(service::install_with(&f.paths, &f.host).is_err());
    assert!(f.no_mutating_commands());
}
#[test]
fn stopped_manual_service_removal_preserves_unrelated_files() {
    let f = Fixture::new("systemctl");
    f.installed(false);
    let sibling = f.unit().with_file_name("someone-else.service");
    fs::write(&sibling, "leave alone").unwrap();
    service::remove_owned_with(&f.paths, &f.host).unwrap();
    assert!(!f.unit().exists());
    assert_eq!(fs::read_to_string(sibling).unwrap(), "leave alone");
}

#[test]
fn symlinked_service_ancestry_preserves_external_fragment_on_refresh_and_removal() {
    let f = Fixture::new("systemctl");
    let external = f._temp.path().join("external-units");
    fs::create_dir_all(&external).unwrap();
    fs::create_dir_all(f.host.config_home.join("systemd")).unwrap();
    symlink(&external, f.unit().parent().unwrap()).unwrap();
    f.installed(true);
    let before = fs::read(f.unit()).unwrap();
    let inode = fs::metadata(f.unit()).unwrap();
    service::install_with(&f.paths, &f.host).unwrap();
    assert!(f.no_mutating_commands());
    service::remove_owned_with(&f.paths, &f.host).unwrap();
    let after = fs::metadata(f.unit()).unwrap();
    assert_eq!(fs::read(f.unit()).unwrap(), before);
    assert_eq!((after.dev(), after.ino()), (inode.dev(), inode.ino()));
    let calls = fixture_lock(&f.commands.calls).unwrap();
    assert!(
        calls
            .iter()
            .any(|(_, args)| args.iter().any(|arg| arg == "stop"))
    );
    assert!(
        !calls
            .iter()
            .any(|(_, args)| args.iter().any(|arg| arg == "disable"))
    );
}

#[test]
fn absent_service_cannot_publish_through_symlinked_ancestry() {
    let f = Fixture::new("systemctl");
    let external = f._temp.path().join("external-units");
    fs::create_dir_all(&external).unwrap();
    fs::create_dir_all(f.host.config_home.join("systemd")).unwrap();
    symlink(&external, f.unit().parent().unwrap()).unwrap();
    assert!(service::install_with(&f.paths, &f.host).is_err());
    assert!(!external.join(service::SYSTEMD_UNIT).exists());
    assert!(f.no_mutating_commands());
}
#[test]
fn runit_requires_exact_target_user_and_preserves_admin_definition() {
    let f = Fixture::new("sv");
    fs::create_dir_all(&f.host.runit_definition).unwrap();
    let run = f.host.runit_definition.join("run");
    fs::write(
        &run,
        format!(
            "#!/bin/sh\nexec 2>&1\nexec chpst -u fixture {}\n",
            f.paths
                .prefix
                .as_ref()
                .unwrap()
                .join("bin/openwave-daemon")
                .display()
        ),
    )
    .unwrap();
    symlink(&f.host.runit_definition, &f.host.runit_link).unwrap();
    *fixture_lock(&f.commands.runit_status).expect("fixture synchronization failed") =
        "run: fixture: (pid 100) 20s\n".into();
    assert!(service::status_with(&f.paths, &f.host).unwrap().running);
    assert_eq!(
        service::install_with(&f.paths, &f.host).unwrap_err().code,
        ErrorCode::Unsupported
    );
    let original = fs::read_to_string(&run).unwrap();
    fs::write(&run, original.replace("-u fixture", "-u somebody")).unwrap();
    assert!(service::stop_owned_with(&f.paths, &f.host).is_err());
    fs::write(&run, &original).unwrap();
    service::remove_owned_with(&f.paths, &f.host).unwrap();
    assert!(!f.host.runit_link.exists());
    assert_eq!(fs::read_to_string(run).unwrap(), original);
}
#[test]
fn desktop_exec_roundtrip_handles_both_escaping_layers() {
    let mut f = Fixture::new("none");
    f.paths.executable = f
        .paths
        .executable
        .with_file_name("open wave\\\"$`% executable");
    fs::write(&f.paths.executable, "fixture").unwrap();
    assert_eq!(
        desktop::set_autostart_with(&f.paths, true, true, &f.host).unwrap(),
        (true, true)
    );
    assert_eq!(
        desktop::set_autostart_with(&f.paths, true, false, &f.host).unwrap(),
        (true, false)
    );
    assert_eq!(
        desktop::set_autostart_with(&f.paths, false, false, &f.host).unwrap(),
        (false, false)
    );
}
#[test]
fn failed_autostart_reports_retained_actual_state_and_preserves_foreign_entry() {
    let f = Fixture::new("none");
    desktop::set_autostart_with(&f.paths, true, true, &f.host).unwrap();
    let path = f.host.config_home.join("autostart/openwave.desktop");
    let saved = fs::read(&path).unwrap();
    *fixture_lock(&f.commands.package_error).expect("fixture synchronization failed") = true;
    let error = desktop::set_autostart_with(&f.paths, false, false, &f.host).unwrap_err();
    assert!(error.message.contains("enabled=true, hidden=true"));
    assert_eq!(desktop::autostart_state_at(&path).unwrap(), (true, true));
    assert_eq!(fs::read(&path).unwrap(), saved);
    *fixture_lock(&f.commands.package_error).expect("fixture synchronization failed") = false;
    fs::write(
        &path,
        "[Desktop Entry]\nType=Application\nExec=\"/foreign/openwave\"\nHidden=true\n",
    )
    .unwrap();
    let foreign = fs::read(&path).unwrap();
    assert_eq!(
        desktop::set_autostart_with(&f.paths, true, true, &f.host)
            .unwrap_err()
            .code,
        ErrorCode::Identity
    );
    assert_eq!(fs::read(path).unwrap(), foreign);
}
#[test]
fn autostart_never_follows_a_symlink() {
    let f = Fixture::new("none");
    let path = f.host.config_home.join("autostart/openwave.desktop");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let foreign = f.host.config_home.join("foreign");
    fs::write(&foreign, "untouched").unwrap();
    symlink(&foreign, &path).unwrap();
    assert!(desktop::set_autostart_with(&f.paths, true, false, &f.host).is_err());
    assert_eq!(fs::read_to_string(foreign).unwrap(), "untouched");
    assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
}

fn launch_fixture_entry(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap();
    let exec = text
        .lines()
        .find_map(|line| line.strip_prefix("Exec="))
        .unwrap();
    let args = service::split_words(exec).unwrap();
    let output = std::process::Command::new(&args[0])
        .args(&args[1..])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn desktop_launchers_survive_profile_upgrade_refresh_and_autostart_toggle() {
    let mut f = Fixture::new("none");
    fs::write(&f.paths.executable, "#!/bin/sh\nprintf v1\n").unwrap();
    let v1 = f.paths.prefix.clone().unwrap();
    let v2 = f._temp.path().join("install-v2");
    fs::create_dir_all(v2.join("bin")).unwrap();
    fs::create_dir_all(v2.join("share/openwave")).unwrap();
    fs::write(v2.join("bin/openwave"), "#!/bin/sh\nprintf v2\n").unwrap();
    fs::set_permissions(v2.join("bin/openwave"), fs::Permissions::from_mode(0o755)).unwrap();
    let profile = f._temp.path().join("profile");
    symlink(&v1, &profile).unwrap();
    f.host.durable_bins.push(profile.join("bin"));
    desktop::ensure_menu_entry_with(&f.paths, &f.host).unwrap();
    desktop::set_autostart_with(&f.paths, true, true, &f.host).unwrap();
    let menu = f.host.data_home.join("applications/openwave.desktop");
    let autostart = f.host.config_home.join("autostart/openwave.desktop");
    assert_eq!(launch_fixture_entry(&menu), "v1");
    assert_eq!(launch_fixture_entry(&autostart), "v1");

    fs::remove_file(&profile).unwrap();
    symlink(&v2, &profile).unwrap();
    let current = RuntimePaths {
        executable: v2.join("bin/openwave"),
        prefix: Some(v2.clone()),
        data: v2.join("share/openwave"),
        identity: v2.join("share/openwave"),
        maintenance: v2.join("libexec/openwave-maintenance"),
        source: None,
    };
    // The former instance cannot use a profile now pointing at a different
    // installation to acquire ownership of the upgraded launchers.
    assert!(desktop::set_autostart_with(&f.paths, false, false, &f.host).is_err());
    desktop::ensure_menu_entry_with(&current, &f.host).unwrap();
    assert_eq!(
        desktop::set_autostart_with(&current, true, false, &f.host).unwrap(),
        (true, false)
    );
    desktop::set_autostart_with(&current, false, false, &f.host).unwrap();
    desktop::set_autostart_with(&current, true, true, &f.host).unwrap();
    fs::remove_dir_all(v1).unwrap();
    assert_eq!(launch_fixture_entry(&menu), "v2");
    assert_eq!(launch_fixture_entry(&autostart), "v2");
}

// Record a complete native installation before replacing just the fixture
// launcher with a harmless script and updating its recorded digest.
fn migration_install(prefix: &Path, version: &str) -> RuntimePaths {
    use openwave_runtime::installation::{self, NATIVE_PAYLOAD};
    for relative in NATIVE_PAYLOAD {
        let path = prefix.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let executable = relative.starts_with("bin/") || relative.starts_with("libexec/");
        fs::write(
            &path,
            if executable {
                b"\x7fELFfixture".as_slice()
            } else {
                b"fixture".as_slice()
            },
        )
        .unwrap();
        fs::set_permissions(
            &path,
            fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 }),
        )
        .unwrap();
    }
    fs::write(
        prefix.join("share/openwave/VERSION"),
        openwave_core::VERSION,
    )
    .unwrap();
    let receipt = installation::record_install(prefix, None, InstallMethod::Manual).unwrap();
    let executable = prefix.join("bin/openwave");
    fs::write(&executable, format!("#!/bin/sh\nprintf {version}\n")).unwrap();
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
    metadata["sha256"][executable.to_str().unwrap()] =
        installation::file_digest(&executable).unwrap().into();
    fs::write(&receipt, serde_json::to_vec(&metadata).unwrap()).unwrap();
    RuntimePaths::for_executable(&executable).unwrap()
}

fn canonical_v1_entry(executable: &Path, autostart: bool) -> String {
    // Actual pre-fix renderer output: canonical executable, not stable profile.
    format!(
        "[Desktop Entry]\nType=Application\nName=OpenWave\nComment=The audio mixing matrix for Linux\nExec={}{}\nIcon=openwave\nCategories=AudioVideo;Audio;Mixer;\nTerminal=false\nStartupWMClass=com.github.openwave\nX-GNOME-UsesNotifications=true\n{}",
        desktop::exec_arg(executable.to_str().unwrap()).unwrap(),
        if autostart { " --hide" } else { "" },
        if autostart {
            "X-GNOME-Autostart-enabled=true\n"
        } else {
            ""
        }
    )
}

fn migration_fixture() -> (Fixture, RuntimePaths, PathBuf, PathBuf) {
    let mut f = Fixture::new("none");
    f.host.uid = rustix::process::geteuid().as_raw();
    f.paths = migration_install(f.paths.prefix.as_ref().unwrap(), "v1");
    let current = migration_install(&f._temp.path().join("install-v2"), "v2");
    let profile = f._temp.path().join("profile");
    symlink(f.paths.prefix.as_ref().unwrap(), &profile).unwrap();
    f.host.durable_bins.push(profile.join("bin"));
    let menu = f.host.data_home.join("applications/openwave.desktop");
    let autostart = f.host.config_home.join("autostart/openwave.desktop");
    for (path, auto) in [(&menu, false), (&autostart, true)] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, canonical_v1_entry(&f.paths.executable, auto)).unwrap();
    }
    fs::remove_file(&profile).unwrap();
    symlink(current.prefix.as_ref().unwrap(), &profile).unwrap();
    (f, current, menu, autostart)
}

#[test]
fn canonical_v1_launchers_require_consent_then_survive_old_install_removal() {
    let (f, current, menu, autostart) = migration_fixture();
    let original = [fs::read(&menu).unwrap(), fs::read(&autostart).unwrap()];
    let inodes = [
        fs::metadata(&menu).unwrap().ino(),
        fs::metadata(&autostart).unwrap().ino(),
    ];
    assert!(desktop::ensure_menu_entry_with(&current, &f.host).is_err());
    assert!(desktop::set_autostart_with(&current, true, false, &f.host).is_err());
    let plan =
        desktop::inspect_launcher_migration_with(&current, &f.paths.executable, &f.host).unwrap();
    assert!(desktop::apply_launcher_migration_with(&current, &plan, false, &f.host).is_err());
    for (index, path) in [&menu, &autostart].iter().enumerate() {
        assert_eq!(fs::read(path).unwrap(), original[index]);
        assert_eq!(fs::metadata(path).unwrap().ino(), inodes[index]);
    }
    assert_eq!(
        desktop::apply_launcher_migration_with(&current, &plan, true, &f.host).unwrap(),
        2
    );
    assert_eq!(
        desktop::autostart_state_at(&autostart).unwrap(),
        (true, true)
    );
    assert!(desktop::set_autostart_with(&f.paths, false, false, &f.host).is_err());
    desktop::ensure_menu_entry_with(&current, &f.host).unwrap();
    desktop::set_autostart_with(&current, false, false, &f.host).unwrap();
    desktop::set_autostart_with(&current, true, true, &f.host).unwrap();
    fs::remove_dir_all(f.paths.prefix.as_ref().unwrap()).unwrap();
    assert_eq!(launch_fixture_entry(&menu), "v2");
    assert_eq!(launch_fixture_entry(&autostart), "v2");
}

#[test]
fn launcher_migration_preserves_disabled_and_hidden_intent() {
    let (f, current, menu, autostart) = migration_fixture();
    let disabled = format!(
        "{}Hidden=true\n",
        fs::read_to_string(&autostart).unwrap().replace(
            "X-GNOME-Autostart-enabled=true",
            "X-GNOME-Autostart-enabled=false"
        )
    );
    fs::write(&autostart, &disabled).unwrap();
    let plan =
        desktop::inspect_launcher_migration_with(&current, &f.paths.executable, &f.host).unwrap();
    desktop::apply_launcher_migration_with(&current, &plan, true, &f.host).unwrap();
    assert_eq!(
        desktop::autostart_state_at(&autostart).unwrap(),
        (false, true)
    );
    assert_eq!(launch_fixture_entry(&menu), "v2");
    assert_eq!(launch_fixture_entry(&autostart), "v2");
}

#[test]
fn launcher_migration_can_finish_a_partly_completed_handoff() {
    let (f, current, menu, autostart) = migration_fixture();
    let stable = f._temp.path().join("profile/bin/openwave");
    fs::write(&menu, canonical_v1_entry(&stable, false)).unwrap();
    let original = fs::read(&menu).unwrap();
    let inode = fs::metadata(&menu).unwrap().ino();
    let plan =
        desktop::inspect_launcher_migration_with(&current, &f.paths.executable, &f.host).unwrap();
    assert_eq!(
        desktop::apply_launcher_migration_with(&current, &plan, true, &f.host).unwrap(),
        1
    );
    assert_eq!(fs::read(&menu).unwrap(), original);
    assert_eq!(fs::metadata(&menu).unwrap().ino(), inode);
    assert_eq!(launch_fixture_entry(&autostart), "v2");
}

#[test]
fn launcher_migration_refuses_foreign_managed_linked_and_missing_authority() {
    let (f, current, menu, autostart) = migration_fixture();
    let original = fs::read(&menu).unwrap();
    let automatic = fs::read(&autostart).unwrap();
    fs::write(
        &menu,
        canonical_v1_entry(&f._temp.path().join("foreign/bin/openwave"), false),
    )
    .unwrap();
    let foreign = fs::read(&menu).unwrap();
    assert!(
        desktop::inspect_launcher_migration_with(&current, &f.paths.executable, &f.host).is_err()
    );
    assert_eq!(fs::read(&menu).unwrap(), foreign);
    fs::write(&menu, &original).unwrap();
    *fixture_lock(&f.commands.owner).unwrap() = Some(InstallMethod::Nix);
    assert!(
        desktop::inspect_launcher_migration_with(&current, &f.paths.executable, &f.host).is_err()
    );
    *fixture_lock(&f.commands.owner).unwrap() = None;
    let external = f._temp.path().join("external-entry");
    fs::rename(&menu, &external).unwrap();
    symlink(&external, &menu).unwrap();
    assert!(
        desktop::inspect_launcher_migration_with(&current, &f.paths.executable, &f.host).is_err()
    );
    assert_eq!(fs::read(&external).unwrap(), original);
    fs::remove_file(&menu).unwrap();
    fs::rename(&external, &menu).unwrap();
    fs::remove_file(f.paths.data.join("install-manifest.json")).unwrap();
    assert!(
        desktop::inspect_launcher_migration_with(&current, &f.paths.executable, &f.host).is_err()
    );
    assert_eq!(fs::read(&menu).unwrap(), original);
    assert_eq!(fs::read(&autostart).unwrap(), automatic);
}

#[test]
fn launcher_migration_rechecks_entry_inode_receipt_and_profile_after_confirmation() {
    let (f, current, menu, autostart) = migration_fixture();
    let original = fs::read(&menu).unwrap();
    let automatic = fs::read(&autostart).unwrap();
    let plan =
        desktop::inspect_launcher_migration_with(&current, &f.paths.executable, &f.host).unwrap();
    let replacement = menu.with_extension("replacement");
    fs::write(&replacement, &original).unwrap();
    fs::rename(&replacement, &menu).unwrap();
    assert!(desktop::apply_launcher_migration_with(&current, &plan, true, &f.host).is_err());
    let plan =
        desktop::inspect_launcher_migration_with(&current, &f.paths.executable, &f.host).unwrap();
    let receipt = f.paths.data.join("install-manifest.json");
    let bytes = fs::read(&receipt).unwrap();
    fs::write(&receipt, [bytes.as_slice(), b"\n"].concat()).unwrap();
    assert!(desktop::apply_launcher_migration_with(&current, &plan, true, &f.host).is_err());
    let plan =
        desktop::inspect_launcher_migration_with(&current, &f.paths.executable, &f.host).unwrap();
    let profile = f._temp.path().join("profile");
    fs::remove_file(&profile).unwrap();
    symlink(f.paths.prefix.as_ref().unwrap(), &profile).unwrap();
    assert!(desktop::apply_launcher_migration_with(&current, &plan, true, &f.host).is_err());
    assert_eq!(fs::read(&menu).unwrap(), original);
    assert_eq!(fs::read(&autostart).unwrap(), automatic);
}
#[test]
fn exact_bare_desktop_launcher_adoption_and_removal_require_current_resolution() {
    let f = Fixture::new("none");
    let path = f.host.config_home.join("autostart/openwave.desktop");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    *fixture_lock(&f.commands.launcher).unwrap() = Some(f.paths.executable.clone());
    for exec in ["openwave", "openwave --hide"] {
        let entry = format!("[Desktop Entry]\nType=Application\nExec={exec}\n");
        fs::write(&path, &entry).unwrap();
        desktop::set_autostart_with(&f.paths, true, true, &f.host).unwrap();
        assert_eq!(desktop::autostart_state_at(&path).unwrap(), (true, true));
        fs::write(&path, &entry).unwrap();
        desktop::remove_owned_with(&f.paths, &f.host).unwrap();
        assert!(!path.exists());
    }
    let foreign = f._temp.path().join("foreign-openwave");
    fs::write(&foreign, "foreign").unwrap();
    *fixture_lock(&f.commands.launcher).unwrap() = Some(foreign);
    let entry = "[Desktop Entry]\nType=Application\nExec=openwave --hide\n";
    fs::write(&path, entry).unwrap();
    assert!(desktop::set_autostart_with(&f.paths, true, false, &f.host).is_err());
    desktop::remove_owned_with(&f.paths, &f.host).unwrap();
    assert_eq!(fs::read_to_string(path).unwrap(), entry);
}

#[test]
fn desktop_refresh_preserves_package_and_linked_parent_entries() {
    let f = Fixture::new("none");
    desktop::ensure_menu_entry_with(&f.paths, &f.host).unwrap();
    let menu = f.host.data_home.join("applications/openwave.desktop");
    let before = fs::read(&menu).unwrap();
    let inode = fs::metadata(&menu).unwrap().ino();
    *fixture_lock(&f.commands.owner).unwrap() = Some(InstallMethod::Deb);
    assert!(desktop::ensure_menu_entry_with(&f.paths, &f.host).is_err());
    desktop::remove_owned_with(&f.paths, &f.host).unwrap();
    assert_eq!(fs::metadata(&menu).unwrap().ino(), inode);
    assert_eq!(fs::read(&menu).unwrap(), before);
    *fixture_lock(&f.commands.owner).unwrap() = None;

    let external = f._temp.path().join("external-autostart");
    fs::create_dir_all(&external).unwrap();
    fs::create_dir_all(&f.host.config_home).unwrap();
    symlink(&external, f.host.config_home.join("autostart")).unwrap();
    let path = external.join("openwave.desktop");
    fs::write(&path, &before).unwrap();
    let inode = fs::metadata(&path).unwrap().ino();
    assert!(desktop::set_autostart_with(&f.paths, true, false, &f.host).is_err());
    desktop::remove_owned_with(&f.paths, &f.host).unwrap();
    assert_eq!(fs::metadata(&path).unwrap().ino(), inode);
    assert_eq!(fs::read(&path).unwrap(), before);
}
#[test]
fn udev_admission_requires_meaning_not_pid_substrings() {
    let rules = setup::udev_rules();
    assert!(setup::udev_contents_cover(&rules));
    assert!(!setup::udev_contents_cover(
        &format!("# {rules}").replace('\n', "\n# ")
    ));
    assert!(!setup::udev_contents_cover(&rules.replace("0666", "0600")));
    assert!(!setup::udev_contents_cover(&rules.replace("0fd9", "1234")));
    assert!(!setup::udev_contents_cover(&rules.replace("007d", "00c7")));
    assert!(!setup::udev_contents_cover(
        &rules.replace("SUBSYSTEM==", "SUBSYSTEM!=")
    ));
    let reordered = "MODE = \"0666\", ATTR{idProduct} == \"007d\", SUBSYSTEM == \"usb\", ATTR{idVendor} == \"0fd9\"\nMODE=\"0666\", ATTR{idProduct}==\"00a6\", ATTR{idVendor}==\"0fd9\", SUBSYSTEM==\"usb\"\nSUBSYSTEM==\"usb\", ATTR{idVendor}==\"0fd9\", ATTR{idProduct}==\"0070\", MODE=\"0666\"\n";
    assert!(setup::udev_contents_cover(reordered));
}
#[test]
fn sandbox_and_unsupported_setup_do_not_mutate_native_integration() {
    let mut f = Fixture::new("none");
    f.host.sandboxed = true;
    assert_eq!(
        setup::run_with(&f.paths, &default_mixes(), &f.host)
            .unwrap_err()
            .code,
        ErrorCode::Unsupported
    );
    assert!(!setup::inspect_with(&f.paths, &f.host).unwrap().required);
    setup::write_mixes_conf_with(&default_mixes(), &f.host).unwrap();
    assert!(
        f.host
            .config_home
            .join("pipewire/pipewire.conf.d")
            .join(setup::MIXES_NAME)
            .is_file()
    );
    f.host.sandboxed = false;
    assert_eq!(
        setup::run_with(&f.paths, &default_mixes(), &f.host)
            .unwrap_err()
            .code,
        ErrorCode::Unsupported
    );
    assert!(!f.unit().exists());
    assert!(
        fixture_lock(&f.commands.calls)
            .expect("fixture synchronization failed")
            .is_empty()
    );
}
#[test]
fn corrupt_definitions_and_invalid_candidate_preserve_generated_file() {
    let f = Fixture::new("none");
    let target = f
        .host
        .config_home
        .join("pipewire/pipewire.conf.d")
        .join(setup::MIXES_NAME);
    setup::write_mixes_conf_with(&default_mixes(), &f.host).unwrap();
    let old = fs::read(&target).unwrap();
    let defs = f.host.config_home.join("openwave/mixdefs.json");
    fs::create_dir_all(defs.parent().unwrap()).unwrap();
    fs::write(&defs, "{not json").unwrap();
    assert_eq!(
        setup::write_mixes_conf_with(&default_mixes(), &f.host)
            .unwrap_err()
            .code,
        ErrorCode::CorruptStore
    );
    assert_eq!(fs::read(&target).unwrap(), old);
    assert_eq!(fs::read_to_string(&defs).unwrap(), "{not json");
    fs::write(&defs, "{}").unwrap();
    let mut invalid = default_mixes();
    invalid.values_mut().next().unwrap().sink = "openwave_loop_foreign".into();
    assert!(setup::write_mixes_conf_with(&invalid, &f.host).is_err());
    assert_eq!(fs::read(target).unwrap(), old);
}
#[test]
fn setup_publishes_audio_definitions_before_starting_only_capture_service() {
    let f = Fixture::new("systemctl");
    fs::write(
        f.host.udev_directory.join("99-openwave.rules"),
        setup::udev_rules(),
    )
    .unwrap();
    let wp = f
        .host
        .config_home
        .join("wireplumber/wireplumber.conf.d")
        .join(setup::WIREPLUMBER_NAME);
    let mixes = f
        .host
        .config_home
        .join("pipewire/pipewire.conf.d")
        .join(setup::MIXES_NAME);
    *fixture_lock(&f.commands.setup_files).expect("fixture synchronization failed") =
        vec![wp, mixes];
    let outcome = setup::run_with(&f.paths, &default_mixes(), &f.host).unwrap();
    assert!(!outcome.needs_replug);
    let calls = fixture_lock(&f.commands.calls).expect("fixture synchronization failed");
    assert!(
        calls
            .iter()
            .any(|(p, a)| p == "systemctl" && a == &["--user", "restart", "openwave.service"])
    );
    assert!(calls.iter().all(|(p, a)| {
        p == "systemctl"
            && !a
                .iter()
                .any(|s| s == "pipewire.service" || s == "wireplumber.service")
    }));
}

#[test]
fn newly_visible_foreign_dropin_blocks_first_service_start() {
    let f = Fixture::new("systemctl");
    f.observe(&f.unit(), Path::new("/foreign/openwave-daemon"), "");
    *fixture_lock(&f.commands.reload_show).expect("fixture synchronization failed") =
        fixture_lock(&f.commands.show)
            .expect("fixture synchronization failed")
            .clone();
    f.missing();
    assert_eq!(
        service::install_with(&f.paths, &f.host).unwrap_err().code,
        ErrorCode::Identity
    );
    let calls = fixture_lock(&f.commands.calls).expect("fixture synchronization failed");
    assert!(
        calls
            .iter()
            .any(|(_, args)| args.iter().any(|a| a == "daemon-reload"))
    );
    assert!(
        !calls
            .iter()
            .any(|(_, args)| args.iter().any(|a| a == "restart" || a == "enable"))
    );
}
#[test]
fn sink_definition_identity_changes_only_with_owned_definition() {
    let mut mixes = default_mixes();
    let mix = mixes.values_mut().next().unwrap();
    let before = setup::mix_definition_token(mix);
    mix.subtitle = "unrelated UI text".into();
    assert_eq!(setup::mix_definition_token(mix), before);
    mix.description = "quote \" } ] context.objects = [ \\ newline\n".into();
    let after = setup::mix_definition_token(mix);
    assert_ne!(after, before);
    let description = serde_json::to_string(&mix.description).unwrap();
    let rendered = setup::render_mixes_conf(&mixes).unwrap();
    assert!(rendered.contains(&format!("node.description = {description}")));
    assert!(rendered.contains(&format!("openwave.definition = \"{after}\"")));
}
