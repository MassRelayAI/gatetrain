//! Forced, measured and landed work: `measure`, `tidy --force`, `land`, `main-watch`, `branches` (founder 2026-11-25: everything can be
//! forced, measured or cleaned; if a merge waits too long, bypass, force to main and post-fix after).
//!
//! Every command here goes through [`Exec`] (an argument vector, never a shell string for git), so a test hands it a [`StubExec`] that
//! records and answers, and a real run hands it the host's runner. The new config tables are `[land]`, `[mainwatch]`, `[limits]` and
//! `[measure]` of hosts.toml; the older loader ignores top-level tables it does not know, so a file stays valid for both.
pub mod branches;
pub mod cfg;
pub mod exec;
pub mod land;
pub mod mainwatch;
pub mod secrets;
pub mod measure;
pub mod tidyforce;

pub use exec::{Exec, LocalExec, Out, RunnerExec, StubExec};
