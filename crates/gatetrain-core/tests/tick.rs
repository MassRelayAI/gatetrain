//! `tick` (engine v2 E10): one bounded pass over every open batch through `advance`, doctor and `tidy --auto` per host, one advice
//! line, the cause-storm line. Temp git repositories, the fake lock and admission, fake host seams: no real host, Docker or repository.
use gatetrain_core::batch::{Admission, AllowAll, Engine, FakeLock, Outcome};
use gatetrain_core::batchcfg::BatchConfig;
use gatetrain_core::config::{self, Config, Host};
use gatetrain_core::diagnose;
use gatetrain_core::doctor::DResult;
use gatetrain_core::ledger::Ledger;
use gatetrain_core::tick::{self, HostSeams, TickOpts, TickReport};
use std::path::{Path, PathBuf};
use std::process::Command;

fn g(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(o.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn now() -> String {
    "2026-11-15T10:00:00+00:00".to_string()
}
#[allow(dead_code)]
static NOW: fn() -> String = now;

/// The engine's clock `secs` after the fixture start.
fn now_at(secs: i64) -> impl Fn() -> String {
    move || chrono::DateTime::from_timestamp(1_794_736_800 + secs, 0).unwrap().format("%Y-%m-%dT%H:%M:%S+00:00").to_string()
}
/// The fixture clock: every ledger row is written at 2026-11-15T10:00:00Z, and a tick runs at the same instant unless a test says.
const NOW_US: i64 = 1_794_736_800_000_000;
const MIN_US: i64 = 60_000_000;

fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Three hosts: one healthy, one with a failing and a warning check, one with no runner.
const HOSTS: &str = "[[hosts]]\nname = \"alpha\"\nreach = \"local\"\n\n[[hosts]]\nname = \"beta\"\nreach = \"ssh beta\"\n\n[[hosts]]\nname = \"gamma\"\nreach = \"none\"\n";

struct FakeHosts;
impl HostSeams for FakeHosts {
    fn doctor(&self, h: &Host) -> Option<Vec<DResult>> {
        match h.name.as_str() {
            "alpha" => Some(vec![DResult::new("alpha", "disks", "PASS", &[], "ok".into(), "")]),
            "beta" => Some(vec![
                DResult::new("beta", "ports", "FAIL", &[], "port 5500 is in the ephemeral range".into(), "move it"),
                DResult::new("beta", "disks", "WARN", &[], "no disk configured".into(), "add one"),
            ]),
            _ => None,
        }
    }
    fn tidy_auto(&self, h: &Host) -> String {
        match h.name.as_str() {
            "alpha" => "2.0 GB reclaimed".into(),
            "beta" => "nothing to tidy".into(),
            _ => "skipped".into(),
        }
    }
}

const GREEN: &str = "STEP_START a 2026-11-11T09:00:00Z\nSTEP_TIME a 1s\nSTEP_EXIT a 0\nGATES_DONE\n";
const RED: &str = "STEP_START a 2026-11-11T09:00:00Z\nSTEP_TIME a 1s\nSTEP_EXIT a 1\nRED_NOW a rc=1\nRED_SUMMARY: a\nGATES_DONE\n";

struct Fx {
    t: tempfile::TempDir,
    repo: PathBuf,
    state: PathBuf,
    ledger: Ledger,
    cfg: BatchConfig,
    lock: FakeLock,
    hosts: Config,
    /// seconds after 2026-11-15T10:00:00Z at which the engine writes its next rows
    clock: std::cell::Cell<i64>,
}

impl Fx {
    fn new(extra_toml: &str) -> Fx {
        let t = tempfile::tempdir().unwrap();
        let repo = t.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        g(&repo, &["init", "-q", "-b", "main"]);
        g(&repo, &["config", "user.email", "t@example.com"]);
        g(&repo, &["config", "user.name", "t"]);
        g(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("a.txt"), "a\n").unwrap();
        g(&repo, &["add", "--", "a.txt"]);
        g(&repo, &["commit", "-q", "-m", "init"]);
        for (lane, file) in [("lane1", "one.txt"), ("lane2", "two.txt"), ("lane3", "three.txt")] {
            g(&repo, &["switch", "-q", "-c", lane, "main"]);
            std::fs::write(repo.join(file), "x\n").unwrap();
            g(&repo, &["add", "--", file]);
            g(&repo, &["commit", "-q", "-m", &format!("feat: {lane}")]);
        }
        g(&repo, &["switch", "-q", "main"]);
        let marker = t.path().join("postmerge.txt");
        let toml = format!(
            "[preflight]\ncommand = [\"sh\", \"-c\", \"true\"]\n[chain]\ncommand = [\"fake-chain\"]\n\
             [postmerge]\ncommand = [\"sh\", \"-c\", \"echo $GATETRAIN_BATCH >> '{}'\"]\n\
             [deploy.staging]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"echo $GATETRAIN_BATCH >> '{}'\"]\nhealth = [\"sh\", \"-c\", \"true\"]\ntier = \"staging\"\n{extra_toml}",
            fwd(&marker),
            fwd(&t.path().join("deploy.txt"))
        );
        let hosts_file = t.path().join("hosts.toml");
        std::fs::write(&hosts_file, HOSTS).unwrap();
        let hosts = config::load(&hosts_file).unwrap();
        let mut ledger = Ledger::open_in_memory().unwrap();
        ledger.migrate().unwrap();
        Fx { state: t.path().join("state"), repo, ledger, cfg: BatchConfig::parse(&toml).unwrap(), lock: FakeLock::default(), hosts, clock: std::cell::Cell::new(0), t }
    }

    fn with<R>(&mut self, f: impl FnOnce(&mut Engine) -> R) -> R {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &now_at(self.clock.get()), state_dir: self.state.clone(), actor: "engine".into(), progress: None };
        f(&mut e)
    }

    /// Each batch gets its own checkout (a worktree of the repository), as batches that run side by side do.
    fn checkout(&self, id: &str) -> PathBuf {
        let wd = self.t.path().join(format!("wt-{id}"));
        g(&self.repo, &["worktree", "add", "-q", "--detach", &fwd(&wd), "main"]);
        wd
    }

    fn open_on(&mut self, id: &str, base: &str, lane: &str) {
        let wd = self.checkout(id);
        self.with(|e| e.new_batch(id, base, &wd, &[lane.into()]).unwrap());
    }

    fn open(&mut self, id: &str, lane: &str) {
        self.open_on(id, "main", lane);
    }

    fn adv(&mut self, id: &str) -> Outcome {
        self.with(|e| e.advance(id).unwrap())
    }

    fn state(&mut self, id: &str) -> String {
        self.with(|e| e.status(id).unwrap()["state"].as_str().unwrap().to_string())
    }

    /// Open `id` on `lane` and advance it to `proving` (the fake lock names its run, as a live holder would).
    fn drive_to_proving(&mut self, id: &str, lane: &str) {
        self.open(id, lane);
        for want in ["stacked", "settled", "preflighted", "proving"] {
            let o = self.adv(id);
            assert!(matches!(&o, Outcome::Advanced { to, .. } if to == want), "{id}: wanted {want}, got {o:?}");
        }
    }

    fn write_log(&self, id: &str, body: &str) {
        let req = self.lock.launches.lock().unwrap().iter().rev().find(|r| r.batch == id).unwrap().clone();
        std::fs::create_dir_all(req.log.parent().unwrap()).unwrap();
        std::fs::write(&req.log, format!("branch b at {} on builder\nCHAIN_MODE full\n{body}", req.sha)).unwrap();
    }

    fn tick(&mut self) -> TickReport {
        self.tick_with(tick::MAX_STEPS)
    }

    fn tick_with(&mut self, max_steps: usize) -> TickReport {
        self.tick_full(max_steps, NOW_US, Box::new(AllowAll))
    }

    fn tick_at(&mut self, now_us: i64) -> TickReport {
        self.tick_full(tick::MAX_STEPS, now_us, Box::new(AllowAll))
    }

    fn tick_full(&mut self, max_steps: usize, now_us: i64, adm: Box<dyn Admission>) -> TickReport {
        let rules: Vec<diagnose::Rule> = diagnose::builtin_rules().to_vec();
        let hosts = self.hosts.clone();
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: adm.as_ref(), now: &now_at(self.clock.get()), state_dir: self.state.clone(), actor: "engine".into(), progress: None };
        let o = TickOpts { hosts: &hosts, seams: &FakeHosts, rules: &rules, storm_reds: self.cfg.storm_reds, now_us, max_steps, max_wait_minutes: self.cfg.max_wait_minutes, max_run_minutes: self.cfg.max_run_minutes };
        tick::tick(&mut e, &o).unwrap()
    }

    fn count(&self, table: &str) -> i64 {
        self.ledger.conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap()
    }
}

/// The spec's fixture: two batches (one with a green proof, one still proving) and three hosts.
fn two_batches_three_hosts() -> Fx {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    fx.drive_to_proving("b2", "lane2");
    fx.write_log("b1", GREEN);
    fx
}

const GOLDEN: &str = "\
b1: proving -> proven -> merged -> deploying -> live -> closed
b2: proving, waits: run b2-p1 is still proving
hosts: alpha ok, tidy 2.0 GB reclaimed; beta 1 FAIL (ports) 1 WARN (disks), tidy nothing to tidy; gamma no runner (reach 'none'), tidy skipped; ingested 1 run
advice: none";

#[test]
fn two_batches_and_three_hosts_give_the_same_lines_every_time() {
    for _ in 0..2 {
        let mut fx = two_batches_three_hosts();
        let r = fx.tick();
        assert_eq!(r.lines.join("\n"), GOLDEN);
        assert!((3..=6).contains(&r.lines.len()), "{:?}", r.lines);
        assert_eq!(r.exit_code(), 0, "nothing needs a person");
    }
}

