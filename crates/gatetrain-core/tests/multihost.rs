//! Multi-host chains (docs/multi-host.md) against a fake transport: no ssh, no remote host. Each test names the rule it pins.
use gatetrain_core::chain::{load, Outcome, Plan};
use gatetrain_core::multihost::{run, AgentResult, Fingerprint, Hello, HostSpec, MhConfig, MhOpts, StepReq, Transport, PROTOCOL, RC_LOST, RC_NO_HOST};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const P: &str = "x86_64-linux";
const V: &str = "test-version";

#[derive(Clone)]
enum Act {
    /// answer this exit code after this many milliseconds
    Rc(i64, u64),
    /// the session dies without a result
    Drop,
    /// a result that names another fingerprint (the host's tools changed)
    Drift,
}

struct FakeHost {
    platform: String,
    /// what each fingerprint command prints on this host
    outputs: HashMap<String, String>,
    /// per step, else `default`
    acts: HashMap<String, Act>,
    default: Act,
    hello_fails: bool,
    sha: Option<String>,
}

fn host(platform: &str, outputs: &[(&str, &str)], default: Act) -> FakeHost {
    FakeHost { platform: platform.into(), outputs: outputs.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect(), acts: HashMap::new(), default, hello_fails: false, sha: None }
}

struct Fake {
    hosts: HashMap<String, FakeHost>,
    calls: Mutex<Vec<(String, String)>>,
}

impl Fake {
    fn fp(&self, h: &str, probes: &[String]) -> Fingerprint {
        let o = &self.hosts[h].outputs;
        Fingerprint::of(V, probes.iter().filter_map(|p| o.get(p).map(|x| (p.clone(), x.clone()))).collect())
    }
    fn ran(&self, host: &str) -> Vec<String> {
        self.calls.lock().unwrap().iter().filter(|(h, _)| h == host).map(|(_, s)| s.clone()).collect()
    }
    fn tries(&self, step: &str) -> usize {
        self.calls.lock().unwrap().iter().filter(|(_, s)| s == step).count()
    }
}

