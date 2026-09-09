use clap::Parser;
use openwave_core::model::{OperationError, Result};
use openwave_runtime::{
    audio::AudioManager,
    health::HealthMonitor,
    paths::{Lease, RuntimePaths},
};
use std::{
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

#[derive(Parser)]
#[command(name = "openwave-daemon", version = openwave_core::VERSION, about = "Headless capture keepalive with observation-only health checks by default")]
struct Arguments {
    /// Allow bounded card-profile cycles and sink suspend/resume on confirmed faults
    #[arg(long)]
    auto_recover: bool,
}
fn main() -> ExitCode {
    // Clap exits for informational options before paths, leases, signals or workers.
    let arguments = Arguments::parse();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .init();
    match run(arguments) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("{error}");
            ExitCode::FAILURE
        }
    }
}
fn run(arguments: Arguments) -> Result<()> {
    if rustix::process::geteuid().is_root() {
        return Err(OperationError::unavailable(
            "Run openwave-daemon as the desktop user, not root",
        ));
    }
    let paths = RuntimePaths::discover()?;
    let _installation = Lease::installation_shared(&paths.identity)?;
    // A competing daemon exits here without graph discovery or capture processes.
    let _daemon = Lease::capture_daemon()?;
    let stopped = Arc::new(AtomicBool::new(false));
    let term = signal_hook::flag::register(signal_hook::consts::SIGTERM, stopped.clone())?;
    let interrupt = match signal_hook::flag::register(signal_hook::consts::SIGINT, stopped.clone())
    {
        Ok(signal) => signal,
        Err(error) => {
            signal_hook::low_level::unregister(term);
            return Err(error.into());
        }
    };
    let result = (|| -> Result<()> {
        log::info!(
            "Starting OpenWave audio daemon (auto-recover: {})",
            arguments.auto_recover
        );
        let mut audio = AudioManager::start()?;
        let readiness = audio.readiness();
        let mut health = match HealthMonitor::start(
            arguments.auto_recover,
            Arc::new(move || readiness.gaps()),
        ) {
            Ok(health) => health,
            Err(error) => {
                let cleanup = audio.stop();
                return Err(OperationError::unavailable(match cleanup {
                    Ok(()) => error.to_string(),
                    Err(cleanup) => format!("{error}; capture shutdown: {cleanup}"),
                }));
            }
        };
        while !stopped.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(100));
        }
        // Keep capture pins and both leases alive while the same health owner
        // retries owed restoration. A replacement identity cannot settle it.
        let restoration = health.stop_until_restored();
        let captures = audio.stop();
        match (restoration, captures) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(health), Err(audio)) => Err(OperationError::unavailable(format!(
                "health shutdown: {health}; capture shutdown: {audio}"
            ))),
        }
    })();
    signal_hook::low_level::unregister(interrupt);
    signal_hook::low_level::unregister(term);
    result
}
