use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

fn private(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn selectors(home: &Path) -> Environment {
    Environment::from([
        ("HOME".into(), home.as_os_str().to_owned()),
        ("XDG_RUNTIME_DIR".into(), "".into()),
        ("XDG_STATE_HOME".into(), "".into()),
        ("XDG_CONFIG_HOME".into(), "".into()),
        ("XDG_DATA_HOME".into(), "".into()),
    ])
}
fn derive(home: &Path, values: &Environment, direct: bool) -> Result<OriginalUserEnvironment> {
    derive_environment(
        rustix::process::geteuid().as_raw(),
        rustix::process::getegid().as_raw(),
        home,
        values,
        direct,
    )
}
fn handoff(home: &Path, values: &Environment, expected: &Path, direct: bool) {
    let identity = home.join("installation");
    private(&identity);
    let original = derive(home, values, direct).unwrap();
    assert_eq!(original.directory, expected);
    assert!(open_existing_exclusive(&identity, original.uid, &original.directory).is_err());
    assert!(
        !expected.exists(),
        "root-side lookup must not create the user directory"
    );
    let mut command = Command::new(std::env::current_exe().unwrap());
    original.configure(&mut command);
    let output = command
        .args([
            "--exact",
            "paths::privilege::tests::login_fixture",
            "--nocapture",
        ])
        .env("OPENWAVE_LOGIN_FIXTURE", &identity)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let user_path = expected.join(installation_lock_name(&identity, true).unwrap());
    let before = fs::metadata(&user_path).unwrap();
    let root_view = open_existing_exclusive(&identity, original.uid, &original.directory).unwrap();
    assert_eq!(root_view.path, user_path);
    let after = fs::metadata(&root_view.path).unwrap();
    assert_eq!(
        (
            before.dev(),
            before.ino(),
            before.uid(),
            before.mode() & 0o7777
        ),
        (after.dev(), after.ino(), original.uid, 0o600)
    );
    drop(root_view);
    fs::remove_dir(&identity).unwrap();
    // A completed/partially removed installation still uses the same lock.
    let retry = open_existing_exclusive(
        &identity,
        original.uid,
        &derive(home, values, direct).unwrap().directory,
    )
    .unwrap();
    assert_eq!(retry.path, user_path);
    assert_eq!(fs::metadata(&retry.path).unwrap().ino(), before.ino());
}

#[test]
fn login_fixture() {
    let Some(identity) = env::var_os("OPENWAVE_LOGIN_FIXTURE").map(PathBuf::from) else {
        return;
    };
    let lease = Lease::installation_exclusive(&identity).unwrap();
    assert_eq!(
        open_existing_exclusive(
            &identity,
            rustix::process::geteuid().as_raw(),
            lease.path.parent().unwrap()
        )
        .unwrap_err()
        .code,
        ErrorCode::Busy
    );
}

#[test]
fn custom_runtime_handoff_and_deleted_identity_retry_share_user_inode() {
    for direct in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let runtime = home.path().join("custom-runtime");
        private(&runtime);
        let mut values = selectors(home.path());
        values.insert("XDG_RUNTIME_DIR".into(), runtime.clone().into_os_string());
        handoff(home.path(), &values, &runtime.join("openwave"), direct);
    }
}

#[test]
fn custom_state_fallback_and_direct_retry_share_user_inode() {
    for direct in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let state = home.path().join("custom-state");
        let mut values = selectors(home.path());
        values.insert("XDG_STATE_HOME".into(), state.clone().into_os_string());
        handoff(
            home.path(),
            &values,
            &state.join("openwave/runtime"),
            direct,
        );
    }
}

#[test]
fn empty_xdg_handoff_uses_home_fallback_without_standard_runtime_override() {
    let home = tempfile::tempdir().unwrap();
    handoff(
        home.path(),
        &selectors(home.path()),
        &home.path().join(".local/state/openwave/runtime"),
        true,
    );
}

#[test]
fn invalid_runtime_falls_back_consistently_but_unsafe_state_blocks() {
    let home = tempfile::tempdir().unwrap();
    let runtime = home.path().join("runtime");
    private(&runtime);
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o777)).unwrap();
    let mut values = selectors(home.path());
    values.insert("XDG_RUNTIME_DIR".into(), runtime.clone().into_os_string());
    handoff(
        home.path(),
        &values,
        &home.path().join(".local/state/openwave/runtime"),
        false,
    );
    fs::remove_dir(&runtime).unwrap();
    symlink(home.path(), &runtime).unwrap();
    assert_eq!(
        derive(home.path(), &values, false).unwrap().directory,
        home.path().join(".local/state/openwave/runtime")
    );
    values.insert("XDG_STATE_HOME".into(), runtime.into_os_string());
    assert!(derive(home.path(), &values, false).is_err());
    let uid = rustix::process::geteuid().as_raw();
    assert!(private_directory_for_uid(home.path(), false, uid.wrapping_add(1)).is_err());
    assert!(
        derive_environment(
            uid.wrapping_add(1),
            0,
            home.path(),
            &selectors(home.path()),
            false
        )
        .is_err()
    );
}

