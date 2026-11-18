//! The one door to external commands (git, docker, ssh, df, ps ...). Real runs call the system binaries as the Python
//! `runner.py` and `subprocess` calls do; tests inject a [`FakeRunner`] with a transcript.
//!
//! Two shapes: `run` takes a shell string and answers `(returncode, stdout)` (the doctor); `run_argv` takes an argument
//! vector and a working directory (git in the lanes radar and `generated`). An `Err` is what Python raises (timeout, no such
//! binary); the doctor turns it into "check could not run".
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// What a command did. `TimedOut` is Python's `subprocess.TimeoutExpired`.
#[derive(Debug, Clone, PartialEq)]
pub enum RunOut {
    Done { rc: i64, out: String },
    TimedOut,
}

impl RunOut {
    pub fn done(rc: i64, out: impl Into<String>) -> RunOut {
        RunOut::Done { rc, out: out.into() }
    }
}

/// The ONE command door (doctor, lanes, generated, check, tidy). An implementor supplies [`Runner::run_for`]; the doctor's
/// two-field view [`Runner::run`] (a timeout is an `Err`, as Python raises) is derived from it.
pub trait Runner {
    /// True when commands run on this machine (so a probe can be answered in-process instead of through a shell).
    fn is_local(&self) -> bool {
        false
    }
    /// Run one command line through the host's shell. `timeout` is seconds; `None` means the runner's default (60 s).
    fn run_for(&self, cmd: &str, timeout: Option<f64>) -> Result<RunOut, String>;
    /// A shell command line: `(returncode, stdout)`; a timeout is an `Err`.
    fn run(&self, cmd: &str) -> Result<(i32, String), String> {
        match self.run_for(cmd, None)? {
            RunOut::Done { rc, out } => Ok((rc as i32, out)),
            RunOut::TimedOut => Err(format!("Command '{cmd}' timed out after 60 seconds")),
        }
    }
    /// An argument vector in a working directory.
    fn run_argv(&self, _cwd: &Path, _argv: &[String]) -> Result<Output, String> {
        Err("run_argv is not supported by this runner".into())
    }
    /// An argument vector with no working directory and a time limit (`timeout` in seconds, `None` = 60 s). A local runner uses no
    /// shell at all; the default (ssh) joins the vector with POSIX single quotes into one command line.
    fn run_argv_to(&self, argv: &[String], timeout: Option<f64>) -> Result<RunOut, String> {
        self.run_for(&crate::pycompat::shlex_join(argv), timeout)
    }
    /// True when [`Runner::run_argv_to`] never involves a shell (every element reaches the program as one argument).
    fn shell_free(&self) -> bool {
        false
    }
    /// Write a generated script as a file (no shell escaping of paths; L43, L45).
    fn put_file(&self, _path: &str, _text: &str) -> Result<(), String> {
        Err("put_file is not supported by this runner".into())
    }
    /// A folder on the host where a generated script may live.
    fn scratch_dir(&self) -> Option<String> {
        None
    }
}

/// Python's `text=True`: universal newlines, undecodable bytes replaced.
fn text(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    if s.contains('\r') {
        s.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        s.into_owned()
    }
}

const TIMEOUT: Duration = Duration::from_secs(60);

/// Run a prepared command to the end. `None` = it did not end within `timeout` (the process tree is killed).
fn spawn_wait(mut c: Command, input: Option<&str>, timeout: Option<f64>) -> Result<Option<Output>, String> {
    c.stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    if let Some(t) = input {
        if let Some(mut si) = child.stdin.take() {
            let _ = std::io::Write::write_all(&mut si, t.as_bytes());
        }
    }
    let mut so = child.stdout.take().unwrap();
    let mut se = child.stderr.take().unwrap();
    let t1 = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = so.read_to_end(&mut b);
        b
    });
    let t2 = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = se.read_to_end(&mut b);
        b
    });
    let limit = timeout.map(|t| Duration::from_secs_f64(t.max(0.0))).unwrap_or(TIMEOUT);
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st;
        }
        if start.elapsed() > limit {
            kill_tree(&mut child);
            let _ = child.wait();
            // The reader threads end when the pipes close; do not wait on a grandchild that kept them open.
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let (o, e) = (t1.join().unwrap_or_default(), t2.join().unwrap_or_default());
    Ok(Some(Output { code: status.code().unwrap_or(-1), stdout: text(&o), stderr: text(&e) }))
}