#[test]
fn a_green_accepted_proof_merges_through_the_configured_commands_a_proving_batch_waits() {
    let mut fx = two_batches_three_hosts();
    let tip = g(&fx.repo, &["rev-parse", "batch/b1/s1"]);
    fx.tick();
    assert_eq!(fx.state("b1"), "closed");
    assert_eq!(g(&fx.repo, &["rev-parse", "main"]), tip, "the protected branch is the proven tip");
    assert_eq!(std::fs::read_to_string(fx.t.path().join("postmerge.txt")).unwrap().trim(), "b1", "the configured postmerge command ran once");
    assert_eq!(fx.state("b2"), "proving", "a running proof is waited on");
    // the transitions are the ledger rows advance writes: one decision per transition, the hash chain still verifies
    let rules: Vec<String> = {
        let mut st = fx.ledger.conn.prepare("SELECT rule FROM decisions WHERE batch='b1' ORDER BY seq").unwrap();
        st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|x| x.unwrap()).collect()
    };
    for r in ["transition.T5", "transition.T7", "transition.T8a", "transition.T8b", "transition.T9"] {
        assert!(rules.iter().any(|x| x == r), "missing {r}: {rules:?}");
    }
    assert!(gatetrain_core::declog::verify(&fx.ledger).unwrap().is_ok());
}

#[test]
fn a_red_proof_stays_red_and_is_reported_blocked_with_its_cause() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b3", "lane3");
    fx.write_log("b3", RED);
    let main0 = g(&fx.repo, &["rev-parse", "main"]);
    let r = fx.tick();
    assert_eq!(r.lines[0], "b3: proving -> red, BLOCKED floor.zero-red: red step a (rc 1); a person commits a fix and restacks", "{:?}", r.lines);
    assert_eq!(r.exit_code(), 1, "a rule that needs a person");
    assert_eq!(fx.state("b3"), "red");
    assert_eq!(g(&fx.repo, &["rev-parse", "main"]), main0, "nothing merged");
    let refusals = fx.count("decisions WHERE rule='floor.zero-red'");
    let r = fx.tick();
    assert_eq!(r.lines[0], "b3: red, BLOCKED floor.zero-red: red step a (rc 1); a person commits a fix and restacks");
    assert_eq!(fx.count("decisions WHERE rule='floor.zero-red'"), refusals, "the same refusal is not logged again every tick");
}

#[test]
fn tick_never_opens_a_batch_and_never_runs_a_probe() {
    let mut fx = Fx::new("");
    let r = fx.tick();
    assert_eq!(r.lines[0], "batches: none open (a person opens one with `gatetrain batch new`)");
    assert_eq!(fx.count("batch_events"), 0, "no batch was opened");
    assert_eq!(fx.count("host_probes") + fx.count("disk_probes"), 0, "no probe ran");
    assert_eq!(r.exit_code(), 0);
}

#[test]
fn a_batch_waiting_on_its_base_proves_in_the_same_tick_once_the_base_is_proven() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    fx.open_on("b2", "b1", "lane2");
    for want in ["stacked", "settled", "preflighted"] {
        assert!(matches!(fx.adv("b2"), Outcome::Advanced { to, .. } if to == want));
    }
    assert!(matches!(fx.adv("b2"), Outcome::Refused { rule, .. } if rule == "pipeline.base-unproven"));
    fx.write_log("b1", GREEN);
    let r = fx.tick();
    assert_eq!(r.lines[0], "b1: proving -> proven -> merged -> deploying -> live -> closed", "{:?}", r.lines);
    assert_eq!(r.lines[1], "b2: preflighted -> proving, waits: run b2-p1 is still proving", "{:?}", r.lines);
}

fn storm_log(reds: usize) -> String {
    let mut s = String::new();
    for i in 0..reds {
        s.push_str(&format!(
            "STEP_START s{i} 2026-11-11T09:00:00Z\n=== s{i}\nFAILED tests/db_{i}.rs > connects\nError: connection refused to the gate database on port 5{i:03}\nSTEP_TIME s{i} 1s\nSTEP_EXIT s{i} 1\nRED_NOW s{i} rc=1\n"
        ));
    }
    s
}

#[test]
fn the_storm_line_names_the_cause_and_the_stop_command_from_storm_reds_on() {
    for (reds, storm) in [(19usize, false), (20, true)] {
        let mut fx = Fx::new("");
        fx.drive_to_proving("b4", "lane1");
        fx.write_log("b4", &storm_log(reds));
        let r = fx.tick();
        let line = r.lines.iter().find(|l| l.starts_with("storm:"));
        assert_eq!(line.is_some(), storm, "{reds} reds: {:?}", r.lines);
        if let Some(l) = line {
            assert!(l.starts_with(&format!("storm: one cause explains {reds} reds in run b4-p1")), "{l}");
            assert!(l.contains("connection refused"), "{l}");
            assert!(l.ends_with("stop it: gatetrain stop b4-p1 --yes"), "{l}");
        }
        assert_eq!(fx.state("b4"), "proving", "the storm is reported, the run is not stopped");
        assert!((3..=6).contains(&r.lines.len()), "{:?}", r.lines);
    }
}

#[test]
fn a_configured_storm_threshold_is_read_from_the_chain_table() {
    let fx = Fx::new("");
    assert_eq!(fx.cfg.storm_reds, 20, "the default");
    let c = BatchConfig::parse("[chain]\ncommand = [\"x\"]\nstorm_reds = 5\n").unwrap();
    assert_eq!(c.storm_reds, 5);
    assert!(BatchConfig::parse("[chain]\nstorm_reds = 0\n").is_err(), "0 would report every red run");
}

#[test]
fn two_ticks_cannot_overlap_the_ledger_lock() {
    let t = tempfile::tempdir().unwrap();
    let db = t.path().join("g.db");
    let lp = tick::lock_path(&db);
    let first = tick::try_lock(&lp).unwrap();
    assert!(first.is_some(), "the first tick takes the lock");
    assert!(tick::try_lock(&lp).unwrap().is_none(), "a second tick finds it held");
    drop(first);
    assert!(free_soon(&lp), "released when the first tick ends");
}

/// A released lock is free at once, except that on Linux a child forked by a concurrent test in the instant before its exec shares
/// the open file (flock belongs to the open file description) until it execs. Poll briefly instead of failing on that window.
fn free_soon(lp: &Path) -> bool {
    for _ in 0..50 {
        if gatetrain_core::tick::try_lock(lp).unwrap().is_some() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}


#[test]
fn more_open_batches_than_lines_are_folded_into_one_line() {
    let mut fx = Fx::new("");
    for (i, lane) in ["lane1", "lane2", "lane3"].iter().enumerate() {
        fx.drive_to_proving(&format!("b{i}"), lane);
    }
    fx.open("b9", "lane1");
    fx.with(|e| e.hold("b9", "a person looks at it").unwrap());
    let r = fx.tick();
    assert!(r.lines.len() <= 6, "{:?}", r.lines);
    assert_eq!(r.lines[2], "+2 more batch(es): 1 waiting, 1 held (gatetrain batch status)", "{:?}", r.lines);
    assert_eq!(r.exit_code(), 0, "a held batch is a person's decision already made");
}

// ---------------------------------------------------------------- review fixes

fn postmerge_runs(fx: &Fx) -> usize {
    std::fs::read_to_string(fx.t.path().join("postmerge.txt")).map(|t| t.lines().count()).unwrap_or(0)
}

#[test]
fn m3_a_crash_between_the_deploy_and_its_done_row_does_not_deploy_again() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    for want in ["proven", "merged"] {
        assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == want));
    }
    gatetrain_core::batch::crash_seam::set(Some("T8a".into()));
    assert!(fx.with(|e| e.advance("b1")).is_err(), "the simulated crash after the deploy");
    gatetrain_core::batch::crash_seam::set(None);
    assert_eq!(postmerge_runs(&fx), 1);
    let r = fx.tick();
    assert_eq!(r.lines[0], "b1: merged -> deploying -> live -> closed", "{:?}", r.lines);
    assert_eq!(postmerge_runs(&fx), 1, "the probe found the deploy done; postmerge and the deploy ran exactly once");
}

#[test]
fn m3_the_ledger_waits_for_a_busy_writer() {
    let t = tempfile::tempdir().unwrap();
    let l = Ledger::open(&t.path().join("g.db")).unwrap();
    let ms: i64 = l.conn.query_row("PRAGMA busy_timeout", [], |r| r.get(0)).unwrap();
    assert_eq!(ms, 30_000);
}

#[test]
fn m2_a_half_written_log_fails_its_own_batch_and_the_other_still_merges() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    fx.drive_to_proving("b2", "lane2");
    fx.write_log("b1", "STEP_START a 2026-11-11T09:0");
    fx.write_log("b2", GREEN);
    let r = fx.tick();
    assert!(r.lines[0].starts_with("b1: proving, FAILED "), "{:?}", r.lines);
    assert_eq!(r.lines[1], "b2: proving -> proven -> merged -> deploying -> live -> closed", "{:?}", r.lines);
    assert!(r.lines[2].contains("alpha ok, tidy 2.0 GB reclaimed"), "tidy still ran: {:?}", r.lines);
    assert_eq!(r.exit_code(), 1);
}

#[test]
fn m2_a_finished_log_that_does_not_parse_is_named_not_ingested() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b3", "lane3");
    fx.write_log("b3", "STEP_START a not-a-time\nGATES_DONE\n");
    let r = fx.tick();
    assert!(r.lines[1].contains("; not ingested: b3-p1 ("), "{:?}", r.lines);
    assert_eq!(fx.count("runs"), 0);
}

