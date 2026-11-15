//! The batch state machine on temp git repos with fake plugin commands. Synthetic only.
use gatetrain_core::batch::{AllowAll, DenyRole, Engine, FakeLock, Outcome};
use gatetrain_core::batchcfg::BatchConfig;
use gatetrain_core::declog;
use gatetrain_core::ledger::Ledger;
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

fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

struct Fx {
    _t: tempfile::TempDir,
    repo: PathBuf,
    state: PathBuf,
    counter: PathBuf,
    ledger: Ledger,
    cfg: BatchConfig,
    lock: FakeLock,
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
        for (lane, file) in [("lane1", "one.txt"), ("lane2", "two.txt")] {
            g(&repo, &["switch", "-q", "-c", lane, "main"]);
            std::fs::write(repo.join(file), "x\n").unwrap();
            g(&repo, &["add", "--", file]);
            g(&repo, &["commit", "-q", "-m", &format!("feat: {lane}")]);
        }
        g(&repo, &["switch", "-q", "main"]);
        let counter = t.path().join("preflight.count");
        let toml = format!(
            "[regen]\ncommand = [\"sh\", \"-c\", \"mkdir -p gen && echo generated > gen/out.txt\"]\nmax_passes = 3\ngenerated_dirs = [\"gen\"]\n\
             [preflight]\ncommand = [\"sh\", \"-c\", \"echo ran >> '{}'\"]\n[chain]\ncommand = [\"fake-chain\"]\n{extra_toml}",
            fwd(&counter)
        );
        let mut ledger = Ledger::open_in_memory().unwrap();
        ledger.migrate().unwrap();
        Fx { state: t.path().join("state"), repo, counter, ledger, cfg: BatchConfig::parse(&toml).unwrap(), lock: FakeLock::default(), crash: None, _t: t }
    }

    fn adv(&mut self, id: &str) -> gatetrain_core::Result<Outcome> {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "engine".into(), crash_after_effect: self.crash.clone() };
        e.advance(id)
    }

    fn state(&mut self, id: &str) -> String {
        let adm = AllowAll;
        let e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "engine".into(), crash_after_effect: None };
        e.status(id).unwrap()["state"].as_str().unwrap().to_string()
    }

    fn open(&mut self, id: &str) {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "engine".into(), crash_after_effect: None };
        e.new_batch(id, "main", &self.repo, &["lane1".into(), "lane2".into()]).unwrap();
    }

    fn step(&mut self, id: &str, want: &str) {
        let o = self.adv(id).unwrap();
        assert!(matches!(&o, Outcome::Advanced { to, .. } if to == want), "wanted {want}, got {o:?}");
        assert_eq!(self.state(id), want);
    }

    fn write_log(&self, body_after_branch: &str) {
        let req = self.lock.launches.lock().unwrap().last().unwrap().clone();
        let log = format!("branch b at {} on h\nCHAIN_MODE full\n{body_after_branch}", req.sha);
        std::fs::write(&req.log, log).unwrap();
    }

    fn drive_to_proving(&mut self, id: &str) {
        self.open(id);
        for s in ["stacked", "settled", "preflighted", "proving"] {
            self.step(id, s);
        }
    }

    fn decisions(&self) -> Vec<(String, Option<String>, String)> {
        let mut st = self.ledger.conn.prepare("SELECT kind,rule,outcome FROM decisions ORDER BY seq").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(|x| x.unwrap()).collect()
    }
}

const GREEN: &str = "STEP_START a 2026-11-11T09:00:00Z\nSTEP_TIME a 1s\nSTEP_EXIT a 0\nGATES_DONE\n";

#[test]
fn happy_path_open_to_proven() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    // no GATES_DONE and the lock names the run: wait, no transition
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Waiting(_)));
    fx.write_log(GREEN);
    fx.step("b1", "proven");
    // the stack carries both lanes' own commits
    let tip = g(&fx.repo, &["rev-parse", "batch/b1/s1"]);
    let subjects = g(&fx.repo, &["log", "--format=%s", "main..batch/b1/s1"]);
    assert!(subjects.contains("feat: lane1") && subjects.contains("feat: lane2"), "{subjects}");
    // the proof is bound to the tip that was settled
    let req = fx.lock.launches.lock().unwrap()[0].clone();
    assert_eq!(req.sha, tip);
    assert!(req.env.iter().any(|(k, v)| k == "GATETRAIN_RUN_ID" && v == "b1-p1"));
    // every transition wrote a decision, and the chain still verifies
    let d = fx.decisions();
    for rule in ["transition.open", "transition.T1", "transition.T2", "transition.T3", "transition.T4", "transition.T5"] {
        assert!(d.iter().any(|(_, r, _)| r.as_deref() == Some(rule)), "missing decision {rule}: {d:?}");
    }
    assert!(declog::verify(&fx.ledger).unwrap().is_ok());
}

