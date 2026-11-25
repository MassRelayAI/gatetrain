//! `gatetrain check` and `gatetrain tidy`: the arguments of the Python commands, run by `gatetrain_core::tidy`.
//! Text and exit codes are Python's (`gatetrain/tidy.py` cmd_check, cmd_tidy).
use clap::Args;
use gatetrain_core::tidy::{cmd_check, cmd_restore_image, cmd_tidy, make_runner, CliOut};
use std::io::Write;
use std::process::ExitCode;

#[derive(Args)]
pub struct CheckArgs {
    #[arg(long, default_value = "hosts.toml")]
    config: String,
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct TidyArgs {
    #[arg(long, default_value = "hosts.toml")]
    config: String,
    #[arg(long)]
    host: Option<String>,
    /// execute the plan (the default is a dry run)
    #[arg(long)]
    apply: bool,
    /// comma list of: targets,worktrees,containers,images,cache,trash
    #[arg(long = "class")]
    cls: Option<String>,
    /// for schedulers: check, then apply only the unattended classes
    #[arg(long)]
    auto: bool,
    /// `docker load -i` of an image archived by `[tidy] image_archive`: an exact .tar directly inside <trash_dir>/docker-images, nothing else
    #[arg(long, value_name = "TAR")]
    restore_image: Option<String>,
    /// everything but the main checkout and the kept or live worktrees: archive (commit by name, push as archive/<worktree>), remove link-safely, clean finished cargo targets and exited throwaway containers. Dry run unless --apply; keep_paths and protect still win
    #[arg(long)]
    force: bool,
    /// with --force: a regex over the worktree name or path; matches are kept (live lanes)
    #[arg(long, value_name = "REGEX")]
    keep: Option<String>,
    /// with --force: accept that ignored files that are not build output (`.env.local`, scratch notes) are deleted with the worktree, unarchived
    #[arg(long)]
    include_ignored: bool,
}

/// Each item is reported on stderr as soon as it is judged (`GATETRAIN_PROGRESS=0` turns it off): a long inventory is never silent.
fn stream_progress() {
    if std::env::var("GATETRAIN_PROGRESS").map(|v| v == "0").unwrap_or(false) {
        return;
    }
    gatetrain_core::tidy::inventory::set_progress(Some(Box::new(|l: &str| {
        let mut e = std::io::stderr().lock();
        let _ = writeln!(e, "{l}");
        let _ = e.flush();
    })));
}

fn emit(o: CliOut) -> ExitCode {
    let _ = std::io::stdout().write_all(o.stdout.as_bytes());
    let _ = std::io::stderr().write_all(o.stderr.as_bytes());
    ExitCode::from(o.rc.clamp(0, 255) as u8)
}

pub fn run_check(a: CheckArgs) -> ExitCode {
    stream_progress();
    emit(cmd_check(&a.config, a.host.as_deref(), a.json, &make_runner, None))
}

pub fn run_tidy(a: TidyArgs) -> ExitCode {
    if let Some(tar) = &a.restore_image {
        return emit(cmd_restore_image(&a.config, a.host.as_deref(), tar, &make_runner));
    }
    if a.force {
        return crate::forcecmd::tidy_force(&a.config, a.host.as_deref(), a.keep.as_deref(), a.apply, a.include_ignored);
    }
    if a.keep.is_some() {
        eprintln!("gatetrain: --keep needs --force");
        return ExitCode::from(2);
    }
    stream_progress();
    emit(cmd_tidy(&a.config, a.host.as_deref(), a.apply, a.cls.as_deref(), a.auto, &make_runner, None))
}
