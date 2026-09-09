//! Bounded command execution and exclusively owned audio process groups.
use openwave_core::model::{ErrorCode, OperationError, Result};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions};
use std::{
    io::{ErrorKind, Read},
    os::{fd::AsFd, unix::process::CommandExt},
    process::{Child, ChildStdin, ChildStdout, Command, ExitStatus, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const TICK: Duration = Duration::from_millis(10);
const TERM_GRACE: Duration = Duration::from_millis(300);
const OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
const PRIVILEGED_DEADLINE: Duration = Duration::from_secs(120);
static SUPERVISED: AtomicBool = AtomicBool::new(false);

fn os_error(error: rustix::io::Errno) -> OperationError {
    std::io::Error::from(error).into()
}
fn process_group(pid: Pid) -> Result<u32> {
    // A group inherited from outside a PID namespace is reported as zero.
    // rustix's nonzero Pid return type cannot represent that valid observation.
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_pid()))?;
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(2))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| OperationError::unavailable("Malformed process-group observation"))
}
fn nonblocking(fd: &impl AsFd) -> Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd).map_err(os_error)?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK).map_err(os_error)
}
fn exited(pid: Pid) -> Result<bool> {
    match rustix::process::waitid(
        WaitId::Pid(pid),
        WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
    ) {
        Ok(status) => Ok(status.is_some()),
        Err(rustix::io::Errno::INTR) => Ok(false),
        Err(error) => Err(os_error(error)),
    }
}
fn signal(pid: Pid, group: bool, value: Signal) -> Result<()> {
    let result = if group {
        rustix::process::kill_process_group(pid, value)
    } else {
        rustix::process::kill_process(pid, value)
    };
    match result {
        Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
        Err(error) => Err(os_error(error)),
    }
}

