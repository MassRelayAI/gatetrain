//! `gatetrain stop`: queued followers first (L1), then the processes, repeated until none match (L2). Linux only.
use crate::runner::Runner;
use regex::Regex;
use std::collections::HashSet;
use std::io::Write;

const ROUNDS: usize = 4;

fn run(r: &dyn Runner, cmd: &str) -> String {
    r.run(cmd).map(|x| x.1).unwrap_or_default()
}

fn procs(r: &dyn Runner, pat: &Regex, exclude: &HashSet<u32>) -> Vec<(String, String)> {
    let mut out = vec![];
    for line in run(r, "ps -eo pid=,args=").lines() {
        let l = line.trim();
        let Some((pid, args)) = l.split_once(char::is_whitespace) else { continue };
        let args = args.trim_start();
        let Ok(n) = pid.parse::<u32>() else { continue };
        if exclude.contains(&n) || args.contains("ps -eo") {
            continue;
        }
        if pat.is_match(args) {
            out.push((pid.to_string(), args.to_string()));
        }
    }
    out
}

fn at_jobs(r: &dyn Runner, f: &[Regex]) -> Vec<String> {
    let mut jobs = vec![];
    for line in run(r, "atq").lines() {
        let Some(id) = line.split_whitespace().next() else { continue };
        if !id.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let body = run(r, &format!("at -c {id}"));
        if f.iter().any(|x| x.is_match(&body)) {
            jobs.push(id.to_string());
        }
    }
    jobs
}

fn timers(r: &dyn Runner, f: &[Regex]) -> Vec<String> {
    let mut found: Vec<String> = vec![];
    for line in run(r, "systemctl --user list-timers --all --no-legend").lines() {
        for tok in line.split_whitespace() {
            if let Some(stem) = tok.strip_suffix(".timer") {
                if found.iter().any(|x| x == tok) {
                    continue;
                }
                let body = run(r, &format!("systemctl --user cat {tok} {stem}.service"));
                if f.iter().any(|x| x.is_match(&body)) {
                    found.push(tok.to_string());
                }
            }
        }
    }
    found
}

type Scan = (Vec<(String, String)>, Vec<String>, Vec<String>);

fn scan(r: &dyn Runner, pat: &Regex, f: &[Regex], ex: &HashSet<u32>) -> Scan {
    (procs(r, pat, ex), if f.is_empty() { vec![] } else { at_jobs(r, f) }, if f.is_empty() { vec![] } else { timers(r, f) })
}

fn show(out: &mut dyn Write, s: &Scan) {
    for (pid, args) in &s.0 {
        let _ = writeln!(out, "  process pid {pid}: {args}");
    }
    for j in &s.1 {
        let _ = writeln!(out, "  queued follower: at job {j}");
    }
    for t in &s.2 {
        let _ = writeln!(out, "  queued follower: systemd timer {t}");
    }
}

fn empty(s: &Scan) -> bool {
    s.0.is_empty() && s.1.is_empty() && s.2.is_empty()
}

/// The process ids of this program and its parent (never stopped).
pub fn own_pids() -> HashSet<u32> {
    let mut s = HashSet::from([std::process::id()]);
    if let Ok(t) = std::fs::read_to_string("/proc/self/stat") {
        if let Some(rest) = t.rsplit_once(')').map(|x| x.1) {
            if let Some(pp) = rest.split_whitespace().nth(1).and_then(|x| x.parse().ok()) {
                s.insert(pp);
            }
        }
    }
    s
}