#[test]
fn existing_lock_opener_never_creates_or_adopts_unsafe_files() {
    let home = tempfile::tempdir().unwrap();
    let uid = rustix::process::geteuid().as_raw();
    let directory = home.path().join("private");
    private(&directory);
    let identity = home.path().join("identity");
    private(&identity);
    let path = directory.join(installation_lock_name(&identity, true).unwrap());
    assert!(open_existing_exclusive(&identity, uid, &directory).is_err());
    assert!(!path.exists());
    let outside = home.path().join("outside");
    fs::write(&outside, b"preserve").unwrap();
    symlink(&outside, &path).unwrap();
    assert!(open_existing_exclusive(&identity, uid, &directory).is_err());
    assert_eq!(fs::read(&outside).unwrap(), b"preserve");
    fs::remove_file(&path).unwrap();
    fs::write(&path, b"unsafe mode").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(open_existing_exclusive(&identity, uid, &directory).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&path, home.path().join("alias")).unwrap();
    assert!(open_existing_exclusive(&identity, uid, &directory).is_err());
    assert!(open_existing_exclusive(&identity, uid.wrapping_add(1), &directory).is_err());
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(open_existing_exclusive(&identity, uid, &directory).is_err());
}

#[test]
fn deleted_identity_with_replaced_ancestor_and_unknown_home_block() {
    let home = tempfile::tempdir().unwrap();
    let identity = home.path().join("installation/identity");
    private(&identity);
    let lock = installation_lock_name(&identity, true).unwrap();
    fs::remove_dir(&identity).unwrap();
    assert_eq!(installation_lock_name(&identity, true).unwrap(), lock);
    fs::remove_dir(identity.parent().unwrap()).unwrap();
    symlink(home.path(), identity.parent().unwrap()).unwrap();
    assert!(installation_lock_name(&identity, true).is_err());
    let mut values = selectors(home.path());
    values.insert(
        "HOME".into(),
        home.path().join("missing-home").into_os_string(),
    );
    assert!(derive(home.path(), &values, false).is_err());
}

#[test]
fn inaccessible_identity_is_not_treated_as_already_deleted() {
    if rustix::process::geteuid().is_root() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let parent = home.path().join("blocked");
    let identity = parent.join("identity");
    private(&identity);
    fs::set_permissions(&parent, fs::Permissions::from_mode(0)).unwrap();
    let result = installation_lock_name(&identity, true);
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.is_err());
}

#[test]
fn custom_user_selectors_preserve_service_location_and_forbid_bus_autolaunch() {
    let home = tempfile::tempdir().unwrap();
    let mut values = selectors(home.path());
    values.insert(
        "XDG_CONFIG_HOME".into(),
        home.path().join("configuration").into_os_string(),
    );
    values.insert(
        "XDG_DATA_HOME".into(),
        home.path().join("data").into_os_string(),
    );
    values.insert(
        "DBUS_SESSION_BUS_ADDRESS".into(),
        "unix:abstract=openwave-fixture".into(),
    );
    values.insert("LD_PRELOAD".into(), "/untrusted/loader".into());
    values.insert("PATH".into(), "/untrusted/bin".into());
    let original = derive(home.path(), &values, false).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    original.configure(&mut command);
    let output = command
        .args([
            "--exact",
            "paths::privilege::tests::selectors_fixture",
            "--nocapture",
        ])
        .env("OPENWAVE_SELECTORS_FIXTURE", home.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    values.insert("DBUS_SESSION_BUS_ADDRESS".into(), "autolaunch:".into());
    assert!(derive(home.path(), &values, false).is_err());
    values.insert(
        "DBUS_SESSION_BUS_ADDRESS".into(),
        "unix:path=/some/bus;autolaunch:".into(),
    );
    assert!(derive(home.path(), &values, false).is_err());
}

#[test]
fn selectors_fixture() {
    let Some(home) = env::var_os("OPENWAVE_SELECTORS_FIXTURE").map(PathBuf::from) else {
        return;
    };
    assert_eq!(config_dir().unwrap(), home.join("configuration/openwave"));
    assert_eq!(data_dir().unwrap(), home.join("data/openwave"));
    assert_eq!(
        env::var("DBUS_SESSION_BUS_ADDRESS").unwrap(),
        "unix:abstract=openwave-fixture"
    );
    assert!(env::var_os("LD_PRELOAD").is_none());
    assert_eq!(env::var("PATH").unwrap(), TOOL_PATH);
}
