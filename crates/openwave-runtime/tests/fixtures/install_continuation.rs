//! Private unit fixtures: stop at real durable publication boundaries without
//! fault-injection controls in the elevated executable. Never run on the host.
use super::*;
use crate::installation::{InstallMethod, record_install, validate_installation};
use std::os::unix::fs::symlink;

struct Fixture {
    _root: tempfile::TempDir,
    prefix: PathBuf,
    stage: PathBuf,
    staged_prefix: PathBuf,
    helper: PathBuf,
}

fn put(path: &Path, bytes: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn payload(prefix: &Path, revision: &[u8]) {
    for relative in NATIVE_PAYLOAD {
        let bytes = if *relative == "share/openwave/VERSION" {
            openwave_core::VERSION.as_bytes()
        } else {
            revision
        };
        put(&prefix.join(relative), bytes, final_mode(relative));
    }
}

impl Fixture {
    fn new(upgrade: bool) -> Self {
        assert_eq!(
            std::env::var("OPENWAVE_DISPOSABLE_ROOT").as_deref(),
            Ok("1"),
            "disposable root filesystem required; never elevate the host test suite"
        );
        assert!(rustix::process::geteuid().is_root());
        // /tmp has writable ancestry and is intentionally not a trusted root.
        let root = tempfile::Builder::new()
            .prefix("openwave-continuation-")
            .tempdir_in("/")
            .unwrap();
        let prefix = root.path().join("prefix");
        let stage = root.path().join("stage");
        let staged_prefix = stage.join(prefix.strip_prefix("/").unwrap());
        let helper = prefix.join("libexec/openwave-bootstrap-fixture/openwave-maintenance");
        put(&helper, b"\x7fELFtrusted fixture; never executed", 0o755);
        fs::set_permissions(helper.parent().unwrap(), fs::Permissions::from_mode(0o755)).unwrap();
        if upgrade {
            payload(&prefix, b"\x7fELFold payload");
            record_install(&prefix, None, InstallMethod::Manual).unwrap();
        }
        payload(&staged_prefix, b"\x7fELFnew payload");
        record_install(&prefix, Some(&stage), InstallMethod::Manual).unwrap();
        Self {
            _root: root,
            prefix,
            stage,
            staged_prefix,
            helper,
        }
    }

    fn hash(&self) -> String {
        file_digest(&self.staged_prefix.join(RECEIPT)).unwrap()
    }
    fn prepare(&self) -> Result<Pending> {
        prepare(&self.stage, &self.prefix, &self.hash(), &self.helper)
    }
    fn authority(&self) -> PathBuf {
        self.helper.parent().unwrap().join(AUTHORITY)
    }

    fn assert_complete(&self) {
        for relative in NATIVE_PAYLOAD {
            let path = self.prefix.join(relative);
            assert_eq!(
                fs::read(&path).unwrap(),
                fs::read(self.staged_prefix.join(relative)).unwrap()
            );
            assert_eq!(
                fs::metadata(path).unwrap().mode() & 0o7777,
                final_mode(relative)
            );
        }
        assert_eq!(
            fs::read(self.prefix.join(RECEIPT)).unwrap(),
            fs::read(self.staged_prefix.join(RECEIPT)).unwrap()
        );
        let snapshot =
            snapshot_from_receipt(&self.prefix.join(RECEIPT), &self.hash(), &self.prefix).unwrap();
        validate_installation(&snapshot).unwrap();
        assert!(!self.authority().exists());
        assert!(self.helper.is_file());
        assert_eq!(
            fs::read_dir(self.helper.parent().unwrap()).unwrap().count(),
            1,
            "only the retained helper remains for installer cleanup"
        );
    }
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 in a disposable trusted-root filesystem"]
fn fresh_continuation_survives_one_published_file_without_adopting_it_normally() {
    let fixture = Fixture::new(false);
    let mut pending = fixture.prepare().unwrap();
    assert_eq!(
        fs::metadata(fixture.authority()).unwrap().mode() & 0o7777,
        0o600
    );
    pending.publish_source(0).unwrap();
    drop(pending);
    assert!(!fixture.prefix.join(RECEIPT).exists());
    assert!(check_install_target(&fixture.prefix, None).is_err());
    fixture.prepare().unwrap().finish().unwrap();
    fixture.assert_complete();
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 in a disposable trusted-root filesystem"]
fn upgrade_continuation_accepts_its_changed_file_but_not_normal_old_receipt_validation() {
    let fixture = Fixture::new(true);
    let old_hash = file_digest(&fixture.prefix.join(RECEIPT)).unwrap();
    let mut pending = fixture.prepare().unwrap();
    pending.publish_source(0).unwrap();
    drop(pending);
    assert_eq!(
        file_digest(&fixture.prefix.join(RECEIPT)).unwrap(),
        old_hash
    );
    assert!(check_install_target(&fixture.prefix, None).is_err());
    fixture.prepare().unwrap().finish().unwrap();
    fixture.assert_complete();
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 in a disposable trusted-root filesystem"]
fn completed_receipt_with_retained_authority_can_finish_cleanup() {
    let fixture = Fixture::new(true);
    let mut pending = fixture.prepare().unwrap();
    for index in 0..pending.sources.len() {
        pending.publish_source(index).unwrap();
    }
    pending.publish_receipt().unwrap();
    drop(pending); // Crash or authority-unlink failure after the durable receipt.
    assert!(fixture.authority().is_file());
    snapshot_from_receipt(
        &fixture.prefix.join(RECEIPT),
        &fixture.hash(),
        &fixture.prefix,
    )
    .unwrap();
    fixture.prepare().unwrap().finish().unwrap();
    fixture.assert_complete();
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 in a disposable trusted-root filesystem"]
fn continuation_rejects_third_hash_and_preserves_foreign_bytes() {
    let fixture = Fixture::new(false);
    let mut pending = fixture.prepare().unwrap();
    let path = pending.sources[0].destination.clone();
    pending.publish_source(0).unwrap();
    drop(pending);
    put(&path, b"foreign replacement", 0o755);
    assert!(fixture.prepare().is_err());
    assert_eq!(fs::read(path).unwrap(), b"foreign replacement");
    assert!(fixture.authority().exists());
    assert!(!fixture.prefix.join(RECEIPT).exists());
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 in a disposable trusted-root filesystem"]
fn continuation_rejects_replaced_source_even_with_identical_bytes() {
    let fixture = Fixture::new(false);
    let pending = fixture.prepare().unwrap();
    let source = fixture.staged_prefix.join(NATIVE_PAYLOAD[0]);
    drop(pending);
    let replacement = source.with_extension("replacement");
    put(
        &replacement,
        &fs::read(&source).unwrap(),
        final_mode(NATIVE_PAYLOAD[0]),
    );
    fs::rename(replacement, source).unwrap();
    assert!(fixture.prepare().is_err());
    assert!(!fixture.prefix.join(NATIVE_PAYLOAD[0]).exists());
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 in a disposable trusted-root filesystem"]
fn continuation_rejects_new_receipt_acceptance_and_other_stage() {
    let fixture = Fixture::new(false);
    drop(fixture.prepare().unwrap());
    let other_stage = fixture._root.path().join("other-stage");
    let other_prefix = other_stage.join(fixture.prefix.strip_prefix("/").unwrap());
    payload(&other_prefix, b"\x7fELFnew payload");
    record_install(&fixture.prefix, Some(&other_stage), InstallMethod::Manual).unwrap();
    assert!(
        prepare(
            &other_stage,
            &fixture.prefix,
            &fixture.hash(),
            &fixture.helper
        )
        .is_err()
    );
    put(
        &fixture.staged_prefix.join("share/openwave/style.css"),
        b"changed source",
        0o644,
    );
    record_install(&fixture.prefix, Some(&fixture.stage), InstallMethod::Manual).unwrap();
    assert!(fixture.prepare().is_err());
    assert!(!fixture.prefix.join(RECEIPT).exists());
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 in a disposable trusted-root filesystem"]
fn continuation_does_not_adopt_another_bootstrap_authority() {
    let fixture = Fixture::new(false);
    drop(fixture.prepare().unwrap());
    let helper = fixture
        .prefix
        .join("libexec/openwave-bootstrap-other/openwave-maintenance");
    put(&helper, &fs::read(&fixture.helper).unwrap(), 0o755);
    put(
        &helper.parent().unwrap().join(AUTHORITY),
        &fs::read(fixture.authority()).unwrap(),
        0o600,
    );
    assert!(prepare(&fixture.stage, &fixture.prefix, &fixture.hash(), &helper).is_err());
    assert!(!fixture.prefix.join(RECEIPT).exists());
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 in a disposable trusted-root filesystem"]
fn continuation_rejects_missing_original_file_and_identical_old_replacement() {
    let fixture = Fixture::new(true);
    drop(fixture.prepare().unwrap());
    let original = fixture.prefix.join(NATIVE_PAYLOAD[0]);
    let saved = original.with_extension("saved");
    fs::rename(&original, &saved).unwrap();
    assert!(fixture.prepare().is_err());
    put(
        &original,
        &fs::read(&saved).unwrap(),
        final_mode(NATIVE_PAYLOAD[0]),
    );
    assert!(fixture.prepare().is_err());
    assert_eq!(fs::read(original).unwrap(), b"\x7fELFold payload");
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 in a disposable trusted-root filesystem"]
fn continuation_rejects_authority_mode_symlink_and_destination_directory_replacement() {
    let fixture = Fixture::new(false);
    drop(fixture.prepare().unwrap());
    fs::set_permissions(fixture.authority(), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(fixture.prepare().is_err());
    fs::set_permissions(fixture.authority(), fs::Permissions::from_mode(0o600)).unwrap();
    let bin = fixture.prefix.join("bin");
    fs::rename(&bin, fixture.prefix.join("old-bin")).unwrap();
    symlink(fixture.prefix.join("old-bin"), &bin).unwrap();
    assert!(fixture.prepare().is_err());
    fs::remove_file(&bin).unwrap();
    fs::create_dir(&bin).unwrap();
    assert!(fixture.prepare().is_err());
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 in a disposable trusted-root filesystem"]
fn no_authority_means_preexisting_exact_new_bytes_are_still_unrecorded() {
    let fixture = Fixture::new(false);
    let path = fixture.prefix.join(NATIVE_PAYLOAD[0]);
    put(
        &path,
        &fs::read(fixture.staged_prefix.join(NATIVE_PAYLOAD[0])).unwrap(),
        final_mode(NATIVE_PAYLOAD[0]),
    );
    assert!(fixture.prepare().is_err());
    assert!(!fixture.authority().exists());
    assert!(path.exists());
}

#[test]
#[ignore = "requires OPENWAVE_DISPOSABLE_ROOT=1 in a disposable trusted-root filesystem"]
fn continuation_rejects_identical_replacement_receipt_and_mismatched_prefix() {
    let fixture = Fixture::new(false);
    drop(fixture.prepare().unwrap());
    let other_prefix = fixture._root.path().join("other-prefix");
    assert!(
        prepare(
            &fixture.stage,
            &other_prefix,
            &fixture.hash(),
            &fixture.helper
        )
        .is_err()
    );
    assert!(!other_prefix.exists());
    let receipt = fixture.staged_prefix.join(RECEIPT);
    let replacement = receipt.with_extension("replacement");
    put(&replacement, &fs::read(&receipt).unwrap(), 0o644);
    fs::rename(replacement, receipt).unwrap();
    assert!(fixture.prepare().is_err());
    assert!(!fixture.prefix.join(RECEIPT).exists());
}
