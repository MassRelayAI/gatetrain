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

fn kill_tree(child: &mut std::process::Child) {
    if cfg!(windows) {
        // the shell's own children (powershell, du) die with it
        let _ = Command::new("taskkill").args(["/F", "/T", "/PID", &child.id().to_string()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    let _ = child.kill();
}

fn exec(c: Command, shown: &str) -> Result<Output, String> {
    spawn_wait(c, None, None)?.ok_or_else(|| format!("Command '{shown}' timed out after 60 seconds"))
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