#[test]
fn m2_a_deleted_workdir_fails_one_batch_only() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    fx.drive_to_proving("b2", "lane2");
    fx.write_log("b1", GREEN);
    fx.write_log("b2", GREEN);
    std::fs::remove_dir_all(fx.t.path().join("wt-b1")).unwrap();
    let r = fx.tick();
    assert!(r.lines[0].starts_with("b1: proving, FAILED "), "{:?}", r.lines);
    assert_eq!(r.lines[1], "b2: proving -> proven -> merged -> deploying -> live -> closed", "{:?}", r.lines);
    assert_eq!(r.exit_code(), 1);
}

#[test]
fn m1_a_dependent_of_an_abandoned_base_is_blocked_not_waiting() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("z1", "lane1");
    fx.open_on("a2", "z1", "lane2");
    for want in ["stacked", "settled", "preflighted"] {
        assert!(matches!(fx.adv("a2"), Outcome::Advanced { to, .. } if to == want));
    }
    fx.with(|e| e.abandon("z1", "a person gave it up").unwrap());
    let r = fx.tick();
    assert!(r.lines[0].starts_with("a2: preflighted, BLOCKED pipeline.base-unproven: "), "{:?}", r.lines);
    assert_eq!(r.exit_code(), 1, "its base will never be proven: a person decides");
}

#[test]
fn m1_a_dependent_that_sorts_before_its_base_proves_in_the_same_tick() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("z1", "lane1");
    fx.open_on("a2", "z1", "lane2");
    for want in ["stacked", "settled", "preflighted"] {
        assert!(matches!(fx.adv("a2"), Outcome::Advanced { to, .. } if to == want));
    }
    fx.write_log("z1", GREEN);
    let r = fx.tick();
    assert_eq!(r.lines[0], "a2: preflighted -> proving, waits: run a2-p1 is still proving", "{:?}", r.lines);
    assert_eq!(r.lines[1], "z1: proving -> proven -> merged -> deploying -> live -> closed", "{:?}", r.lines);
    assert_eq!(r.exit_code(), 0);
}

#[test]
fn minor4_the_step_cap_is_reported_not_a_silent_wait() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    let r = fx.tick_with(2);
    assert!(r.lines[0].starts_with("b1: proving -> proven -> merged, BLOCKED tick.step-cap: "), "{:?}", r.lines);
    assert_eq!(r.exit_code(), 1);
}

#[test]
fn minor5_a_running_snapshot_does_not_block_ingesting_the_finished_run() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", "STEP_START a 2026-11-11T09:00:00Z\n");
    let req = fx.lock.launches.lock().unwrap()[0].clone();
    let p = gatetrain_core::logfmt::parse_log(&std::fs::read_to_string(&req.log).unwrap()).unwrap();
    assert!(matches!(gatetrain_core::runs::ingest(&fx.ledger, "g", "b1-p1", &p, "running", None, false, "2026-11-11T09:00:01Z").unwrap(), gatetrain_core::runs::Ingest::Done));
    fx.write_log("b1", GREEN);
    let r = fx.tick();
    assert!(r.lines[1].ends_with("; ingested 1 run"), "{:?}", r.lines);
    assert_eq!(fx.count("runs WHERE id='b1-p1'"), 1);
}

#[test]
fn minor6_a_broken_decision_log_stops_every_automatic_transition() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    fx.ledger.conn.execute_batch("DROP TRIGGER decisions_no_update; UPDATE decisions SET reason='edited' WHERE seq=2;").unwrap();
    let r = fx.tick();
    assert!(r.lines[0].starts_with("log: BROKEN at seq 2: "), "{:?}", r.lines);
    assert!(r.lines[0].ends_with("floor.log-append-only: no batch is advanced (gatetrain log verify)"), "{:?}", r.lines);
    assert_eq!(fx.state("b1"), "proving");
    assert_eq!(r.exit_code(), 1);
}

// ---------------------------------------------------------------- round-2 review fixes

fn lines_in(fx: &Fx, file: &str) -> usize {
    std::fs::read_to_string(fx.t.path().join(file)).map(|t| t.lines().count()).unwrap_or(0)
}

#[test]
fn major1_a_failed_transition_is_not_retried_by_the_next_ticks() {
    let broken = "[deploy.broken]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"echo x >> ../broken.txt; exit 1\"]\nhealth = [\"sh\", \"-c\", \"true\"]\ntier = \"staging\"\n";
    let mut fx = Fx::new(broken);
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    let mut exits = vec![];
    let mut last = String::new();
    for _ in 0..3 {
        let r = fx.tick();
        exits.push(r.exit_code());
        last = r.lines[0].clone();
    }
    assert_eq!(exits, vec![1, 1, 1]);
    assert_eq!(lines_in(&fx, "broken.txt"), 1, "one deploy call in three ticks");
    assert_eq!(postmerge_runs(&fx), 1, "one postmerge run in three ticks");
    assert!(last.starts_with("b1: merged, BLOCKED batch.failed-needs-person: "), "{last}");
    // a person's plain advance is refused too; an explicit retry override tries again
    assert!(matches!(fx.adv("b1"), Outcome::Refused { rule, .. } if rule == "batch.failed-needs-person"));
    fx.with(|e| e.retry_failed("b1", "a person tries again")).unwrap();
    assert!(matches!(fx.adv("b1"), Outcome::Failed(_)));
    assert_eq!(lines_in(&fx, "broken.txt"), 2);
}

#[test]
fn major2_a_proving_run_that_never_finishes_needs_a_person_after_max_run_minutes() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b2", "lane2");
    let r = fx.tick_at(NOW_US + 300 * MIN_US);
    assert_eq!(r.lines[0], "b2: proving, waits: run b2-p1 is still proving", "{:?}", r.lines);
    assert_eq!(r.exit_code(), 0);
    let r = fx.tick_at(NOW_US + 361 * MIN_US);
    assert_eq!(r.lines[0], "b2: proving, BLOCKED tick.wait-too-long: run b2-p1 is still proving, for 361 min (over [tick] max_run_minutes = 360)", "{:?}", r.lines);
    assert_eq!(r.exit_code(), 1);
}

#[test]
fn major2_a_deploy_that_never_turns_healthy_needs_a_person_after_max_wait_minutes() {
    let sick = "[deploy.sick]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"true\"]\nhealth = [\"sh\", \"-c\", \"exit 1\"]\ntier = \"staging\"\n";
    let mut fx = Fx::new(sick);
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    let r = fx.tick();
    assert!(r.lines[0].starts_with("b1: proving -> proven -> merged -> deploying, waits: target sick does not report "), "{:?}", r.lines);
    assert_eq!(r.exit_code(), 0);
    fx.clock.set(121 * 60);
    let r = fx.tick_at(NOW_US + 121 * MIN_US);
    // one health timer: the engine's, with [tick] max_wait_minutes as the limit of a target that names none
    assert!(r.lines[0].starts_with("b1: deploying, BLOCKED deploy.health-timeout: target sick did not report "), "{:?}", r.lines);
    assert!(r.lines[0].contains("healthy within 120 min"), "{:?}", r.lines);
    assert_eq!(r.exit_code(), 1);
}

#[test]
fn major2_a_configured_wait_limit_is_read() {
    let c = BatchConfig::parse("[tick]\nmax_wait_minutes = 30\nmax_run_minutes = 90\n").unwrap();
    assert_eq!((c.max_wait_minutes, c.max_run_minutes), (30.0, 90.0));
    let d = BatchConfig::parse("").unwrap();
    assert_eq!((d.max_wait_minutes, d.max_run_minutes), (120.0, 360.0));
}

/// Admission whose host has no close environment, with the reason the seam could not build one.
struct NoCloseEnv;
impl Admission for NoCloseEnv {
    fn admit(&self, _: &str, _: &str) -> gatetrain_core::Result<Option<String>> {
        Ok(None)
    }
    fn close_env_error(&self) -> Option<String> {
        Some("tidy config of builder: trash_dir is missing".into())
    }
}

#[test]
fn major2_no_close_environment_is_a_refusal_with_the_error_not_a_wait() {
    let roots = format!("[close]\ntidy = true\nroots = [\"{}\"]\n", fwd(&std::env::temp_dir().join("gt-close-root")));
    let mut fx = Fx::new(&roots);
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    let r = fx.tick_full(tick::MAX_STEPS, NOW_US, Box::new(NoCloseEnv));
    assert_eq!(
        r.lines[0],
        "b1: proving -> proven -> merged -> deploying -> live, BLOCKED close.no-environment: batch b1: close needs a tidy environment for this host: tidy config of builder: trash_dir is missing",
        "{:?}",
        r.lines
    );
    assert_eq!(r.exit_code(), 1);
}

#[test]
fn minor1_s3_a_crash_inside_a_target_is_not_deployed_again_unless_the_target_is_idempotent() {
    for idempotent in [false, true] {
        let mut fx = Fx::new("");
        fx.cfg.targets.get_mut("staging").unwrap().idempotent = idempotent;
        fx.drive_to_proving("b1", "lane1");
        fx.write_log("b1", GREEN);
        for want in ["proven", "merged"] {
            assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == want));
        }
        gatetrain_core::batch::crash_seam::set(Some("T8a:staging".into()));
        assert!(fx.with(|e| e.advance("b1")).is_err(), "the simulated crash right after the staging deploy");
        gatetrain_core::batch::crash_seam::set(None);
        assert_eq!(lines_in(&fx, "deploy.txt"), 1);
        let r = fx.tick();
        if idempotent {
            assert_eq!(r.lines[0], "b1: merged -> deploying -> live -> closed", "{:?}", r.lines);
            assert_eq!(lines_in(&fx, "deploy.txt"), 2, "a target declared idempotent is deployed again on resume");
        } else {
            assert!(r.lines[0].starts_with("b1: merged, BLOCKED batch.failed-needs-person: the last attempt of T8a failed (interrupted: "), "{:?}", r.lines);
            assert_eq!(lines_in(&fx, "deploy.txt"), 1, "a target that may be half deployed is not deployed again without a person");
            assert_eq!(r.exit_code(), 1);
        }
    }
}