pub(crate) fn kill_tree(child: &mut std::process::Child) {
    if cfg!(windows) {
        // the shell's own children (powershell, du) die with it
        let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &child.id().to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    } else {
        // `Child::kill` signals only the direct child: `sh -c 'npm test'` would leave npm and its workers running. Collect the
        // whole tree first (`pgrep -P`, on Linux and macOS), then kill it, so nothing is re-parented to init between the two.
        let mut tree: Vec<u32> = Vec::new();
        let mut todo = vec![child.id()];
        while let Some(p) = todo.pop() {
            if let Ok(o) = Command::new("pgrep").args(["-P", &p.to_string()]).stdin(Stdio::null()).stderr(Stdio::null()).output() {
                for kid in String::from_utf8_lossy(&o.stdout).split_whitespace().filter_map(|x| x.parse::<u32>().ok()) {
                    if !tree.contains(&kid) {
                        tree.push(kid);
                        todo.push(kid);
                    }
                }
            }
        }
        if !tree.is_empty() {
            let ids: Vec<String> = tree.iter().map(u32::to_string).collect();
            let _ = Command::new("kill").arg("-KILL").args(&ids).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
    }
    let _ = child.kill();
}

/// What [`run_isolated`] saw: the exit code with both streams, or a timeout after which its process group (job) was ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Isolated {
    Done { rc: i32, stdout: String, stderr: String },
    TimedOut,
}

/// How long the output of a command that ended is waited for: a daemon it left behind may hold its pipes open for ever.
const DRAIN: Duration = Duration::from_secs(2);

/// Run a plugin command with a time limit of `secs` (a finite number above 0, else an `Err` and nothing runs). It runs in a
/// process group of its own (Unix) or a job object (Windows), so a timeout ends every process it started, an orphaned grandchild
/// included; a command that ends by itself leaves whatever it started on purpose alone. A daemon that detached itself into its own
/// session (or a remote deploy) is beyond any local kill: the caller says so.
pub fn run_isolated(c: Command, secs: f64) -> Result<Isolated, String> {
    Ok(match run_isolated_raw(c, secs)? {
        IsolatedRaw::Done { rc, stdout, stderr } => Isolated::Done { rc, stdout: text(&stdout), stderr: text(&stderr) },
        IsolatedRaw::TimedOut => Isolated::TimedOut,
    })
}

/// [`run_isolated`] with the streams as raw bytes (a git blob must not be re-encoded).
#[derive(Debug, Clone, PartialEq)]
pub enum IsolatedRaw {
    Done { rc: i32, stdout: Vec<u8>, stderr: Vec<u8> },
    TimedOut,
}

pub fn run_isolated_raw(c: Command, secs: f64) -> Result<IsolatedRaw, String> {
    run_isolated_with(c, secs, Opts::default())
}

/// How [`run_isolated_with`] runs a command beyond its time limit.
#[derive(Default)]
pub struct Opts<'a> {
    /// The command may outlive this process (a deploy that starts a long-lived service). Otherwise it dies with its parent: on Linux
    /// by `PR_SET_PDEATHSIG` (SIGKILL when the spawning thread ends), on Windows by a job object that kills its processes when the
    /// last handle to it closes, which is set only while the command runs (one that ended keeps what it started on purpose).
    pub survives_parent: bool,
    /// Called once the command runs, with its pid (its process group id on Unix). An `Err` abandons it where it stands, as a dead
    /// parent would (a test's crash), and is returned.
    pub on_spawn: Option<&'a mut dyn FnMut(u32) -> Result<(), String>>,
}