/// The unreaped child pins its PID until the last group signal. Never signal a
/// numeric PID after Child::wait/try_wait has released that ownership witness.
struct Process {
    child: Option<Child>,
    pid: Pid,
    group: bool,
    status: Option<ExitStatus>,
    cancellation: Option<ChildStdin>,
    reaper: Option<JoinHandle<std::io::Result<ExitStatus>>>,
}
impl Process {
    fn new(child: Child, group: bool) -> Self {
        let pid = Pid::from_raw(child.id() as i32).expect("spawn returned a positive PID");
        Self {
            child: Some(child),
            pid,
            group,
            status: None,
            cancellation: None,
            reaper: None,
        }
    }
    fn stop(&mut self) -> Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        if self.child.is_none() {
            return self.finish_reaper();
        }
        // Closing the only writer asks a root supervisor to kill/reap its job.
        // The login user may have no signal permission after pkexec elevates.
        let cooperative = self.cancellation.take().is_some();
        if !self.group {
            self.group = process_group(self.pid).ok() == Some(self.pid.as_raw_pid() as u32);
        }
        let mut failure = signal(self.pid, self.group, Signal::TERM).err();
        let deadline = Instant::now()
            + if cooperative {
                Duration::from_secs(2)
            } else {
                TERM_GRACE
            };
        while Instant::now() < deadline {
            match exited(self.pid) {
                Ok(true) => break,
                Ok(false) => thread::sleep(TICK),
                Err(error) => {
                    failure.get_or_insert(error);
                    break;
                }
            }
        }
        // The unreaped leader still pins the group ID. Nested supervised
        // queries are not group leaders and must never signal the shared job.
        if !self.group {
            self.group = process_group(self.pid).ok() == Some(self.pid.as_raw_pid() as u32);
        }
        if let Err(error) = signal(self.pid, self.group, Signal::KILL) {
            failure.get_or_insert(error);
        }
        let deadline = Instant::now() + TERM_GRACE;
        while Instant::now() < deadline {
            if exited(self.pid)? {
                self.status = Some(self.child.as_mut().expect("owned child").wait()?);
                // A cooperative root exit acknowledges cleanup even if the
                // caller's signals were denied. Its nonzero status remains data.
                return if cooperative {
                    Ok(())
                } else {
                    failure.map_or(Ok(()), Err)
                };
            }
            thread::sleep(TICK);
        }
        let mut child = self.child.take().expect("owned child");
        self.reaper = Some(thread::spawn(move || child.wait()));
        Err(OperationError::unavailable(
            "Owned command has not acknowledged termination; cleanup is pending, not complete",
        ))
    }
    fn finish_reaper(&mut self) -> Result<()> {
        if !self.reaper.as_ref().is_some_and(JoinHandle::is_finished) {
            return Err(OperationError::unavailable(
                "Owned command cleanup remains pending",
            ));
        }
        let status = self
            .reaper
            .take()
            .expect("finished reaper")
            .join()
            .map_err(|_| OperationError::unavailable("Owned command reaper panicked"))??;
        self.status = Some(status);
        Ok(())
    }
    fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        if self.child.is_none() && self.status.is_none() {
            self.finish_reaper()?;
        }
        if self.status.is_none() && exited(self.pid)? {
            self.stop()?;
        }
        Ok(self.status)
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Cancellation is shared with the owning worker. Owed restoration deliberately
/// ignores it, but retains the same execution deadline and process ownership.
#[derive(Clone, Default)]
pub struct CommandRunner {
    cancel: Arc<AtomicBool>,
}
impl CommandRunner {
    pub fn new(cancel: Arc<AtomicBool>) -> Self {
        Self { cancel }
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Acquire)
    }
    pub fn run(&self, program: &str, args: &[String], timeout: Duration) -> Result<Output> {
        self.execute(program, args, timeout, true, true, false, false)
    }
    pub fn run_uncancelled(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Output> {
        self.execute(program, args, timeout, false, true, false, false)
    }
    /// Expected nonzero status (for example a package query miss) is data.
    pub fn run_status(&self, program: &str, args: &[String], timeout: Duration) -> Result<Output> {
        if program == "pkexec" {
            return self.run_privileged_status(program, args, timeout);
        }
        self.execute(program, args, timeout, true, false, false, false)
    }
    pub fn run_status_clean(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Output> {
        self.execute(program, args, timeout, true, false, true, false)
    }
    /// Only fixed maintenance operations may cross this privilege boundary.
    /// The root CLI independently parses and validates the operation again.
    pub fn run_privileged_status(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
    ) -> Result<Output> {
        if program != "pkexec"
            || args.len() < 2
            || !matches!(
                args[1].as_str(),
                "install-udev"
                    | "remove-udev"
                    | "prepare-remove-files"
                    | "remove-files"
                    | "resume-privileged"
            )
        {
            return Err(OperationError::invalid(
                "Unsupported privileged maintenance invocation",
            ));
        }
        if std::path::Path::new(&args[0])
            .file_name()
            .and_then(|name| name.to_str())
            != Some("openwave-maintenance")
        {
            return Err(OperationError::invalid(
                "Privilege boundary requires the private maintenance helper",
            ));
        }
        crate::paths::trusted_for_root(std::path::Path::new(&args[0]))?;
        let mut invocation = Vec::with_capacity(args.len() + 1);
        invocation.push(args[0].clone());
        invocation.push("--cancel-on-stdin".into());
        invocation.extend_from_slice(&args[1..]);
        self.execute(program, &invocation, timeout, true, false, false, true)
    }
    fn execute(
        &self,
        program: &str,
        args: &[String],
        timeout: Duration,
        cancellable: bool,
        check: bool,
        clean_env: bool,
        privileged: bool,
    ) -> Result<Output> {
        if timeout.is_zero() {
            return Err(OperationError::invalid("Command deadline must be positive"));
        }
        let start = Instant::now();
        if cancellable && self.is_cancelled() {
            return Err(cancelled());
        }
        let mut command = Command::new(program);
        if clean_env {
            command.env_clear().env(
                "PATH",
                "/usr/sbin:/usr/bin:/sbin:/bin:/run/current-system/sw/bin",
            );
        }
        command
            .args(args)
            .env("LC_ALL", "C")
            .stdin(if privileged {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let own_group = !SUPERVISED.load(Ordering::Acquire);
        if own_group {
            command.process_group(0);
        }
        let child = command.spawn()?;
        let mut process = Process::new(child, own_group);
        if privileged {
            process.cancellation = process.child.as_mut().expect("owned child").stdin.take();
        }
        let mut stdout = process
            .child
            .as_mut()
            .expect("owned child")
            .stdout
            .take()
            .expect("piped stdout");
        let mut stderr = process
            .child
            .as_mut()
            .expect("owned child")
            .stderr
            .take()
            .expect("piped stderr");
        nonblocking(&stdout)?;
        nonblocking(&stderr)?;
        let mut out = Vec::new();
        let mut err = Vec::new();
        loop {
            if cancellable && self.is_cancelled() {
                return Err(cancelled());
            }
            if start.elapsed() >= timeout {
                return Err(OperationError::unavailable(format!(
                    "{program} exceeded its {:.3}s deadline",
                    timeout.as_secs_f64()
                )));
            }
            let out_done = drain(&mut stdout, &mut out, OUTPUT_LIMIT, true)?;
            let err_done = drain(&mut stderr, &mut err, OUTPUT_LIMIT, true)?;
            if exited(process.pid)? {
                process.stop()?;
                // All owned writers have been terminated. Nonblocking reads
                // also remain bounded if an external process inherited a pipe.
                drain(&mut stdout, &mut out, OUTPUT_LIMIT, true)?;
                drain(&mut stderr, &mut err, OUTPUT_LIMIT, true)?;
                break;
            }
            if out_done && err_done {
                thread::sleep(TICK);
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
        let output = Output {
            status: process.status.expect("reaped command"),
            stdout: out,
            stderr: err,
        };
        if check && !output.status.success() {
            return Err(OperationError::unavailable(format!(
                "{program} exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr[..output.stderr.len().min(4096)]).trim()
            )));
        }
        Ok(output)
    }
}
fn cancelled() -> OperationError {
    OperationError::new(ErrorCode::Cancelled, "Command cancelled")
}

// Limit work per turn as well as total retained output: an endless producer
// cannot starve cancellation/deadline checks by continuously refilling a pipe.
fn drain(
    reader: &mut impl Read,
    output: &mut Vec<u8>,
    limit: usize,
    reject_overflow: bool,
) -> Result<bool> {
    let mut buffer = [0_u8; 8192];
    for _ in 0..128 {
        match reader.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                let keep = n.min(limit.saturating_sub(output.len()));
                output.extend_from_slice(&buffer[..keep]);
                if reject_overflow && keep < n {
                    return Err(OperationError::unavailable(
                        "Command output exceeded 64 MiB",
                    ));
                }
            }
            Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(false),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

pub struct OwnedChild {
    process: Process,
    stderr_stop: Arc<AtomicBool>,
    stderr_thread: Option<JoinHandle<Result<Vec<u8>>>>,
}
impl OwnedChild {
    pub fn spawn(program: &str, args: &[String], stdout: Stdio) -> Result<Self> {
        let helper = crate::paths::maintenance_executable()?;
        Self::spawn_with_helper(&helper, program, args, stdout)
    }
    /// Explicit same-layout paths support isolated staged installations. Never
    /// resolve an audio helper through PATH or a caller-controlled override.
    pub fn spawn_in(
        paths: &crate::paths::RuntimePaths,
        program: &str,
        args: &[String],
        stdout: Stdio,
    ) -> Result<Self> {
        Self::spawn_with_helper(&paths.maintenance, program, args, stdout)
    }
    fn spawn_with_helper(
        helper: &std::path::Path,
        program: &str,
        args: &[String],
        stdout: Stdio,
    ) -> Result<Self> {
        let child = Command::new(helper)
            .arg("exec-child")
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .arg("--")
            .arg(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::piped())
            .spawn()?;
        let mut process = Process::new(child, false);
        let mut stderr = process
            .child
            .as_mut()
            .expect("owned child")
            .stderr
            .take()
            .expect("piped stderr");
        nonblocking(&stderr)?;
        let stderr_stop = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stderr_stop);
        let stderr_thread = thread::Builder::new()
            .name("openwave-child-stderr".into())
            .spawn(move || {
                let mut bytes = Vec::new();
                loop {
                    if drain(&mut stderr, &mut bytes, 65536, false)? || stop.load(Ordering::Acquire)
                    {
                        break;
                    }
                    thread::sleep(TICK);
                }
                Ok(bytes)
            })?;
        Ok(Self {
            process,
            stderr_stop,
            stderr_thread: Some(stderr_thread),
        })
    }
    pub fn id(&self) -> u32 {
        self.process.pid.as_raw_pid() as u32
    }
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.process.child.as_mut()?.stdout.take()
    }
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        self.process.try_wait()
    }
    pub fn terminate(&mut self) -> Result<()> {
        let result = self.process.stop();
        self.stderr_stop.store(true, Ordering::Release);
        let drained = self
            .stderr_thread
            .take()
            .map(|thread| {
                thread
                    .join()
                    .map_err(|_| OperationError::unavailable("Child stderr reader panicked"))
                    .and_then(|result| result.map(|_| ()))
            })
            .unwrap_or(Ok(()));
        result.and(drained)
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

/// Called only by the freshly executed, single-threaded maintenance binary.
/// Success replaces that process; no Rust pre_exec hook runs in the parent.
pub fn exec_child(parent_pid: u32, program: &str, args: &[String]) -> Result<()> {
    if rustix::process::geteuid().is_root() || rustix::process::getuid().is_root() {
        return Err(OperationError::invalid(
            "exec-child must run as the ordinary login user, never root",
        ));
    }
    let parent = i32::try_from(parent_pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| OperationError::invalid("Invalid parent PID"))?;
    rustix::process::set_parent_process_death_signal(Some(Signal::TERM)).map_err(os_error)?;
    if rustix::process::getppid() != Some(parent) {
        return Err(OperationError::new(
            ErrorCode::Cancelled,
            "Audio owner exited before child protection was established",
        ));
    }
    rustix::process::setsid().map_err(os_error)?;
    Err(Command::new(program).args(args).exec().into())
}

pub fn require_user() -> Result<()> {
    if rustix::process::geteuid().is_root() || rustix::process::getuid().is_root() {
        return Err(OperationError::invalid(
            "Run OpenWave as the login user; root is reserved for validated maintenance operations",
        ));
    }
    Ok(())
}

/// Applied only to the trusted same-executable login-user handoff. The fresh
/// child verifies its root parent before changing credentials; no pre_exec hook.
pub fn configure_supervised_child(command: &mut Command) -> Result<()> {
    if !SUPERVISED.load(Ordering::Acquire) {
        return Ok(());
    }
    if !rustix::process::geteuid().is_root() {
        return Err(OperationError::invalid(
            "Only the root job may create a login-user handoff",
        ));
    }
    let executable = std::env::current_exe()?;
    if command.get_program() != executable.as_os_str() {
        return Err(OperationError::invalid(
            "Supervised handoff must use the same trusted executable",
        ));
    }
    command
        .arg("--supervised-parent")
        .arg(std::process::id().to_string())
        .arg("--supervised-group")
        .arg(process_group(rustix::process::getpid())?.to_string());
    Ok(())
}

/// Hidden entries cannot turn an arbitrary root process into a trusted parent.
/// All checks run in a new single-threaded root process, before dropping UID.
pub fn enter_supervised(
    parent_pid: u32,
    group: Option<u32>,
    login: Option<(u32, u32)>,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    if std::fs::read_dir("/proc/self/task")?.take(2).count() != 1 {
        return Err(OperationError::invalid(
            "Credential handoff requires a fresh single-threaded process",
        ));
    }
    if !rustix::process::getuid().is_root() || !rustix::process::geteuid().is_root() {
        return Err(OperationError::invalid(
            "Internal supervision requires a root parent and fresh root child",
        ));
    }
    let parent = i32::try_from(parent_pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| OperationError::invalid("Invalid supervisor PID"))?;
    rustix::process::set_parent_process_death_signal(Some(Signal::KILL)).map_err(os_error)?;
    if rustix::process::getppid() != Some(parent) {
        return Err(cancelled());
    }
    let own = std::fs::metadata("/proc/self/exe")?;
    let parent_exe = std::fs::metadata(format!("/proc/{parent_pid}/exe"))?;
    let status = std::fs::read_to_string(format!("/proc/{parent_pid}/status"))?;
    let root = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .is_some_and(|ids| ids.split_whitespace().all(|id| id == "0"));
    let pgid = process_group(rustix::process::getpid())?;
    let parent_group = process_group(parent)?;
    let correct_group = match group {
        None => pgid == std::process::id() && parent_group != pgid,
        Some(group) => group != 0 && pgid == group && parent_group == pgid,
    };
    if !root || own.dev() != parent_exe.dev() || own.ino() != parent_exe.ino() || !correct_group {
        return Err(OperationError::invalid(
            "Untrusted internal supervision parent or process group",
        ));
    }
    crate::paths::trusted_for_root(&std::env::current_exe()?)?;
    if let Some((uid, gid)) = login {
        if uid == 0 {
            return Err(OperationError::invalid("Login handoff cannot retain root"));
        }
        // The trusted root caller selected this account and environment. Clear
        // inherited supplementary authority before the irreversible UID drop.
        rustix::thread::set_thread_groups(&[]).map_err(os_error)?;
        rustix::thread::set_thread_gid(rustix::process::Gid::from_raw(gid)).map_err(os_error)?;
        rustix::thread::set_thread_uid(rustix::process::Uid::from_raw(uid)).map_err(os_error)?;
        // Changing credentials clears PDEATHSIG on Linux.
        rustix::process::set_parent_process_death_signal(Some(Signal::KILL)).map_err(os_error)?;
        require_user()?;
    }
    if rustix::process::getppid() != Some(parent) {
        return Err(cancelled());
    }
    SUPERVISED.store(true, Ordering::Release);
    Ok(())
}

/// Root-only supervisor for an already parsed allowed CLI operation. This never
/// accepts an executable: it starts a fresh copy of this trusted executable.
pub fn supervise_maintenance(args: &[std::ffi::OsString], cancel_on_stdin: bool) -> Result<u8> {
    if !rustix::process::getuid().is_root() || !rustix::process::geteuid().is_root() {
        return Err(OperationError::invalid(
            "Maintenance supervision requires root",
        ));
    }
    let executable = std::env::current_exe()?;
    crate::paths::trusted_for_root(&executable)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let term = signal_hook::flag::register(signal_hook::consts::SIGTERM, cancelled.clone())?;
    let interrupt =
        match signal_hook::flag::register(signal_hook::consts::SIGINT, cancelled.clone()) {
            Ok(id) => id,
            Err(error) => {
                signal_hook::low_level::unregister(term);
                return Err(error.into());
            }
        };
    let result = (|| {
        let parent = rustix::process::getppid();
        rustix::process::set_parent_process_death_signal(Some(Signal::TERM)).map_err(os_error)?;
        if rustix::process::getppid() != parent {
            return Err(self::cancelled());
        }
        // Rustix exposes this boolean prctl through Option<Pid>; 1 enables it.
        rustix::process::set_child_subreaper(Some(Pid::INIT)).map_err(os_error)?;
        let mut stdin = std::io::stdin();
        if cancel_on_stdin {
            nonblocking(&stdin)?;
        }
        let child = Command::new(executable)
            .arg("--worker-parent")
            .arg(std::process::id().to_string())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?;
        let mut process = Process::new(child, true);
        let mut stdout = process
            .child
            .as_mut()
            .expect("owned worker")
            .stdout
            .take()
            .expect("piped stdout");
        let mut stderr = process
            .child
            .as_mut()
            .expect("owned worker")
            .stderr
            .take()
            .expect("piped stderr");
        let mut out = Vec::new();
        let mut err = Vec::new();
        let run = (|| {
            nonblocking(&stdout)?;
            nonblocking(&stderr)?;
            let start = Instant::now();
            loop {
                if cancelled.load(Ordering::Acquire) {
                    return Err(self::cancelled());
                }
                if start.elapsed() >= PRIVILEGED_DEADLINE {
                    return Err(OperationError::unavailable(
                        "Privileged operation exceeded its total 120s deadline; retained authority is required for retry",
                    ));
                }
                if cancel_on_stdin {
                    let mut byte = [0; 1];
                    match stdin.read(&mut byte) {
                        Ok(0) => return Err(self::cancelled()),
                        Ok(_) => {
                            return Err(OperationError::invalid(
                                "Cancellation pipe must carry only EOF",
                            ));
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                ErrorKind::WouldBlock | ErrorKind::Interrupted
                            ) =>
                        {
                            ()
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                drain(&mut stdout, &mut out, OUTPUT_LIMIT, true)?;
                drain(&mut stderr, &mut err, OUTPUT_LIMIT, true)?;
                if exited(process.pid)? {
                    return Ok(());
                }
                thread::sleep(TICK);
            }
        })();
        let stopped = process.stop();
        let drained = drain(&mut stdout, &mut out, OUTPUT_LIMIT, false)
            .and_then(|_| drain(&mut stderr, &mut err, OUTPUT_LIMIT, false));
        if let Some(reaper) = process.reaper.take() {
            // An uninterruptible kernel wait cannot be made deadline-bounded.
            // Retain this root owner until reap; the user-side pipe/owned reaper
            // returns a bounded partial failure instead of waiting for us.
            let _ = reaper.join();
        }
        // The dead worker's orphaned queries are now our children. Reap only
        // this job group, never another child or a process found by name. A
        // kernel-stuck descendant keeps this root owner alive, not the caller.
        loop {
            match rustix::process::waitid(
                WaitId::Pgid(Some(process.pid)),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG,
            ) {
                Ok(Some(_)) => (),
                Ok(None) => thread::sleep(TICK),
                Err(rustix::io::Errno::INTR) => (),
                Err(rustix::io::Errno::CHILD) => break,
                Err(error) => return Err(os_error(error)),
            }
        }
        drained?;
        stopped?;
        run?;
        forward_output(&mut std::io::stdout(), &out)?;
        forward_output(&mut std::io::stderr(), &err)?;
        Ok(process
            .status
            .expect("reaped root worker")
            .code()
            .unwrap_or(1)
            .clamp(0, 255) as u8)
    })();
    signal_hook::low_level::unregister(term);
    signal_hook::low_level::unregister(interrupt);
    result
}

fn forward_output(writer: &mut (impl std::io::Write + AsFd), mut bytes: &[u8]) -> Result<()> {
    nonblocking(writer)?;
    let deadline = Instant::now() + TERM_GRACE;
    while !bytes.is_empty() {
        match writer.write(bytes) {
            Ok(0) => {
                return Err(OperationError::unavailable(
                    "Maintenance output pipe closed",
                ));
            }
            Ok(n) => bytes = &bytes[n..],
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted)
                    && Instant::now() < deadline =>
            {
                thread::sleep(TICK)
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod supervision_tests {
    use super::*;
    use std::{fs, path::Path};

    fn dead(pid: u32) -> bool {
        fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
            stat.rsplit_once(')')
                .is_some_and(|(_, fields)| fields.trim_start().starts_with('Z'))
        })
    }

    fn disposable() {
        assert_eq!(
            std::env::var("OPENWAVE_DISPOSABLE_SUPERVISION_PROOF").as_deref(),
            Ok("yes")
        );
        assert!(rustix::process::getuid().is_root());
        let map = fs::read_to_string("/proc/self/uid_map").unwrap();
        assert_ne!(
            map.split_whitespace().collect::<Vec<_>>(),
            ["0", "0", "4294967295"],
            "Refuse the initial host user namespace"
        );
    }

    /// The fixture requires a disposable root filesystem/user namespace mapping
    /// UID/GID 0 and 1000, a trusted helper at the named /opt path, and a trusted
    /// /usr/bin/dpkg-query shell fixture. That query writes its PPID, PID and a
    /// TERM-ignoring sleep child's PID to worker.pid/query.pid/descendant.pid in
    /// /run/openwave-supervision-fixture, then waits. The shell/sleep binaries
    /// must exist inside this namespace. No fixture replaces any host program.
    #[test]
    #[ignore = "requires a provisioned disposable root/user namespace; total deadline case takes 120 seconds"]
    fn privileged_deadline_eof_and_sigterm_reap_the_root_job() {
        disposable();
        for mode in ["eof", "sigterm", "total-deadline"] {
            for name in ["worker.pid", "query.pid", "descendant.pid"] {
                let path = Path::new("/run/openwave-supervision-fixture").join(name);
                if path.exists() {
                    fs::remove_file(path).unwrap();
                }
            }
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "process::supervision_tests::elevated_owner_fixture",
                    "--nocapture",
                ])
                .env("OPENWAVE_SUPERVISION_CASE", mode)
                .status()
                .unwrap();
            assert!(status.success(), "disposable scenario {mode} failed");
        }
    }

    #[test]
    fn elevated_owner_fixture() {
        let Ok(mode) = std::env::var("OPENWAVE_SUPERVISION_CASE") else {
            return;
        };
        disposable();
        let helper = std::env::var("OPENWAVE_DISPOSABLE_SUPERVISION_HELPER").unwrap();
        assert!(helper.starts_with("/opt/openwave-supervision-fixture/"));
        crate::paths::trusted_for_root(Path::new(&helper)).unwrap();
        let child = Command::new(helper)
            .args(["--cancel-on-stdin", "install-udev"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut process = Process::new(child, true);
        process.cancellation = process.child.as_mut().unwrap().stdin.take();
        let start = Instant::now();
        let mut pids = Vec::new();
        for name in ["worker.pid", "query.pid", "descendant.pid"] {
            let path = Path::new("/run/openwave-supervision-fixture").join(name);
            loop {
                if let Ok(text) = fs::read_to_string(&path) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        pids.push(pid);
                        break;
                    }
                }
                assert!(
                    start.elapsed() < Duration::from_secs(3),
                    "query failed to start"
                );
                thread::sleep(TICK);
            }
        }
        let group = Pid::from_raw(pids[0] as i32).unwrap();
        for pid in &pids {
            assert_eq!(
                rustix::process::getpgid(Pid::from_raw(*pid as i32)).unwrap(),
                group
            );
        }
        let mut unrelated = Command::new("sleep")
            .arg("180")
            .uid(1000)
            .gid(1000)
            .process_group(0)
            .spawn()
            .unwrap();
        if mode == "total-deadline" {
            // Freeze the root worker before its shorter query deadline. Its
            // root supervisor must still bound the entire job independently.
            rustix::process::kill_process(group, Signal::STOP).unwrap();
        }
        if mode == "sigterm" {
            rustix::process::kill_process(process.pid, Signal::TERM).unwrap();
        }
        rustix::thread::set_thread_groups(&[]).unwrap();
        rustix::thread::set_thread_gid(rustix::process::Gid::from_raw(1000)).unwrap();
        rustix::thread::set_thread_uid(rustix::process::Uid::from_raw(1000)).unwrap();
        require_user().unwrap();
        if mode != "sigterm" {
            assert_eq!(
                rustix::process::kill_process(process.pid, Signal::TERM),
                Err(rustix::io::Errno::PERM),
                "fixture must exercise a genuinely unsignalable elevated child"
            );
        }
        let cancellation_start = Instant::now();
        if mode == "total-deadline" {
            while !exited(process.pid).unwrap() {
                assert!(start.elapsed() < PRIVILEGED_DEADLINE + Duration::from_secs(4));
                thread::sleep(TICK);
            }
        }
        process.stop().unwrap();
        assert!(
            !process.status.unwrap().success(),
            "cancellation is partial failure, never removal success"
        );
        if mode != "total-deadline" {
            assert!(cancellation_start.elapsed() < Duration::from_secs(3));
        }
        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "cleanup escaped its owned group"
        );
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while pids.iter().any(|pid| !dead(*pid)) && Instant::now() < deadline {
            thread::sleep(TICK);
        }
        assert!(
            pids.iter().all(|pid| dead(*pid)),
            "root worker/query descendant survived cancellation"
        );
        assert!(
            process.reaper.is_none(),
            "cooperative root cancellation did not acknowledge reaping"
        );
    }
}

#[cfg(test)]
mod pending_reaper_tests {
    use super::*;
    use std::sync::mpsc;

    struct FixtureChild(Child);
    impl Drop for FixtureChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    struct PendingFixture {
        process: Process,
        release: Option<mpsc::Sender<()>>,
    }
    impl Drop for PendingFixture {
        fn drop(&mut self) {
            self.release.take();
            if let Some(reaper) = self.process.reaper.take() {
                let _ = reaper.join();
            }
        }
    }
    #[test]
    fn pending_reaper_requires_actual_completion_and_retains_status() {
        let mut child = FixtureChild(Command::new("sleep").arg("30").spawn().unwrap());
        let pid = Pid::from_raw(child.0.id() as i32).unwrap();
        let (release, blocked) = mpsc::channel();
        let reaper = thread::spawn(move || {
            let _ = blocked.recv();
            child.0.kill()?;
            child.0.wait()
        });
        let mut fixture = PendingFixture {
            process: Process {
                child: None,
                pid,
                group: false,
                status: None,
                cancellation: None,
                reaper: Some(reaper),
            },
            release: Some(release),
        };
        for _ in 0..3 {
            assert!(fixture.process.stop().is_err());
            assert!(fixture.process.try_wait().is_err());
        }
        fixture.release.take().unwrap().send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fixture.process.reaper.as_ref().unwrap().is_finished() {
            assert!(Instant::now() < deadline, "fixture reaper did not finish");
            thread::sleep(TICK);
        }
        let status = fixture.process.try_wait().unwrap().unwrap();
        assert!(!status.success());
        fixture.process.stop().unwrap();
        assert_eq!(fixture.process.try_wait().unwrap(), Some(status));
    }
}
