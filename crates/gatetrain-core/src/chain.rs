//! `gatetrain chain`: the step runner. STUB (red-first commit): the real body follows.
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Step {
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub cwd: Option<String>,
    pub steps: Vec<Step>,
}

pub struct Opts {
    pub par: usize,
    pub out: PathBuf,
    pub keep_going: bool,
    pub only: Vec<String>,
    pub skip: Vec<String>,
}

pub struct Outcome {
    pub reds: Vec<(String, i64)>,
}

/// Parse and validate a steps file. Err = config error (exit 2).
pub fn load(_text: &str) -> Result<Plan, String> {
    Err("chain: not implemented".into())
}

/// Run the plan, calling `emit` once per log-format line.
pub fn run(_plan: &Plan, _digest: &str, _o: &Opts, _emit: &mut dyn FnMut(&str)) -> Result<Outcome, String> {
    Err("chain: not implemented".into())
}
