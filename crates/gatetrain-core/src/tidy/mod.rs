//! `gatetrain check` (read-only inventory) and `gatetrain tidy` (guarded cleanup), ported from `gatetrain/tidy.py`.
//! Lessons L12, L31, L43-L51. Everything goes through a `Runner`; nothing here uses a bulk prune, a glob or a recursive
//! force-remove; every removal names one exact path or object.
pub mod apply;
pub mod cmd;
pub mod close;
pub mod config;
pub mod inventory;
pub mod naming;
pub mod ops;
pub mod report;

pub use cmd::{CliOut, cmd_check, cmd_restore_image, cmd_tidy, make_runner, RunnerFactory, tidy_auto_note, TidyRun};
pub use ops::{Fatal, Ops};
pub use crate::runner::{LocalRunner, RunOut, Runner, SshRunner};