#[test]
fn merge_then_deploy_to_live_and_production_stays_manual() {
    let mut fx = Fx::new(
        "[deploy.staging]\ncommand = [\"sh\", \"-c\", \"true\"]\nhealth = [\"sh\", \"-c\", \"true\"]\ntier = \"staging\"\n\
         [deploy.prod]\ncommand = [\"sh\", \"-c\", \"exit 9\"]\ntier = \"production\"\n",
    );
    fx.drive_to_proving("b1");
    fx.write_log(GREEN);
    fx.step("b1", "proven");
    let tip = g(&fx.repo, &["rev-parse", "batch/b1/s1"]);
    fx.step("b1", "merged");
    assert_eq!(g(&fx.repo, &["rev-parse", "main"]), tip);
    fx.step("b1", "deploying");
    fx.step("b1", "live");
    fx.step("b1", "closed");
    let d = fx.decisions();
    assert!(d.iter().any(|(k, r, o)| k == "refusal" && r.as_deref() == Some("floor.production-human") && o == "refused"), "{d:?}");
}

#[test]
fn crash_between_intent_and_done_resumes_without_rerunning_the_effect() {
    let mut fx = Fx::new("");
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    fx.crash = Some("T3".into());
    assert!(fx.adv("b1").is_err(), "the simulated crash must surface");
    assert_eq!(fx.state("b1"), "settled", "no done was written");
    assert_eq!(std::fs::read_to_string(&fx.counter).unwrap().lines().count(), 1);
    fx.crash = None;
    // the probe finds the stamp for exactly this sha: happened, the preflight command is not run again
    fx.step("b1", "preflighted");
    assert_eq!(std::fs::read_to_string(&fx.counter).unwrap().lines().count(), 1, "preflight must not run twice");
    // a crash after the launch: the lock names the run id, so the launch is adopted, never doubled
    fx.crash = Some("T4".into());
    assert!(fx.adv("b1").is_err());
    fx.crash = None;
    fx.step("b1", "proving");
    assert_eq!(fx.lock.launches.lock().unwrap().len(), 1, "launch must not run twice");
}

#[test]
fn an_intent_whose_effect_did_not_happen_runs_again_under_the_same_run_id() {
    let mut fx = Fx::new("");
    fx.open("b1");
    for s in ["stacked", "settled", "preflighted"] {
        fx.step("b1", s);
    }
    // simulate a crash before the launch: an intent with no effect
    fx.crash = Some("T4".into());
    // the fake lock records the launch before the crash hook fires, so clear it to model "never happened"
    assert!(fx.adv("b1").is_err());
    fx.lock.named.lock().unwrap().clear();
    fx.lock.launches.lock().unwrap().clear();
    fx.crash = None;
    fx.step("b1", "proving");
    let l = fx.lock.launches.lock().unwrap();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].run_id, "b1-p1", "the run id fixed before launch is reused");
}

#[test]
fn a_red_verdict_blocks_merged() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    fx.write_log("STEP_START a 2026-11-11T09:00:00Z\nSTEP_TIME a 1s\nSTEP_EXIT a 1\nRED_NOW a rc=1\nRED_SUMMARY: a\nGATES_DONE\n");
    fx.step("b1", "red");
    let o = fx.adv("b1").unwrap();
    match &o {
        Outcome::Refused { rule, .. } => assert_eq!(rule, "floor.zero-red"),
        x => panic!("{x:?}"),
    }
    assert_eq!(o.exit_code(), 3);
    assert_eq!(fx.state("b1"), "red");
    assert_eq!(g(&fx.repo, &["rev-parse", "main"]), g(&fx.repo, &["rev-parse", "lane1~1"]), "main did not move");
}

#[test]
fn a_run_without_gates_done_and_without_a_lock_is_stopped_not_red() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    fx.lock.named.lock().unwrap().clear(); // the holder is gone
    fx.step("b1", "preflighted");
    fx.step("b1", "proving");
    assert_eq!(fx.lock.launches.lock().unwrap().last().unwrap().run_id, "b1-p2", "a new run id and a new log");
}