#[test]
fn minor5_the_lock_path_is_the_same_however_the_ledger_is_named() {
    let t = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(t.path().join("sub")).unwrap();
    let db = t.path().join("g.db");
    Ledger::open(&db).unwrap();
    let other = t.path().join("sub").join("..").join("g.db");
    assert_eq!(tick::lock_path(&db), tick::lock_path(&other));
}

#[test]
fn minor6_samples_that_fail_after_the_run_went_in_are_a_warning_not_not_ingested() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    let req = fx.lock.launches.lock().unwrap()[0].clone();
    let mut sf = req.log.clone().into_os_string();
    sf.push(".samples.jsonl");
    std::fs::write(PathBuf::from(sf), "{\"host\": \"builder\", \"at\": \"2026-11-11T09:00:00Z\", \"load_1m\": 1.0}\n").unwrap();
    fx.ledger.conn.execute_batch("DROP TABLE host_samples;").unwrap();
    let r = fx.tick();
    assert!(r.lines[1].contains("; ingested 1 run (samples of b1-p1 not recorded: "), "{:?}", r.lines);
    assert!(!r.lines[1].contains("not ingested"), "{:?}", r.lines);
    assert_eq!(fx.count("runs WHERE id='b1-p1'"), 1);
}

#[test]
fn minor7_a_held_batch_is_reported_with_its_reason_and_exit_0() {
    let mut fx = Fx::new("");
    fx.open("b9", "lane1");
    fx.with(|e| e.hold("b9", "waiting for the vendor").unwrap());
    let r = fx.tick();
    assert_eq!(r.lines[0], "b9: held (waiting for the vendor)", "{:?}", r.lines);
    assert_eq!(r.exit_code(), 0);
}

// ---------------------------------------------------------------- round-3 review fixes

fn three_ticks(fx: &mut Fx) -> Vec<u8> {
    (0..3).map(|_| fx.tick().exit_code()).collect()
}

#[test]
fn rev3_b1_a_target_that_cannot_be_spawned_fails_once_and_is_not_rerun() {
    let zz = "[deploy.zz]\nidempotent = true\ncommand = [\"gatetrain-no-such-binary-rev3\"]\nhealth = [\"sh\", \"-c\", \"true\"]\ntier = \"staging\"\n";
    let mut fx = Fx::new(zz);
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    assert_eq!(three_ticks(&mut fx), vec![1, 1, 1]);
    assert_eq!(lines_in(&fx, "deploy.txt"), 1, "staging deployed once");
    assert_eq!(postmerge_runs(&fx), 1, "postmerge ran once");
    let r = fx.tick();
    assert!(r.lines[0].starts_with("b1: merged, BLOCKED batch.failed-needs-person: the last attempt of T8a failed ("), "{:?}", r.lines);
}

#[test]
fn rev3_minor1_the_wait_age_runs_from_the_row_that_entered_the_state() {
    let mut fx = Fx::new("[tick]\nmax_run_minutes = 120\n");
    fx.open("b1", "lane1");
    for want in ["stacked", "settled", "preflighted"] {
        assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == want));
    }
    fx.clock.set(3 * 3600);
    assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "proving"));
    let r = fx.tick_at(NOW_US + (3 * 60 + 1) * MIN_US);
    assert_eq!(r.lines[0], "b1: proving, waits: run b1-p1 is still proving", "opened 3 h ago, proving for 1 min: {:?}", r.lines);
    assert_eq!(r.exit_code(), 0);
}

#[test]
fn rev3_minor1_a_release_starts_the_wait_again() {
    let sick = "[deploy.sick]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"true\"]\nhealth = [\"sh\", \"-c\", \"exit 1\"]\ntier = \"staging\"\n";
    let mut fx = Fx::new(sick);
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    fx.tick();
    fx.clock.set(100 * 60);
    fx.with(|e| e.hold("b1", "looking").unwrap());
    fx.clock.set(110 * 60);
    fx.with(|e| e.release("b1").unwrap());
    let r = fx.tick_at(NOW_US + 130 * MIN_US);
    assert!(r.lines[0].starts_with("b1: deploying, waits: target sick does not report "), "released 20 min ago: {:?}", r.lines);
    assert_eq!(r.exit_code(), 0);
}

#[test]
fn rev3_minor2_a_deploy_over_its_timeout_ends_as_failed() {
    let slow = "[deploy.slow]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"sleep 30\"]\nhealth = [\"sh\", \"-c\", \"true\"]\ntier = \"staging\"\ntimeout_min = 0.02\n";
    let mut fx = Fx::new(slow);
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    let t0 = std::time::Instant::now();
    let r = fx.tick();
    assert!(t0.elapsed() < std::time::Duration::from_secs(20), "the deploy was ended at its timeout, not waited for");
    assert!(r.lines[0].starts_with("b1: proving -> proven -> merged, FAILED T8a: deploy slow timed out after 0.02 min"), "{:?}", r.lines);
    assert_eq!(r.exit_code(), 1);
}

#[test]
fn rev3_minor2_the_holder_refreshes_its_heartbeat() {
    let t = tempfile::tempdir().unwrap();
    let lp = t.path().join("g.db.tick.lock");
    let _held = match tick::take_beating(&lp, "tick", std::time::Duration::from_millis(50)).unwrap() {
        tick::Taken::Got(l) => l,
        tick::Taken::Held(_) => panic!("free"),
    };
    let b0 = tick::read_info(&lp).unwrap().beat_ms.unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    let b1 = tick::read_info(&lp).unwrap().beat_ms.unwrap();
    assert!(b1 > b0, "{b0} -> {b1}");
}

// ---------------------------------------------------------------- the failure-model rebuild (S1..S8)

use gatetrain_core::batch::crash_seam;

/// review round 4: a crash right after the staging deploy leaves a pending T8a intent; a person holds the batch; a tick must not resume it
#[test]
fn s1_rev4_a_held_batch_with_a_pending_t8a_intent_is_not_deployed_by_a_tick() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    for want in ["proven", "merged"] {
        assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == want));
    }
    crash_seam::set(Some("T8a:staging".into()));
    assert!(fx.with(|e| e.advance("b1")).is_err());
    crash_seam::set(None);
    assert_eq!(lines_in(&fx, "deploy.txt"), 1);
    fx.with(|e| e.hold("b1", "a person stops it after the crash")).unwrap();
    let r = fx.tick();
    assert_eq!(r.lines[0], "b1: held (a person stops it after the crash)", "{:?}", r.lines);
    assert_eq!(lines_in(&fx, "deploy.txt"), 1, "a held batch is never deployed");
    assert_eq!(fx.state("b1"), "held");
    assert_eq!(r.exit_code(), 0);
}

/// review round 4: a crash right after the merge; the batch is held; the resume's `done` must not overwrite `held`
#[test]
fn s1_rev4_a_held_batch_with_a_pending_t7_intent_is_not_moved_by_a_tick() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "proven"));
    crash_seam::set(Some("T7".into()));
    assert!(fx.with(|e| e.advance("b1")).is_err());
    crash_seam::set(None);
    fx.with(|e| e.hold("b1", "a person stops it")).unwrap();
    let r = fx.tick();
    assert_eq!(r.lines[0], "b1: held (a person stops it)", "{:?}", r.lines);
    assert_eq!(lines_in(&fx, "deploy.txt"), 0, "a held batch is never deployed");
    assert_eq!(fx.state("b1"), "held");
    // the hold names the intent it left pending, and a release resumes it
    let held: String = fx.ledger.conn.query_row("SELECT detail FROM batch_events WHERE batch='b1' AND transition='hold'", [], |r| r.get(0)).unwrap();
    assert!(held.contains("\"pending_intent\":{\"attempt\":1,\"transition\":\"T7\"}"), "{held}");
    fx.with(|e| e.release("b1")).unwrap();
    let r = fx.tick();
    assert_eq!(r.lines[0], "b1: proven -> merged -> deploying -> live -> closed", "{:?}", r.lines);
    assert_eq!(lines_in(&fx, "deploy.txt"), 1);
}

const BROKEN: &str = "[deploy.broken]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"echo x >> ../broken.txt; exit 1\"]\nhealth = [\"sh\", \"-c\", \"true\"]\ntier = \"staging\"\n";

#[test]
fn s2_a_plain_advance_after_a_failed_attempt_is_refused_and_a_retry_override_tries_once_more() {
    let mut fx = Fx::new(BROKEN);
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    let r = fx.tick();
    assert!(r.lines[0].starts_with("b1: proving -> proven -> merged, FAILED T8a: deploy broken exited 1"), "{:?}", r.lines);
    assert_eq!(lines_in(&fx, "broken.txt"), 1);
    // a person's plain advance is refused by the engine, naming the rule and the way past it
    match fx.adv("b1") {
        Outcome::Refused { rule, because, .. } => {
            assert_eq!(rule, "batch.failed-needs-person");
            assert!(because.contains("--retry"), "{because}");
            assert!(because.contains("[retry] budget"), "{because}");
        }
        o => panic!("{o:?}"),
    }
    assert_eq!(lines_in(&fx, "broken.txt"), 1, "the refused advance deployed nothing");
    // the override is a row, and it lets exactly one more attempt through
    assert!(fx.with(|e| e.retry_failed("b1", "")).is_err(), "an override needs a reason");
    fx.with(|e| e.retry_failed("b1", "the broken target was fixed")).unwrap();
    assert_eq!(fx.count("decisions WHERE kind='override' AND rule='retry.person'"), 1);
    assert!(matches!(fx.adv("b1"), Outcome::Failed(_)));
    assert_eq!(lines_in(&fx, "broken.txt"), 2);
    assert!(matches!(fx.adv("b1"), Outcome::Refused { rule, .. } if rule == "batch.failed-needs-person"), "one override, one attempt");
    assert_eq!(lines_in(&fx, "broken.txt"), 2);
}