pub fn run_isolated_with(mut c: Command, secs: f64, mut opts: Opts) -> Result<IsolatedRaw, String> {
    let limit = Duration::try_from_secs_f64(secs).ok().filter(|d| !d.is_zero()).ok_or_else(|| format!("{secs} s is not a usable time limit"))?;
    c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        c.process_group(0);
        #[cfg(target_os = "linux")]
        if !opts.survives_parent {
            // SAFETY: prctl is async-signal-safe and touches no memory of the parent; it runs in the child between fork and exec
            unsafe {
                c.pre_exec(|| {
                    group::set_pdeathsig();
                    Ok(())
                });
            }
        }
    }
    #[cfg(windows)]
    {
        // created suspended, put in its job, then resumed: nothing it starts can escape the job in between
        use std::os::windows::process::CommandExt;
        c.creation_flags(group::CREATE_SUSPENDED);
    }
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let job = group::Job::adopt(&child, !opts.survives_parent);
    group::resume(&child);
    if let Some(f) = opts.on_spawn.as_mut() {
        if let Err(e) = f(child.id()) {
            // the parent "dies" here: the command is left as it is (the job closes as it would with a dead process)
            job.abandon();
            return Err(e);
        }
    }
    let drain = |mut r: Box<dyn Read + Send>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut b = Vec::new();
            let _ = r.read_to_end(&mut b);
            let _ = tx.send(b);
        });
        rx
    };
    let so = drain(Box::new(child.stdout.take().unwrap()));
    let se = drain(Box::new(child.stderr.take().unwrap()));
    let start = Instant::now();
    let status = loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            break st;
        }
        if start.elapsed() > limit {
            job.kill(&child);
            kill_tree(&mut child);
            let _ = child.wait();
            return Ok(IsolatedRaw::TimedOut);
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    // it ended by itself: what it started on purpose (a service) is not killed when the job closes
    job.release();
    let (o, e) = (so.recv_timeout(DRAIN).unwrap_or_default(), se.recv_timeout(DRAIN).unwrap_or_default());
    Ok(IsolatedRaw::Done { rc: status.code().unwrap_or(-1), stdout: o, stderr: e })
}

/// A token of when process `pid` started, to tell it from a later process that reuses the pid (Linux: `starttime` of
/// `/proc/<pid>/stat`; Windows: the creation time). `None` where it cannot be read.
pub fn process_token(pid: u32) -> Option<u64> {
    group::token(pid)
}

/// Is `pid` running and, when `token` is given, still the process the token was taken from? A zombie is not running.
pub fn process_alive(pid: u32, token: Option<u64>) -> bool {
    match group::token(pid) {
        Some(now) => token.is_none_or(|t| t == now) && group::running(pid),
        None => token.is_none() && group::running(pid),
    }
}

/// Unix: does any process of process group `pgid` live (a grandchild of a plugin whose leader died)? Elsewhere `false`.
pub fn group_alive(pgid: u32) -> bool {
    // Linux: a member that is not a zombie (a leader that exited but was not reaped yet still counts for `kill -0`)
    #[cfg(target_os = "linux")]
    {
        let Ok(dir) = std::fs::read_dir("/proc") else { return false };
        dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok()).any(|pid| {
            let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return false };
            let Some(rest) = s.rfind(')').and_then(|i| s.get(i + 2..)) else { return false };
            let f: Vec<&str> = rest.split_whitespace().collect();
            f.get(2).and_then(|g| g.parse::<u32>().ok()) == Some(pgid) && !matches!(f.first(), Some(&"Z") | Some(&"X"))
        })
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        Command::new("kill").args(["-0", "--", &format!("-{pgid}")]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    }
    #[cfg(not(unix))]
    {
        let _ = pgid;
        false
    }
}

/// The process group (Unix) or job object (Windows) a plugin runs in.
mod group {
    pub struct Job {
        #[cfg(windows)]
        handle: *mut std::ffi::c_void,
    }

    #[cfg(windows)]
    type Handle = *mut std::ffi::c_void;

    #[cfg(windows)]
    pub const CREATE_SUSPENDED: u32 = 0x0000_0004;
    #[cfg(windows)]
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x2000;
    #[cfg(windows)]
    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;
    #[cfg(windows)]
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    #[cfg(windows)]
    const STILL_ACTIVE: u32 = 259;

    #[cfg(windows)]
    #[repr(C)]
    #[derive(Default)]
    struct BasicLimit {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: u32,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: u32,
        affinity: usize,
        priority_class: u32,
        scheduling_class: u32,
    }

