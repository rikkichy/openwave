//! Engineer-only known-profile probing; all vendor access is lease guarded.
use crate::{
    device::VendorDevice,
    diag::hexdump,
    paths::{Lease, RuntimePaths},
};
use clap::{Parser, Subcommand};
use openwave_core::{
    model::{OperationError, Result, UnitId},
    profiles::ProfileId,
    protocol::{ConfigBuffer, MAX_CONFIG_LEN},
};
use std::{
    io::{self, BufRead, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn integer(text: &str) -> std::result::Result<u16, String> {
    let value = if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16)
    } else {
        text.parse()
    };
    value.map_err(|_| "expected a decimal or 0x hexadecimal integer in 0..65535".into())
}
fn length(text: &str) -> std::result::Result<u16, String> {
    let value = integer(text)?;
    if value == 0 {
        Err("length must be in 1..65535".into())
    } else {
        Ok(value)
    }
}
fn byte(text: &str) -> std::result::Result<u8, String> {
    u8::try_from(integer(text)?).map_err(|_| "byte must be in 0..255".into())
}
fn offset(text: &str) -> std::result::Result<u16, String> {
    let value = integer(text)?;
    if usize::from(value) >= MAX_CONFIG_LEN {
        Err(format!(
            "offset must be below {MAX_CONFIG_LEN}; the selected profile may be shorter"
        ))
    } else {
        Ok(value)
    }
}
fn interval(text: &str) -> std::result::Result<Duration, String> {
    let value = text
        .parse::<f64>()
        .map_err(|_| "interval must be positive finite seconds".to_owned())?;
    if !value.is_finite() || value <= 0.0 {
        return Err("interval must be positive finite seconds".into());
    }
    let duration = Duration::try_from_secs_f64(value)
        .map_err(|_| "interval is outside the supported duration range".to_owned())?;
    if duration.is_zero() {
        Err("interval is below clock resolution".into())
    } else {
        Ok(duration)
    }
}
#[derive(Parser, Debug)]
#[command(name = "openwave-probe", version = crate::VERSION, about = "Known-profile USB protocol probe. Close OpenWave (including its tray) and other vendor clients first.")]
pub struct Args {
    #[command(subcommand)]
    pub command: ProbeCommand,
}
#[derive(Subcommand, Debug)]
pub enum ProbeCommand {
    /// Read blocks and report actual lengths; dumps may contain serials.
    Dump {
        #[arg(long, value_parser = integer)]
        wvalue: Option<u16>,
        #[arg(long, default_value = "512", value_parser = length)]
        len: u16,
    },
    /// Observe config byte changes until Ctrl+C; no writes.
    Watch {
        #[arg(long, default_value = "0.1", value_parser = interval)]
        interval: Duration,
    },
    /// Dangerous real full-block write, including --noop. Never use to test phantom power.
    Poke {
        #[arg(long, value_parser = offset, requires = "byte", conflicts_with = "noop", required_unless_present = "noop")]
        offset: Option<u16>,
        #[arg(long, value_parser = byte, requires = "offset", conflicts_with = "noop", required_unless_present = "noop")]
        byte: Option<u8>,
        #[arg(long, conflicts_with_all = ["offset", "byte"])]
        noop: bool,
        #[arg(long)]
        yes: bool,
    },
}
impl ProbeCommand {
    /// Validate again for non-CLI callers, before any config read or write.
    pub fn validate(&self, profile: ProfileId) -> Result<()> {
        match self {
            Self::Dump { len: 0, .. } => Err(OperationError::invalid(
                "USB read length must be in 1..65535",
            )),
            Self::Watch { interval } if interval.is_zero() => {
                Err(OperationError::invalid("Watch interval must be positive"))
            }
            Self::Poke {
                offset, byte, noop, ..
            } => {
                if *noop {
                    if offset.is_some() || byte.is_some() {
                        return Err(OperationError::invalid(
                            "--noop excludes --offset and --byte",
                        ));
                    }
                } else {
                    let off = offset.ok_or_else(|| {
                        OperationError::invalid("poke requires --offset and --byte, or --noop")
                    })?;
                    if byte.is_none() || usize::from(off) >= profile.profile().config_len {
                        return Err(OperationError::invalid(
                            "poke offset is outside the selected profile config, or byte is missing",
                        ));
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
/// Raw boundary shared by production transport and deterministic protocol fixtures.
/// Strict config decoding stays here even when dump accepts a short transfer.
pub trait ProbeDevice {
    fn profile(&self) -> ProfileId;
    fn read_raw(&mut self, selector: u16, buffer: &mut [u8]) -> Result<usize>;
    fn write_config(&mut self, bytes: &[u8]) -> Result<()>;
}
impl ProbeDevice for VendorDevice {
    fn profile(&self) -> ProfileId {
        self.unit.profile
    }
    fn read_raw(&mut self, selector: u16, buffer: &mut [u8]) -> Result<usize> {
        VendorDevice::read_raw(self, selector, buffer)
    }
    fn write_config(&mut self, bytes: &[u8]) -> Result<()> {
        self.write_config_raw(bytes)
    }
}
fn read_config(device: &mut dyn ProbeDevice) -> Result<ConfigBuffer> {
    let p = device.profile();
    let mut bytes = [0; MAX_CONFIG_LEN];
    let count = device.read_raw(
        p.profile().wvalue_config,
        &mut bytes[..p.profile().config_len],
    )?;
    if count > p.profile().config_len {
        return Err(OperationError::unavailable(
            "USB transfer exceeded config buffer",
        ));
    }
    ConfigBuffer::decode(p, &bytes[..count])
}
pub fn execute(
    device: &mut dyn ProbeDevice,
    command: &ProbeCommand,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    cancelled: &AtomicBool,
) -> Result<()> {
    command.validate(device.profile())?;
    match command {
        ProbeCommand::Dump { wvalue, len } => {
            let p = device.profile().profile();
            let mut bytes = vec![0; usize::from(*len)];
            let blocks = [
                ("config", p.wvalue_config, p.config_len),
                ("meter", p.wvalue_meter, p.meter_len),
                ("devinfo", p.wvalue_devinfo, p.devinfo_len),
            ];
            let mut first_error = None;
            for (name, selector, expected) in blocks {
                let selector = wvalue.unwrap_or(selector);
                if wvalue.is_none() {
                    writeln!(output, "-- {name} (expected {expected} bytes)")?;
                }
                let result = device.read_raw(selector, &mut bytes).and_then(|count| {
                    if count > bytes.len() {
                        return Err(OperationError::unavailable(
                            "USB transfer exceeded requested buffer",
                        ));
                    }
                    writeln!(
                        output,
                        "wValue 0x{selector:04X}: {count} bytes\n{}",
                        hexdump(&bytes[..count])
                    )?;
                    if wvalue.is_none() && count != expected {
                        writeln!(
                            output,
                            "   DEVIATION: expected {expected} bytes, got {count}"
                        )?;
                    }
                    Ok(())
                });
                if let Err(error) = result {
                    writeln!(output, "wValue 0x{selector:04X}: {error}")?;
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
                if wvalue.is_some() || cancelled.load(Ordering::Acquire) {
                    break;
                }
            }
            output.flush()?;
            if let Some(error) = first_error {
                return Err(error);
            }
        }
        ProbeCommand::Watch { interval } => {
            let mut last = read_config(device)?;
            writeln!(
                output,
                "config: {} bytes — twiddle controls, Ctrl+C to stop\n{}",
                last.as_bytes().len(),
                hexdump(last.as_bytes())
            )?;
            output.flush()?;
            while !cancelled.load(Ordering::Acquire) {
                let start = Instant::now();
                while start.elapsed() < *interval && !cancelled.load(Ordering::Acquire) {
                    thread::sleep(
                        interval
                            .saturating_sub(start.elapsed())
                            .min(Duration::from_millis(100)),
                    );
                }
                if cancelled.load(Ordering::Acquire) {
                    break;
                }
                let current = read_config(device)?;
                if current != last {
                    let stamp = glib::DateTime::now_local()
                        .and_then(|d| d.format("%H:%M:%S"))
                        .map_err(|e| OperationError::unavailable(e.to_string()))?;
                    for (off, (a, b)) in last.as_bytes().iter().zip(current.as_bytes()).enumerate()
                    {
                        if a != b {
                            writeln!(output, "{stamp}  off {off:2}: {a:02x} -> {b:02x}")?;
                        }
                    }
                    output.flush()?;
                    last = current;
                }
            }
        }
        ProbeCommand::Poke {
            offset,
            byte,
            noop,
            yes,
        } => {
            writeln!(
                output,
                "WARNING: poke performs a REAL full config write, even with --noop. An unchanged write may trigger firmware side effects. Unknown offsets can damage connected equipment; never use this to test phantom power."
            )?;
            output.flush()?;
            if !yes {
                write!(output, "write? [y/N] ")?;
                output.flush()?;
                let mut answer = String::new();
                input.take(128).read_line(&mut answer)?;
                if answer.trim().to_lowercase() != "y" {
                    writeln!(output, "Cancelled; no write performed.")?;
                    return Ok(());
                }
            }
            if cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            // Read after consent, so a slow interactive prompt cannot replay a
            // stale full block over a physical adjustment made while waiting.
            let config = read_config(device)?;
            let mut bytes = [0; MAX_CONFIG_LEN];
            let bytes = &mut bytes[..config.as_bytes().len()];
            bytes.copy_from_slice(config.as_bytes());
            if let (Some(off), Some(value)) = (offset, byte) {
                writeln!(
                    output,
                    "off {off}: {:02x} -> {value:02x}",
                    bytes[usize::from(*off)]
                )?;
                bytes[usize::from(*off)] = *value;
            }
            if cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            device.write_config(bytes)?;
            let verified = read_config(device)?;
            if verified.as_bytes() != bytes {
                return Err(OperationError::unavailable(
                    "USB write verification failed: read-back differs from the submitted config",
                ));
            }
            writeln!(
                output,
                "wrote and verified {} bytes{}\n{}",
                bytes.len(),
                if *noop { " unchanged" } else { "" },
                hexdump(verified.as_bytes())
            )?;
            output.flush()?;
        }
    }
    Ok(())
}
pub fn cli() -> i32 {
    let args = Args::parse();
    let cancelled = Arc::new(AtomicBool::new(false));
    let result = (|| -> Result<()> {
        let paths = RuntimePaths::discover()?;
        let _installation = Lease::installation_shared(&paths.identity)?;
        let _vendor = Lease::vendor_control(None)?;
        let (profile, bus, address) = VendorDevice::scan()?
            .into_iter()
            .next()
            .ok_or_else(|| OperationError::unavailable("No supported USB device found"))?;
        args.command.validate(profile)?;
        let p = profile.profile();
        println!(
            "selected first bus/address-sorted supported unit: {} ({:04x}:{:04x}) at {bus:03}/{address:03}",
            p.display_name, p.vid, p.pid
        );
        println!(
            "Use only one supported unit when investigating a specific device. Dumps may contain serials."
        );
        let mut device = VendorDevice::open(UnitId {
            profile,
            bus,
            address,
            incarnation: 0,
        })?;
        let interrupt =
            signal_hook::flag::register(signal_hook::consts::SIGINT, cancelled.clone())?;
        let terminate =
            match signal_hook::flag::register(signal_hook::consts::SIGTERM, cancelled.clone()) {
                Ok(id) => id,
                Err(e) => {
                    signal_hook::low_level::unregister(interrupt);
                    return Err(e.into());
                }
            };
        let result = execute(
            &mut device,
            &args.command,
            &mut io::stdin().lock(),
            &mut io::stdout().lock(),
            &cancelled,
        );
        signal_hook::low_level::unregister(interrupt);
        signal_hook::low_level::unregister(terminate);
        result
    })();
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("openwave-probe: {e}");
            1
        }
    }
}
