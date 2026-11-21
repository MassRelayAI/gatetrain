//! `gatetrain metrics`: the minutes-lost view (`gatetrain_core::metrics`). Read-only: the ledger is never migrated or written.
use anyhow::{Context, Result};
use gatetrain_core::{ledger::Ledger, metrics};
use std::path::Path;
use std::process::ExitCode;

pub fn run(db: &str, last: usize, json: bool, csv_dir: Option<&Path>) -> Result<ExitCode> {
    if db != ":memory:" && !Path::new(db).exists() {
        anyhow::bail!("no ledger at {db}");
    }
    let l = Ledger::open(Path::new(db)).with_context(|| format!("opening {db}"))?;
    let m = metrics::compute(&l, last)?;
    if let Some(dir) = csv_dir {
        std::fs::create_dir_all(dir)?;
        for (name, text) in metrics::to_csv(&m) {
            std::fs::write(dir.join(format!("{name}.csv")), text)?;
        }
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&metrics::to_json(&m))?);
    } else {
        print!("{}", metrics::to_text(&m));
    }
    Ok(ExitCode::SUCCESS)
}
