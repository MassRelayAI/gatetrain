//! `gatetrain generated`: list UNTRACKED files under the configured generated directories (L53). Port of gatetrain/stackcheck.py.
//! A stack or finish step that commits only tracked changes leaves a NEW generated file behind, and a later "disk equals build"
//! test goes red. Run this after regenerating, before the chain.
use crate::runner::Runner;
use std::path::Path;

pub fn untracked_generated(repo: &Path, dirs: &[String], runner: &dyn Runner) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = vec![];
    for d in dirs {
        let argv: Vec<String> = ["git", "-c", "core.quotepath=off", "ls-files", "--others", "--exclude-standard", "--", d].iter().map(|s| s.to_string()).collect();
        let r = runner.run_argv(repo, &argv)?;
        if r.code != 0 {
            return Err(format!("git ls-files failed in {}: {}", repo.display(), r.stderr.trim()));
        }
        out.extend(crate::pycompat::splitlines(&r.stdout).into_iter().filter(|l| !l.is_empty()));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// What `gatetrain generated` prints, and its exit code: `(stdout, stderr, code)`.
pub fn report(repo: &Path, dirs: &[String], runner: &dyn Runner) -> (String, String, i32) {
    if dirs.is_empty() {
        return (String::new(), "no generated directories: give --dir or set [stack] generated_dirs in the config\n".into(), 2);
    }
    match untracked_generated(repo, dirs, runner) {
        Err(e) => (String::new(), format!("{e}\n"), 2),
        Ok(files) if files.is_empty() => (format!("ok: no untracked files under {}\n", dirs.join(", ")), String::new(), 0),
        Ok(files) => {
            let mut s = format!("{} untracked generated file(s) (a later disk-equals-build test will go red; git add them by name):\n", files.len());
            for f in &files {
                s += &format!("  {f}\n");
            }
            (s, String::new(), 1)
        }
    }
}