    #[cfg(windows)]
    #[repr(C)]
    #[derive(Default)]
    struct ExtendedLimit {
        basic: BasicLimit,
        io: [u64; 6],
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[cfg(windows)]
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[cfg(windows)]
    extern "system" {
        fn CreateJobObjectW(attrs: Handle, name: *const u16) -> Handle;
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> i32;
        fn TerminateJobObject(job: Handle, code: u32) -> i32;
        fn SetInformationJobObject(job: Handle, class: i32, info: *mut std::ffi::c_void, len: u32) -> i32;
        fn CloseHandle(h: Handle) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn GetExitCodeProcess(h: Handle, code: *mut u32) -> i32;
        fn GetProcessTimes(h: Handle, creation: *mut FileTime, exit: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
    }

    #[cfg(windows)]
    #[link(name = "ntdll")]
    extern "system" {
        fn NtResumeProcess(process: Handle) -> i32;
    }

    /// Resume a child created suspended (Windows); elsewhere nothing.
    #[cfg(windows)]
    pub fn resume(child: &std::process::Child) {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: the process handle is borrowed from `child`, which created the process suspended
        unsafe {
            NtResumeProcess(child.as_raw_handle());
        }
    }
    #[cfg(not(windows))]
    pub fn resume(_child: &std::process::Child) {}

    /// Kill this child when its parent dies (Linux; called between fork and exec).
    #[cfg(target_os = "linux")]
    pub fn set_pdeathsig() {
        extern "C" {
            fn prctl(option: i32, ...) -> i32;
        }
        const PR_SET_PDEATHSIG: i32 = 1;
        const SIGKILL: u64 = 9;
        // SAFETY: a plain syscall wrapper with integer arguments
        unsafe {
            prctl(PR_SET_PDEATHSIG, SIGKILL);
        }
    }

    #[cfg(windows)]
    fn set_kill_on_close(job: Handle, on: bool) -> bool {
        let mut info = ExtendedLimit::default();
        info.basic.limit_flags = if on { JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE } else { 0 };
        // SAFETY: `info` is a live, correctly laid out JOBOBJECT_EXTENDED_LIMIT_INFORMATION; the job handle is ours
        unsafe { SetInformationJobObject(job, JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS, (&mut info as *mut ExtendedLimit).cast(), std::mem::size_of::<ExtendedLimit>() as u32) != 0 }
    }

    /// The creation time of `pid` (Windows).
    #[cfg(windows)]
    pub fn token(pid: u32) -> Option<u64> {
        // SAFETY: plain Win32 calls on a handle we own and close
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return None;
            }
            let (mut c, mut e, mut k, mut u) = (FileTime::default(), FileTime::default(), FileTime::default(), FileTime::default());
            let ok = GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u) != 0;
            CloseHandle(h);
            ok.then(|| (u64::from(c.high) << 32) | u64::from(c.low))
        }
    }

    #[cfg(windows)]
    pub fn running(pid: u32) -> bool {
        // SAFETY: plain Win32 calls on a handle we own and close
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code) != 0;
            CloseHandle(h);
            ok && code == STILL_ACTIVE
        }
    }

    /// `starttime` of `/proc/<pid>/stat`, and whether the process is anything but a zombie (Linux).
    #[cfg(target_os = "linux")]
    fn stat(pid: u32) -> Option<(char, u64)> {
        let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &s[s.rfind(')')? + 2..];
        let f: Vec<&str> = rest.split_whitespace().collect();
        Some((f.first()?.chars().next()?, f.get(19)?.parse().ok()?))
    }

    #[cfg(target_os = "linux")]
    pub fn token(pid: u32) -> Option<u64> {
        stat(pid).map(|(_, t)| t)
    }

    #[cfg(target_os = "linux")]
    pub fn running(pid: u32) -> bool {
        stat(pid).is_some_and(|(state, _)| state != 'Z' && state != 'X')
    }

    #[cfg(not(any(windows, target_os = "linux")))]
    pub fn token(_pid: u32) -> Option<u64> {
        None
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    pub fn running(pid: u32) -> bool {
        std::process::Command::new("kill").args(["-0", &pid.to_string()]).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success())
    }

    #[cfg(not(any(unix, windows)))]
    pub fn running(_pid: u32) -> bool {
        false
    }

    impl Job {
        /// Put the child in a job of its own (Windows), killing it with the job's last handle when `kill_on_close`; on Unix the child
        /// leads its own process group already. A job that cannot be made leaves the parent-pid walk of `kill_tree` as the only kill.
        #[cfg(windows)]
        pub fn adopt(child: &std::process::Child, kill_on_close: bool) -> Job {
            use std::os::windows::io::AsRawHandle;
            // SAFETY: plain Win32 calls; the job handle is ours and closed in Drop; the process handle is borrowed from `child`.
            unsafe {
                let h = CreateJobObjectW(std::ptr::null_mut(), std::ptr::null());
                if h.is_null() {
                    return Job { handle: h };
                }
                if kill_on_close {
                    set_kill_on_close(h, true);
                }
                if AssignProcessToJobObject(h, child.as_raw_handle()) == 0 {
                    CloseHandle(h);
                    return Job { handle: std::ptr::null_mut() };
                }
                Job { handle: h }
            }
        }
        #[cfg(not(windows))]
        pub fn adopt(_child: &std::process::Child, _kill_on_close: bool) -> Job {
            Job {}
        }

        /// Let go of the job as a parent that died would: on Windows its handle closes (and kill-on-close fires when it is set).
        pub fn abandon(self) {}

        /// The command ended by itself: closing the job must not kill what it started on purpose.
        #[cfg(windows)]
        pub fn release(&self) {
            if !self.handle.is_null() {
                set_kill_on_close(self.handle, false);
            }
        }
        #[cfg(not(windows))]
        pub fn release(&self) {}

        /// End every process of the group (job).
        #[cfg(windows)]
        pub fn kill(&self, _child: &std::process::Child) {
            if !self.handle.is_null() {
                // SAFETY: the handle is a live job object owned by self
                unsafe {
                    TerminateJobObject(self.handle, 1);
                }
            }
        }
        #[cfg(unix)]
        pub fn kill(&self, child: &std::process::Child) {
            // the child is the leader of its own process group: `kill -KILL -<pgid>` reaches an orphaned grandchild as well
            let _ = std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{}", child.id())])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
        #[cfg(not(any(unix, windows)))]
        pub fn kill(&self, _child: &std::process::Child) {}
    }

    #[cfg(windows)]
    impl Drop for Job {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                // SAFETY: closing our own handle once; with KILL_ON_JOB_CLOSE still set (the command did not end) its processes end too
                unsafe {
                    CloseHandle(self.handle);
                }
            }
        }
    }
}

