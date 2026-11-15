//! `doctor`, `lanes` and `generated`: the glue between the argument parser and gatetrain-core (GT-PORT-B).
use gatetrain_core::runner::LocalRunner;
use gatetrain_core::{config, doctor, lanes, stackcheck};
use std::path::Path;
use std::process::ExitCode;

fn code(n: i32) -> ExitCode {
    ExitCode::from(n.clamp(0, 255) as u8)
}

pub fn doctor_cmd(config: &str, host: Option<&str>, json: bool) -> ExitCode {
    let cfg = match config::load(Path::new(config)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return code(2);
        }
    };
    let res = match doctor::run_doctor(&cfg, host, &|h| doctor::make_runner(h)) {
        Ok(r) => r,
        Err(m) => {
            eprintln!("{m}");
            return code(1);
        }
    };
    if json {
        println!("{}", gatetrain_core::pycompat::dumps_indent2(&doctor::to_json(&res)));
    } else {
        println!("{}", doctor::format_text(&res));
    }
    code(doctor::exit_code(&res))
}

pub struct LanesArgs {
    pub repo: String,
    pub main: String,
    pub refs: Vec<String>,
    pub json: bool,
    pub shared_files: Option<String>,
    pub resource_regex: Vec<String>,
    pub baseline_files: Option<String>,
    pub new_item_regex: Vec<String>,
}

pub fn lanes_cmd(a: LanesArgs) -> ExitCode {
    let resources = match lanes::parse_resource(&a.resource_regex) {
        Ok(r) => r,
        Err(m) => {
            eprintln!("{m}");
            return code(1);
        }
    };
    let opt = lanes::Options {
        shared: a.shared_files.as_deref().map(lanes::split_list),
        resources,
        baseline: a.baseline_files.as_deref().map(lanes::split_list),
        new_item: if a.new_item_regex.is_empty() { None } else { Some(a.new_item_regex.clone()) },
    };
    match lanes::analyse(Path::new(&a.repo), &a.main, &a.refs, &opt, &LocalRunner) {
        Ok(rep) => {
            println!("{}", if a.json { rep.json() } else { rep.render() });
            code(0)
        }
        Err(e) => {
            eprintln!("git failed: {e}");
            code(2)
        }
    }
}

pub fn generated_cmd(repo: &str, dir: Vec<String>, config: Option<&str>) -> ExitCode {
    let mut dirs = dir;
    if let Some(c) = config {
        match config::load(Path::new(c)) {
            Ok(cfg) => dirs.extend(cfg.generated_dirs()),
            Err(e) => {
                eprintln!("config error: {e}");
                return code(2);
            }
        }
    }
    let (o, e, c) = stackcheck::report(Path::new(repo), &dirs, &LocalRunner);
    print!("{o}");
    eprint!("{e}");
    code(c)
}
