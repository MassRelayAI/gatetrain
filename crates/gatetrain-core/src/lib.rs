//! gatetrain core. No terminal and no argument parsing here (design 9.1).
pub mod analysis;
pub mod batch;
pub mod batchcfg;
pub mod canon;
pub mod chain;
pub mod config;
pub mod declog;
pub mod diagnose;
pub mod disk;
pub mod evidence;
pub mod doctor;
pub mod history;
pub mod hold;
pub mod hostlock;
pub mod lanes;
pub mod ledger;
pub mod logfmt;
pub mod memory;
pub mod portscan;
pub mod predict;
pub mod privacy;
pub mod pycompat;
pub mod runner;
pub mod runs;
pub mod stackcheck;
pub mod stop;
pub mod stats;
pub mod tidy;
pub mod trace;
pub mod winprobe;
pub mod watch;

/// Errors of the core. A caller matches on the variant, not on a string.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("canonical json: {0}")]
    Canon(String),
    #[error("ledger schema version {found} is newer than this binary understands ({known}); upgrade gatetrain")]
    SchemaTooNew { found: i64, known: i64 },
    #[error("ledger schema version {found} needs `gatetrain ledger migrate` (this binary writes version {known})")]
    NeedsMigration { found: i64, known: i64 },
    #[error("batch: {0}")]
    Batch(String),
    #[error("bad timestamp: {0:?}")]
    Timestamp(String),
}

pub type Result<T> = std::result::Result<T, Error>;
