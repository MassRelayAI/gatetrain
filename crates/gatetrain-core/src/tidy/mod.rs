//! `gatetrain check` (read-only inventory) and `gatetrain tidy` (guarded cleanup), ported from `gatetrain/tidy.py`.
//! Lessons L12, L31, L43-L51. Everything goes through a `Runner`; nothing here uses a bulk prune, a glob or a recursive
//! force-remove; every removal names one exact path or object.
pub mod apply;
pub mod cmd;
pub mod config;
pub mod inventory;
pub mod ops;
pub mod report;

pub use cmd::{cmd_check, cmd_tidy, make_runner, CliOut, RunnerFactory};
pub use ops::{Fatal, Ops};
pub use crate::runner::{LocalRunner, RunOut, Runner, SshRunner};
