use clap::{Parser, Subcommand};
use openwave_core::{
    VERSION,
    model::{OperationError, Result},
};
use openwave_runtime::{
    installation::{self, InstallMethod},
    process, release, setup, store, uninstall,
};
use std::{fs, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(name = "openwave-maintenance", version = VERSION, about = "Private native OpenWave maintenance operations")]
struct Cli {
    #[arg(long, hide = true, global = true)]
    cancel_on_stdin: bool,
    #[arg(long, hide = true, global = true)]
    worker_parent: Option<u32>,
    #[arg(long, hide = true, global = true)]
    supervised_parent: Option<u32>,
    #[arg(long, hide = true, global = true)]
    supervised_group: Option<u32>,
    #[arg(long, hide = true, global = true)]
    login_uid: Option<u32>,
    #[arg(long, hide = true, global = true)]
    login_gid: Option<u32>,
    #[command(subcommand)]
    command: Operation,
}
#[derive(Subcommand)]
enum Operation {
    /// Validate the canonical release value and optional exact stable tag.
    Version {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        tag: Option<String>,
    },
    /// Render the release AUR recipe from the sole embedded PKGBUILD template.
    RenderAur {
        #[arg(long)]
        version: String,
        #[arg(long)]
        sha256: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Merge locked vendor source configuration in an explicit staging tree.
    MergeVendorConfig {
        #[arg(long)]
        existing: PathBuf,
        #[arg(long)]
        emitted: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Print permissions for only the enabled USB profiles.
    UdevRules,
    RecordInstall {
        #[arg(long)]
        prefix: PathBuf,
        #[arg(long)]
        destdir: Option<PathBuf>,
        #[arg(long, value_parser = ["manual", "deb", "rpm", "arch", "nix", "flatpak"])]
        method: String,
        #[arg(long)]
        check: bool,
    },
    /// Copy a checksummed fixed payload through a trusted root bootstrap.
    InstallPayload {
        #[arg(long)]
        stage: PathBuf,
        #[arg(long)]
        prefix: PathBuf,
        #[arg(long)]
        expected_sha256: String,
    },
    FilesOnly {
        #[arg(long)]
        prefix: PathBuf,
        #[arg(long)]
        destdir: Option<PathBuf>,
        #[arg(long)]
        yes: bool,
    },
    RetireLegacy {
        #[arg(long)]
        prefix: PathBuf,
        #[arg(long)]
        module_dir: Option<PathBuf>,
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        yes: bool,
    },
    ResumeUninstall {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        delete_settings: bool,
    },
    ResumeRetirement {
        #[arg(long)]
        plan: PathBuf,
        #[arg(long)]
        yes: bool,
    },
    /// Same-user quiescence invoked by a trusted privileged legacy bootstrap.
    QuiesceLegacy {
        #[arg(long)]
        prefix: PathBuf,
        #[arg(long)]
        module_dir: Option<PathBuf>,
    },
    InstallUdev,
    RemoveUdev,
    PrepareRemoveFiles {
        #[arg(long)]
        receipt: PathBuf,
        #[arg(long)]
        expected_sha256: String,
        #[arg(long)]
        prefix: PathBuf,
    },
    RemoveFiles {
        #[arg(long)]
        receipt: PathBuf,
        #[arg(long)]
        expected_sha256: String,
        #[arg(long)]
        prefix: PathBuf,
    },
    ResumePrivileged {
        #[arg(long)]
        transaction: PathBuf,
    },
    /// Execute one captured audio child, with no shell or elevated mode.
    ExecChild {
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        parent_pid: u32,
        #[arg(last = true, required = true, num_args = 1..)]
        argv: Vec<String>,
    },
}

impl Operation {
    fn privileged_job(&self) -> bool {
        matches!(
            self,
            Self::RecordInstall { .. }
                | Self::InstallPayload { .. }
                | Self::FilesOnly { .. }
                | Self::RetireLegacy { .. }
                | Self::InstallUdev
                | Self::RemoveUdev
                | Self::PrepareRemoveFiles { .. }
                | Self::RemoveFiles { .. }
                | Self::ResumePrivileged { .. }
        )
    }
}

fn dispatch(cli: Cli) -> Result<u8> {
    if let Some(parent) = cli.worker_parent {
        if cli.supervised_parent.is_some()
            || cli.supervised_group.is_some()
            || cli.login_uid.is_some()
            || cli.login_gid.is_some()
            || !cli.command.privileged_job()
        {
            return Err(OperationError::invalid(
                "Invalid internal root worker operation",
            ));
        }
        process::enter_supervised(parent, None, None)?;
    } else if let Some(parent) = cli.supervised_parent {
        if cli.cancel_on_stdin || !matches!(cli.command, Operation::QuiesceLegacy { .. }) {
            return Err(OperationError::invalid(
                "Invalid internal login-user operation",
            ));
        }
        let group = cli
            .supervised_group
            .ok_or_else(|| OperationError::invalid("Missing supervised group"))?;
        let uid = cli
            .login_uid
            .ok_or_else(|| OperationError::invalid("Missing login UID"))?;
        let gid = cli
            .login_gid
            .ok_or_else(|| OperationError::invalid("Missing login GID"))?;
        process::enter_supervised(parent, Some(group), Some((uid, gid)))?;
    } else {
        if cli.supervised_group.is_some() || cli.login_uid.is_some() || cli.login_gid.is_some() {
            return Err(OperationError::invalid(
                "Credential handoff requires a verified internal parent",
            ));
        }
        if rustix::process::geteuid().is_root() && cli.command.privileged_job() {
            // The exact argv was parsed above as an allowed operation. A fresh
            // child parses it again; no staged binary or arbitrary argv runs.
            return process::supervise_maintenance(
                &std::env::args_os().skip(1).collect::<Vec<_>>(),
                cli.cancel_on_stdin,
            );
        }
        if cli.cancel_on_stdin {
            return Err(OperationError::invalid(
                "Cancellation pipe is reserved for privileged maintenance",
            ));
        }
    }
    run(cli.command)
}

fn required_confirmation(yes: bool) -> bool {
    if !yes {
        eprintln!(
            "Explicit --yes is required for this private removal operation; use the public --uninstall interface for interactive confirmation."
        );
    }
    yes
}
fn report(result: uninstall::UninstallResult) -> u8 {
    for phase in result.removed {
        println!("{phase}");
    }
    if let Some(error) = result.error.as_deref() {
        eprintln!("{error}");
    }
    if !result.guidance.is_empty() {
        println!("{}", result.guidance);
    }
    if result.success { 0 } else { 1 }
}
fn run(operation: Operation) -> Result<u8> {
    if rustix::process::geteuid().is_root()
        && matches!(
            operation,
            Operation::ExecChild { .. }
                | Operation::RenderAur { .. }
                | Operation::MergeVendorConfig { .. }
                | Operation::ResumeUninstall { .. }
                | Operation::ResumeRetirement { .. }
                | Operation::QuiesceLegacy { .. }
        )
    {
        return Err(OperationError::invalid(
            "This operation belongs to the login user, not an elevated helper",
        ));
    }
    // Copied retry/bootstrap helpers deliberately dispatch without asset/layout
    // discovery. Their accepted receipt or recovery authority supplies identity.
    match operation {
        Operation::Version { file, tag } => println!(
            "{}",
            release::validate_version(&file, tag.as_deref())
                .map_err(|error| OperationError::invalid(error.to_string()))?
        ),
        Operation::RenderAur {
            version,
            sha256,
            output,
        } => release::render_aur(&version, &sha256, &output)
            .map_err(|error| OperationError::invalid(error.to_string()))?,
        Operation::MergeVendorConfig {
            existing,
            emitted,
            output,
        } => {
            let current = match fs::read_to_string(&existing) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => return Err(error.into()),
            };
            let replacement = fs::read_to_string(emitted)?;
            let merged = release::merge_vendor_config(&current, &replacement)
                .map_err(|error| OperationError::invalid(error.to_string()))?;
            let output = if output.is_absolute() {
                output
            } else {
                std::env::current_dir()?.join(output)
            };
            store::atomic_write(&output, merged.as_bytes())?;
        }
        Operation::UdevRules => print!("{}", setup::udev_rules()),
        Operation::RecordInstall {
            prefix,
            destdir,
            method,
            check,
        } => {
            let method: InstallMethod = serde_json::from_value(serde_json::Value::String(method))?;
            if check {
                installation::check_install_target(&prefix, destdir.as_deref())?;
            } else {
                let receipt = installation::record_install(&prefix, destdir.as_deref(), method)?;
                println!("{}", receipt.display());
            }
        }
        Operation::InstallPayload {
            stage,
            prefix,
            expected_sha256,
        } => installation::install_payload(&stage, &prefix, &expected_sha256)?,
        Operation::FilesOnly {
            prefix,
            destdir,
            yes,
        } => {
            if !required_confirmation(yes) {
                return Ok(2);
            }
            uninstall::files_only(&prefix, destdir.as_deref())?;
        }
        Operation::RetireLegacy {
            prefix,
            module_dir,
            dry_run,
            yes,
        } => {
            if !dry_run && !required_confirmation(yes) {
                return Ok(2);
            }
            return Ok(report(uninstall::retire_legacy(
                &prefix,
                module_dir.as_deref(),
                dry_run,
                yes,
            )));
        }
        Operation::ResumeUninstall {
            plan,
            yes,
            delete_settings,
        } => {
            if !required_confirmation(yes) {
                return Ok(2);
            }
            return Ok(report(uninstall::resume_uninstall(
                &plan,
                true,
                delete_settings,
            )));
        }
        Operation::ResumeRetirement { plan, yes } => {
            if !required_confirmation(yes) {
                return Ok(2);
            }
            return Ok(report(uninstall::resume_retirement(&plan, true)));
        }
        Operation::QuiesceLegacy { prefix, module_dir } => {
            uninstall::quiesce_legacy(&prefix, module_dir.as_deref())?
        }
        Operation::InstallUdev => setup::install_udev()?,
        Operation::RemoveUdev => setup::remove_udev()?,
        Operation::PrepareRemoveFiles {
            receipt,
            expected_sha256,
            prefix,
        } => {
            println!(
                "{}",
                uninstall::prepare_files_privileged(&receipt, &expected_sha256, &prefix)?.display()
            );
        }
        Operation::RemoveFiles {
            receipt,
            expected_sha256,
            prefix,
        } => uninstall::remove_files_privileged(&receipt, &expected_sha256, &prefix)?,
        Operation::ResumePrivileged { transaction } => uninstall::resume_privileged(&transaction)?,
        Operation::ExecChild { parent_pid, argv } => {
            process::exec_child(parent_pid, &argv[0], &argv[1..])?
        }
    }
    Ok(0)
}
fn main() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(status) => ExitCode::from(status),
        Err(error) => {
            eprintln!("openwave-maintenance: {error}");
            ExitCode::FAILURE
        }
    }
}
