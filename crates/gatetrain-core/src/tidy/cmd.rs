//! `cmd_check` and `cmd_tidy` of tidy.py. Output is collected, not printed, so the CLI and the tests read the same text.
//! Exit codes as Python: 0 clean, 1 threshold crossed or action failed, 3 guard violation; 1 for a config or usage error (SystemExit).
use super::apply::apply_plan;
use super::config::{build_cfg, CLASSES};
use crate::config::{self, Config, Host};
use super::inventory::{gather, Inventory};
use crate::pycompat::{dumps, J};
use super::ops::Fatal;
use crate::pycompat::{fmt_f, fmt_g, repr_list};
use super::report::{format_check, total_str};
use crate::runner::Runner;

pub struct CliOut {
    pub stdout: String,
    pub stderr: String,
    pub rc: i32,
}

impl CliOut {
    fn new() -> CliOut {
        CliOut { stdout: String::new(), stderr: String::new(), rc: 0 }
    }
    fn say(&mut self, s: impl AsRef<str>) {
        self.stdout.push_str(s.as_ref());
        self.stdout.push('\n');
    }
    fn err(&mut self, s: impl AsRef<str>) {
        self.stderr.push_str(s.as_ref());
        self.stderr.push('\n');
    }
    fn exit(mut self, rc: i32, msg: impl AsRef<str>) -> CliOut {
        self.err(msg);
        self.rc = rc;
        self
    }
}

pub type RunnerFactory<'a> = &'a dyn Fn(&Host) -> Option<Box<dyn Runner>>;

/// `doctor.make_runner`: local, or ssh; anything else is skipped.
pub fn make_runner(h: &Host) -> Option<Box<dyn Runner>> {
    crate::runner::for_reach(&h.reach)
}

fn hosts(config_path: &str, host: Option<&str>) -> Result<(Config, Vec<usize>), String> {
    let cfg = config::load(std::path::Path::new(config_path)).map_err(|e| format!("config error: {e}"))?;
    let host = host.filter(|h| !h.is_empty());
    let hs: Vec<usize> = cfg.hosts.iter().enumerate().filter(|(_, h)| if let Some(n) = host { h.name == n } else { h.enabled }).map(|(i, _)| i).collect();
    if let Some(n) = host {
        if hs.is_empty() {
            return Err(format!("no host {n} in the config"));
        }
    }
    Ok((cfg, hs))
}

fn fatal(mut o: CliOut, h: &Host, e: Fatal, stop: &str) -> CliOut {
    match e {
        Fatal::Guard(m) | Fatal::Item(m) => {
            o.err(format!("guard violation on {}: {}{}", h.name, m, stop));
            o.rc = 3;
        }
        Fatal::Other(m) => {
            o.err(format!("gatetrain: {m}"));
            o.rc = 2;
        }
    }
    o
}

fn skipped(o: &mut CliOut, h: &Host) {
    o.say(format!("== {}\nskipped: reach {} is not local or ssh", h.name, crate::pycompat::repr_str(&h.reach)));
}

pub fn cmd_check(config_path: &str, host: Option<&str>, json: bool, mk: RunnerFactory, now: Option<f64>) -> CliOut {
    let mut o = CliOut::new();
    let (cfg, hs) = match hosts(config_path, host) {
        Ok(x) => x,
        Err(m) => return o.exit(1, m),
    };
    let mut rc = 0;
    for i in hs {
        let h = &cfg.hosts[i];
        let Some(r) = mk(h) else {
            skipped(&mut o, h);
            continue;
        };
        let t = match build_cfg(&cfg, h) {
            Ok(t) => t,
            Err(e) => return o.exit(1, format!("config error: {e}")),
        };
        let inv = match gather(h, r.as_ref(), &t, now) {
            Ok(inv) => inv,
            Err(e) => return fatal(o, h, e, ""),
        };
        if json {
            let v = J::Obj(vec![
                ("host".into(), J::s(&inv.host)),
                ("plan".into(), J::Arr(inv.items.iter().map(|i| i.to_json()).collect())),
                ("breaches".into(), J::Arr(inv.breaches.iter().map(J::s).collect())),
            ]);
            o.say(dumps(&v, Some(2), false));
        } else {
            o.say(format_check(&inv));
        }
        if !inv.breaches.is_empty() {
            rc = 1;
        }
    }
    o.rc = rc;
    o
}

