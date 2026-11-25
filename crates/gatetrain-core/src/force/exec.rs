//! The one door of the forced commands: an argument vector in, `(rc, stdout, stderr)` out.
use crate::runner::{RunOut, Runner};
use std::cell::RefCell;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Out {
    pub rc: i32,
    pub out: String,
    pub err: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.rc == 0
    }
    pub fn new(rc: i32, out: &str) -> Out {
        Out { rc, out: out.into(), err: String::new() }
    }
    pub fn text(&self) -> String {
        format!("{}{}", self.out, self.err)
    }
}

pub trait Exec {
    /// A program and its arguments, no shell.
    fn run(&self, argv: &[String]) -> Out;
    /// One shell line in a folder (a configured check or command).
    fn sh(&self, cwd: &str, cmd: &str) -> Out;
}

/// `git -C <repo> <args>`.
pub fn git(e: &dyn Exec, repo: &str, args: &[&str]) -> Out {
    let mut v: Vec<String> = vec!["git".into(), "-C".into(), repo.into()];
    v.extend(args.iter().map(|a| a.to_string()));
    e.run(&v)
}

pub struct LocalExec;

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).replace("\r\n", "\n")
}

impl Exec for LocalExec {
    fn run(&self, argv: &[String]) -> Out {
        let Some(p) = argv.first() else { return Out::new(127, "") };
        match Command::new(p).args(&argv[1..]).output() {
            Ok(o) => Out { rc: o.status.code().unwrap_or(-1), out: lossy(&o.stdout), err: lossy(&o.stderr) },
            Err(e) => Out { rc: 127, out: String::new(), err: format!("cannot run {p}: {e}") },
        }
    }
    fn sh(&self, cwd: &str, cmd: &str) -> Out {
        let mut c = crate::runner::shell_command(cmd);
        c.current_dir(cwd);
        match c.output() {
            Ok(o) => Out { rc: o.status.code().unwrap_or(-1), out: lossy(&o.stdout), err: lossy(&o.stderr) },
            Err(e) => Out { rc: 127, out: String::new(), err: format!("cannot run `{cmd}`: {e}") },
        }
    }
}

/// A host's runner (local or ssh) as an [`Exec`]; its stdout and stderr arrive merged.
pub struct RunnerExec<'a>(pub &'a dyn Runner);

impl Exec for RunnerExec<'_> {
    fn run(&self, argv: &[String]) -> Out {
        match self.0.run_argv_to(argv, Some(600.0)) {
            Ok(RunOut::Done { rc, out }) => Out { rc: rc as i32, out, err: String::new() },
            Ok(RunOut::TimedOut) => Out { rc: 124, out: String::new(), err: "timed out".into() },
            Err(e) => Out { rc: 127, out: String::new(), err: e },
        }
    }
    fn sh(&self, cwd: &str, cmd: &str) -> Out {
        let line = if self.0.is_local() { cmd.to_string() } else { format!("cd {} && {cmd}", crate::pycompat::shlex_join(&[cwd.to_string()])) };
        match self.0.run_for(&line, Some(3600.0)) {
            Ok(RunOut::Done { rc, out }) => Out { rc: rc as i32, out, err: String::new() },
            Ok(RunOut::TimedOut) => Out { rc: 124, out: String::new(), err: "timed out".into() },
            Err(e) => Out { rc: 127, out: String::new(), err: e },
        }
    }
}

/// A test double: the first rule whose text is a prefix of the joined command answers; no rule = rc 127. Every call is recorded.
#[derive(Default)]
pub struct StubExec {
    rules: Vec<(String, Out)>,
    pub calls: RefCell<Vec<String>>,
}

impl StubExec {
    pub fn new() -> StubExec {
        StubExec::default()
    }
    pub fn on(mut self, prefix: &str, rc: i32, out: &str) -> StubExec {
        self.rules.push((prefix.to_string(), Out::new(rc, out)));
        self
    }
    fn answer(&self, line: String) -> Out {
        let o = self.rules.iter().find(|(p, _)| line.starts_with(p.as_str())).map(|(_, o)| o.clone()).unwrap_or(Out::new(127, ""));
        self.calls.borrow_mut().push(line);
        o
    }
    pub fn called(&self, needle: &str) -> bool {
        self.calls.borrow().iter().any(|c| c.contains(needle))
    }
}

impl Exec for StubExec {
    fn run(&self, argv: &[String]) -> Out {
        self.answer(argv.join(" "))
    }
    fn sh(&self, cwd: &str, cmd: &str) -> Out {
        self.answer(format!("sh {cwd} :: {cmd}"))
    }
}