/// Returns the exit code. `sleep` is injected so tests do not wait.
pub fn stop(r: &dyn Runner, pattern: &str, followers: &[String], yes: bool, out: &mut dyn Write, sleep: &dyn Fn(u64), exclude: &HashSet<u32>) -> Result<i32, String> {
    let pat = Regex::new(pattern).map_err(|e| format!("bad pattern {pattern:?}: {e}"))?;
    let f: Vec<Regex> = followers.iter().map(|x| Regex::new(x).map_err(|e| format!("bad follower pattern {x:?}: {e}"))).collect::<Result<_, _>>()?;
    let mut s = scan(r, &pat, &f, exclude);
    if !yes {
        if empty(&s) {
            let _ = writeln!(out, "nothing matches");
        } else {
            let _ = writeln!(out, "would stop (dry run; pass --yes to act):");
            show(out, &s);
        }
        return Ok(0);
    }
    for rnd in 1..=ROUNDS {
        if rnd > 1 {
            s = scan(r, &pat, &f, exclude);
        }
        if empty(&s) {
            let _ = writeln!(out, "nothing left");
            return Ok(0);
        }
        let _ = writeln!(out, "round {rnd}:");
        show(out, &s);
        for j in &s.1 {
            let _ = r.run(&format!("atrm {j}"));
        }
        for t in &s.2 {
            let _ = r.run(&format!("systemctl --user stop {t}"));
        }
        if !s.0.is_empty() {
            let sig = if rnd <= 2 { "-TERM" } else { "-9" };
            let ids: Vec<&str> = s.0.iter().map(|p| p.0.as_str()).collect();
            let _ = r.run(&format!("kill {sig} {}", ids.join(" ")));
        }
        sleep(2);
    }
    let s = scan(r, &pat, &f, exclude);
    if empty(&s) {
        let _ = writeln!(out, "nothing left");
        return Ok(0);
    }
    let _ = writeln!(out, "left after 4 rounds:");
    show(out, &s);
    Ok(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A runner whose answers to one command are a queue (the Python FakeRunner's rule lists).
    struct Fake {
        rules: Vec<(String, RefCell<Vec<String>>)>,
        calls: RefCell<Vec<String>>,
    }
    impl Fake {
        fn new(rules: &[(&str, Vec<&str>)]) -> Fake {
            Fake { rules: rules.iter().map(|(p, v)| (p.to_string(), RefCell::new(v.iter().rev().map(|s| s.to_string()).collect()))).collect(), calls: RefCell::default() }
        }
        fn called(&self, prefix: &str) -> Vec<String> {
            self.calls.borrow().iter().filter(|c| c.starts_with(prefix)).cloned().collect()
        }
    }
    impl Runner for Fake {
        fn run_for(&self, cmd: &str, _t: Option<f64>) -> Result<crate::runner::RunOut, String> {
            self.calls.borrow_mut().push(cmd.to_string());
            for (p, q) in &self.rules {
                if cmd.starts_with(p.as_str()) {
                    let mut q = q.borrow_mut();
                    let a = if q.len() > 1 { q.pop().unwrap() } else { q.last().cloned().unwrap_or_default() };
                    return Ok(crate::runner::RunOut::done(0, a));
                }
            }
            Ok(crate::runner::RunOut::done(127, ""))
        }
    }

    const PS: &str = "  1 /sbin/init\n100 /bin/sh run-chain.sh batch-9\n101 sleep 5\n102 python other.py\n103 run-chain.sh step a\n";
    const ATQ: &str = "7\tWed Nov 11 18:20:00 2026 a me\n8\tWed Nov 11 18:30:00 2026 a me\n";
    fn base(ps: Vec<&'static str>, atq: Vec<&'static str>) -> Fake {
        Fake::new(&[
            ("ps -eo pid=,args=", ps),
            ("atq", atq),
            ("at -c 7", vec!["#!/bin/sh\nexec merge-follow.sh batch-9\n"]),
            ("at -c 8", vec!["#!/bin/sh\necho unrelated\n"]),
            ("systemctl --user list-timers", vec![""]),
            ("kill", vec![""]),
            ("atrm", vec![""]),
        ])
    }
    fn go(f: &Fake, pat: &str, fol: &[&str], yes: bool) -> (i32, String) {
        let mut out = Vec::new();
        let fol: Vec<String> = fol.iter().map(|s| s.to_string()).collect();
        let rc = stop(f, pat, &fol, yes, &mut out, &|_| {}, &HashSet::new()).unwrap();
        (rc, String::from_utf8(out).unwrap())
    }

    #[test]
    fn dry_run_lists_only_matches() {
        let f = base(vec![PS], vec![ATQ]);
        let (rc, txt) = go(&f, "run-chain", &["merge-follow"], false);
        assert_eq!(rc, 0);
        assert!(txt.contains("100") && txt.contains("103") && txt.contains("at job 7"));
        for no in ["101", "102", "/sbin/init", "other.py", "unrelated", "at job 8"] {
            assert!(!txt.contains(no), "{no} in {txt}");
        }
        assert!(f.called("kill").is_empty() && f.called("atrm").is_empty());
    }

    #[test]
    fn yes_removes_followers_before_killing() {
        let f = base(vec![PS, ""], vec![ATQ, "8\tWed Nov 11 18:30:00 2026 a me\n"]);
        let (rc, txt) = go(&f, "run-chain", &["merge-follow"], true);
        assert_eq!(rc, 0);
        let calls = f.calls.borrow();
        let i_at = calls.iter().position(|c| c.starts_with("atrm 7")).unwrap();
        let i_kill = calls.iter().position(|c| c.starts_with("kill")).unwrap();
        assert!(i_at < i_kill);
        assert!(!calls.iter().any(|c| c.starts_with("atrm 8")));
        let killed = f.called("kill");
        assert!(killed.iter().all(|c| !c.contains("101") && !c.contains("102") && !c.contains("-9")));
        assert!(txt.contains("nothing left"));
    }

    #[test]
    fn a_stubborn_process_escalates_and_is_reported_left() {
        let f = base(vec![PS], vec![""]);
        let (rc, txt) = go(&f, "run-chain", &[], true);
        assert_eq!(rc, 1);
        assert!(f.called("kill").iter().any(|c| c.contains("-9")));
        assert!(txt.contains("left"));
        assert!(f.called("ps -eo").len() <= 5);
    }

    #[test]
    fn a_systemd_timer_follower_is_stopped() {
        let f = Fake::new(&[
            ("ps -eo pid=,args=", vec![""]),
            ("atq", vec![""]),
            ("systemctl --user list-timers", vec!["Wed 18:20 5min merge-batch9.timer merge-batch9.service\n"]),
            ("systemctl --user cat merge-batch9.timer", vec!["ExecStart=merge-follow.sh\n"]),
            ("systemctl --user stop merge-batch9.timer", vec![""]),
        ]);
        let (_, txt) = go(&f, "run-chain", &["merge-follow"], true);
        assert!(txt.contains("merge-batch9.timer"));
        assert!(!f.called("systemctl --user stop merge-batch9.timer").is_empty());
    }
}
