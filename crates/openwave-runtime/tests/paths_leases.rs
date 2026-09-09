use openwave_core::{VERSION, model::ErrorCode};
use openwave_runtime::paths::{self, Lease, RuntimePaths};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::Command,
};

fn executable(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, b"inert native layout fixture").unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}
fn installed(root: &Path) {
    executable(&root.join("bin/openwave"));
    executable(&root.join("bin/openwave-daemon"));
    executable(&root.join("libexec/openwave-maintenance"));
    fs::create_dir_all(root.join("share/openwave/pipewire")).unwrap();
    fs::write(root.join("share/openwave/VERSION"), format!("{VERSION}\n")).unwrap();
    fs::write(
        root.join("share/openwave/pipewire/example.conf"),
        b"fixture",
    )
    .unwrap();
}
fn fixture(root: &Path, mode: &str) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "environment_fixture", "--nocapture"])
        .env("OPENWAVE_PATH_FIXTURE", mode)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", "")
        .env("XDG_DATA_HOME", "")
        .env("XDG_STATE_HOME", "")
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .current_dir("/")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn moved_stage_uses_its_own_assets_not_receipt_or_cwd() {
    let directory = tempfile::tempdir().unwrap();
    let original = directory.path().join("original/usr");
    installed(&original);
    fs::write(
        original.join("share/openwave/install-manifest.json"),
        br#"{"prefix":"/usr"}"#,
    )
    .unwrap();
    let moved = directory.path().join("moved");
    fs::rename(directory.path().join("original"), &moved).unwrap();
    let paths = RuntimePaths::for_executable(&moved.join("usr/bin/openwave")).unwrap();
    assert_eq!(paths.identity, moved.join("usr/share/openwave"));
    assert_eq!(
        paths.bin_file("openwave-daemon").unwrap(),
        moved.join("usr/bin/openwave-daemon")
    );
    assert_eq!(
        fs::read(paths.data_file("pipewire/example.conf").unwrap()).unwrap(),
        b"fixture"
    );
    assert!(paths.data_file("../../outside").is_err());
    fs::write(moved.join("outside"), b"outside").unwrap();
    symlink(moved.join("outside"), paths.data.join("escape")).unwrap();
    assert!(paths.data_file("escape").is_err());
    fixture(directory.path(), "discover-moved");
}

#[test]
fn rejects_unrelated_binary_and_wrong_version_witness() {
    let directory = tempfile::tempdir().unwrap();
    installed(directory.path());
    fs::write(
        directory.path().join("share/openwave/VERSION"),
        b"999.0.0\n",
    )
    .unwrap();
    assert!(RuntimePaths::for_executable(&directory.path().join("bin/openwave")).is_err());
    executable(&directory.path().join("unrelated"));
    assert!(RuntimePaths::for_executable(&directory.path().join("unrelated")).is_err());
}

#[test]
fn source_layout_requires_version_and_sibling_helper() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("crates/openwave-runtime")).unwrap();
    fs::write(directory.path().join("Cargo.toml"), b"[workspace]\n").unwrap();
    fs::write(
        directory.path().join("crates/openwave-runtime/Cargo.toml"),
        b"[package]\n",
    )
    .unwrap();
    fs::write(directory.path().join("VERSION"), VERSION).unwrap();
    let binary = directory.path().join("target/debug/openwave");
    executable(&binary);
    assert!(RuntimePaths::for_executable(&binary).is_err());
    executable(&directory.path().join("target/debug/openwave-maintenance"));
    let paths = RuntimePaths::for_executable(&binary).unwrap();
    assert_eq!(paths.source.as_deref(), Some(directory.path()));
    assert_eq!(paths.identity, directory.path());
}