/// Run a prepared command with stdout AND stderr written straight to `file` as they come (nothing is held in memory, the file can
/// be followed while the command runs, and a timeout keeps what was written). `Ok(Some(code))` = it ended; `Ok(None)` = it ran past
/// `timeout` seconds and its whole process tree was killed.
pub fn run_to_file(mut c: Command, file: &Path, timeout: f64) -> Result<Option<i32>, String> {
    let f = std::fs::File::create(file).map_err(|e| format!("cannot create {}: {e}", file.display()))?;
    let f2 = f.try_clone().map_err(|e| e.to_string())?;
    c.stdin(Stdio::null()).stdout(Stdio::from(f)).stderr(Stdio::from(f2));
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let limit = Duration::from_secs_f64(timeout.max(0.0));
    let start = Instant::now();
    loop {
        if let Some(st) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok(Some(st.code().unwrap_or(-1)));
        }
        if start.elapsed() > limit {
            kill_tree(&mut child);
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn exec(c: Command, shown: &str) -> Result<Output, String> {
    spawn_wait(c, None, None)?.ok_or_else(|| format!("Command '{shown}' timed out after 60 seconds"))
}

/// Run a prepared command (its own working directory and environment) with a time limit; the process tree is killed on a timeout.
pub fn run_cmd(c: Command, timeout: Option<f64>) -> Result<RunOut, String> {
    run_out(c, None, timeout)
}

fn run_out(c: Command, input: Option<&str>, timeout: Option<f64>) -> Result<RunOut, String> {
    Ok(match spawn_wait(c, input, timeout)? {
        Some(o) => RunOut::Done { rc: i64::from(o.code), out: o.stdout },
        None => RunOut::TimedOut,
    })
}

fn shell(cmd: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into());
        let mut c = Command::new(comspec);
        c.raw_arg(format!("/c \"{}\"", cmd));
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(cmd);
        c
    }
}

/// Commands on this machine (`reach = "local"`).
pub struct LocalRunner;

