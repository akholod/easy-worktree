use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;

#[derive(Clone, Default)]
pub(crate) struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    #[cfg(test)]
    pub(crate) fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    fn cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
#[derive(Clone, Copy)]
pub(crate) struct TimingPolicy {
    pub poll: Duration,
    pub term_grace: Duration,
    pub drain_grace: Duration,
}
impl Default for TimingPolicy {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(25),
            term_grace: Duration::from_secs(2),
            drain_grace: Duration::from_secs(2),
        }
    }
}
#[cfg(test)]
fn test_timing() -> TimingPolicy {
    TimingPolicy {
        poll: Duration::from_millis(2),
        term_grace: Duration::from_millis(40),
        drain_grace: Duration::from_secs(2),
    }
}
pub(crate) struct RuntimeInput<'a> {
    pub common_dir: &'a Path,
    pub operation_id: Uuid,
    pub step_id: &'a str,
    pub argv: &'a [String],
    pub cwd: &'a Path,
    pub environment_allowlist: &'a [String],
    pub token: CancellationToken,
    pub timing: TimingPolicy,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskOutcome {
    Success,
    NonZero,
    Signaled,
    Cancelled,
    RuntimeFailed,
    SpawnFailed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskResult {
    pub outcome: TaskOutcome,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub stdout_count: u64,
    pub stderr_count: u64,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskRuntimeError {
    #[cfg(not(unix))]
    Unsupported,
    InvalidInput,
    Collision,
    Spawn,
    Io,
    NonZero,
    Cancelled,
    Signaled,
    Runtime,
}
impl std::fmt::Display for TaskRuntimeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            #[cfg(not(unix))]
            Self::Unsupported => "task runtime unsupported",
            Self::InvalidInput => "invalid task runtime input",
            Self::Collision => "task log collision",
            Self::Spawn => "task spawn failed",
            Self::Io => "task runtime I/O failed",
            Self::NonZero => "task exited unsuccessfully",
            Self::Cancelled => "task cancelled",
            Self::Signaled => "task terminated by signal",
            Self::Runtime => "task runtime failed",
        })
    }
}
impl std::error::Error for TaskRuntimeError {}