#[test]
fn s2_rev4_hold_then_release_after_a_failed_transition_does_not_unmask_it() {
    let mut fx = Fx::new(BROKEN);
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    assert_eq!(fx.tick().exit_code(), 1);
    fx.with(|e| e.hold("b1", "looking at the deploy")).unwrap();
    fx.with(|e| e.release("b1")).unwrap();
    let r = fx.tick();
    assert!(r.lines[0].starts_with("b1: merged, BLOCKED batch.failed-needs-person: "), "{:?}", r.lines);
    assert_eq!(lines_in(&fx, "broken.txt"), 1, "the failed deploy is not run again by the release");
    assert_eq!(postmerge_runs(&fx), 1);
}

#[test]
fn s2_a_retry_budget_lets_the_engine_try_a_failed_transition_again_that_many_times() {
    let mut fx = Fx::new(&format!("{BROKEN}[retry]\nbudget = {{ T8a = 1 }}\n"));
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    let exits: Vec<u8> = (0..3).map(|_| fx.tick().exit_code()).collect();
    assert_eq!(exits, vec![1, 1, 1]);
    assert_eq!(lines_in(&fx, "broken.txt"), 2, "the first attempt and one retry from the budget");
    assert_eq!(fx.count("decisions WHERE kind='retry' AND rule='retry.budget'"), 1);
    assert!(BatchConfig::parse("[retry]\nbudget = { T99 = 1 }\n").is_err(), "an unknown transition");
    assert!(BatchConfig::parse("[retry]\nbudget = { T4 = -1 }\n").is_err(), "a negative budget");
    let d = BatchConfig::parse("").unwrap();
    assert_eq!((d.retry_budget("T7"), d.retry_budget("T8a"), d.retry_budget("T4")), (0, 0, 0), "default 0");
}

#[test]
fn s4_a_target_that_deployed_is_not_deployed_again_after_a_later_target_fails() {
    // `a-first` always works; `b-flaky` fails until the file `b.ok` exists
    let two = "[deploy.a-first]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"echo a >> ../a.txt\"]\nhealth = [\"sh\", \"-c\", \"true\"]\ntier = \"staging\"\n\
               [deploy.b-flaky]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"echo b >> ../b.txt; test -f ../b.ok\"]\nhealth = [\"sh\", \"-c\", \"true\"]\ntier = \"staging\"\n";
    let mut fx = Fx::new(two);
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    assert_eq!(fx.tick().exit_code(), 1);
    assert_eq!((lines_in(&fx, "a.txt"), lines_in(&fx, "b.txt"), postmerge_runs(&fx)), (1, 1, 1));
    // each target that deployed is a `deploy` row of the ledger, not a file
    assert_eq!(fx.count("decisions WHERE kind='deploy' AND rule='deploy.checkpoint' AND subject='a-first'"), 1);
    assert_eq!(fx.count("decisions WHERE kind='deploy' AND rule='deploy.checkpoint' AND subject='b-flaky'"), 0);
    assert!(!fx.state.join("deploys").exists(), "no marker file");
    std::fs::write(fx.t.path().join("b.ok"), "").unwrap();
    fx.with(|e| e.retry_failed("b1", "b-flaky was fixed")).unwrap();
    let r = fx.tick();
    assert_eq!(r.lines[0], "b1: merged -> deploying -> live -> closed", "{:?}", r.lines);
    assert_eq!(lines_in(&fx, "a.txt"), 1, "a-first deployed once: its checkpoint says so");
    assert_eq!(lines_in(&fx, "b.txt"), 2);
    assert_eq!(postmerge_runs(&fx), 1, "the postmerge command has its checkpoint too");
    let outcome: String = fx.ledger.conn.query_row("SELECT json_extract(detail,'$.outcome') FROM batch_events WHERE transition='T8a' AND phase='failed'", [], |r| r.get(0)).unwrap();
    assert_eq!(outcome, "failed-dirty", "a-first was deployed when b-flaky failed");
}

/// review round 4: a holder that was spawned but has not taken the host lock yet. Without a handshake the same tick took T5s
/// ("stopped") and launched again, eight times.
struct LateLock {
    launches: std::sync::Mutex<Vec<gatetrain_core::batch::LaunchRequest>>,
}
impl gatetrain_core::batch::HostLock for LateLock {
    fn names_run(&self, _run_id: &str) -> gatetrain_core::Result<bool> {
        Ok(false)
    }
    fn launch(&self, req: &gatetrain_core::batch::LaunchRequest) -> gatetrain_core::Result<()> {
        self.launches.lock().unwrap().push(req.clone());
        Ok(())
    }
}

#[test]
fn s5_rev4_a_holder_that_never_names_its_run_is_launched_once_and_fails_clean() {
    let mut fx = Fx::new("");
    fx.cfg.launch_ack_secs = 0.3;
    fx.open("b1", "lane1");
    for want in ["stacked", "settled", "preflighted"] {
        let o = fx.adv("b1");
        assert!(matches!(&o, Outcome::Advanced { to, .. } if to == want), "{o:?}");
    }
    let late = LateLock { launches: Default::default() };
    let rules: Vec<diagnose::Rule> = diagnose::builtin_rules().to_vec();
    let hosts = fx.hosts.clone();
    let r = {
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &late, adm: &AllowAll, now: &now_at(0), state_dir: fx.state.clone(), actor: "engine".into(), progress: None };
        let o = TickOpts { hosts: &hosts, seams: &FakeHosts, rules: &rules, storm_reds: fx.cfg.storm_reds, now_us: NOW_US, max_steps: tick::MAX_STEPS, max_wait_minutes: fx.cfg.max_wait_minutes, max_run_minutes: fx.cfg.max_run_minutes };
        tick::tick(&mut e, &o).unwrap()
    };
    let n = late.launches.lock().unwrap().len();
    assert_eq!(n, 1, "one tick launched the proof {n} times: {:?}", r.lines);
    assert!(r.lines[0].starts_with("b1: preflighted, FAILED T4: the holder did not name run b1-p1 within 0.3 s"), "{:?}", r.lines);
    assert_eq!(fx.count("batch_events WHERE transition='T5s'"), 0, "never a stopped run for a launch that was not acknowledged");
    let outcome: String = fx.ledger.conn.query_row("SELECT json_extract(detail,'$.outcome') FROM batch_events WHERE transition='T4' AND phase='failed'", [], |r| r.get(0)).unwrap();
    assert_eq!(outcome, "failed-clean");
}

#[test]
fn s5_a_holder_that_names_its_run_late_is_waited_for_not_stopped_and_launched_again() {
    let mut fx = Fx::new("");
    *fx.lock.late_polls.lock().unwrap() = 3;
    fx.open("b1", "lane1");
    let r = fx.tick();
    assert_eq!(r.lines[0], "b1: open -> stacked -> settled -> preflighted -> proving, waits: run b1-p1 is still proving", "{:?}", r.lines);
    assert_eq!(fx.lock.launches.lock().unwrap().len(), 1);
    assert_eq!(fx.count("batch_events WHERE transition='T5s'"), 0);
}

fn sh(cmd: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), cmd.into()]
}

#[test]
fn s6_a_hung_postmerge_fails_at_its_timeout() {
    let mut fx = Fx::new("");
    fx.cfg.postmerge_command = sh("sleep 30");
    fx.cfg.postmerge_timeout_min = 0.02;
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    let t0 = std::time::Instant::now();
    let r = fx.tick();
    assert!(t0.elapsed() < std::time::Duration::from_secs(20), "the post-merge command was ended at its timeout");
    assert!(r.lines[0].starts_with("b1: proving -> proven -> merged, FAILED T8a: post-merge timed out after 0.02 min"), "{:?}", r.lines);
    assert!(r.lines[0].contains("the local process tree was signalled; a remote or detached deploy may still run"), "{:?}", r.lines);
    assert_eq!(lines_in(&fx, "deploy.txt"), 0);
}

#[test]
fn s6_a_hung_health_check_is_a_wait_not_a_hang() {
    let mut fx = Fx::new("");
    fx.cfg.targets.get_mut("staging").unwrap().health = sh("sleep 30");
    fx.cfg.plugin_timeout_min = 0.1; // the health call's limit (git calls have their own, `git_min`): room for a loaded host
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    let t0 = std::time::Instant::now();
    let r = fx.tick();
    assert!(t0.elapsed() < std::time::Duration::from_secs(20));
    assert!(r.lines[0].starts_with("b1: proving -> proven -> merged -> deploying, waits: target staging: health timed out after 0.1 min"), "{:?}", r.lines);
}

#[test]
fn s6_a_deploy_timeout_that_is_not_a_finite_number_fails_closed_with_a_row() {
    let mut fx = Fx::new("");
    fx.cfg.targets.get_mut("staging").unwrap().timeout_min = Some(f64::INFINITY);
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    let r = fx.tick();
    assert!(r.lines[0].starts_with("b1: proving -> proven -> merged, FAILED T8a: "), "{:?}", r.lines);
    assert!(r.lines[0].contains("is not a usable time limit"), "{:?}", r.lines);
    assert_eq!(lines_in(&fx, "deploy.txt"), 0, "fails before the command runs");
    assert_eq!(fx.count("batch_events WHERE transition='T8a' AND phase='failed'"), 1, "a failed row, so no tick deploys it again");
}

