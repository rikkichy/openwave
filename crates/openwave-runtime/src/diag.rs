//! Opt-in diagnostics. Disclosure (--full) never grants vendor-control access.
use crate::{
    device::VendorDevice,
    paths::{self, Lease, RuntimePaths},
    process::CommandRunner,
    service, setup,
};
use clap::Parser;
use openwave_core::{
    model::{OperationError, Result, UnitId},
    profiles::PROFILES,
    protocol::{AlsaCardIdentity, card_for_unit},
};
use std::{
    fmt::Write as _,
    fs::{self, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Parser, Debug)]
#[command(name = "openwave-diag", version = crate::VERSION, about = "Export privacy-reduced OpenWave diagnostics; no hardware writes")]
pub struct Args {
    /// Include serials, settings, journal and node names; does not authorize USB access.
    #[arg(long)]
    pub full: bool,
    /// Read vendor details. Close OpenWave, including its tray, and competing clients first.
    #[arg(long)]
    pub device: bool,
    /// Refuse to overwrite an existing report.
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Versions,
    Usb,
    Device,
    Udev,
    Service,
    Journal,
    Pipewire,
    Configs,
}
impl Section {
    fn title(self) -> &'static str {
        match self {
            Self::Versions => "Versions",
            Self::Usb => "USB devices",
            Self::Device => "Device",
            Self::Udev => "udev",
            Self::Service => "Service",
            Self::Journal => "Journal",
            Self::Pipewire => "PipeWire",
            Self::Configs => "Config files",
        }
    }
}
const SECTIONS: [Section; 8] = [
    Section::Versions,
    Section::Usb,
    Section::Device,
    Section::Udev,
    Section::Service,
    Section::Journal,
    Section::Pipewire,
    Section::Configs,
];
/// Each collector is independently fallible; the report remains exportable.
pub trait Collectors {
    fn collect(&self, section: Section, full: bool) -> Result<String>;
}
pub fn assemble_with(collectors: &dyn Collectors, full: bool, device: bool, stamp: &str) -> String {
    let mut out = format!(
        "OpenWave diagnostics — {stamp}\nSource: https://github.com/rikkichy/openwave\nPrivacy: filesystem paths redacted; vendor/product IDs retained. {}\n",
        if full {
            "Serials and other private details included (--full); review before sharing."
        } else {
            "Serials, node names, journal and config bodies withheld."
        }
    );
    for section in SECTIONS {
        let _ = writeln!(out, "\n== {} ==", section.title());
        let result = if section == Section::Device && !device {
            Ok("(USB detail reads disabled; close OpenWave, including its tray, then use --device)".into())
        } else if section == Section::Journal && !full {
            Ok(
                "(journal withheld: may contain serials, paths and app names; --full includes it)"
                    .into(),
            )
        } else {
            collectors.collect(section, full)
        };
        match result {
            Ok(text) => {
                out.push_str(&text);
                out.push('\n');
            }
            Err(error) => {
                let _ = writeln!(
                    out,
                    "unavailable ({})",
                    if full {
                        error.to_string()
                    } else {
                        format!("{:?}; details withheld", error.code)
                    }
                );
            }
        }
    }
    redact_paths(&out)
}

/// Redact quoted paths (including spaces), file URIs and unquoted absolute
/// paths, but retain complete web URLs. No installation-prefix allowlist.
pub fn redact_paths(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap_or_default();
        if c == '\'' || c == '"' {
            let start = i + c.len_utf8();
            let inner = &text[start..];
            if inner.starts_with('/') || inner.starts_with("~/") || inner.starts_with("file:///") {
                let end = inner.find(['\n', c]).unwrap_or(inner.len());
                out.push_str("\"[path]\"");
                i = start + end;
                if text[i..].starts_with(c) {
                    i += c.len_utf8();
                }
                continue;
            }
        }
        if rest.starts_with("https://") || rest.starts_with("http://") {
            let end = rest
                .find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | '<' | '>'))
                .unwrap_or(rest.len());
            out.push_str(&rest[..end]);
            i += end;
            continue;
        }
        if rest.starts_with("file:///") || rest.starts_with("~/") || c == '/' {
            let end = rest
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, '\'' | '"' | '<' | '>' | ')' | ',' | ';')
                })
                .unwrap_or(rest.len());
            out.push_str("[path]");
            i += end;
            continue;
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