#[cfg(unix)]
mod unix {
    use super::*;
    use rustix::{
        fs::{
            FileType, Mode, OFlags, fchmod, fcntl_getfl, fcntl_setfl, fstat, fsync, mkdirat, open,
            openat,
        },
        process::{Pid, Signal, kill_process_group, test_kill_process_group},
    };
    use sha2::{Digest, Sha256};
    use std::{
        io::{Read, Seek, SeekFrom, Write},
        os::{
            fd::OwnedFd,
            unix::process::{CommandExt, ExitStatusExt},
        },
        process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio},
        time::Instant,
    };
    const LIMIT: usize = 1024 * 1024;
    const MAX_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
    const DOMAIN: &[u8] = b"ewtm-task-log-v1\0";
    struct StreamPump<R> {
        reader: R,
        log: std::fs::File,
        total: u64,
        retained: usize,
        truncated: bool,
        eof: bool,
        read_error: bool,
        log_error: bool,
        watermark: usize,
    }
    impl<R: Read + std::os::fd::AsFd> StreamPump<R> {
        fn nonblocking(&self) -> Result<(), TaskRuntimeError> {
            let f = fcntl_getfl(&self.reader).map_err(|_| TaskRuntimeError::Io)?;
            fcntl_setfl(&self.reader, f | OFlags::NONBLOCK).map_err(|_| TaskRuntimeError::Io)
        }
        fn pump(&mut self, budget: usize) -> bool {
            let mut b = [0u8; 16384];
            let mut left = budget;
            let mut progress = false;
            while left != 0 && !self.eof {
                #[cfg(test)]
                if TEST_READ_FAULT.with(|x| x.get()) {
                    self.read_error = true;
                    break;
                }
                match self.reader.read(&mut b) {
                    Ok(0) => {
                        self.eof = true;
                        progress = true
                    }
                    Ok(n) => {
                        progress = true;
                        left = left.saturating_sub(n);
                        self.total += n as u64;
                        let keep = (LIMIT - self.retained).min(n);
                        if keep != 0 {
                            #[cfg(test)]
                            let injected = TEST_LOG_FAULT.with(|x| x.get());
                            #[cfg(not(test))]
                            let injected = false;
                            if injected || self.log.write_all(&b[..keep]).is_err() {
                                self.log_error = true;
                            }
                            self.retained += keep;
                            while self.watermark <= self.retained {
                                self.watermark += 65536;
                                if self.log.sync_data().is_err() {
                                    self.log_error = true;
                                }
                            }
                        }
                        if keep < n {
                            self.truncated = true;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(_) => {
                        self.read_error = true;
                        break;
                    }
                }
            }
            progress
        }
        fn finish(&mut self) {
            if self.log.sync_all().is_err() {
                self.log_error = true;
            }
        }
    }
    #[cfg(test)]
    thread_local! {
        static TEST_LOG_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static TEST_READ_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static TEST_RESULT_SYNC_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static TEST_GROUP_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static TEST_GROUP_PRESENT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
        static TEST_SIGNAL_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static TEST_REAP_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    #[cfg(test)]
    #[derive(Clone, Copy, Debug)]
    pub(super) enum TestFault {
        Read,
        Log,
        ResultSync,
        Group,
        Signal,
        Reap,
        GroupPresent(u32),
    }
    #[cfg(test)]
    pub(super) struct TestFaultGuard {
        fault: TestFault,
    }
    #[cfg(test)]
    impl TestFaultGuard {
        pub(super) fn new(fault: TestFault) -> Self {
            match fault {
                TestFault::Read => TEST_READ_FAULT.with(|x| x.set(true)),
                TestFault::Log => TEST_LOG_FAULT.with(|x| x.set(true)),
                TestFault::ResultSync => TEST_RESULT_SYNC_FAULT.with(|x| x.set(true)),
                TestFault::Group => TEST_GROUP_FAULT.with(|x| x.set(true)),
                TestFault::Signal => TEST_SIGNAL_FAULT.with(|x| x.set(true)),
                TestFault::Reap => TEST_REAP_FAULT.with(|x| x.set(true)),
                TestFault::GroupPresent(n) => TEST_GROUP_PRESENT.with(|x| x.set(n)),
            }
            Self { fault }
        }
    }
    #[cfg(test)]
    impl Drop for TestFaultGuard {
        fn drop(&mut self) {
            match self.fault {
                TestFault::Read => TEST_READ_FAULT.with(|x| x.set(false)),
                TestFault::Log => TEST_LOG_FAULT.with(|x| x.set(false)),
                TestFault::ResultSync => TEST_RESULT_SYNC_FAULT.with(|x| x.set(false)),
                TestFault::Group => TEST_GROUP_FAULT.with(|x| x.set(false)),
                TestFault::Signal => TEST_SIGNAL_FAULT.with(|x| x.set(false)),
                TestFault::Reap => TEST_REAP_FAULT.with(|x| x.set(false)),
                TestFault::GroupPresent(_) => TEST_GROUP_PRESENT.with(|x| x.set(0)),
            }
        }
    }
    #[cfg(test)]
    pub(super) fn test_faults_clear() -> bool {
        !TEST_READ_FAULT.with(|value| value.get())
            && !TEST_LOG_FAULT.with(|value| value.get())
            && !TEST_RESULT_SYNC_FAULT.with(|value| value.get())
            && !TEST_GROUP_FAULT.with(|value| value.get())
            && TEST_GROUP_PRESENT.with(|value| value.get()) == 0
            && !TEST_SIGNAL_FAULT.with(|value| value.get())
            && !TEST_REAP_FAULT.with(|value| value.get())
    }
    struct LogFiles {
        dir: OwnedFd,
        stdout: std::fs::File,
        stderr: std::fs::File,
        result: std::fs::File,
    }
    fn checked_dir(fd: &OwnedFd) -> Result<(), TaskRuntimeError> {
        let s = fstat(fd).map_err(|_| TaskRuntimeError::Io)?;
        if FileType::from_raw_mode(s.st_mode).is_dir()
            && s.st_mode & 0o7777 == 0o700
            && s.st_uid == rustix::process::geteuid().as_raw()
        {
            Ok(())
        } else {
            Err(TaskRuntimeError::Collision)
        }
    }
    fn child_dir(
        parent: &OwnedFd,
        name: &std::ffi::OsStr,
        create: bool,
    ) -> Result<OwnedFd, TaskRuntimeError> {
        let fl = OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::RDONLY;
        match openat(parent, name, fl, Mode::empty()) {
            Ok(fd) => {
                checked_dir(&fd)?;
                Ok(fd)
            }
            Err(e) if create && e == rustix::io::Errno::NOENT => {
                mkdirat(parent, name, Mode::from_raw_mode(0o700))
                    .map_err(|_| TaskRuntimeError::Io)?;
                fsync(parent).map_err(|_| TaskRuntimeError::Io)?;
                let fd =
                    openat(parent, name, fl, Mode::empty()).map_err(|_| TaskRuntimeError::Io)?;
                fchmod(&fd, Mode::from_raw_mode(0o700)).map_err(|_| TaskRuntimeError::Io)?;
                checked_dir(&fd)?;
                fsync(&fd).map_err(|_| TaskRuntimeError::Io)?;
                Ok(fd)
            }
            Err(_) => Err(TaskRuntimeError::Collision),
        }
    }
    fn private_state_root(parent: &OwnedFd) -> Result<OwnedFd, TaskRuntimeError> {
        let name = std::ffi::OsStr::new("ewtm");
        let flags = OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::RDONLY;
        let (fd, created) = match openat(parent, name, flags, Mode::empty()) {
            Ok(fd) => (fd, false),
            Err(e) if e == rustix::io::Errno::NOENT => {
                mkdirat(parent, name, Mode::from_raw_mode(0o700))
                    .map_err(|_| TaskRuntimeError::Io)?;
                (
                    openat(parent, name, flags, Mode::empty()).map_err(|_| TaskRuntimeError::Io)?,
                    true,
                )
            }
            Err(_) => return Err(TaskRuntimeError::Collision),
        };
        let s = fstat(&fd).map_err(|_| TaskRuntimeError::Io)?;
        if !FileType::from_raw_mode(s.st_mode).is_dir()
            || s.st_uid != rustix::process::geteuid().as_raw()
        {
            return Err(TaskRuntimeError::Collision);
        }
        fchmod(&fd, Mode::from_raw_mode(0o700)).map_err(|_| TaskRuntimeError::Io)?;
        checked_dir(&fd)?;
        fsync(&fd).map_err(|_| TaskRuntimeError::Io)?;
        if created {
            fsync(parent).map_err(|_| TaskRuntimeError::Io)?;
        }
        Ok(fd)
    }
    fn files(i: &RuntimeInput<'_>) -> Result<LogFiles, TaskRuntimeError> {
        if !i.common_dir.is_absolute()
            || !i.cwd.is_absolute()
            || i.argv.is_empty()
            || i.argv.iter().any(|x| x.contains('\0'))
            || i.step_id.is_empty()
            || i.step_id.contains('\0')
            || i.environment_allowlist
                .iter()
                .any(|x| x.is_empty() || x.contains('\0') || x.contains('='))
        {
            return Err(TaskRuntimeError::InvalidInput);
        }
        let mut seen = std::collections::BTreeSet::new();
        if i.environment_allowlist.iter().any(|x| !seen.insert(x)) {
            return Err(TaskRuntimeError::InvalidInput);
        }
        if !crate::infrastructure::readonly_safe_directory(i.cwd)
            .map_err(|_| TaskRuntimeError::Io)?
        {
            return Err(TaskRuntimeError::InvalidInput);
        }
        let common = open(
            i.common_dir,
            OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::RDONLY,
            Mode::empty(),
        )
        .map_err(|_| TaskRuntimeError::Io)?;
        let e = private_state_root(&common)?;
        let l = child_dir(&e, std::ffi::OsStr::new("task-logs"), true)?;
        let v = child_dir(&l, std::ffi::OsStr::new("v1"), true)?;
        let op = child_dir(&v, std::ffi::OsStr::new(&i.operation_id.to_string()), true)?;
        let mut h = Sha256::new();
        h.update(DOMAIN);
        h.update(i.operation_id.as_bytes());
        h.update([0]);
        h.update(i.step_id.as_bytes());
        let n = format!("{:x}", h.finalize());
        mkdirat(&op, std::ffi::OsStr::new(&n), Mode::from_raw_mode(0o700)).map_err(|e| {
            if e == rustix::io::Errno::EXIST {
                TaskRuntimeError::Collision
            } else {
                TaskRuntimeError::Io
            }
        })?;
        fsync(&op).map_err(|_| TaskRuntimeError::Io)?;
        let dir = openat(
            &op,
            std::ffi::OsStr::new(&n),
            OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::RDONLY,
            Mode::empty(),
        )
        .map_err(|_| TaskRuntimeError::Io)?;
        fchmod(&dir, Mode::from_raw_mode(0o700)).map_err(|_| TaskRuntimeError::Io)?;
        checked_dir(&dir)?;
        fsync(&dir).map_err(|_| TaskRuntimeError::Io)?;
        let make = |name: &str| -> Result<std::fs::File, TaskRuntimeError> {
            let fd = openat(
                &dir,
                std::ffi::OsStr::new(name),
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|e| {
                if e == rustix::io::Errno::EXIST {
                    TaskRuntimeError::Collision
                } else {
                    TaskRuntimeError::Io
                }
            })?;
            fchmod(&fd, Mode::from_raw_mode(0o600)).map_err(|_| TaskRuntimeError::Io)?;
            let s = fstat(&fd).map_err(|_| TaskRuntimeError::Io)?;
            if !FileType::from_raw_mode(s.st_mode).is_file()
                || s.st_mode & 0o7777 != 0o600
                || s.st_uid != rustix::process::geteuid().as_raw()
            {
                return Err(TaskRuntimeError::Collision);
            }
            fsync(&fd).map_err(|_| TaskRuntimeError::Io)?;
            Ok(std::fs::File::from(fd))
        };
        let stdout = make("stdout.log")?;
        let stderr = make("stderr.log")?;
        let result = make("result.json")?;
        fsync(&dir).map_err(|_| TaskRuntimeError::Io)?;
        Ok(LogFiles {
            dir,
            stdout,
            stderr,
            result,
        })
    }
    struct ChildSession {
        child: Child,
        pid: Pid,
        stdout: Option<StreamPump<ChildStdout>>,
        stderr: Option<StreamPump<ChildStderr>>,
        result: std::fs::File,
        dir: OwnedFd,
        finalized: bool,
    }
    impl Drop for ChildSession {
        fn drop(&mut self) {
            if !self.finalized {
                let _ = kill_process_group(self.pid, Signal::KILL);
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
    fn group(pid: Pid) -> Result<bool, TaskRuntimeError> {
        #[cfg(test)]
        if TEST_GROUP_FAULT.with(|x| x.get()) {
            return Err(TaskRuntimeError::Runtime);
        }
        #[cfg(test)]
        if TEST_GROUP_PRESENT.with(|x| {
            let n = x.get();
            if n != 0 {
                x.set(n - 1);
                true
            } else {
                false
            }
        }) {
            return Ok(true);
        }
        match test_kill_process_group(pid) {
            Ok(()) => Ok(true),
            Err(e) if e == rustix::io::Errno::SRCH || e == rustix::io::Errno::NOENT => Ok(false),
            Err(_) => Err(TaskRuntimeError::Runtime),
        }
    }
    fn signal_group(pid: Pid, signal: Signal) -> Result<bool, TaskRuntimeError> {
        #[cfg(test)]
        if TEST_SIGNAL_FAULT.with(|x| x.get()) {
            return Err(TaskRuntimeError::Runtime);
        }
        match kill_process_group(pid, signal) {
            Ok(()) => Ok(true),
            Err(e) if e == rustix::io::Errno::SRCH || e == rustix::io::Errno::NOENT => Ok(false),
            Err(_) => Err(TaskRuntimeError::Runtime),
        }
    }
    fn deadline(d: Duration) -> Option<Instant> {
        Instant::now().checked_add(d)
    }
    fn metadata(
        f: &mut std::fs::File,
        r: &TaskResult,
        phase: Option<&str>,
        flags: [bool; 8],
    ) -> Result<(), TaskRuntimeError> {
        f.seek(SeekFrom::Start(0))
            .map_err(|_| TaskRuntimeError::Io)?;
        let v = serde_json::json!({"version":1,"outcome":match r.outcome{TaskOutcome::Success=>"success",TaskOutcome::NonZero=>"nonzero",TaskOutcome::Signaled=>"signaled",TaskOutcome::Cancelled=>"cancelled",TaskOutcome::RuntimeFailed=>"runtime_failed",TaskOutcome::SpawnFailed=>"spawn_failed"},"exit_code":r.exit_code,"signal":r.signal,"cancellation_phase":phase,"stdout_count":r.stdout_count,"stderr_count":r.stderr_count,"stdout_truncated":r.stdout_truncated,"stderr_truncated":r.stderr_truncated,"stdout_read_error":flags[0],"stdout_log_error":flags[1],"stderr_read_error":flags[2],"stderr_log_error":flags[3],"setup_error":flags[4],"group_error":flags[5],"reap_error":flags[6],"runtime_shutdown":flags[7]});
        let b = serde_json::to_vec(&v).map_err(|_| TaskRuntimeError::Io)?;
        if b.len() > 4096 {
            return Err(TaskRuntimeError::Io);
        }
        f.set_len(0).map_err(|_| TaskRuntimeError::Io)?;
        f.write_all(&b).map_err(|_| TaskRuntimeError::Io)?;
        #[cfg(test)]
        if TEST_RESULT_SYNC_FAULT.with(|x| x.get()) {
            return Err(TaskRuntimeError::Io);
        }
        f.sync_all().map_err(|_| TaskRuntimeError::Io)
    }

    pub(super) fn run(i: &RuntimeInput<'_>) -> Result<TaskResult, TaskRuntimeError> {
        if i.timing.poll.is_zero()
            || i.timing.term_grace.is_zero()
            || i.timing.drain_grace.is_zero()
            || i.timing.poll > MAX_DURATION
            || i.timing.term_grace > MAX_DURATION
            || i.timing.drain_grace > MAX_DURATION
            || deadline(i.timing.poll).is_none()
            || deadline(i.timing.term_grace).is_none()
            || deadline(i.timing.drain_grace).is_none()
        {
            return Err(TaskRuntimeError::InvalidInput);
        }
        let mut logs = files(i)?;
        let mut r = TaskResult {
            outcome: TaskOutcome::RuntimeFailed,
            exit_code: None,
            signal: None,
            stdout_count: 0,
            stderr_count: 0,
            stdout_truncated: false,
            stderr_truncated: false,
        };
        if i.token.cancelled() {
            r.outcome = TaskOutcome::Cancelled;
            let m = metadata(&mut logs.result, &r, Some("before_spawn"), [false; 8]);
            let s = fsync(&logs.dir);
            return if m.is_err() || s.is_err() {
                Err(TaskRuntimeError::Io)
            } else {
                Err(TaskRuntimeError::Cancelled)
            };
        }
        let mut cmd = Command::new(&i.argv[0]);
        cmd.args(&i.argv[1..])
            .current_dir(i.cwd)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        for n in i.environment_allowlist {
            if let Some(v) = std::env::var_os(n) {
                cmd.env(n, v);
            }
        }
        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(_) => {
                r.outcome = TaskOutcome::SpawnFailed;
                let m = metadata(&mut logs.result, &r, Some("spawn"), [false; 8]);
                let s = fsync(&logs.dir);
                return if m.is_err() || s.is_err() {
                    Err(TaskRuntimeError::Io)
                } else {
                    Err(TaskRuntimeError::Spawn)
                };
            }
        };
        let pid = match i32::try_from(child.id()).ok().and_then(Pid::from_raw) {
            Some(p) => p,
            None => {
                let mut c = child;
                let _ = c.kill();
                let _ = c.wait();
                r.outcome = TaskOutcome::RuntimeFailed;
                let _ = metadata(
                    &mut logs.result,
                    &r,
                    Some("setup"),
                    [false, false, false, false, true, false, true, true],
                );
                let _ = fsync(&logs.dir);
                return Err(TaskRuntimeError::Runtime);
            }
        };
        let mut s = ChildSession {
            child,
            pid,
            stdout: None,
            stderr: None,
            result: logs.result,
            dir: logs.dir,
            finalized: false,
        };
        let mut setup = false;
        let out = s.child.stdout.take();
        let err = s.child.stderr.take();
        match (out, err) {
            (Some(o), Some(e)) => {
                let a = StreamPump {
                    reader: o,
                    log: logs.stdout,
                    total: 0,
                    retained: 0,
                    truncated: false,
                    eof: false,
                    read_error: false,
                    log_error: false,
                    watermark: 65536,
                };
                let b = StreamPump {
                    reader: e,
                    log: logs.stderr,
                    total: 0,
                    retained: 0,
                    truncated: false,
                    eof: false,
                    read_error: false,
                    log_error: false,
                    watermark: 65536,
                };
                if a.nonblocking().is_err() || b.nonblocking().is_err() {
                    setup = true
                } else {
                    s.stdout = Some(a);
                    s.stderr = Some(b)
                }
            }
            _ => setup = true,
        }
        let mut status: Option<ExitStatus> = None;
        let mut cancel = false;
        let mut group_error = false;
        let mut reap_error = false;
        let mut runtime_shutdown = setup;
        let mut shutdown = setup;
        let mut end: Option<Instant> = None;
        'monitor: loop {
            let mut progress = false;
            if let Some(p) = s.stdout.as_mut() {
                progress |= p.pump(65536)
            }
            if let Some(p) = s.stderr.as_mut() {
                progress |= p.pump(65536)
            }
            let fault = s
                .stdout
                .as_ref()
                .is_none_or(|p| p.read_error || p.log_error)
                || s.stderr
                    .as_ref()
                    .is_none_or(|p| p.read_error || p.log_error);
            if fault {
                runtime_shutdown = true;
                shutdown = true
            }
            let tok = i.token.cancelled();
            if tok {
                cancel = true;
                shutdown = true
            }
            if status.is_none() {
                #[cfg(test)]
                if TEST_REAP_FAULT.with(|x| x.get()) {
                    reap_error = true;
                    runtime_shutdown = true;
                    shutdown = true;
                }
                match s.child.try_wait() {
                    Ok(v) => {
                        status = v;
                        if status.is_some() && end.is_none() {
                            let out_eof = s.stdout.as_ref().is_some_and(|p| p.eof);
                            let err_eof = s.stderr.as_ref().is_some_and(|p| p.eof);
                            let present = match group(s.pid) {
                                Ok(v) => v,
                                Err(_) => {
                                    group_error = true;
                                    runtime_shutdown = true;
                                    shutdown = true;
                                    false
                                }
                            };
                            if !(out_eof && err_eof) || present {
                                end = deadline(i.timing.drain_grace)
                            }
                        }
                    }
                    Err(_) => {
                        reap_error = true;
                        runtime_shutdown = true;
                        shutdown = true
                    }
                }
            }
            let ge = group(s.pid);
            match ge {
                Ok(false) => {}
                Ok(true) => {
                    if status.is_some() && end.is_none() {
                        end = deadline(i.timing.drain_grace)
                    }
                }
                Err(_) => {
                    group_error = true;
                    runtime_shutdown = true;
                    shutdown = true
                }
            }
            if !shutdown
                && status.is_some()
                && s.stdout.as_ref().is_some_and(|p| p.eof)
                && s.stderr.as_ref().is_some_and(|p| p.eof)
                && matches!(ge, Ok(false))
                && !i.token.cancelled()
            {
                break 'monitor;
            }
            if shutdown {
                break 'monitor;
            }
            if end.is_some_and(|x| Instant::now() >= x) {
                runtime_shutdown = true;
                shutdown = true;
                break 'monitor;
            }
            if !progress {
                std::thread::sleep(i.timing.poll)
            }
        }
        if shutdown {
            if signal_group(s.pid, Signal::TERM).is_err() {
                group_error = true;
                runtime_shutdown = true;
            }
            let term = deadline(i.timing.term_grace);
            while term.is_some_and(|x| Instant::now() < x) {
                if let Some(p) = s.stdout.as_mut() {
                    p.pump(65536);
                }
                if let Some(p) = s.stderr.as_mut() {
                    p.pump(65536);
                }
                if status.is_none() {
                    status = s.child.try_wait().ok().flatten()
                }
                match group(s.pid) {
                    Ok(false) => break,
                    Ok(true) => {}
                    Err(_) => {
                        group_error = true;
                        runtime_shutdown = true;
                    }
                }
                std::thread::sleep(i.timing.poll)
            }
            let still_present = match group(s.pid) {
                Ok(v) => v,
                Err(_) => {
                    group_error = true;
                    runtime_shutdown = true;
                    true
                }
            };
            if still_present {
                if signal_group(s.pid, Signal::KILL).is_err() {
                    group_error = true;
                    runtime_shutdown = true;
                }
                let kill = deadline(i.timing.drain_grace);
                while kill.is_some_and(|x| Instant::now() < x) {
                    if let Some(p) = s.stdout.as_mut() {
                        p.pump(65536);
                    }
                    if let Some(p) = s.stderr.as_mut() {
                        p.pump(65536);
                    }
                    if status.is_none() {
                        status = s.child.try_wait().ok().flatten()
                    }
                    match group(s.pid) {
                        Ok(false) => break,
                        Ok(true) => {}
                        Err(_) => {
                            group_error = true;
                            runtime_shutdown = true;
                        }
                    }
                    std::thread::sleep(i.timing.poll)
                }
                match group(s.pid) {
                    Ok(false) => {}
                    Ok(true) | Err(_) => {
                        group_error = true;
                        runtime_shutdown = true;
                    }
                }
            }
        }
        #[cfg(test)]
        let reap_fault = TEST_REAP_FAULT.with(|x| x.get());
        #[cfg(not(test))]
        let reap_fault = false;
        if status.is_none() && (reap_fault || s.child.kill().is_err()) {
            reap_error = true;
        }
        if status.is_none() {
            match if reap_fault {
                Err(std::io::Error::other("injected reap"))
            } else {
                s.child.wait()
            } {
                Ok(v) => status = Some(v),
                Err(_) => reap_error = true,
            }
        } else if !reap_fault && s.child.wait().is_err() {
            reap_error = true
        }
        if shutdown && i.token.cancelled() {
            cancel = true;
        }
        if let Some(p) = s.stdout.as_mut() {
            p.finish()
        }
        if let Some(p) = s.stderr.as_mut() {
            p.finish()
        }
        let (sr, sl, er, el) = (
            s.stdout.as_ref().is_none_or(|p| p.read_error),
            s.stdout.as_ref().is_none_or(|p| p.log_error),
            s.stderr.as_ref().is_none_or(|p| p.read_error),
            s.stderr.as_ref().is_none_or(|p| p.log_error),
        );
        if sr || sl || er || el {
            runtime_shutdown = true;
        }
        if let Some(p) = s.stdout.as_ref() {
            r.stdout_count = p.total;
            r.stdout_truncated = p.truncated
        }
        if let Some(p) = s.stderr.as_ref() {
            r.stderr_count = p.total;
            r.stderr_truncated = p.truncated
        }
        r.exit_code = status.and_then(|x| x.code());
        r.signal = status.and_then(|x| x.signal());
        r.outcome = if runtime_shutdown || group_error || reap_error {
            TaskOutcome::RuntimeFailed
        } else if cancel {
            TaskOutcome::Cancelled
        } else if r.signal.is_some() {
            TaskOutcome::Signaled
        } else if r.exit_code.is_some_and(|x| x != 0) {
            TaskOutcome::NonZero
        } else {
            TaskOutcome::Success
        };
        let m = metadata(
            &mut s.result,
            &r,
            if cancel { Some("during_run") } else { None },
            [
                sr,
                sl,
                er,
                el,
                setup,
                group_error,
                reap_error,
                runtime_shutdown,
            ],
        );
        let d = fsync(&s.dir);
        s.finalized = true;
        if m.is_err() || d.is_err() {
            return Err(TaskRuntimeError::Io);
        }
        match r.outcome {
            TaskOutcome::Success => Ok(r),
            TaskOutcome::Cancelled => Err(TaskRuntimeError::Cancelled),
            TaskOutcome::NonZero => Err(TaskRuntimeError::NonZero),
            TaskOutcome::Signaled => Err(TaskRuntimeError::Signaled),
            TaskOutcome::SpawnFailed => Err(TaskRuntimeError::Spawn),
            TaskOutcome::RuntimeFailed => Err(TaskRuntimeError::Runtime),
        }
    }
}
pub(crate) fn run_task(i: &RuntimeInput<'_>) -> Result<TaskResult, TaskRuntimeError> {
    #[cfg(unix)]
    {
        unix::run(i)
    }
    #[cfg(not(unix))]
    {
        let _ = i;
        Err(TaskRuntimeError::Unsupported)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn token_is_one_way() {
        let t = CancellationToken::default();
        let c = t.clone();
        assert!(!t.cancelled());
        c.cancel();
        assert!(t.cancelled())
    }
    #[cfg(unix)]
    #[test]
    fn invalid_timing_is_rejected_before_layout() {
        let d = tempfile::tempdir().unwrap();
        let r = run_task(&RuntimeInput {
            common_dir: d.path(),
            operation_id: Uuid::new_v4(),
            step_id: "step",
            argv: &["true".into()],
            cwd: d.path(),
            environment_allowlist: &[],
            token: CancellationToken::default(),
            timing: TimingPolicy {
                poll: Duration::ZERO,
                ..test_timing()
            },
        });
        assert_eq!(r, Err(TaskRuntimeError::InvalidInput));
        assert!(!d.path().join("ewtm").exists())
    }
}
#[cfg(all(test, unix))]
mod runtime_tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{fs, os::unix::fs::PermissionsExt, process::Command, thread, time::Instant};
    fn leaf(d: &Path) -> std::path::PathBuf {
        fs::read_dir(d.join("ewtm/task-logs/v1"))
            .unwrap()
            .flat_map(|o| fs::read_dir(o.unwrap().path()).unwrap())
            .map(|o| o.unwrap().path())
            .next()
            .unwrap()
    }
    fn wait_pid_file(p: &Path) -> u32 {
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(pid) = fs::read_to_string(p)
                .ok()
                .and_then(|value| value.trim().parse().ok())
            {
                return pid;
            }
            assert!(Instant::now() < end, "readiness PID file was not written");
            thread::sleep(test_timing().poll);
        }
    }
    fn gone(pid: u32) -> bool {
        Command::new(utility(&["/bin/kill", "/usr/bin/kill"]))
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|s| !s.success())
    }
    fn wait_gone(pid: u32) -> bool {
        let end = Instant::now() + Duration::from_secs(2);
        while !gone(pid) {
            if Instant::now() >= end {
                return false;
            }
            thread::sleep(test_timing().poll);
        }
        true
    }
    fn input<'a>(d: &'a Path, op: Uuid, step: &'a str, argv: &'a [String]) -> RuntimeInput<'a> {
        RuntimeInput {
            common_dir: d,
            operation_id: op,
            step_id: step,
            argv,
            cwd: d,
            environment_allowlist: &[],
            token: CancellationToken::default(),
            timing: test_timing(),
        }
    }
    fn utility(candidates: &[&str]) -> String {
        for candidate in candidates {
            if Path::new(candidate).is_file() {
                return (*candidate).to_owned();
            }
        }
        panic!("required test utility is unavailable")
    }
    fn shell(body: &str, args: &[&str]) -> Vec<String> {
        let mut argv = vec![
            "/bin/sh".into(),
            "-c".into(),
            body.into(),
            "ewtm-test".into(),
        ];
        argv.extend(args.iter().map(|x| (*x).into()));
        argv
    }
    fn layout(d: &Path, op: Uuid, step: &str) -> std::path::PathBuf {
        let mut h = Sha256::new();
        h.update(b"ewtm-task-log-v1\0");
        h.update(op.as_bytes());
        h.update([0]);
        h.update(step.as_bytes());
        d.join("ewtm/task-logs/v1")
            .join(op.to_string())
            .join(format!("{:x}", h.finalize()))
    }
    fn metadata_at(p: &Path) -> serde_json::Value {
        serde_json::from_slice(&fs::read(p.join("result.json")).unwrap()).unwrap()
    }

    #[test]
    fn direct_argv_cwd_and_null_stdin_are_exact() {
        let d = tempfile::tempdir().unwrap();
        let marker = d.path().join("marker");
        let side = d.path().join("side");
        let body = format!(
            "printf '%s\\n' \"$1\" > '{}'\nread x || true\nprintf '%s' \"$PWD\"",
            marker.display()
        );
        let arg = format!("$(touch '{}');*?'\"", side.display());
        let argv = shell(&body, &[&arg]);
        let operation_id = Uuid::new_v4();
        let step_id = "a/b?*";
        let r = run_task(&input(d.path(), operation_id, step_id, &argv)).unwrap();
        assert_eq!(r.outcome, TaskOutcome::Success);
        assert_eq!(fs::read_to_string(&marker).unwrap().trim_end(), arg);
        assert!(!side.exists());
        let actual = leaf(d.path());
        let child_cwd = fs::read_to_string(actual.join("stdout.log")).unwrap();
        assert_eq!(
            fs::canonicalize(child_cwd.trim_end()).unwrap(),
            fs::canonicalize(d.path()).unwrap()
        );
        assert_eq!(actual, layout(d.path(), operation_id, step_id));
        let root = d.path().join("ewtm/task-logs/v1");
        assert_eq!(actual.strip_prefix(&root).unwrap().components().count(), 2);
        assert!(!root.join(operation_id.to_string()).join("a").exists());
    }

    #[test]
    fn env_clear_is_allowlist_only_and_validation_precedes_layout() {
        let d = tempfile::tempdir().unwrap();
        let env = utility(&["/usr/bin/env", "/bin/env"]);
        let names = vec!["HOME".into(), "PATH".into(), "EWTM_MISSING_9".into()];
        let argv = vec![env];
        run_task(&RuntimeInput {
            environment_allowlist: &names,
            ..input(d.path(), Uuid::new_v4(), "env", &argv)
        })
        .unwrap();
        let out = fs::read_to_string(leaf(d.path()).join("stdout.log")).unwrap();
        let expected: std::collections::BTreeSet<_> = names
            .iter()
            .filter_map(|n| std::env::var(n).ok().map(|v| format!("{n}={v}")))
            .collect();
        let actual: std::collections::BTreeSet<_> = out.lines().map(str::to_owned).collect();
        assert_eq!(actual, expected);
        let before = fs::read_dir(d.path()).unwrap().count();
        for bad in [
            vec!["".into()],
            vec!["A=B".into()],
            vec!["A\0B".into()],
            vec!["A".into(), "A".into()],
        ] {
            assert_eq!(
                run_task(&RuntimeInput {
                    environment_allowlist: &bad,
                    ..input(d.path(), Uuid::new_v4(), "bad", &argv)
                }),
                Err(TaskRuntimeError::InvalidInput)
            );
        }
        assert_eq!(fs::read_dir(d.path()).unwrap().count(), before);
    }

    #[test]
    fn dual_streams_are_drained_capped_and_tagged() {
        for n in 0..10 {
            let d = tempfile::tempdir().unwrap();
            let perl = utility(&["/usr/bin/perl", "/bin/perl"]);
            let argv = vec![
                perl,
                "-e".into(),
                "print 'O' x (2 * 1024 * 1024); print STDERR 'E' x (2 * 1024 * 1024);".into(),
            ];
            let r = run_task(&input(
                d.path(),
                Uuid::new_v4(),
                &format!("streams-{n}"),
                &argv,
            ))
            .unwrap();
            assert_eq!(
                (r.stdout_count, r.stderr_count),
                (2 * 1024 * 1024, 2 * 1024 * 1024)
            );
            assert!(r.stdout_truncated && r.stderr_truncated);
            let p = leaf(d.path());
            assert_eq!(
                fs::metadata(p.join("stdout.log")).unwrap().len(),
                1024 * 1024
            );
            assert_eq!(
                fs::metadata(p.join("stderr.log")).unwrap().len(),
                1024 * 1024
            );
            assert_eq!(fs::read(p.join("stdout.log")).unwrap()[0], b'O');
            assert_eq!(fs::read(p.join("stderr.log")).unwrap()[0], b'E');
            assert!(metadata_at(&p)["stdout_log_error"] == false);
        }
    }

    #[test]
    fn cancellation_on_normal_paths_is_bounded() {
        let base = tempfile::tempdir().unwrap();
        let common = base.path().join("common");
        let cwd = base.path().join("cwd");
        fs::create_dir(&common).unwrap();
        fs::create_dir(&cwd).unwrap();
        let marker = base.path().join("ready");
        let sleep = utility(&["/bin/sleep", "/usr/bin/sleep"]);
        let touch = utility(&["/usr/bin/touch", "/bin/touch"]);
        let argv = shell(
            &format!("{} '{}'\n{} 30", touch, marker.display(), sleep),
            &[],
        );
        let token = CancellationToken::default();
        let worker_token = token.clone();
        let common2 = common.clone();
        let cwd2 = cwd.clone();
        let worker = thread::spawn(move || {
            run_task(&RuntimeInput {
                common_dir: &common2,
                cwd: &cwd2,
                operation_id: Uuid::new_v4(),
                step_id: "bytes",
                argv: &argv,
                environment_allowlist: &[],
                token: worker_token,
                timing: test_timing(),
            })
        });
        let end = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < end {
            thread::sleep(test_timing().poll);
        }
        token.cancel();
        assert_eq!(worker.join().unwrap(), Err(TaskRuntimeError::Cancelled));
        assert!(leaf(&common).join("result.json").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_authority_paths_are_bounded() {
        use std::os::unix::ffi::OsStringExt;
        let base = tempfile::tempdir().unwrap();
        let common = base
            .path()
            .join(std::ffi::OsString::from_vec(b"common-\xff".to_vec()));
        let cwd = base
            .path()
            .join(std::ffi::OsString::from_vec(b"cwd-\xfe".to_vec()));
        fs::create_dir(&common).unwrap();
        fs::create_dir(&cwd).unwrap();
        let argv = shell("exit 0", &[]);
        assert_eq!(
            run_task(&input(&common, Uuid::new_v4(), "bytes", &argv))
                .unwrap()
                .outcome,
            TaskOutcome::Success
        );
    }

    #[test]
    fn nonzero_and_signal_categories_are_exact() {
        let d = tempfile::tempdir().unwrap();
        let argv = shell("exit 23", &[]);
        let r = run_task(&input(d.path(), Uuid::new_v4(), "nonzero", &argv));
        assert_eq!(r, Err(TaskRuntimeError::NonZero));
        let d = tempfile::tempdir().unwrap();
        let argv = shell("kill -TERM $$", &[]);
        let r = run_task(&input(d.path(), Uuid::new_v4(), "signal", &argv));
        assert_eq!(r, Err(TaskRuntimeError::Signaled));
        assert_eq!(metadata_at(&leaf(d.path()))["signal"], 15);
    }

    #[test]
    fn layout_authority_and_collision_matrix_is_fail_closed() {
        let d = tempfile::tempdir().unwrap();
        let op = Uuid::new_v4();
        let argv = shell("printf stable", &[]);
        run_task(&input(d.path(), op, "stable/step", &argv)).unwrap();
        let p = layout(d.path(), op, "stable/step");
        let stdout = fs::read(p.join("stdout.log")).unwrap();
        assert_eq!(
            run_task(&input(d.path(), op, "stable/step", &argv)),
            Err(TaskRuntimeError::Collision)
        );
        assert_eq!(fs::read(p.join("stdout.log")).unwrap(), stdout);
        let mode = |x: &Path| fs::metadata(x).unwrap().permissions().mode() & 0o7777;
        let uid = |x: &Path| {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(x).unwrap().uid()
        };
        for path in [
            d.path().join("ewtm"),
            d.path().join("ewtm/task-logs"),
            d.path().join("ewtm/task-logs/v1"),
            d.path().join("ewtm/task-logs/v1").join(op.to_string()),
            p.clone(),
        ] {
            assert!(path.is_dir());
            assert_eq!(mode(&path), 0o700);
            assert_eq!(uid(&path), rustix::process::geteuid().as_raw());
        }
        assert_eq!(mode(&p), 0o700);
        for n in ["stdout.log", "stderr.log", "result.json"] {
            assert!(p.join(n).is_file());
            assert_eq!(mode(&p.join(n)), 0o600);
            assert_eq!(uid(&p.join(n)), rustix::process::geteuid().as_raw());
        }
        assert!(fs::metadata(p.join("result.json")).unwrap().len() <= 4096);
        let shared = d.path().join("ewtm");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        let upgrade_argv = vec!["/bin/true".into()];
        let upgrade_input = input(d.path(), Uuid::new_v4(), "upgrade", &upgrade_argv);
        let _ = run_task(&upgrade_input);
        assert_eq!(mode(&shared), 0o700);
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("ewtm")).unwrap();
        fs::create_dir(root.path().join("ewtm/task-logs")).unwrap();
        fs::set_permissions(
            root.path().join("ewtm/task-logs"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert!(matches!(
            run_task(&input(root.path(), Uuid::new_v4(), "x", &argv)),
            Err(TaskRuntimeError::Collision)
        ));
        assert_eq!(mode(&root.path().join("ewtm/task-logs")), 0o755);
        assert!(!root.path().join("ewtm/task-logs/v1").exists());

        let symlink_root = tempfile::tempdir().unwrap();
        let target = tempfile::tempdir().unwrap();
        fs::set_permissions(target.path(), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(target.path().join("keep"), b"unchanged").unwrap();
        std::os::unix::fs::symlink(target.path(), symlink_root.path().join("ewtm")).unwrap();
        assert_eq!(
            run_task(&input(symlink_root.path(), Uuid::new_v4(), "x", &argv)),
            Err(TaskRuntimeError::Collision)
        );
        assert_eq!(fs::read(target.path().join("keep")).unwrap(), b"unchanged");
        assert_eq!(mode(target.path()), 0o755);
    }

    #[test]
    fn result_errors_are_redacted_and_fault_seams_are_raii() {
        for fault in [
            unix::TestFault::Read,
            unix::TestFault::Log,
            unix::TestFault::ResultSync,
            unix::TestFault::Group,
            unix::TestFault::Signal,
            unix::TestFault::Reap,
        ] {
            let d2 = tempfile::tempdir().unwrap();
            let r = if matches!(fault, unix::TestFault::Group | unix::TestFault::Signal) {
                let marker = d2.path().join("ready");
                let long_argv = if matches!(fault, unix::TestFault::Signal) {
                    shell(
                        &format!("echo $$ > '{}'; while :; do :; done", marker.display()),
                        &[],
                    )
                } else {
                    let sleep = utility(&["/bin/sleep", "/usr/bin/sleep"]);
                    let touch = utility(&["/usr/bin/touch", "/bin/touch"]);
                    shell(
                        &format!("{} '{}'\n{} 30", touch, marker.display(), sleep),
                        &[],
                    )
                };
                let token = CancellationToken::default();
                let worker_token = token.clone();
                let common = d2.path().to_path_buf();
                let worker = thread::spawn(move || {
                    let _guard = unix::TestFaultGuard::new(fault);
                    run_task(&RuntimeInput {
                        token: worker_token,
                        ..input(&common, Uuid::new_v4(), "fault", &long_argv)
                    })
                });
                let end = Instant::now() + Duration::from_secs(2);
                while !marker.exists() && Instant::now() < end {
                    thread::sleep(test_timing().poll);
                }
                token.cancel();
                let result = worker.join().unwrap();
                if matches!(fault, unix::TestFault::Signal) {
                    let pid = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
                    assert!(wait_gone(pid));
                }
                result
            } else {
                let fault_argv = shell("printf '%s' secret >&2; exit 7", &["secret-argv"]);
                let _guard = unix::TestFaultGuard::new(fault);
                run_task(&input(d2.path(), Uuid::new_v4(), "fault", &fault_argv))
            };
            assert!(
                matches!(r, Err(TaskRuntimeError::Runtime | TaskRuntimeError::Io)),
                "fault {fault:?}: {r:?}"
            );
            assert!(unix::test_faults_clear(), "fault {fault:?} leaked state");
        }
        let clean = tempfile::tempdir().unwrap();
        let argv = shell("exit 0", &[]);
        assert_eq!(
            run_task(&input(clean.path(), Uuid::new_v4(), "reset", &argv))
                .unwrap()
                .outcome,
            TaskOutcome::Success
        );
    }
    #[test]
    fn spawn_failed_metadata_is_durable() {
        let d = tempfile::tempdir().unwrap();
        let r = run_task(&RuntimeInput {
            common_dir: d.path(),
            operation_id: Uuid::new_v4(),
            step_id: "spawn",
            argv: &["/no/such/ewtm-command".into()],
            cwd: d.path(),
            environment_allowlist: &[],
            token: CancellationToken::default(),
            timing: test_timing(),
        });
        assert_eq!(r, Err(TaskRuntimeError::Spawn));
        let v: serde_json::Value =
            serde_json::from_slice(&fs::read(leaf(d.path()).join("result.json")).unwrap()).unwrap();
        assert_eq!(v["outcome"], "spawn_failed");
    }

    #[test]
    fn pre_cancel_is_durable_and_never_spawns() {
        let d = tempfile::tempdir().unwrap();
        let marker = d.path().join("child-marker");
        let touch = utility(&["/usr/bin/touch", "/bin/touch"]);
        let argv = shell(&format!("{} '{}'; exit 0", touch, marker.display()), &[]);
        let token = CancellationToken::default();
        token.cancel();
        assert_eq!(
            run_task(&RuntimeInput {
                token,
                ..input(d.path(), Uuid::new_v4(), "pre", &argv)
            }),
            Err(TaskRuntimeError::Cancelled)
        );
        assert!(!marker.exists());
        let v = metadata_at(&leaf(d.path()));
        assert_eq!(v["outcome"], "cancelled");
        assert_eq!(v["cancellation_phase"], "before_spawn");
    }

    #[test]
    fn cancellation_after_clean_completion_preserves_result_and_logs() {
        let d = tempfile::tempdir().unwrap();
        let argv = shell("printf immutable", &[]);
        let token = CancellationToken::default();
        let op = Uuid::new_v4();
        assert_eq!(
            run_task(&RuntimeInput {
                token: token.clone(),
                ..input(d.path(), op, "complete", &argv)
            })
            .unwrap()
            .outcome,
            TaskOutcome::Success
        );
        let p = layout(d.path(), op, "complete");
        let before = (
            fs::read(p.join("stdout.log")).unwrap(),
            fs::read(p.join("result.json")).unwrap(),
        );
        token.cancel();
        assert_eq!(
            (
                fs::read(p.join("stdout.log")).unwrap(),
                fs::read(p.join("result.json")).unwrap()
            ),
            before
        );
    }
    #[test]
    fn zero_exit_output_is_fully_drained() {
        let d = tempfile::tempdir().unwrap();
        let r = run_task(&RuntimeInput {
            common_dir: d.path(),
            operation_id: Uuid::new_v4(),
            step_id: "drain",
            argv: &shell("printf drained", &[]),
            cwd: d.path(),
            environment_allowlist: &[],
            token: CancellationToken::default(),
            timing: test_timing(),
        })
        .unwrap();
        assert_eq!(r.outcome, TaskOutcome::Success);
        let p = leaf(d.path());
        assert_eq!(fs::read_to_string(p.join("stdout.log")).unwrap(), "drained");
    }

    #[test]
    fn transient_group_presence_does_not_fail_clean_exit() {
        let d = tempfile::tempdir().unwrap();
        let _guard = unix::TestFaultGuard::new(unix::TestFault::GroupPresent(1));
        let op = Uuid::new_v4();
        let argv = shell("printf clean", &[]);
        assert_eq!(
            run_task(&input(d.path(), op, "transient", &argv))
                .unwrap()
                .outcome,
            TaskOutcome::Success
        );
        assert_eq!(
            metadata_at(&layout(d.path(), op, "transient"))["runtime_shutdown"],
            false
        );
        drop(_guard);
        let argv = shell("exit 0", &[]);
        assert_eq!(
            run_task(&input(d.path(), Uuid::new_v4(), "reset", &argv))
                .unwrap()
                .outcome,
            TaskOutcome::Success
        );
    }

    #[test]
    fn zero_exit_with_in_group_descendant_is_runtime_failure() {
        let d = tempfile::tempdir().unwrap();
        let pid_file = d.path().join("descendant.pid");
        let sleep = utility(&["/bin/sleep", "/usr/bin/sleep"]);
        let argv = shell(
            &format!("{} 30 & echo $! > {}; exit 0", sleep, pid_file.display()),
            &[],
        );
        let r = run_task(&RuntimeInput {
            common_dir: d.path(),
            operation_id: Uuid::new_v4(),
            step_id: "descendant",
            argv: &argv,
            cwd: d.path(),
            environment_allowlist: &[],
            token: CancellationToken::default(),
            timing: test_timing(),
        });
        let pid = wait_pid_file(&pid_file);
        assert_eq!(r, Err(TaskRuntimeError::Runtime));
        assert!(wait_gone(pid), "in-group descendant survived containment");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn escaped_stderr_writer_is_bounded_runtime_failure() {
        let d = tempfile::tempdir().unwrap();
        let pid_file = d.path().join("escaped.pid");
        let setsid = utility(&["/usr/bin/setsid", "/bin/setsid"]);
        let script = format!(
            "{} /bin/sh -c 'echo $$ > {}; while :; do printf escaped >&2; done' & exit 0",
            setsid,
            pid_file.display()
        );
        let argv = shell(&script, &[]);
        let started = Instant::now();
        let r = run_task(&RuntimeInput {
            common_dir: d.path(),
            operation_id: Uuid::new_v4(),
            step_id: "escaped",
            argv: &argv,
            cwd: d.path(),
            environment_allowlist: &[],
            token: CancellationToken::default(),
            timing: test_timing(),
        });
        let pid = wait_pid_file(&pid_file);
        assert_eq!(r, Err(TaskRuntimeError::Runtime));
        assert!(started.elapsed() < Duration::from_secs(3));
        let _ = Command::new(utility(&["/bin/kill", "/usr/bin/kill"]))
            .args(["-KILL", &pid.to_string()])
            .status();
    }

    #[test]
    fn cancellation_requires_child_and_group_containment() {
        let d = tempfile::tempdir().unwrap();
        let pid_file = d.path().join("running.pid");
        let script = format!(
            "echo $$ > '{}'; trap '' TERM; while :; do :; done",
            pid_file.display()
        );
        let argv = shell(&script, &[]);
        let token = CancellationToken::default();
        let worker_token = token.clone();
        let common = d.path().to_path_buf();
        let worker = thread::spawn(move || {
            run_task(&RuntimeInput {
                common_dir: &common,
                operation_id: Uuid::new_v4(),
                step_id: "cancel",
                argv: &argv,
                cwd: &common,
                environment_allowlist: &[],
                token: worker_token,
                timing: test_timing(),
            })
        });
        let pid = wait_pid_file(&pid_file);
        token.cancel();
        assert_eq!(worker.join().unwrap(), Err(TaskRuntimeError::Cancelled));
        let metadata = metadata_at(&leaf(d.path()));
        assert_eq!(metadata["outcome"], "cancelled");
        assert_eq!(metadata["cancellation_phase"], "during_run");
        assert!(
            wait_gone(pid),
            "cancelled direct child survived containment"
        );
    }
}