#[test]
fn s6_a_failing_deploy_reports_its_stderr() {
    let mut fx = Fx::new("");
    fx.cfg.targets.get_mut("staging").unwrap().command = sh("echo to-stdout; echo the-real-cause >&2; exit 4");
    fx.cfg.targets.get_mut("staging").unwrap().timeout_min = Some(1.0);
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    let r = fx.tick();
    assert!(r.lines[0].contains("deploy staging exited 4: the-real-cause"), "{:?}", r.lines);
}

#[test]
fn s6_a_timed_out_plugin_takes_its_orphaned_grandchildren_with_it() {
    let mut fx = Fx::new("");
    let late = fwd(&fx.t.path().join("late.txt"));
    // the inner subshell is orphaned at once (its parent exits), so a walk over parent pids cannot find it; its group or job can
    fx.cfg.postmerge_command = sh(&format!("( (sleep 3; echo late >> '{late}') & ); sleep 30"));
    fx.cfg.postmerge_timeout_min = 0.02;
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    fx.tick();
    std::thread::sleep(std::time::Duration::from_secs(4));
    assert!(!fx.t.path().join("late.txt").exists(), "the orphaned grandchild outlived the timeout kill");
}

#[test]
fn s6_the_heartbeat_names_the_running_step_and_its_deadline() {
    let mut fx = Fx::new("");
    fx.cfg.postmerge_command = sh("sleep 1");
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    for want in ["proven", "merged"] {
        assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == want));
    }
    let lp = fx.t.path().join("g.db.tick.lock");
    let held = match tick::take_beating(&lp, "tick", std::time::Duration::from_millis(50)).unwrap() {
        tick::Taken::Got(l) => l,
        tick::Taken::Held(_) => panic!("free"),
    };
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (s2, lp2) = (stop.clone(), lp.clone());
    let reader = std::thread::spawn(move || {
        let mut seen = vec![];
        while !s2.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(i) = tick::read_info(&lp2) {
                if let (Some(e), Some(d)) = (i.effect.clone(), i.deadline_ms) {
                    seen.push((e, d, i.beat_ms.unwrap_or(0)));
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        seen
    });
    let beat = held.beat();
    let adm = AllowAll;
    {
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &now_at(0), state_dir: fx.state.clone(), actor: "engine".into(), progress: Some(beat) };
        assert!(matches!(e.advance("b1").unwrap(), Outcome::Advanced { to, .. } if to == "deploying"));
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let seen = reader.join().unwrap();
    let (effect, deadline, beat_ms) = seen.iter().find(|(e, _, _)| e.contains("postmerge")).cloned().unwrap_or_else(|| panic!("never named the step: {seen:?}"));
    assert!(effect.starts_with("b1 T8a postmerge"), "{effect}");
    assert!(deadline > beat_ms, "the deadline lies ahead of the beat: {deadline} vs {beat_ms}");
    drop(held);
    assert!(tick::read_info(&lp).is_none());
}

#[test]
fn s8_a_deploy_target_must_be_idempotent_or_probed_and_its_limit_finite() {
    let t = |extra: &str| BatchConfig::parse(&format!("[deploy.web]\ncommand = [\"x\"]\nhealth = [\"y\"]\n{extra}"));
    let e = t("").unwrap_err().to_string();
    assert!(e.contains("[deploy.web]") && e.contains("idempotent = true") && e.contains("probe"), "{e}");
    assert!(t("idempotent = true\n").is_ok());
    assert!(t("probe = [\"sh\", \"-c\", \"true\"]\n").is_ok());
    assert!(t("probe = []\n").is_err(), "an empty probe is no probe");
    assert!(t("tier = \"production\"\n").is_ok(), "a production target is never deployed by the engine");
    // review round 4: `inf` (and a huge value) panicked in Duration::from_secs_f64 inside T8a, after postmerge and earlier targets ran
    for bad in ["inf", "nan", "1e12", "0", "-1"] {
        assert!(t(&format!("idempotent = true\ntimeout_min = {bad}\n")).is_err(), "timeout_min = {bad}");
    }
    assert!(t("idempotent = true\ntimeout_min = 1440\n").is_ok());
    // with the tick on, every automatic target says how long it may take
    let e = t("idempotent = true\ntier = \"staging\"\n[tick]\nenabled = true\n").unwrap_err().to_string();
    assert!(e.contains("timeout_min") && e.contains("[tick] enabled"), "{e}");
    assert!(t("idempotent = true\ntimeout_min = 10\ntier = \"staging\"\n[tick]\nenabled = true\n").is_ok());
}

#[test]
fn s8_a_probe_settles_a_target_a_crash_left_in_doubt() {
    let mut fx = Fx::new("");
    {
        let t = fx.cfg.targets.get_mut("staging").unwrap();
        t.idempotent = false;
        t.probe = vec!["sh".into(), "-c".into(), "test -f ../deploy.txt".into()];
    }
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    for want in ["proven", "merged"] {
        assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == want));
    }
    crash_seam::set(Some("T8a:staging".into()));
    assert!(fx.with(|e| e.advance("b1")).is_err());
    crash_seam::set(None);
    let r = fx.tick();
    assert_eq!(r.lines[0], "b1: merged -> deploying -> live -> closed", "{:?}", r.lines);
    assert_eq!(lines_in(&fx, "deploy.txt"), 1, "the probe found staging deployed: not deployed again");
    assert_eq!(fx.count("decisions WHERE rule='deploy.checkpoint' AND subject='staging'"), 1);
}

#[test]
fn one_health_timer_the_targets_own_limit_decides_not_a_second_one_in_the_tick() {
    let sick = "[deploy.sick]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"true\"]\nhealth = [\"sh\", \"-c\", \"exit 1\"]\ntier = \"staging\"\nhealth_timeout_min = 300\n";
    let mut fx = Fx::new(sick);
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    assert_eq!(fx.tick().exit_code(), 0);
    // past [tick] max_wait_minutes (120), within the target's own 300 min: still a wait
    fx.clock.set(121 * 60);
    let r = fx.tick_at(NOW_US + 121 * MIN_US);
    assert!(r.lines[0].starts_with("b1: deploying, waits: target sick does not report "), "{:?}", r.lines);
    assert_eq!(r.exit_code(), 0);
    fx.clock.set(301 * 60);
    let r = fx.tick_at(NOW_US + 301 * MIN_US);
    assert!(r.lines[0].starts_with("b1: deploying, BLOCKED deploy.health-timeout: target sick did not report "), "{:?}", r.lines);
    assert_eq!(r.exit_code(), 1);
}

// ---------------------------------------------------------------- before tick may be enabled (final review, round 5)

#[test]
fn m4_a_tier_is_one_of_a_known_set_spelled_exactly() {
    let t = |tier: &str, extra: &str| BatchConfig::parse(&format!("{extra}[deploy.web]\nidempotent = true\ncommand = [\"x\"]\ntier = \"{tier}\"\n"));
    for bad in ["Production", "prod", "PRODUCTION", "staging "] {
        let e = t(bad, "").unwrap_err().to_string();
        assert!(e.contains(&format!("{bad:?}")) && e.contains("[deploy.web]"), "{bad}: {e}");
    }
    assert!(t("staging", "").is_ok());
    assert!(t("production", "").is_ok());
    assert!(t("qa", "").is_err(), "not listed");
    let c = t("qa", "[deploy]\nauto_tiers = [\"qa\"]\n").unwrap();
    assert_eq!(c.targets.len(), 1, "auto_tiers is not a target");
    assert_eq!(c.targets["web"].tier, "qa");
    assert!(BatchConfig::parse("[deploy]\nauto_tiers = [\"production\"]\n").is_err(), "production is never automatic");
}

/// Enable-readiness MINOR 5: with the tick on, every target names its tier (no silent `staging` default), and `[deploy] auto_tiers`
/// refuses any spelling of production or prod, whatever its case.
#[test]
fn minor5_tick_targets_name_their_tier_and_auto_tiers_never_names_production() {
    let on = "[tick]\nenabled = true\n";
    let web = |tier: &str| format!("[deploy.web]\nidempotent = true\ncommand = [\"x\"]\ntimeout_min = 5\n{tier}");
    let e = BatchConfig::parse(&format!("{on}{}", web(""))).unwrap_err().to_string();
    assert!(e.contains("[deploy.web]") && e.contains("tier") && e.contains("[tick] enabled"), "{e}");
    assert!(BatchConfig::parse(&format!("{on}{}", web("tier = \"staging\"\n"))).is_ok());
    assert!(BatchConfig::parse(&web("")).is_ok(), "without the tick a person deploys, and the default tier stays staging");
    for bad in ["Production", "PRODUCTION", "prod", "Prod", "PROD"] {
        let e = BatchConfig::parse(&format!("[deploy]\nauto_tiers = [\"{bad}\"]\n")).unwrap_err().to_string();
        assert!(e.contains("auto_tiers") && e.contains(bad), "{bad}: {e}");
    }
    assert!(BatchConfig::parse("[deploy]\nauto_tiers = [\"qa\", \"preprod-eu\"]\n").is_ok(), "only the word itself, not a tier that contains it");
}

