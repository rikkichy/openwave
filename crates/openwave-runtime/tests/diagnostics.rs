use openwave_core::model::{OperationError, Result};
use openwave_runtime::diag::{self, Collectors, Section};
use std::{
    cell::RefCell,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};

struct Fixture {
    calls: RefCell<Vec<Section>>,
}
impl Collectors for Fixture {
    fn collect(&self, section: Section, _full: bool) -> Result<String> {
        self.calls.borrow_mut().push(section);
        match section {
            Section::Versions => Err(OperationError::unavailable(
                "failed at '/srv/private folder/workspace'",
            )),
            Section::Device => Ok("authorized device details".into()),
            Section::Pipewire => Ok("survived collector failure".into()),
            _ => Ok("read-only fixture".into()),
        }
    }
}
#[test]
fn full_disclosure_never_grants_usb_and_journal_is_opt_in() {
    let fixture = Fixture {
        calls: RefCell::new(Vec::new()),
    };
    for full in [false, true] {
        fixture.calls.borrow_mut().clear();
        let report = diag::assemble_with(&fixture, full, false, "fixture");
        assert!(!fixture.calls.borrow().contains(&Section::Device));
        assert_eq!(fixture.calls.borrow().contains(&Section::Journal), full);
        assert!(report.contains("unavailable"));
        assert!(report.contains("survived collector failure"));
        assert!(!report.contains("private folder"));
    }
    let report = diag::assemble_with(&fixture, false, true, "fixture");
    assert!(report.contains("authorized device details"));
    assert!(fixture.calls.borrow().contains(&Section::Device));
}
#[test]
fn all_absolute_path_forms_are_removed_but_source_url_survives() {
    let text = "'/home/user/private/project' \"/tmp/private folder/audio.wav\" /opt/private/compiler file:///srv/private/data ~/private/stuff https://github.com/rikkichy/openwave";
    let output = diag::redact_paths(text);
    assert!(!output.contains("private"));
    assert!(!output.contains("/home/user"));
    assert!(output.contains("https://github.com/rikkichy/openwave"));
}
#[test]
fn private_graph_keeps_health_without_serial_description_or_arbitrary_state() {
    let graph = r#"[{"info":{"state":"running","props":{"node.name":"alsa_input.usb-Elgato_PRIVATE_SERIAL","node.description":"PRIVATE_DESCRIPTION"}}},{"info":{"state":"SECRET_STATE","props":{"node.name":"openwave_PRIVATE_NAME"}}}]"#;
    let private = diag::describe_pipewire(graph, false).unwrap();
    assert!(!private.contains("PRIVATE"));
    assert!(!private.contains("SECRET_STATE"));
    assert!(private.contains("running"));
    let full = diag::describe_pipewire(graph, true).unwrap();
    assert!(full.contains("PRIVATE_SERIAL"));
    assert!(full.contains("PRIVATE_DESCRIPTION"));
    assert!(diag::describe_pipewire("{", false).is_err());
    assert!(diag::describe_pipewire("{}", false).is_err());
}
#[test]
fn config_summary_withholds_bodies_and_malformed_private_values() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("sources.json"),
        r#"{"name":"SECRET_APP_NAME"}"#,
    )
    .unwrap();
    fs::write(root.path().join("ui-state.json"), "SECRET_BROKEN_CONTENT").unwrap();
    let private = diag::collect_configs(root.path(), false).unwrap();
    assert!(!private.contains("SECRET"));
    assert!(private.contains("BROKEN"));
    assert!(private.contains("mixdefs.json: absent"));
    let full = diag::collect_configs(root.path(), true).unwrap();
    assert!(full.contains("SECRET_APP_NAME"));
    assert_eq!(
        fs::read_to_string(root.path().join("ui-state.json")).unwrap(),
        "SECRET_BROKEN_CONTENT"
    );
}
#[test]
fn export_is_private_absolute_and_cannot_clobber_file_or_symlink() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("report.txt");
    assert!(diag::export(&path, "safe report").unwrap().is_absolute());
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(diag::export(&path, "replacement").is_err());
    let link = root.path().join("report-link.txt");
    symlink(&path, &link).unwrap();
    assert!(diag::export(&link, "replacement").is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), "safe report");
}
#[test]
fn native_informational_cli_needs_no_runtime_environment() {
    for executable in [
        env!("CARGO_BIN_EXE_openwave-diag"),
        env!("CARGO_BIN_EXE_openwave-probe"),
    ] {
        for flag in ["--help", "--version"] {
            let output = std::process::Command::new(executable)
                .env_clear()
                .arg(flag)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            if flag == "--version" {
                assert!(String::from_utf8_lossy(&output.stdout).contains(openwave_core::VERSION));
            }
        }
    }
}
