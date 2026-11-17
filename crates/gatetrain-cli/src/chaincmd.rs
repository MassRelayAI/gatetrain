//! `gatetrain chain` (STUB for the red-first commit).
use clap::Args;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Args)]
pub struct ChainArgs {
    #[arg(long)]
    steps: PathBuf,
    #[arg(long, default_value_t = 4)]
    par: usize,
    #[arg(long)]
    out: PathBuf,
    #[arg(long)]
    keep_going: bool,
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    #[arg(long, value_delimiter = ',')]
    skip: Vec<String>,
}

pub fn run(_a: ChainArgs) -> ExitCode {
    eprintln!("chain: not implemented");
    ExitCode::from(70)
}