fn hook(fx: &Fx, name: &str, body: &str) {
    let hooks = fx.repo.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(hooks.join(name), format!("#!/bin/sh\n{body}\n")).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(hooks.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn m3_a_hung_git_hook_ends_as_failed_within_the_limit_and_the_tick_reports_it() {
    let mut fx = Fx::new("[regen]\ncommand = [\"sh\", \"-c\", \"mkdir -p gen && echo x > gen/out.txt\"]\ngenerated_dirs = [\"gen\"]\n");
    fx.open("b1", "lane1");
    assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "stacked"));
    // the regen commit runs the pre-commit hook, which never ends; the limit is set only now, and with room for a loaded host, so
    // the other git calls of the transition never meet it
    hook(&fx, "pre-commit", "sleep 30");
    fx.cfg.git_timeout_min = 0.1;
    let t0 = std::time::Instant::now();
    let r = fx.tick();
    assert!(t0.elapsed() < std::time::Duration::from_secs(20), "the hook was ended at the limit");
    assert!(r.lines[0].starts_with("b1: stacked, FAILED T2: git commit"), "{:?}", r.lines);
    assert!(r.lines[0].contains("timed out after 0.1 min"), "{:?}", r.lines);
    assert_eq!(r.exit_code(), 1);
}

/// Enable-readiness MINOR 4: git calls have their own limit, `[timeouts] git_min`, not the health checks' `plugin_min`.
#[test]
fn minor4_git_calls_run_under_their_own_git_min_limit() {
    assert_eq!(BatchConfig::parse("").unwrap().git_timeout_min, 30.0, "default 30");
    assert!(BatchConfig::parse("[timeouts]\ngit_min = 0\n").is_err(), "a limit above 0");
    // a hung hook ends at git_min, though plugin_min stays at its default of 30 min
    let mut fx = Fx::new("[regen]\ncommand = [\"sh\", \"-c\", \"mkdir -p gen && echo x > gen/out.txt\"]\ngenerated_dirs = [\"gen\"]\n");
    fx.open("b1", "lane1");
    assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "stacked"));
    hook(&fx, "pre-commit", "sleep 30");
    fx.cfg = BatchConfig::parse(&format!("{}\n[timeouts]\ngit_min = 0.1\n", fx_toml(&fx))).unwrap();
    assert_eq!(fx.cfg.plugin_timeout_min, 30.0);
    let t0 = std::time::Instant::now();
    let r = fx.tick();
    assert!(t0.elapsed() < std::time::Duration::from_secs(20), "the hook was ended at git_min");
    assert!(r.lines[0].contains("timed out after 0.1 min ([timeouts] git_min"), "{:?}", r.lines);
    // and a short plugin_min does not cut a git call short
    let mut fx = Fx::new("");
    fx.open("b1", "lane1");
    assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "stacked"));
    // the regen command itself is quick; the commit's hook runs past plugin_min
    fx.cfg = BatchConfig::parse(&format!("{}\n[timeouts]\nplugin_min = 0.03\n", fx_toml(&fx))).unwrap();
    hook(&fx, "pre-commit", "sleep 4");
    let o = fx.adv("b1");
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "settled"), "{o:?}");
}

/// The gatetrain.toml the fixture was built from, without the extra table.
fn fx_toml(fx: &Fx) -> String {
    format!(
        "[preflight]\ncommand = [\"sh\", \"-c\", \"true\"]\n[chain]\ncommand = [\"fake-chain\"]\n\
         [postmerge]\ncommand = [\"sh\", \"-c\", \"echo $GATETRAIN_BATCH >> '{}'\"]\n\
         [deploy.staging]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"echo $GATETRAIN_BATCH >> '{}'\"]\nhealth = [\"sh\", \"-c\", \"true\"]\ntier = \"staging\"\n\
         [regen]\ncommand = [\"sh\", \"-c\", \"mkdir -p gen && echo x > gen/out.txt\"]\ngenerated_dirs = [\"gen\"]\n",
        fwd(&fx.t.path().join("postmerge.txt")),
        fwd(&fx.t.path().join("deploy.txt"))
    )
}

#[test]
fn m3_a_git_call_is_named_in_the_heartbeat_while_it_runs() {
    let mut fx = Fx::new("[regen]\ncommand = [\"sh\", \"-c\", \"mkdir -p gen && echo x > gen/out.txt\"]\ngenerated_dirs = [\"gen\"]\n");
    fx.open("b1", "lane1");
    assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "stacked"));
    hook(&fx, "pre-commit", "sleep 1");
    let cell = tick::Beat::default();
    let (stop, c2) = (std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)), cell.clone());
    let s2 = stop.clone();
    let reader = std::thread::spawn(move || {
        let mut seen: Vec<String> = vec![];
        while !s2.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(v) = c2.lock().unwrap().clone() {
                seen.push(v["effect"].as_str().unwrap_or_default().to_string());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        seen
    });
    let adm = AllowAll;
    {
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &now_at(0), state_dir: fx.state.clone(), actor: "engine".into(), progress: Some(cell) };
        assert!(matches!(e.advance("b1").unwrap(), Outcome::Advanced { to, .. } if to == "settled"));
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let seen = reader.join().unwrap();
    assert!(seen.iter().any(|e| e.starts_with("b1 git commit")), "{seen:?}");
}

/// Drive `fx` to `merged`, with the staging deploy changed by `f`.
fn merged_with(fx: &mut Fx, f: impl FnOnce(&mut gatetrain_core::batchcfg::Target)) {
    f(fx.cfg.targets.get_mut("staging").unwrap());
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    for want in ["proven", "merged"] {
        assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == want));
    }
}

fn wait_for(what: impl Fn() -> bool, secs: u64) -> bool {
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_secs(secs) {
        if what() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

#[test]
fn m2_a_deploy_that_outlives_a_dead_parent_is_waited_for_and_never_deployed_twice_at_once() {
    let mut fx = Fx::new("");
    merged_with(&mut fx, |t| {
        t.command[2].push_str("; sleep 4");
        t.survives_parent = true;
        t.timeout_min = Some(1.0);
    });
    // gatetrain "dies" right after it spawned the staging deploy; the deploy keeps running
    crash_seam::set(Some("T8a:spawned:deploy staging".into()));
    assert!(fx.with(|e| e.advance("b1")).is_err());
    crash_seam::set(None);
    assert!(wait_for(|| lines_in(&fx, "deploy.txt") == 1, 10), "the orphaned deploy runs");
    let r = fx.tick();
    assert!(r.lines[0].starts_with("b1: merged, waits: ") && r.lines[0].contains("still running"), "{:?}", r.lines);
    assert_eq!(r.exit_code(), 0);
    assert_eq!(lines_in(&fx, "deploy.txt"), 1, "no second deploy while the first one lives");
    // once it is gone the batch moves on (staging is idempotent: it is deployed again under a new attempt)
    std::thread::sleep(std::time::Duration::from_secs(5));
    let r = fx.tick();
    assert_eq!(r.lines[0], "b1: merged -> deploying -> live -> closed", "{:?}", r.lines);
    assert_eq!(lines_in(&fx, "deploy.txt"), 2);
}

#[test]
fn m2_without_survives_parent_a_plugin_dies_with_its_parent() {
    let mut fx = Fx::new("");
    let late = fwd(&fx.t.path().join("late.txt"));
    merged_with(&mut fx, |t| {
        t.command = vec!["sh".into(), "-c".into(), format!("sleep 3; echo late >> '{late}'")];
        t.timeout_min = Some(1.0);
    });
    // the advance runs on a thread of its own that ends with the crash, as a dead parent does
    std::thread::scope(|s| {
        s.spawn(|| {
            crash_seam::set(Some("T8a:spawned:deploy staging".into()));
            assert!(fx.with(|e| e.advance("b1")).is_err());
        });
    });
    std::thread::sleep(std::time::Duration::from_secs(5));
    assert!(!fx.t.path().join("late.txt").exists(), "the deploy outlived its parent");
}

fn t8a_outcome(fx: &Fx) -> String {
    fx.ledger.conn.query_row("SELECT json_extract(detail,'$.outcome') FROM batch_events WHERE transition='T8a' AND phase='failed' ORDER BY seq DESC LIMIT 1", [], |r| r.get(0)).unwrap()
}

#[test]
fn minor1_a_t8a_plugin_that_fails_or_times_out_leaves_the_attempt_dirty() {
    // the post-merge command ran and failed: it may have changed the target checkout
    let mut fx = Fx::new("");
    fx.cfg.postmerge_command = sh("exit 5");
    merged_with(&mut fx, |_| {});
    assert!(matches!(fx.adv("b1"), Outcome::Failed(_)));
    assert_eq!(t8a_outcome(&fx), "failed-dirty");
    // a deploy that ran out of time
    let mut fx = Fx::new("");
    fx.cfg.postmerge_command = vec![];
    merged_with(&mut fx, |t| {
        t.command = sh("sleep 30");
        t.timeout_min = Some(0.02);
    });
    assert!(matches!(fx.adv("b1"), Outcome::Failed(_)));
    assert_eq!(t8a_outcome(&fx), "failed-dirty");
    // nothing ran (a limit that is not a number): clean
    let mut fx = Fx::new("");
    fx.cfg.postmerge_command = vec![];
    merged_with(&mut fx, |t| t.timeout_min = Some(f64::NAN));
    assert!(matches!(fx.adv("b1"), Outcome::Failed(_)));
    assert_eq!(t8a_outcome(&fx), "failed-clean");
}

#[test]
fn minor2_the_postmerge_command_says_it_is_idempotent_when_the_tick_is_on() {
    let base = "[deploy.web]\nidempotent = true\ncommand = [\"x\"]\ntimeout_min = 5\ntier = \"staging\"\n[tick]\nenabled = true\n";
    let e = BatchConfig::parse(&format!("{base}[postmerge]\ncommand = [\"npm\", \"ci\"]\n")).unwrap_err().to_string();
    assert!(e.contains("[postmerge] idempotent = true"), "{e}");
    assert!(BatchConfig::parse(&format!("{base}[postmerge]\ncommand = [\"npm\", \"ci\"]\nidempotent = true\n")).is_ok());
    assert!(BatchConfig::parse("[postmerge]\ncommand = [\"npm\", \"ci\"]\n").is_ok(), "without the tick a person runs it");
}

#[test]
fn minor3_consecutive_interruptions_of_one_transition_stop_at_a_cap_and_need_a_person() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    fx.write_log("b1", GREEN);
    assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "proven"));
    crash_seam::set(Some("before:T7".into()));
    for _ in 0..3 {
        assert!(fx.with(|e| e.advance("b1")).is_err(), "a crash loop");
    }
    match fx.with(|e| e.advance("b1")).unwrap() {
        Outcome::Refused { rule, because, .. } => {
            assert_eq!(rule, "batch.interrupt-cap");
            assert!(because.contains("[retry] max_interrupts = 3"), "{because}");
        }
        o => panic!("{o:?}"),
    }
    crash_seam::set(None);
    fx.with(|e| e.retry_failed("b1", "the host was fixed")).unwrap();
    assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "merged"));
}