pub fn cmd_tidy(config_path: &str, host: Option<&str>, apply: bool, cls: Option<&str>, auto: bool, mk: RunnerFactory, now: Option<f64>) -> CliOut {
    let mut o = CliOut::new();
    let (cfg, hs) = match hosts(config_path, host) {
        Ok(x) => x,
        Err(m) => return o.exit(1, m),
    };
    let wanted: Option<Vec<String>> = cls.filter(|c| !c.is_empty()).map(|c| c.split(',').map(|x| crate::pycompat::strip(x).to_string()).collect());
    if let Some(w) = &wanted {
        if w.iter().any(|c| !CLASSES.contains(&c.as_str())) {
            return o.exit(1, format!("unknown class in --class; allowed {}", repr_list(&CLASSES)));
        }
    }
    let mut rc = 0;
    for i in hs {
        let h = &cfg.hosts[i];
        let Some(r) = mk(h) else {
            skipped(&mut o, h);
            continue;
        };
        let t = match build_cfg(&cfg, h) {
            Ok(t) => t,
            Err(e) => return o.exit(1, format!("config error: {e}")),
        };
        let mut classes: Vec<String> = wanted.clone().unwrap_or_else(|| if auto { t.unattended.clone() } else { CLASSES.iter().map(|c| c.to_string()).collect() });
        if auto {
            classes.retain(|c| t.unattended.contains(c));
        }
        let stop = "; stopped, nothing further touched";
        let mut inv: Inventory = match gather(h, r.as_ref(), &t, now) {
            Ok(x) => x,
            Err(e) => return fatal(o, h, e, stop),
        };
        let plan: Vec<_> = inv.items.iter().filter(|i| classes.contains(&i.cls) && !(auto && i.attended_only)).cloned().collect();
        if !apply && !auto {
            o.say(format_check(&inv));
            o.say(format!("dry run: {} action(s) in classes {}; add --apply to execute", plan.len(), classes.join(",")));
            if !inv.breaches.is_empty() {
                rc = 1;
            }
            continue;
        }
        inv.items = plan.clone();
        let (done, failed) = match apply_plan(h, r.as_ref(), &t, &inv, &classes, now, None, !auto) {
            Ok(x) => x,
            Err(e) => return fatal(o, h, e, stop),
        };
        let after = match gather(h, r.as_ref(), &t, now) {
            Ok(x) => x,
            Err(e) => return fatal(o, h, e, stop),
        };
        let freed = total_str(&plan);
        o.say(format!("{}: {} planned in {}; {} done, {} failed", h.name, plan.len(), classes.join(","), done, failed));
        o.say(format!("{}: about {} reclaimed (planned bytes); log {}", h.name, freed, t.log));
        for d in &after.disks {
            o.say(format!(
                "{}: disk {} {} (min {})",
                h.name,
                d.0,
                match d.1 {
                    None => "unreadable".to_string(),
                    Some(f) => format!("{} GB free", fmt_f(f, 0)),
                },
                fmt_g(d.2)
            ));
        }
        o.say(format!("{}: {} item(s) still plannable, {} threshold(s) crossed", h.name, after.items.len(), after.breaches.len()));
        if let Some(b) = after.breaches.first() {
            o.say(format!("{}: THRESHOLD: {}", h.name, b));
        }
        if failed > 0 || !after.breaches.is_empty() {
            rc = 1;
        }
    }
    o.rc = rc;
    o
}
