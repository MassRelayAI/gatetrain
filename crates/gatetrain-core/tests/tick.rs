//! `tick` (engine v2 E10): one bounded pass over every open batch through `advance`, doctor and `tidy --auto` per host, one advice
//! line, the cause-storm line. Temp git repositories, the fake lock and admission, fake host seams: no real host, Docker or repository.
use gatetrain_core::batch::{AllowAll, Engine, FakeLock, Outcome};
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
static NOW: fn() -> String = now;
const NOW_US: i64 = 1_763_200_800_000_000;

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
    crash: Option<String>,
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
             [deploy.staging]\ncommand = [\"sh\", \"-c\", \"true\"]\nhealth = [\"sh\", \"-c\", \"true\"]\ntier = \"staging\"\n{extra_toml}",
            fwd(&marker)
        );
        let hosts_file = t.path().join("hosts.toml");
        std::fs::write(&hosts_file, HOSTS).unwrap();
        let hosts = config::load(&hosts_file).unwrap();
        let mut ledger = Ledger::open_in_memory().unwrap();
        ledger.migrate().unwrap();
        Fx { state: t.path().join("state"), repo, ledger, cfg: BatchConfig::parse(&toml).unwrap(), lock: FakeLock::default(), hosts, crash: None, t }
    }

    fn with<R>(&mut self, f: impl FnOnce(&mut Engine) -> R) -> R {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "engine".into(), crash_after_effect: self.crash.clone() };
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
        let rules: Vec<diagnose::Rule> = diagnose::builtin_rules().to_vec();
        let storm = self.cfg.storm_reds;
        let hosts = self.hosts.clone();
        self.with(|e| {
            let o = TickOpts { hosts: &hosts, seams: &FakeHosts, rules: &rules, storm_reds: storm, now_us: NOW_US, max_steps };
            tick::tick(e, &o).unwrap()
        })
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
    assert!(tick::try_lock(&lp).unwrap().is_some(), "released when the first tick ends");
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
    assert_eq!(r.lines[2], "+2 more batch(es): 1 waiting, 1 blocked (gatetrain batch status)", "{:?}", r.lines);
    assert_eq!(r.exit_code(), 1, "a held batch needs a person");
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
    fx.crash = Some("T8a".into());
    assert!(fx.with(|e| e.advance("b1")).is_err(), "the simulated crash after the deploy");
    fx.crash = None;
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
