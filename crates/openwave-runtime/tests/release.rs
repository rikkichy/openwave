use openwave_runtime::release::{merge_vendor_config, render_aur, validate_version};
use std::{fs, process::Command};

#[test]
fn release_version_is_canonical_and_tag_must_match() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("VERSION");
    for contents in ["0.12.300", "0.12.300\n"] {
        fs::write(&file, contents).unwrap();
        assert_eq!(
            validate_version(&file, Some("v0.12.300")).unwrap(),
            "0.12.300"
        );
        assert!(validate_version(&file, Some("0.12.300")).is_err());
        assert!(validate_version(&file, Some("v0.12.301")).is_err());
    }
    for contents in [
        "01.2.3",
        "1.2",
        "1.2.3.4",
        "1.2.3-rc.1",
        " 1.2.3",
        "1.2.3\r\n",
        "1.2.3\n\n",
        "1.2.3\n4.5.6",
        "١.2.3",
    ] {
        fs::write(&file, contents).unwrap();
        assert!(
            validate_version(&file, None).is_err(),
            "accepted {contents:?}"
        );
    }
}

#[test]
fn aur_recipe_resolves_the_exact_release_archive_for_x86_64() {
    let directory = tempfile::tempdir().unwrap();
    let recipe = directory.path().join("PKGBUILD");
    let digest = "ABCDEF0123456789".repeat(4);
    render_aur("2.3.4", &digest, &recipe).unwrap();
    // Evaluate the generated recipe as makepkg does, without invoking any build
    // or package function or fetching its source. These are consumer values,
    // not assertions about incidental whitespace/source implementation.
    let result = Command::new("bash")
        .arg("-c")
        .arg("set -eu; source \"$1\"; printf '%s\\n' \"$pkgver\" \"${source[0]}\" \"${sha256sums[0]}\" \"$_srcdir\" \"${arch[@]}\"")
        .arg("recipe-check")
        .arg(&recipe)
        .output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        String::from_utf8(result.stdout).unwrap(),
        format!(
            "2.3.4\nhttps://github.com/rikkichy/openwave/releases/download/v2.3.4/openwave-2.3.4.tar.gz\n{}\nopenwave-2.3.4\nx86_64\n",
            digest.to_ascii_lowercase()
        )
    );
    let original = fs::read(&recipe).unwrap();
    assert!(render_aur("3.0.0", &digest, &recipe).is_err());
    assert_eq!(fs::read(&recipe).unwrap(), original);
}

#[test]
fn invalid_aur_arguments_never_create_an_output() {
    let directory = tempfile::tempdir().unwrap();
    let recipe = directory.path().join("PKGBUILD");
    for (version, digest) in [
        ("1.2.3;touch injected", "a".repeat(64)),
        ("1.2.3", "a".repeat(63)),
        ("1.2.3", "z".repeat(64)),
    ] {
        assert!(render_aur(version, &digest, &recipe).is_err());
        assert!(!recipe.exists());
    }
}

#[test]
fn vendoring_changes_source_resolution_without_losing_other_cargo_settings() {
    let existing = r#"
[build]
target-dir = "custom-target"
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"
[alias]
check-all = "check --workspace"
[source.crates-io]
registry = "https://github.com/rust-lang/crates.io-index"
replace-with = "old-vendor"
[source.private]
registry = "https://example.invalid/index"
[net]
retry = 2
"#;
    let emitted = r#"
[source.crates-io]
replace-with = "vendored-sources"
[source.vendored-sources]
directory = "vendor"
"#;
    let output = merge_vendor_config(existing, emitted).unwrap();
    let config: toml::Value = toml::from_str(&output).unwrap();
    assert_eq!(
        config["source"]["crates-io"]["replace-with"].as_str(),
        Some("vendored-sources")
    );
    assert_eq!(
        config["source"]["vendored-sources"]["directory"].as_str(),
        Some("vendor")
    );
    assert_eq!(
        config["source"]["private"]["registry"].as_str(),
        Some("https://example.invalid/index")
    );
    assert_eq!(
        config["source"]["crates-io"]["registry"].as_str(),
        Some("https://github.com/rust-lang/crates.io-index")
    );
    assert_eq!(
        config["target"]["aarch64-unknown-linux-gnu"]["linker"].as_str(),
        Some("aarch64-linux-gnu-gcc")
    );
    assert_eq!(
        config["build"]["target-dir"].as_str(),
        Some("custom-target")
    );
    assert_eq!(
        config["alias"]["check-all"].as_str(),
        Some("check --workspace")
    );
    assert_eq!(config["net"]["retry"].as_integer(), Some(2));
    let fresh: toml::Value = toml::from_str(&merge_vendor_config("", emitted).unwrap()).unwrap();
    assert_eq!(
        fresh["source"]["vendored-sources"]["directory"].as_str(),
        Some("vendor")
    );
}

#[test]
fn malformed_or_unrelated_vendor_configuration_fails_closed() {
    let valid = "[source.vendored-sources]\ndirectory = 'vendor'\n";
    assert!(merge_vendor_config("[build", valid).is_err());
    for emitted in [
        "",
        "[source]\n",
        "[source",
        "[build]\ntarget-dir='elsewhere'",
        "[source.x]\ndirectory='vendor'\n[net]\nretry=0",
    ] {
        assert!(
            merge_vendor_config("", emitted).is_err(),
            "accepted {emitted:?}"
        );
    }
}
