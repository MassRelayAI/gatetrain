//! `gatetrain chain`: run a steps file (see `gatetrain_core::chain`). Exit 0 green, 1 red, 2 config error.
use clap::Args;
use gatetrain_core::chain::{digest_of, load, run as run_plan, Opts};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Args)]
pub struct ChainArgs {
    /// the steps file: [[step]] name, run (shell string) or argv, tier = "fast"|"slow", needs, timeout_s; optional top-level cwd
    #[arg(long)]
    steps: PathBuf,
    /// how many steps of a tier may run at once
    #[arg(long, default_value_t = 4)]
    par: usize,
    /// where each step's full output (<step>.full) and the stamp go
    #[arg(long)]
    out: PathBuf,
    /// run the slow tier even when a fast step is red
    #[arg(long)]
    keep_going: bool,
    /// run only these steps (comma list)
    #[arg(long, value_delimiter = ',')]
    only: Vec<String>,
    /// leave these steps out (comma list)
    #[arg(long, value_delimiter = ',')]
    skip: Vec<String>,
}

pub fn run(a: ChainArgs) -> ExitCode {
    let text = match std::fs::read_to_string(&a.steps) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("chain: cannot read {}: {e}", a.steps.display());
            return ExitCode::from(2);
        }
    };
    let plan = match load(&text) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("chain: {e}");
            return ExitCode::from(2);
        }
    };
    if a.par == 0 {
        eprintln!("chain: --par must be at least 1");
        return ExitCode::from(2);
    }
    let o = Opts { par: a.par, out: a.out, keep_going: a.keep_going, only: a.only, skip: a.skip };
    let mut emit = |l: &str| {
        let mut so = std::io::stdout().lock();
        let _ = writeln!(so, "{l}");
        let _ = so.flush();
    };
    match run_plan(&plan, &digest_of(&text), &o, &mut emit) {
        Ok(r) => {
            match &r.stamp {
                Ok(p) => eprintln!("chain: green, stamp {}", p.display()),
                Err(why) if r.reds.is_empty() => eprintln!("chain: {why}"),
                Err(_) => {}
            }
            ExitCode::from(u8::from(!r.reds.is_empty()))
        }
        Err(e) => {
            eprintln!("chain: {e}");
            ExitCode::from(2)
        }
    }
}