#[test]
fn a_preview_log_is_never_a_proof() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    let req = fx.lock.launches.lock().unwrap()[0].clone();
    std::fs::write(&req.log, format!("branch b at {} on h\nCHAIN_MODE preview\n{GREEN}", req.sha)).unwrap();
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Refused { rule, .. } if rule == "floor.full-mode"));
    assert_eq!(fx.state("b1"), "proving");
}

#[test]
fn a_sha_mismatch_refuses_with_the_rule_id() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    fx.write_log(GREEN);
    fx.step("b1", "proven");
    // one more commit lands on the batch branch after the proof began
    std::fs::write(fx.repo.join("late.txt"), "late\n").unwrap();
    g(&fx.repo, &["add", "--", "late.txt"]);
    g(&fx.repo, &["commit", "-q", "-m", "late"]);
    let before = g(&fx.repo, &["rev-parse", "main"]);
    let o = fx.adv("b1").unwrap();
    match &o {
        Outcome::Refused { rule, because, decision } => {
            assert_eq!(rule, "merge.tested-equals-merged");
            assert!(because.contains("tested"), "{because}");
            assert!(*decision > 0);
        }
        x => panic!("{x:?}"),
    }
    assert_eq!(o.exit_code(), 3);
    assert_eq!(fx.state("b1"), "proven");
    assert_eq!(g(&fx.repo, &["rev-parse", "main"]), before, "the protected branch did not move");
    // an identical repeat is not logged twice
    let n = fx.decisions().len();
    fx.adv("b1").unwrap();
    assert_eq!(fx.decisions().len(), n);
}

#[test]
fn a_new_generated_file_is_committed_in_settle_by_name() {
    let mut fx = Fx::new("");
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    assert_eq!(g(&fx.repo, &["ls-files", "gen/out.txt"]), "gen/out.txt", "the new file under generated_dirs is committed");
    assert_eq!(g(&fx.repo, &["status", "--porcelain"]), "", "nothing is left over");
    let subject = g(&fx.repo, &["log", "-1", "--format=%s"]);
    assert!(subject.starts_with("chore(batch): regenerate"), "{subject}");
}

#[test]
fn a_stray_path_stops_settle_and_is_named() {
    let mut fx = Fx::new("");
    fx.open("b1");
    fx.step("b1", "stacked");
    std::fs::write(fx.repo.join("stray.txt"), "oops\n").unwrap();
    match fx.adv("b1").unwrap() {
        Outcome::Refused { rule, because, .. } => {
            assert_eq!(rule, "floor.named-paths");
            assert!(because.contains("stray.txt"), "{because}");
        }
        x => panic!("{x:?}"),
    }
    assert_eq!(fx.state("b1"), "stacked");
}

#[test]
fn admission_refusal_is_a_refusal_with_its_reason() {
    let mut fx = Fx::new("");
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    let deny = DenyRole("light", "no free slot on the light host");
    let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &deny, now: &NOW, state_dir: fx.state.clone(), actor: "engine".into(), crash_after_effect: None };
    assert!(matches!(e.advance("b1").unwrap(), Outcome::Refused { rule, .. } if rule == "host.admission"));
}

#[test]
fn hold_and_release_round_trip_and_abandon_deletes_nothing() {
    let mut fx = Fx::new("");
    fx.open("b1");
    fx.step("b1", "stacked");
    let adm = AllowAll;
    {
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), crash_after_effect: None };
        e.hold("b1", "waiting on a review").unwrap();
    }
    assert_eq!(fx.state("b1"), "held");
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Refused { rule, .. } if rule == "batch.held"));
    {
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), crash_after_effect: None };
        e.release("b1").unwrap();
    }
    assert_eq!(fx.state("b1"), "stacked");
    {
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), crash_after_effect: None };
        e.abandon("b1", "dropped").unwrap();
    }
    assert_eq!(fx.state("b1"), "abandoned");
    assert!(!g(&fx.repo, &["branch", "--list", "batch/b1/s1"]).is_empty(), "the stack branch is kept");
}

#[test]
fn update_and_delete_on_batch_events_abort() {
    let mut fx = Fx::new("");
    fx.open("b1");
    let u = fx.ledger.conn.execute("UPDATE batch_events SET state_to='merged'", []);
    assert!(u.unwrap_err().to_string().contains("append-only"));
    let d = fx.ledger.conn.execute("DELETE FROM batch_events", []);
    assert!(d.unwrap_err().to_string().contains("append-only"));
    assert_eq!(fx.state("b1"), "open", "state is only what the events fold to");
}
