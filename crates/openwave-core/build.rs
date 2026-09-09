use std::{env, fs, path::PathBuf};

fn main() {
    let path = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"))
        .join("../../VERSION");
    println!("cargo:rerun-if-changed={}", path.display());
    let text = fs::read_to_string(&path).expect("read release VERSION");
    let version = text.strip_suffix('\n').unwrap_or(&text);
    let parts: Vec<_> = version.split('.').collect();
    assert!(
        parts.len() == 3
            && parts.iter().all(|part| {
                !part.is_empty()
                    && part.bytes().all(|b| b.is_ascii_digit())
                    && (part.len() == 1 || !part.starts_with('0'))
                    && part.parse::<u64>().is_ok()
            }),
        "VERSION must be canonical MAJOR.MINOR.PATCH"
    );
    assert_eq!(
        version,
        env::var("CARGO_PKG_VERSION").expect("package version"),
        "workspace package version must match VERSION"
    );
    println!("cargo:rustc-env=OPENWAVE_VERSION={version}");
    println!("cargo:rerun-if-env-changed=RUSTC");
    let compiler = std::process::Command::new(env::var_os("RUSTC").expect("Cargo compiler"))
        .arg("--version")
        .output()
        .expect("identify build compiler");
    assert!(
        compiler.status.success(),
        "build compiler version query failed"
    );
    let compiler = String::from_utf8(compiler.stdout).expect("compiler version is UTF-8");
    let compiler = compiler
        .lines()
        .next()
        .expect("compiler version is nonempty");
    println!("cargo:rustc-env=OPENWAVE_BUILD_RUSTC={compiler}");
    println!(
        "cargo:rustc-env=OPENWAVE_BUILD_TARGET={}",
        env::var("TARGET").expect("Cargo target")
    );
}