#[test]
fn compiled_target_finds_sibling_helper_from_unrelated_cwd() {
    let directory = tempfile::tempdir().unwrap();
    fixture(directory.path(), "compiled-build");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[test]
fn empty_xdg_is_read_only_and_leases_exclude_without_unlinking() {
    let directory = tempfile::tempdir().unwrap();
    fixture(directory.path(), "read-only");
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    fixture(directory.path(), "leases");
}

#[test]
fn runtime_falls_back_but_never_adopts_symlink_or_unsafe_lock() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("runtime")).unwrap();
    fs::set_permissions(
        directory.path().join("runtime"),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    fixture(directory.path(), "fallback");
    fs::set_permissions(
        directory.path().join("runtime"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fixture(directory.path(), "unsafe-lock");
}

#[test]
fn elevated_helper_rejects_user_or_writable_or_symlink_authority() {
    let directory = tempfile::tempdir().unwrap();
    let helper = directory.path().join("helper");
    executable(&helper);
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(paths::trusted_for_root(&helper).is_err());
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).unwrap();
    let link = directory.path().join("link");
    symlink(&helper, &link).unwrap();
    assert!(paths::trusted_for_root(&link).is_err());
    assert!(paths::trusted_for_root(&directory.path().join("missing")).is_err());
    if !rustix::process::geteuid().is_root() {
        assert!(paths::trusted_for_root(&helper).is_err());
    }
}

#[test]
fn private_bus_owner_is_checked_without_activating_gui() {
    use glib::variant::ToVariant;
    use openwave_runtime::process::OwnedChild;
    use std::{
        io::{BufRead, BufReader},
        process::Stdio,
    };
    if rustix::process::geteuid().is_root() {
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let address = format!("unix:path={}", directory.path().join("bus").display());
    let config = directory.path().join("bus.conf");
    fs::write(&config, format!(
        "<busconfig><type>session</type><listen>{}</listen><auth>EXTERNAL</auth><policy context=\"default\"><allow send_destination=\"*\"/><allow receive_sender=\"*\"/><allow own=\"*\"/></policy></busconfig>",
        glib::markup_escape_text(&address),
    )).unwrap();
    let mut daemon = OwnedChild::spawn(
        "dbus-daemon",
        &[
            format!("--config-file={}", config.display()),
            "--nofork".into(),
            "--print-address=1".into(),
        ],
        Stdio::piped(),
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(daemon.take_stdout().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.starts_with("unix:"));
    let connection = gio::DBusConnection::for_address_sync(
        line.trim(),
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
            | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None,
        gio::Cancellable::NONE,
    )
    .unwrap();
    let child = |mode: &str, owner: &str| {
        let output = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "environment_fixture", "--nocapture"])
            .env("OPENWAVE_PATH_FIXTURE", mode)
            .env("OPENWAVE_ALLOWED_OWNER", owner)
            .env("HOME", directory.path())
            .env("XDG_CONFIG_HOME", "")
            .env("XDG_DATA_HOME", "")
            .env("XDG_STATE_HOME", "")
            .env("XDG_RUNTIME_DIR", directory.path().join("runtime"))
            .env("DBUS_SESSION_BUS_ADDRESS", line.trim())
            .current_dir("/")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    child("bus-empty", "");
    let requested = connection
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "RequestName",
            Some(&("com.github.openwave", 4_u32).to_variant()),
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            2000,
            gio::Cancellable::NONE,
        )
        .unwrap();
    assert_eq!(requested.get::<(u32,)>(), Some((1,)));
    child("bus-refuse", "");
    child("bus-allow", connection.unique_name().unwrap().as_str());
    daemon.terminate().unwrap();
}

#[test]
fn environment_fixture() {
    let Ok(mode) = std::env::var("OPENWAVE_PATH_FIXTURE") else {
        return;
    };
    let root = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    match mode.as_str() {
        "compiled-build" => {
            let found = RuntimePaths::discover().unwrap();
            let source =
                fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
            assert_eq!(found.source.as_ref(), Some(&source));
            assert_eq!(
                fs::read_to_string(found.data_file("VERSION").unwrap())
                    .unwrap()
                    .trim(),
                VERSION
            );
            let binary_dir = std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .to_owned();
            assert_eq!(found.maintenance, binary_dir.join("openwave-maintenance"));
            assert!(paths::maintenance_executable().unwrap().is_file());
        }
        "bus-empty" => {
            let lease = Lease::vendor_control(None).unwrap();
            assert_eq!(
                Lease::vendor_control(None).unwrap_err().code,
                ErrorCode::Busy
            );
            drop(lease);
        }
        "bus-refuse" => {
            assert_eq!(
                Lease::vendor_control(None).unwrap_err().code,
                ErrorCode::Busy
            );
        }
        "bus-allow" => {
            let owner = std::env::var("OPENWAVE_ALLOWED_OWNER").unwrap();
            drop(Lease::vendor_control(Some(&owner)).unwrap());
        }
        "exclusive-busy" => {
            assert_eq!(
                Lease::installation_exclusive(&root).unwrap_err().code,
                ErrorCode::Busy
            );
        }
        "shared-busy" => {
            assert_eq!(
                Lease::installation_shared(&root).unwrap_err().code,
                ErrorCode::Busy
            );
        }
        "discover-moved" => {
            let paths = RuntimePaths::for_executable(&root.join("moved/usr/bin/openwave")).unwrap();
            assert_eq!(paths.data, root.join("moved/usr/share/openwave"));
        }
        "read-only" => {
            assert_eq!(paths::config_dir().unwrap(), root.join(".config/openwave"));
            assert_eq!(
                paths::data_dir().unwrap(),
                root.join(".local/share/openwave")
            );
            assert_eq!(
                paths::state_dir().unwrap(),
                root.join(".local/state/openwave")
            );
            assert_eq!(
                paths::runtime_private_dir().unwrap(),
                root.join(".local/state/openwave/runtime")
            );
        }
        "leases" => {
            let first = Lease::installation_shared(&root).unwrap();
            let second = Lease::installation_shared(&root).unwrap();
            assert_eq!(
                Lease::installation_exclusive(&root).unwrap_err().code,
                ErrorCode::Busy
            );
            fixture(&root, "exclusive-busy");
            let lock = first.path.clone();
            drop(first);
            drop(second);
            assert!(lock.exists());
            let exclusive = Lease::installation_exclusive(&root).unwrap();
            assert_eq!(
                Lease::installation_shared(&root).unwrap_err().code,
                ErrorCode::Busy
            );
            fixture(&root, "shared-busy");
            drop(exclusive);
            let daemon = Lease::capture_daemon().unwrap();
            let busy = Lease::capture_daemon().unwrap_err();
            assert_eq!(busy.code, ErrorCode::Busy);
            assert!(busy.message.contains(&std::process::id().to_string()));
            assert_eq!(
                Lease::vendor_control(None).unwrap_err().code,
                ErrorCode::Unavailable
            );
            let private = daemon.path.parent().unwrap();
            assert_eq!(
                fs::metadata(private).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(private.join("vendor-control.lock"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            drop(daemon);
        }
        "fallback" => {
            let lease = Lease::capture_daemon().unwrap();
            assert_eq!(
                lease.path.parent().unwrap(),
                root.join(".local/state/openwave/runtime")
            );
            assert!(!root.join("runtime/openwave").exists());
        }
        "unsafe-lock" => {
            let private = root.join("runtime/openwave");
            fs::create_dir(&private).unwrap();
            fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
            let unrelated = root.join("unrelated");
            fs::write(&unrelated, b"preserve").unwrap();
            symlink(&unrelated, private.join("vendor-control.lock")).unwrap();
            assert!(Lease::vendor_control(None).is_err());
            assert_eq!(fs::read(&unrelated).unwrap(), b"preserve");
            fs::write(private.join("capture-daemon.lock"), b"wrong permissions").unwrap();
            fs::set_permissions(
                private.join("capture-daemon.lock"),
                fs::Permissions::from_mode(0o644),
            )
            .unwrap();
            assert!(Lease::capture_daemon().is_err());
        }
        _ => panic!("unknown private fixture mode"),
    }
}