fn command(program: &str, args: &[&str]) -> Result<String> {
    let output = CommandRunner::default().run(
        program,
        &args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
        Duration::from_secs(3),
    )?;
    String::from_utf8(output.stdout)
        .map(|s| s.trim().to_owned())
        .map_err(|_| OperationError::unavailable(format!("{program}: invalid UTF-8 output")))
}
fn bounded_text(path: &Path) -> Result<String> {
    let mut text = String::new();
    fs::File::open(path)?
        .take(4 * 1024 * 1024 + 1)
        .read_to_string(&mut text)?;
    if text.len() > 4 * 1024 * 1024 {
        return Err(OperationError::unavailable(
            "Diagnostic input exceeds 4 MiB",
        ));
    }
    Ok(text)
}
fn note(result: Result<String>, full: bool) -> String {
    match result {
        Ok(s) => s,
        Err(e) if full => format!("unavailable ({e})"),
        Err(_) => "unavailable (details withheld)".into(),
    }
}
pub struct NativeCollectors;
impl Collectors for NativeCollectors {
    fn collect(&self, section: Section, full: bool) -> Result<String> {
        match section {
            Section::Versions => {
                let distro = bounded_text(Path::new("/etc/os-release")).map(|s| {
                    s.lines()
                        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                        .unwrap_or("unknown")
                        .trim_matches('"')
                        .to_owned()
                });
                Ok(format!(
                    "OpenWave: {}\nruntime: Rust native\ncompiler: {}\nbuild target: {}\ndistro: {}\npipewire: {}\nwireplumber: {}",
                    crate::VERSION,
                    openwave_core::RUST_COMPILER,
                    openwave_core::BUILD_TARGET,
                    note(distro, full),
                    note(command("pipewire", &["--version"]), full),
                    note(command("wireplumber", &["--version"]), full)
                ))
            }
            Section::Usb => collect_usb(Path::new("/sys/bus/usb/devices")),
            Section::Device => collect_device(full),
            Section::Udev => collect_udev(Path::new("/etc/udev/rules.d")),
            Section::Service => {
                let host = service::HostContext::discover()?;
                let backend = if host.commands.available("systemctl") {
                    "systemd"
                } else if host.commands.available("sv") {
                    "runit"
                } else {
                    "unsupported"
                };
                let status = service::status_with(&RuntimePaths::discover()?, &host)?;
                let installed = if backend == "systemd" {
                    note(
                        command(
                            "systemctl",
                            &[
                                "--user",
                                "show",
                                service::SYSTEMD_UNIT,
                                "--property=LoadState",
                                "--value",
                            ],
                        )
                        .and_then(|state| match state.as_str() {
                            "not-found" => Ok("false".into()),
                            "loaded" | "masked" | "error" | "bad-setting" => Ok("true".into()),
                            _ => Err(OperationError::unavailable(
                                "Unknown systemd unit load state",
                            )),
                        }),
                        full,
                    )
                } else {
                    host.runit_link.try_exists()?.to_string()
                };
                Ok(format!(
                    "backend: {backend}\ninstalled: {installed}\nrunning: {}\nfailed: {}\nstatus: {}",
                    status.running, status.failed, status.message
                ))
            }
            Section::Journal => {
                if !full {
                    return Ok("(journal withheld; --full includes it)".into());
                }
                if service::find_program("systemctl").is_none() {
                    return Ok("(journal only collected on systemd)".into());
                }
                command(
                    "journalctl",
                    &["--user", "-u", "openwave", "-n", "100", "--no-pager"],
                )
            }
            Section::Pipewire => describe_pipewire(&command("pw-dump", &[])?, full),
            Section::Configs => collect_configs(&paths::config_dir()?, full),
        }
    }
}
fn collect_usb(root: &Path) -> Result<String> {
    let mut present = std::collections::HashSet::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let vid = fs::read_to_string(path.join("idVendor"));
        let pid = fs::read_to_string(path.join("idProduct"));
        match (vid, pid) {
            (Ok(vid), Ok(pid)) => {
                present.insert((vid.trim().to_owned(), pid.trim().to_owned()));
            }
            (Err(e), _) | (_, Err(e)) if e.kind() == std::io::ErrorKind::NotFound => continue,
            (Err(e), _) | (_, Err(e)) => return Err(e.into()),
        }
    }
    Ok(PROFILES
        .iter()
        .map(|p| {
            format!(
                "{} ({:04x}:{:04x}): {}",
                p.display_name,
                p.vid,
                p.pid,
                if present.contains(&(format!("{:04x}", p.vid), format!("{:04x}", p.pid))) {
                    "present"
                } else {
                    "absent"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}
fn collect_udev(root: &Path) -> Result<String> {
    let mut text = String::new();
    let mut lines = Vec::new();
    for name in ["99-openwave.rules", "99-wavexlr.rules"] {
        match bounded_text(&root.join(name)) {
            Ok(body) => {
                text.push_str(&body);
                text.push('\n');
                lines.push(format!("{name}: present"));
            }
            Err(e) if !root.join(name).try_exists()? => {
                let _ = e;
                lines.push(format!("{name}: absent"));
            }
            Err(e) => return Err(e),
        }
    }
    lines.insert(
        0,
        format!("udev rules complete: {}", setup::udev_contents_cover(&text)),
    );
    Ok(lines.join("\n"))
}
pub fn describe_pipewire(text: &str, full: bool) -> Result<String> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| OperationError::unavailable(format!("pw-dump output unparseable: {e}")))?;
    let objects = value
        .as_array()
        .ok_or_else(|| OperationError::unavailable("pw-dump output is not an array"))?;
    let mut lines = vec![
        if full {
            "all nodes:"
        } else {
            "openwave / Elgato nodes:"
        }
        .to_owned(),
    ];
    for object in objects {
        let props = &object["info"]["props"];
        let Some(name) = props["node.name"].as_str().filter(|s| !s.is_empty()) else {
            continue;
        };
        let description = props["node.description"].as_str().unwrap_or("");
        if !full
            && !name.starts_with("openwave_")
            && !name.contains("Elgato")
            && !description.contains("Wave")
        {
            continue;
        }
        let state = object["info"]["state"].as_str().unwrap_or("?");
        if full {
            lines.push(format!("{name}  [{state}]  {description}"));
        } else {
            let state = match state {
                "running" | "idle" | "suspended" | "creating" | "error" => state,
                _ => "unknown",
            };
            lines.push(format!(
                "{}  [{state}] (name and description withheld)",
                if name.starts_with("openwave_") {
                    "OpenWave node"
                } else {
                    "supported hardware node"
                }
            ));
        }
    }
    if lines.len() == 1 {
        lines.push("(none)".into());
    }
    Ok(lines.join("\n"))
}
pub fn collect_configs(root: &Path, full: bool) -> Result<String> {
    let mut out = String::new();
    for name in [
        "sources.json",
        "mixdefs.json",
        "mixes.json",
        "scenes.json",
        "ui-state.json",
    ] {
        let path = root.join(name);
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let _ = writeln!(out, "{name}: absent");
                continue;
            }
            Err(e) => {
                let _ = writeln!(out, "{name}: {}", note(Err(e.into()), full));
                continue;
            }
        };
        let body = bounded_text(&path);
        let state = body.as_ref().map_err(|e| e.to_string()).and_then(|s| {
            serde_json::from_str::<serde_json::Value>(s)
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
        let _ = writeln!(
            out,
            "{name}: {} bytes, {}",
            metadata.len(),
            match state {
                Ok(()) => "parses".into(),
                Err(e) if full => format!("BROKEN ({e})"),
                Err(_) => "BROKEN (details withheld)".into(),
            }
        );
        if full {
            if let Ok(body) = body {
                out.push_str(body.trim_end());
                out.push('\n');
            }
        }
    }
    if !full {
        out.push_str("(contents withheld — app names are personal; --full includes them)\n");
    }
    Ok(out)
}
fn alsa_card(unit: UnitId) -> Result<String> {
    let mut cards = Vec::new();
    for entry in fs::read_dir("/proc/asound")? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(card) = name
            .to_str()
            .and_then(|n| n.strip_prefix("card"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let path = entry.path();
        let (Ok(id), Ok(bus)) = (
            fs::read_to_string(path.join("usbid")),
            fs::read_to_string(path.join("usbbus")),
        ) else {
            continue;
        };
        cards.push(AlsaCardIdentity::parse(card, &id, &bus)?);
    }
    Ok(card_for_unit(unit, &cards)
        .map(|c| c.to_string())
        .unwrap_or_else(|| "unavailable (no unique exact USB identity)".into()))
}
pub fn collect_device(full: bool) -> Result<String> {
    let paths = RuntimePaths::discover()?;
    let _installation = Lease::installation_shared(&paths.identity)?;
    let _vendor = Lease::vendor_control(None)?;
    let units = VendorDevice::scan()?;
    if units.is_empty() {
        return Ok("no supported device on the bus".into());
    }
    let mut out = String::new();
    for (profile, bus, address) in units {
        let unit = UnitId {
            profile,
            bus,
            address,
            incarnation: 0,
        };
        let p = profile.profile();
        let _ = writeln!(
            out,
            "profile: {} ({:04x}:{:04x}) at {bus:03}/{address:03}\nalsa card: {}",
            p.display_name,
            p.vid,
            p.pid,
            note(alsa_card(unit), full)
        );
        match VendorDevice::open(unit) {
            Err(e) => {
                let _ = writeln!(out, "could not open: {}", note(Err(e), full));
            }
            Ok(mut device) => {
                match device.read_info() {
                    Ok(info) => {
                        let _ = writeln!(
                            out,
                            "firmware: {}  api: {}  serial: {}",
                            info.firmware,
                            info.api,
                            if full { &info.serial } else { "[withheld]" }
                        );
                    }
                    Err(e) => {
                        let _ = writeln!(out, "devinfo: {}", note(Err(e), full));
                    }
                }
                if full {
                    let _ = writeln!(
                        out,
                        "config ({} bytes expected):\n{}",
                        p.config_len,
                        note(device.read_config().map(|b| hexdump(b.as_bytes())), full)
                    );
                } else {
                    out.push_str("device config: contents withheld (--full includes them)\n");
                }
            }
        }
        out.push('\n');
    }
    Ok(out)
}
pub fn hexdump(bytes: &[u8]) -> String {
    let mut out = String::new();
    for (i, chunk) in bytes.chunks(16).enumerate() {
        let hex = chunk
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let ascii: String = chunk
            .iter()
            .map(|b| {
                if (32..127).contains(b) {
                    char::from(*b)
                } else {
                    '.'
                }
            })
            .collect();
        let _ = writeln!(out, "  {:04x}  {hex:<47}  {ascii}", i * 16);
    }
    out
}
fn timestamp(format: &str) -> Result<String> {
    glib::DateTime::now_local()
        .and_then(|d| d.format(format))
        .map(|s| s.to_string())
        .map_err(|e| OperationError::unavailable(format!("Local time unavailable: {e}")))
}
pub fn export(path: &Path, report: &str) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    file.write_all(report.as_bytes())?;
    file.sync_all()?;
    Ok(path)
}
pub fn cli() -> i32 {
    let args = Args::parse();
    let result = (|| -> Result<PathBuf> {
        let path = match args.output {
            Some(p) => p,
            None => PathBuf::from(format!("openwave-diag-{}.txt", timestamp("%Y%m%d-%H%M%S")?)),
        };
        let report = assemble_with(
            &NativeCollectors,
            args.full,
            args.device,
            &timestamp("%Y-%m-%d %H:%M:%S %z")?,
        );
        export(&path, &report)
    })();
    match result {
        Ok(path) => {
            println!("{}", path.display());
            0
        }
        Err(e) => {
            eprintln!("openwave-diag: {}", redact_paths(&e.to_string()));
            1
        }
    }
}
