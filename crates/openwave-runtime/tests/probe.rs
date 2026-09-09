use clap::Parser;
use openwave_core::{
    model::{OperationError, Result},
    profiles::ProfileId,
};
use openwave_runtime::probe::{Args, ProbeCommand, ProbeDevice, execute};
use std::{io::Cursor, sync::atomic::AtomicBool, time::Duration};
struct Device {
    config: Vec<u8>,
    writes: Vec<Vec<u8>>,
    reads: usize,
    short: bool,
    fail_read: bool,
    fail_write: bool,
    mismatch: bool,
}
impl Device {
    fn new() -> Self {
        Self {
            config: vec![0; 16],
            writes: Vec::new(),
            reads: 0,
            short: false,
            fail_read: false,
            fail_write: false,
            mismatch: false,
        }
    }
}
impl ProbeDevice for Device {
    fn profile(&self) -> ProfileId {
        ProfileId::Wave3
    }
    fn read_raw(&mut self, _selector: u16, buffer: &mut [u8]) -> Result<usize> {
        self.reads += 1;
        if self.fail_read {
            return Err(OperationError::unavailable("fixture transfer failed"));
        }
        let count = if self.short { 4 } else { self.config.len() }.min(buffer.len());
        buffer[..count].copy_from_slice(&self.config[..count]);
        if self.mismatch && !self.writes.is_empty() {
            buffer[0] ^= 1;
        }
        Ok(count)
    }
    fn write_config(&mut self, bytes: &[u8]) -> Result<()> {
        if self.fail_write {
            return Err(OperationError::unavailable("fixture write failed"));
        }
        self.config.copy_from_slice(bytes);
        self.writes.push(bytes.to_vec());
        Ok(())
    }
}
fn run(device: &mut Device, command: ProbeCommand, consent: &str) -> (Result<()>, String) {
    let mut output = Vec::new();
    let result = execute(
        device,
        &command,
        &mut Cursor::new(consent),
        &mut output,
        &AtomicBool::new(false),
    );
    (result, String::from_utf8(output).unwrap())
}
fn noop(yes: bool) -> ProbeCommand {
    ProbeCommand::Poke {
        offset: None,
        byte: None,
        noop: true,
        yes,
    }
}
#[test]
fn short_raw_dump_is_reported_but_cannot_be_replayed_as_a_production_config() {
    let mut device = Device::new();
    device.short = true;
    let (result, output) = run(
        &mut device,
        ProbeCommand::Dump {
            wvalue: None,
            len: 512,
        },
        "",
    );
    result.unwrap();
    assert!(output.contains("4 bytes"));
    assert!(output.contains("DEVIATION: expected 16 bytes, got 4"));
    let (result, _) = run(&mut device, noop(true), "");
    assert!(result.is_err());
    assert!(device.writes.is_empty());
}
#[test]
fn noop_warns_and_requires_consent_then_really_writes_and_verifies() {
    let mut device = Device::new();
    device.config[7] = 0xaa;
    let original = device.config.clone();
    let (result, output) = run(&mut device, noop(false), "n\n");
    result.unwrap();
    assert!(output.contains("REAL full config write"));
    assert!(output.contains("write? [y/N]"));
    assert!(device.writes.is_empty());
    assert_eq!(device.reads, 0);
    let (result, _) = run(&mut device, noop(false), "y\n");
    result.unwrap();
    assert_eq!(device.writes, vec![original]);
    assert_eq!(device.reads, 2);
}
#[test]
fn patch_preserves_every_other_byte_and_transfer_or_verification_failure_propagates() {
    let mut device = Device::new();
    device.config = (0..16).collect();
    let mut expected = device.config.clone();
    expected[10] = 0x55;
    run(
        &mut device,
        ProbeCommand::Poke {
            offset: Some(10),
            byte: Some(0x55),
            noop: false,
            yes: true,
        },
        "",
    )
    .0
    .unwrap();
    assert_eq!(device.writes, vec![expected]);
    device.mismatch = true;
    assert!(run(&mut device, noop(true), "").0.is_err());
    device.mismatch = false;
    device.fail_write = true;
    assert!(run(&mut device, noop(true), "").0.is_err());
    device.fail_read = true;
    assert!(
        run(
            &mut device,
            ProbeCommand::Dump {
                wvalue: Some(0),
                len: 512
            },
            ""
        )
        .0
        .is_err()
    );
}
#[test]
fn selected_short_profile_rejects_out_of_range_offsets_before_io() {
    let mut device = Device::new();
    assert!(
        run(
            &mut device,
            ProbeCommand::Poke {
                offset: Some(16),
                byte: Some(1),
                noop: false,
                yes: true
            },
            ""
        )
        .0
        .is_err()
    );
    assert_eq!(device.reads, 0);
    assert!(device.writes.is_empty());
}
#[test]
fn cli_rejects_invalid_ranges_conflicts_and_nonfinite_intervals_without_io() {
    for args in [
        vec!["dump", "--len", "0"],
        vec!["dump", "--len", "65536"],
        vec!["dump", "--wvalue", "0x10000"],
        vec!["watch", "--interval", "NaN"],
        vec!["watch", "--interval", "inf"],
        vec!["watch", "--interval", "0"],
        vec!["poke"],
        vec!["poke", "--offset", "3"],
        vec!["poke", "--offset", "34", "--byte", "1"],
        vec!["poke", "--offset", "0", "--byte", "256"],
        vec!["poke", "--noop", "--offset", "1", "--byte", "2"],
    ] {
        assert!(Args::try_parse_from(std::iter::once("openwave-probe").chain(args)).is_err());
    }
    let args = Args::try_parse_from(["openwave-probe", "dump", "--wvalue", "0xA", "--len", "512"])
        .unwrap();
    assert!(matches!(
        args.command,
        ProbeCommand::Dump {
            wvalue: Some(10),
            len: 512
        }
    ));
    let args = Args::try_parse_from(["openwave-probe", "watch"]).unwrap();
    assert!(
        matches!(args.command, ProbeCommand::Watch { interval } if interval == Duration::from_millis(100))
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_openwave-probe"))
        .env_clear()
        .args(["watch", "--interval", "NaN"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
}