impl Runner for LocalRunner {
    fn run_for(&self, cmd: &str, timeout: Option<f64>) -> Result<RunOut, String> {
        run_out(shell(cmd), None, timeout)
    }
    fn run_argv(&self, cwd: &Path, argv: &[String]) -> Result<Output, String> {
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]).current_dir(cwd);
        exec(c, &argv.join(" "))
    }
    fn run_argv_to(&self, argv: &[String], timeout: Option<f64>) -> Result<RunOut, String> {
        let mut c = Command::new(&argv[0]);
        c.args(&argv[1..]);
        run_out(c, None, timeout)
    }
    fn shell_free(&self) -> bool {
        true
    }
    fn is_local(&self) -> bool {
        true
    }
    fn put_file(&self, path: &str, text: &str) -> Result<(), String> {
        std::fs::write(path, text.as_bytes()).map_err(|e| format!("cannot write {path}: {e}"))
    }
    fn scratch_dir(&self) -> Option<String> {
        Some(std::env::temp_dir().to_string_lossy().into_owned())
    }
}

/// `ssh <target> -o BatchMode=yes -o ConnectTimeout=10 <cmd>`; `reach` may be written with or without the word ssh.
pub struct SshRunner {
    target: String,
}

impl SshRunner {
    pub fn new(reach: &str) -> Self {
        let r = reach.trim();
        SshRunner { target: r.strip_prefix("ssh ").map(|t| t.trim().to_string()).unwrap_or_else(|| r.to_string()) }
    }
    fn base(&self) -> Command {
        let mut c = Command::new("ssh");
        c.arg(&self.target).args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"]);
        c
    }
}

impl Runner for SshRunner {
    fn run_for(&self, cmd: &str, timeout: Option<f64>) -> Result<RunOut, String> {
        let mut c = self.base();
        c.arg(cmd);
        run_out(c, None, timeout)
    }
    fn put_file(&self, path: &str, text: &str) -> Result<(), String> {
        let mut c = self.base();
        c.arg(format!("cat > {}", crate::pycompat::shlex_quote(path)));
        match run_out(c, Some(text), Some(60.0))? {
            RunOut::Done { rc: 0, .. } => Ok(()),
            _ => Err(format!("cannot write {path} over ssh")),
        }
    }
    fn scratch_dir(&self) -> Option<String> {
        Some("/tmp".into())
    }
}

/// The runner for a host's `reach`: `local` or `ssh <name>`; anything else has none.
pub fn for_reach(reach: &str) -> Option<Box<dyn Runner>> {
    if reach == "local" {
        Some(Box::new(LocalRunner))
    } else if reach.starts_with("ssh ") {
        Some(Box::new(SshRunner::new(reach)))
    } else {
        None
    }
}

/// A canned transcript: shell command -> answer, argv (joined by one space) -> answer. A command it does not name answers
/// `(127, "")`, like the FakeRunner of the Python tests. `raise` entries make the call fail as a Python exception would.
#[derive(Default)]
pub struct FakeRunner {
    pub shell: HashMap<String, Result<(i32, String), String>>,
    pub argv: HashMap<String, Result<Output, String>>,
    pub calls: std::cell::RefCell<Vec<String>>,
}

impl Runner for FakeRunner {
    fn run_for(&self, cmd: &str, _timeout: Option<f64>) -> Result<RunOut, String> {
        self.calls.borrow_mut().push(cmd.to_string());
        let r = self.shell.get(cmd).cloned().unwrap_or(Ok((127, String::new())))?;
        Ok(RunOut::Done { rc: i64::from(r.0), out: r.1 })
    }
    fn run_argv(&self, _cwd: &Path, argv: &[String]) -> Result<Output, String> {
        let k = argv.join(" ");
        self.calls.borrow_mut().push(k.clone());
        self.argv.get(&k).cloned().unwrap_or(Ok(Output { code: 127, stdout: String::new(), stderr: String::new() }))
    }
}

impl FakeRunner {
    /// Build from the `transcript.json` object of one host: `{"cmd": [rc, "stdout"] | {"raise": "message"}}`.
    pub fn from_json(v: &serde_json::Value) -> FakeRunner {
        let mut f = FakeRunner::default();
        if let Some(o) = v.as_object() {
            for (k, e) in o {
                let r = match e.get("raise").and_then(|m| m.as_str()) {
                    Some(m) => Err(m.to_string()),
                    None => Ok((e[0].as_i64().unwrap_or(0) as i32, e[1].as_str().unwrap_or("").to_string())),
                };
                f.shell.insert(k.clone(), r);
            }
        }
        f
    }
}

/// A shell command line as a prepared [`Command`] (cmd.exe on Windows, `/bin/sh -c` elsewhere), for callers that set their own directory.
pub fn shell_command(cmd: &str) -> Command {
    shell(cmd)
}