impl Transport for Fake {
    fn hello(&self, h: &HostSpec, sha: &str, probes: &[String]) -> Result<Hello, String> {
        let fh = &self.hosts[&h.name];
        if fh.hello_fails {
            return Err("ssh: connect to host: No route to host".into());
        }
        Ok(Hello { protocol: PROTOCOL, sha: fh.sha.clone().unwrap_or_else(|| sha.to_string()), fingerprint: Some(self.fp(&h.name, probes)), platform: fh.platform.clone(), error: None })
    }
    fn run_step(&self, h: &HostSpec, req: &StepReq, line: &mut dyn FnMut(&str)) -> Result<AgentResult, String> {
        self.calls.lock().unwrap().push((h.name.clone(), req.step.clone()));
        let fh = &self.hosts[&h.name];
        let act = fh.acts.get(&req.step).cloned().unwrap_or_else(|| fh.default.clone());
        line(&format!("STEP_START {} now", req.step));
        let fp = self.fp(&h.name, &req.probes).hash;
        let res = |rc: i64, fp: String| AgentResult { protocol: PROTOCOL, sha: req.sha.clone(), fingerprint: fp, step: req.step.clone(), rc: Some(rc), secs: 0.1, files: vec![format!("{}.full", req.step)], ..Default::default() };
        match act {
            Act::Rc(rc, ms) => {
                std::thread::sleep(Duration::from_millis(ms));
                Ok(res(rc, fp))
            }
            Act::Drop => Err("the session ended without a result (exit 255): Connection reset by peer".into()),
            Act::Drift => Ok(res(0, "0".repeat(64))),
        }
    }
    fn fetch(&self, _h: &HostSpec, r: &AgentResult, out: &Path) -> Result<(), String> {
        for f in &r.files {
            std::fs::write(out.join(f), "remote output\n").map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

struct Repo {
    _d: tempfile::TempDir,
    dir: PathBuf,
    plan: Plan,
}

fn git(dir: &Path, args: &[&str]) {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
}

/// A committed repository whose steps file is `steps` (raw TOML step tables).
fn repo(steps: &str) -> Repo {
    let d = tempfile::tempdir().unwrap();
    let dir = d.path().to_path_buf();
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@example.invalid"]);
    git(&dir, &["config", "user.name", "t"]);
    git(&dir, &["config", "core.autocrlf", "false"]);
    std::fs::write(dir.join(".gitignore"), "out/\n").unwrap();
    std::fs::write(dir.join("steps.toml"), steps).unwrap();
    git(&dir, &["add", ".gitignore", "steps.toml"]);
    git(&dir, &["commit", "-q", "-m", "steps"]);
    let mut plan = load(steps).unwrap();
    plan.cwd = Some(dir.to_string_lossy().into_owned());
    Repo { _d: d, dir, plan }
}

fn steps(names: &[(&str, &str)]) -> String {
    names.iter().map(|(n, extra)| format!("[[step]]\nname = \"{n}\"\ntier = \"slow\"\nargv = [\"true\"]\n{extra}\n\n")).collect()
}

fn worker(name: &str, tags: &[&str], slots: usize) -> HostSpec {
    HostSpec { name: name.into(), tags: tags.iter().map(|s| s.to_string()).collect(), slots, cores: None, local: false }
}

fn coord_fp(cfg: &MhConfig, outputs: &[(&str, &str)]) -> Fingerprint {
    let o: HashMap<&str, &str> = outputs.iter().copied().collect();
    Fingerprint::of(V, cfg.all_probes().iter().map(|p| (p.clone(), o.get(p.as_str()).copied().unwrap_or("").to_string())).collect())
}

const TOOLS: [(&str, &str); 2] = [("rustc -V", "rustc 1.90.0"), ("node -v", "v22.1.0")];

fn cfg() -> MhConfig {
    MhConfig { fingerprint: vec!["rustc -V".into()], ..Default::default() }
}

fn opts(r: &Repo, cfg: MhConfig, workers: Vec<HostSpec>, weights: &[(&str, f64)]) -> MhOpts {
    MhOpts {
        out: r.dir.join("out"),
        keep_going: false,
        only: vec![],
        skip: vec![],
        mode: "preflight".into(),
        host: Some("coord".into()),
        run_id: "run-1".into(),
        weights: weights.iter().map(|(n, w)| (n.to_string(), *w)).collect(),
        local_slots: 0,
        local_cores: None,
        workers,
        fingerprint: coord_fp(&cfg, &TOOLS),
        cfg,
        platform: P.into(),
    }
}

fn go(r: &Repo, o: &MhOpts, f: Arc<Fake>) -> (Outcome, Vec<String>) {
    let mut lines = Vec::new();
    let out = run(&r.plan, "digest", o, f, &mut |l| lines.push(l.to_string())).unwrap();
    (out, lines)
}

fn fake(hosts: Vec<(&str, FakeHost)>) -> Arc<Fake> {
    Arc::new(Fake { hosts: hosts.into_iter().map(|(n, h)| (n.to_string(), h)).collect(), calls: Mutex::new(vec![]) })
}

fn rec_host(o: &Outcome, step: &str) -> Option<String> {
    o.record.steps.iter().find(|s| s.name == step).and_then(|s| s.host.clone())
}

fn rc_of(o: &Outcome, step: &str) -> i64 {
    o.record.steps.iter().find(|s| s.name == step).map(|s| s.rc).unwrap_or_else(|| panic!("no record of {step}"))
}

#[test]
fn placement_follows_tags_and_a_step_no_host_can_run_is_red() {
    let r = repo(&steps(&[("plain", ""), ("js", "requires = [\"js\"]"), ("gpu", "requires = [\"gpu\"]")]));
    let f = fake(vec![("lin", host(P, &TOOLS, Act::Rc(0, 0))), ("jsbox", host(P, &TOOLS, Act::Rc(0, 0)))]);
    let o = opts(&r, cfg(), vec![worker("lin", &[P], 2), worker("jsbox", &[P, "js"], 2)], &[]);
    let (out, lines) = go(&r, &o, f.clone());
    assert_eq!(rec_host(&out, "js").as_deref(), Some("jsbox"), "a js step runs only where js is");
    assert!(f.ran("lin").iter().all(|s| s != "js"));
    assert_eq!(rc_of(&out, "gpu"), RC_NO_HOST, "no host has gpu: red, never skipped");
    assert_eq!(f.tries("gpu"), 0);
    assert!(lines.iter().any(|l| l.starts_with("RED_NOW gpu rc=126")), "{lines:#?}");
    assert!(out.stamp.is_err(), "a run with an unplaceable step writes no stamp");
}

#[test]
fn a_fingerprint_mismatch_refuses_the_host_and_its_share_runs_elsewhere() {
    let r = repo(&steps(&[("a", ""), ("b", ""), ("c", ""), ("d", "")]));
    let f = fake(vec![("good", host(P, &TOOLS, Act::Rc(0, 0))), ("bad", host(P, &[("rustc -V", "rustc 1.89.0")], Act::Rc(0, 0)))]);
    let o = opts(&r, cfg(), vec![worker("good", &[P], 1), worker("bad", &[P], 4)], &[]);
    let (out, lines) = go(&r, &o, f.clone());
    assert!(out.reds.is_empty(), "{lines:#?}");
    assert!(f.ran("bad").is_empty(), "a refused host runs nothing");
    assert_eq!(f.ran("good").len(), 4);
    let refused = lines.iter().find(|l| l.starts_with("HOST_REFUSED bad")).expect("a HOST_REFUSED line");
    assert!(refused.contains("rustc -V") && refused.contains("1.89.0"), "{refused}");
    let bad = out.record.hosts.iter().find(|h| h.host == "bad").unwrap();
    assert!(!bad.accepted && bad.reason.as_deref().unwrap().contains("fingerprint mismatch"));
    assert!(out.record.host_events.iter().any(|e| e.host == "bad" && e.event == "refused"));
}

#[test]
fn a_host_on_another_commit_or_unreachable_is_refused() {
    let r = repo(&steps(&[("a", "")]));
    let mut other = host(P, &TOOLS, Act::Rc(0, 0));
    other.sha = Some("0".repeat(40));
    let mut away = host(P, &TOOLS, Act::Rc(0, 0));
    away.hello_fails = true;
    let f = fake(vec![("other", other), ("away", away), ("ok", host(P, &TOOLS, Act::Rc(0, 0)))]);
    let o = opts(&r, cfg(), vec![worker("other", &[P], 4), worker("away", &[P], 4), worker("ok", &[P], 1)], &[]);
    let (out, lines) = go(&r, &o, f.clone());
    assert!(out.reds.is_empty());
    assert!(f.ran("other").is_empty() && f.ran("away").is_empty());
    assert!(lines.iter().any(|l| l.starts_with("HOST_REFUSED other it checked out 0000")));
    assert!(lines.iter().any(|l| l.starts_with("HOST_REFUSED away no answer")));
}

#[test]
fn a_js_only_host_of_another_platform_gets_only_js_steps_and_a_tag_whose_tools_differ_is_dropped() {
    let r = repo(&steps(&[("rust", ""), ("vitest-1", "requires = [\"js\"]"), ("vitest-2", "requires = [\"js\"]")]));
    let c = MhConfig { fingerprint: vec![], tag_fingerprint: vec![(P.into(), vec!["rustc -V".into()]), ("js".into(), vec!["node -v".into()])], ..Default::default() };
    // the laptop has node but no rustc; `old` has another node, so it keeps its platform tag and loses js
    let f = fake(vec![("laptop", host("aarch64-macos", &[("node -v", "v22.1.0")], Act::Rc(0, 0))), ("old", host(P, &[("rustc -V", "rustc 1.90.0"), ("node -v", "v20.0.0")], Act::Rc(0, 0)))]);
    let o = opts(&r, c, vec![worker("laptop", &["js"], 4), worker("old", &[P, "js"], 4)], &[]);
    let (out, lines) = go(&r, &o, f.clone());
    assert!(out.reds.is_empty(), "{lines:#?}");
    assert_eq!(rec_host(&out, "rust").as_deref(), Some("old"), "a step without requires needs the coordinator's platform");
    assert_eq!(f.ran("laptop").len(), 2);
    assert!(f.ran("laptop").iter().all(|s| s.starts_with("vitest")));
    assert!(lines.iter().any(|l| l.starts_with("HOST_TAG_DROPPED old js `node -v`")), "{lines:#?}");
}

#[test]
fn a_dropped_host_loses_its_steps_to_another_qualified_host_and_is_removed() {
    let r = repo(&steps(&[("a", ""), ("b", ""), ("c", ""), ("d", "")]));
    let f = fake(vec![("flaky", host(P, &TOOLS, Act::Drop)), ("solid", host(P, &TOOLS, Act::Rc(0, 5)))]);
    let o = opts(&r, cfg(), vec![worker("flaky", &[P], 1), worker("solid", &[P], 1)], &[]);
    let (out, lines) = go(&r, &o, f.clone());
    assert!(out.reds.is_empty(), "{lines:#?}");
    for s in ["a", "b", "c", "d"] {
        assert_eq!(rec_host(&out, s).as_deref(), Some("solid"), "{s} counts only from the host that passed it");
    }
    assert_eq!(f.ran("flaky").len(), 2, "two losses (max_host_failures = 2), then removed");
    assert!(lines.iter().any(|l| l.starts_with("HOST_REMOVED flaky")));
    assert_eq!(lines.iter().filter(|l| l.starts_with("STEP_HOST_LOST")).count(), 2);
    assert_eq!(out.record.host_events.iter().filter(|e| e.event == "lost").count(), 2);
    assert!(out.record.host_events.iter().any(|e| e.event == "removed" && e.host == "flaky"));
    assert!(out.stamp.is_ok(), "{:?}", out.stamp);
}

#[test]
fn a_step_every_host_loses_is_red_never_green() {
    let r = repo(&steps(&[("a", "")]));
    let f = fake(vec![("x", host(P, &TOOLS, Act::Drop)), ("y", host(P, &TOOLS, Act::Drop))]);
    let o = opts(&r, cfg(), vec![worker("x", &[P], 1), worker("y", &[P], 1)], &[]);
    let (out, lines) = go(&r, &o, f.clone());
    assert_eq!(out.reds, vec![("a".to_string(), RC_LOST)], "{lines:#?}");
    assert_eq!(f.tries("a"), 2, "once per host: never again on a host that lost it");
    assert!(out.stamp.is_err());
}

#[test]
fn a_host_whose_tools_change_mid_run_is_removed_at_once() {
    let r = repo(&steps(&[("a", ""), ("b", "")]));
    let mut drifting = host(P, &TOOLS, Act::Rc(0, 0));
    drifting.acts.insert("a".into(), Act::Drift);
    let f = fake(vec![("drift", drifting), ("ok", host(P, &TOOLS, Act::Rc(0, 30)))]);
    let o = opts(&r, cfg(), vec![worker("drift", &[P], 1), worker("ok", &[P], 1)], &[("a", 10.0), ("b", 1.0)]);
    let (out, lines) = go(&r, &o, f.clone());
    assert!(out.reds.is_empty(), "{lines:#?}");
    assert_eq!(rec_host(&out, "a").as_deref(), Some("ok"), "the drifted green is not taken");
    assert!(lines.iter().any(|l| l.starts_with("HOST_REMOVED drift") && l.contains("fingerprint")));
}

#[test]
fn a_red_step_stays_red_and_is_not_tried_on_another_host() {
    let r = repo(&steps(&[("r", ""), ("g", "")]));
    let mut one = host(P, &TOOLS, Act::Rc(0, 0));
    one.acts.insert("r".into(), Act::Rc(1, 0));
    let f = fake(vec![("one", one), ("two", host(P, &TOOLS, Act::Rc(0, 0)))]);
    let o = opts(&r, cfg(), vec![worker("one", &[P], 1), worker("two", &[P], 1)], &[("r", 9.0), ("g", 1.0)]);
    let (out, lines) = go(&r, &o, f.clone());
    assert_eq!(out.reds, vec![("r".to_string(), 1)], "{lines:#?}");
    assert_eq!(f.tries("r"), 1, "no host-shopping");
    assert!(lines.iter().any(|l| l.starts_with("RED_NOW r rc=1") && l.contains("host one")));
    assert!(out.stamp.is_err());
}

#[test]
fn the_initial_plan_is_longest_first_by_slots() {
    let r = repo(&steps(&[("s3", ""), ("s5", ""), ("s1", ""), ("s4", ""), ("s3b", "")]));
    let f = fake(vec![("a", host(P, &TOOLS, Act::Rc(0, 0))), ("b", host(P, &TOOLS, Act::Rc(0, 0)))]);
    let o = opts(&r, cfg(), vec![worker("a", &[P], 1), worker("b", &[P], 1)], &[("s3", 3.0), ("s5", 5.0), ("s1", 1.0), ("s4", 4.0), ("s3b", 3.0)]);
    let (out, _) = go(&r, &o, f);
    let placed = |h: &str| -> Vec<String> { out.record.host_events.iter().filter(|e| e.event == "placed" && e.host == h).filter_map(|e| e.step.clone()).collect() };
    // 5 -> a, 4 -> b, 3 -> b (7 < 8), 3 -> a (8 < 10), 1 -> b (8 < 9)
    assert_eq!(placed("a"), ["s5", "s3b"]);
    assert_eq!(placed("b"), ["s4", "s3", "s1"]);
}

#[test]
fn an_idle_host_steals_from_a_slow_one() {
    let r = repo(&steps(&[("a", ""), ("b", ""), ("c", ""), ("d", "")]));
    let f = fake(vec![("fast", host(P, &TOOLS, Act::Rc(0, 5))), ("slow", host(P, &TOOLS, Act::Rc(0, 600)))]);
    let o = opts(&r, cfg(), vec![worker("fast", &[P], 1), worker("slow", &[P], 1)], &[]);
    let (out, lines) = go(&r, &o, f.clone());
    assert!(out.reds.is_empty());
    assert!(lines.iter().any(|l| l.starts_with("STEP_STOLEN") && l.contains("from=slow by=fast")), "{lines:#?}");
    assert_eq!(f.ran("fast").len(), 3, "equal weights pack 2 and 2; the fast host takes one of the slow host's");
    assert!(out.record.host_events.iter().any(|e| e.event == "stolen"));
}

#[test]
fn the_merged_stamp_names_every_steps_host_and_keeps_the_consumers_fields() {
    let r = repo(&steps(&[("a", ""), ("b", ""), ("c", "")]));
    let f = fake(vec![("w1", host(P, &TOOLS, Act::Rc(0, 0))), ("w2", host(P, &TOOLS, Act::Rc(0, 0)))]);
    let mut o = opts(&r, cfg(), vec![worker("w1", &[P], 1), worker("w2", &[P], 1)], &[]);
    o.local_slots = 0;
    let (out, lines) = go(&r, &o, f);
    let p = out.stamp.clone().unwrap_or_else(|e| panic!("{e}: {lines:#?}"));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    let head = String::from_utf8(Command::new("git").arg("-C").arg(&r.dir).args(["rev-parse", "HEAD"]).output().unwrap().stdout).unwrap();
    assert_eq!(v["sha"].as_str(), Some(head.trim()), "the field the lane-stamp and pre-flight rules read");
    assert_eq!(v["digest"].as_str(), Some("digest"));
    assert_eq!(v["multi_host"], true);
    let steps = v["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 3);
    for s in steps {
        assert_eq!(s["rc"], 0);
        assert!(["w1", "w2"].contains(&s["host"].as_str().unwrap()), "{s}");
        assert_eq!(s["fingerprint"].as_str().unwrap().len(), 64);
    }
    assert_eq!(v["hosts"].as_array().unwrap().len(), 3, "the coordinator and both workers");
    // the ledger keeps who ran what
    let mut l = gatetrain_core::ledger::Ledger::open_in_memory().unwrap();
    l.migrate().unwrap();
    l.record_chain_run("run-1", &out.record).unwrap();
    let n: i64 = l.conn.query_row("SELECT COUNT(*) FROM steps WHERE run_id='run-1' AND host IN ('w1','w2') AND length(host_fingerprint)=64", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 3);
    let hosts: i64 = l.conn.query_row("SELECT COUNT(*) FROM run_hosts WHERE run_id='run-1'", [], |r| r.get(0)).unwrap();
    assert_eq!(hosts, 3);
    let placed: i64 = l.conn.query_row("SELECT COUNT(*) FROM host_events WHERE run_id='run-1' AND event='placed'", [], |r| r.get(0)).unwrap();
    assert_eq!(placed, 3);
}

#[test]
fn a_dirty_coordinator_refuses_a_multi_host_run() {
    let r = repo(&steps(&[("a", "")]));
    std::fs::write(r.dir.join("stray.txt"), "x").unwrap();
    let f = fake(vec![("w", host(P, &TOOLS, Act::Rc(0, 0)))]);
    let o = opts(&r, cfg(), vec![worker("w", &[P], 1)], &[]);
    let e = run(&r.plan, "d", &o, f, &mut |_| {}).err().expect("refused");
    assert!(e.contains("dirty"), "{e}");
}