#[test]
fn minor3_consecutive_stopped_runs_stop_at_a_cap_and_need_a_person() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1", "lane1");
    for _ in 0..3 {
        fx.lock.named.lock().unwrap().clear(); // the holder dies every time
        assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "preflighted"));
        let o = fx.adv("b1");
        if fx.count("batch_events WHERE transition='T5s'") < 3 {
            assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "proving"), "{o:?}");
        } else {
            assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "batch.stopped-cap"), "{o:?}");
        }
    }
    assert_eq!(fx.lock.launches.lock().unwrap().len(), 3, "the first launch and two relaunches");
    fx.with(|e| e.retry_failed("b1", "the host is stable again")).unwrap();
    assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "proving"));
}

#[test]
fn minor4_a_target_removed_from_the_config_is_a_refusal_not_a_panic() {
    let mut fx = Fx::new("");
    merged_with(&mut fx, |_| {});
    assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "deploying"));
    fx.cfg.targets.clear();
    match fx.with(|e| e.advance("b1")).unwrap() {
        Outcome::Refused { rule, because, .. } => {
            assert_eq!(rule, "deploy.target-unknown");
            assert!(because.contains("staging"), "{because}");
        }
        o => panic!("{o:?}"),
    }
    // the same while a deploy intent is pending
    let mut fx = Fx::new("");
    merged_with(&mut fx, |_| {});
    crash_seam::set(Some("before:T8a".into()));
    assert!(fx.with(|e| e.advance("b1")).is_err());
    crash_seam::set(None);
    fx.cfg.targets.clear();
    assert!(fx.with(|e| e.advance("b1")).is_ok(), "no panic");
}

// ---------------------------------------------------------------- enable-readiness review (2026-11-18)

fn log_lines(fx: &Fx, file: &str) -> Vec<String> {
    std::fs::read_to_string(fx.t.path().join(file)).map(|t| t.lines().map(|l| l.trim().to_string()).collect()).unwrap_or_default()
}

/// b1 merged, and b2 stacked on b1 and proven (a green proof each). The staging deploy is `f`.
fn b1_merged_b2_proven(fx: &mut Fx, f: impl FnOnce(&mut gatetrain_core::batchcfg::Target)) {
    f(fx.cfg.targets.get_mut("staging").unwrap());
    fx.drive_to_proving("b1", "lane1");
    fx.open_on("b2", "b1", "lane2");
    for want in ["stacked", "settled", "preflighted"] {
        assert!(matches!(fx.adv("b2"), Outcome::Advanced { to, .. } if to == want));
    }
    fx.write_log("b1", GREEN);
    assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "proven"));
    assert!(matches!(fx.adv("b2"), Outcome::Advanced { to, .. } if to == "proving"));
    fx.write_log("b2", GREEN);
    assert!(matches!(fx.adv("b2"), Outcome::Advanced { to, .. } if to == "proven"));
    assert!(matches!(fx.adv("b1"), Outcome::Advanced { to, .. } if to == "merged"));
}

/// MAJOR 1 (the reviewer's repro): gatetrain dies right after it spawned b1's staging deploy, which outlives it. The next tick must
/// neither merge nor deploy b2 beside it, and once it is gone b1 settles first: b2's newer sha is the last one deployed.
#[test]
fn major1_a_second_batch_never_deploys_beside_or_after_an_orphaned_deploy_of_the_first() {
    let mut fx = Fx::new("");
    let log = fwd(&fx.t.path().join("deploy-log.txt"));
    b1_merged_b2_proven(&mut fx, |t| {
        t.command = vec!["sh".into(), "-c".into(), format!("echo start $GATETRAIN_BATCH >> '{log}'; sleep 4; echo end $GATETRAIN_BATCH >> '{log}'")];
        t.survives_parent = true;
        t.timeout_min = Some(1.0);
    });
    crash_seam::set(Some("T8a:spawned:deploy staging".into()));
    assert!(fx.with(|e| e.advance("b1")).is_err());
    crash_seam::set(None);
    assert!(wait_for(|| log_lines(&fx, "deploy-log.txt").len() == 1, 10), "the orphaned deploy runs");
    let r = fx.tick();
    assert!(r.lines[0].starts_with("b1: merged, waits: ") && r.lines[0].contains("still running"), "{:?}", r.lines);
    assert!(r.lines[1].starts_with("b2: proven, BLOCKED deploy.another-batch-deploying: "), "{:?}", r.lines);
    assert!(r.lines[1].contains("b1"), "the wait names the batch it waits for: {:?}", r.lines);
    assert_eq!(r.exit_code(), 0, "a wait that clears by itself");
    assert_eq!(fx.state("b2"), "proven", "b2 is not merged while b1's deploy is unsettled");
    assert!(wait_for(|| log_lines(&fx, "deploy-log.txt").len() == 2, 10), "the orphaned deploy ends");
    let r = fx.tick();
    assert_eq!(r.lines[0], "b1: merged -> deploying -> live -> closed", "{:?}", r.lines);
    assert_eq!(r.lines[1], "b2: proven -> merged -> deploying -> live -> closed", "{:?}", r.lines);
    fx.tick();
    assert_eq!(log_lines(&fx, "deploy-log.txt"), ["start b1", "end b1", "start b1", "end b1", "start b2", "end b2"], "no overlap, and b1 never deploys after b2");
}

/// MAJOR 1: a deploy whose sha is behind what a newer batch already deployed to the target is refused (`deploy.superseded`, a person
/// decides), whether it is a fresh attempt after a person's retry or a resumed one; it is never run.
#[test]
fn major1_a_deploy_behind_a_newer_batch_on_its_target_is_refused_superseded() {
    // b1's deploy fails once; it needs a person, so b2 merges and deploys; the person's retry of b1 must not deploy the older sha
    let mut fx = Fx::new("");
    let (log, flag) = (fwd(&fx.t.path().join("deploy-log.txt")), fwd(&fx.t.path().join("failed-once")));
    b1_merged_b2_proven(&mut fx, |t| t.command = vec!["sh".into(), "-c".into(), format!("if [ $GATETRAIN_BATCH = b1 ] && [ ! -e '{flag}' ]; then touch '{flag}'; exit 1; fi; echo deployed $GATETRAIN_BATCH >> '{log}'")]);
    assert!(matches!(fx.adv("b1"), Outcome::Failed(_)));
    let r = fx.tick();
    assert!(r.lines[0].starts_with("b1: merged, BLOCKED batch.failed-needs-person"), "{:?}", r.lines);
    assert_eq!(r.lines[1], "b2: proven -> merged -> deploying -> live -> closed", "a batch whose deploy needs a person does not hold the next one: {:?}", r.lines);
    fx.with(|e| e.retry_failed("b1", "the deploy host was fixed")).unwrap();
    match fx.adv("b1") {
        Outcome::Refused { rule, because, clears_by_itself, .. } => {
            assert_eq!(rule, "deploy.superseded");
            assert!(!clears_by_itself, "a person decides");
            assert!(because.contains("staging") && because.contains("b2"), "{because}");
        }
        o => panic!("{o:?}"),
    }
    assert_eq!(log_lines(&fx, "deploy-log.txt"), ["deployed b2"], "the older sha was never deployed over the newer one");
    // a resumed attempt: b1 crashed inside its deploy intent and was held; b2 deployed meanwhile; after the release b1 resumes
    let mut fx = Fx::new("");
    let log = fwd(&fx.t.path().join("deploy-log.txt"));
    b1_merged_b2_proven(&mut fx, |t| t.command = vec!["sh".into(), "-c".into(), format!("echo deployed $GATETRAIN_BATCH >> '{log}'")]);
    crash_seam::set(Some("before:T8a".into()));
    assert!(fx.with(|e| e.advance("b1")).is_err());
    crash_seam::set(None);
    fx.with(|e| e.hold("b1", "looking at the deploy host")).unwrap();
    let r = fx.tick();
    assert_eq!(r.lines[1], "b2: proven -> merged -> deploying -> live -> closed", "{:?}", r.lines);
    fx.with(|e| e.release("b1")).unwrap();
    assert!(matches!(fx.adv("b1"), Outcome::Refused { rule, .. } if rule == "deploy.superseded"));
    assert!(matches!(fx.adv("b1"), Outcome::Refused { rule, .. } if rule == "deploy.superseded"), "never re-run by a later advance");
    assert_eq!(log_lines(&fx, "deploy-log.txt"), ["deployed b2"]);
}
