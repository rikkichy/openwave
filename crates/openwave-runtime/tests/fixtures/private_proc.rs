use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};

const SESSION_RESOURCES: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "DBUS_SYSTEM_BUS_ADDRESS",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "PULSE_SERVER",
    "PIPEWIRE_REMOTE",
];

// A nested proc mount may not expose kernel-global entries masked by the
// outer sandbox. Reuse that mount only after proving its PID/user ownership;
// the opt-in is not itself evidence and never grants a host fallback.
pub fn require_private_proc() {
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
    for name in SESSION_RESOURCES {
        assert!(
            std::env::var_os(name).is_none(),
            "Unexpected session resource: {name}"
        );
    }
}

pub fn fixture_command(root: &Path) -> Command {
    let executable = std::env::current_exe().unwrap();
    let mut command = if std::env::var_os("OPENWAVE_TEST_PRIVATE_PROC").is_some() {
        require_private_proc();
        Command::new(&executable)
    } else {
        let bwrap = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|directory| directory.join("bwrap"))
            .find(|path| path.is_file())
            .expect("uninstall process-ownership fixtures require bubblewrap from nix develop");
        let mut command = Command::new(bwrap);
        command.args([
            "--unshare-all",
            "--as-pid-1",
            "--die-with-parent",
            "--new-session",
            "--cap-drop",
            "ALL",
        ]);
        // Match the installed smoke's runtime mounts, not the host root: no
        // sysfs, host devices, home directory or session sockets are exposed.
        for path in ["/nix/store", "/usr", "/bin", "/sbin", "/lib", "/lib64"] {
            if Path::new(path).exists() {
                command.args(["--ro-bind", path, path]);
            }
        }
        command.args(["--dir", "/etc"]);
        for path in [
            "/etc/ld.so.cache",
            "/etc/ld.so.conf",
            "/etc/ld.so.conf.d",
            "/etc/passwd",
            "/etc/group",
        ] {
            if Path::new(path).exists() {
                command.args(["--ro-bind", path, path]);
            }
        }
        command.args(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp"]);
        if Path::new("/proc/asound").exists() {
            command.args(["--tmpfs", "/proc/asound"]);
        }
        let binaries = executable.parent().unwrap().parent().unwrap();
        command
            .arg("--ro-bind")
            .arg(binaries)
            .arg(binaries)
            .arg("--bind")
            .arg(root)
            .arg(root)
            .args(["--chdir", "/"])
            .arg(&executable);
        command
    };
    for name in SESSION_RESOURCES {
        command.env_remove(name);
    }
    command.env_remove("FLATPAK_ID").env("TMPDIR", "/tmp");
    command
}
