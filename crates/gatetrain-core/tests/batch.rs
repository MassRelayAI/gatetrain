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

// ============================================================ the guards of the real-batch review (GT-COVER section C)

static LATER: fn() -> String = later;
fn later() -> String {
    "2026-11-15T11:00:00+00:00".to_string()
}

impl Fx {
    fn adv_at(&mut self, id: &str, now: &'static fn() -> String) -> gatetrain_core::Result<Outcome> {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now, state_dir: self.state.clone(), actor: "engine".into(), crash_after_effect: None };
        e.advance(id)
    }

    /// lane1 and lane2 both add a line after the same line of a.txt: the second pick of the stack conflicts.
    fn conflicting(extra: &str) -> Fx {
        let fx = Fx::new(extra);
        for (lane, line) in [("lane1", "from lane1"), ("lane2", "from lane2")] {
            g(&fx.repo, &["switch", "-q", lane]);
            std::fs::write(fx.repo.join("a.txt"), format!("a\n{line}\n")).unwrap();
            g(&fx.repo, &["commit", "-q", "-am", &format!("fix: {lane} edits a.txt")]);
        }
        g(&fx.repo, &["switch", "-q", "main"]);
        fx
    }

    fn failed_text(&mut self, id: &str) -> String {
        match self.adv(id).unwrap() {
            Outcome::Failed(m) => m,
            x => panic!("expected a failure, got {x:?}"),
        }
    }

    fn refused(&mut self, id: &str) -> (String, String) {
        match self.adv(id).unwrap() {
            Outcome::Refused { rule, because, .. } => (rule, because),
            x => panic!("expected a refusal, got {x:?}"),
        }
    }

    fn outside(&self, name: &str) -> PathBuf {
        self.repo.parent().unwrap().join(name)
    }
}

// ---- 1. the stack plugin and the hub resolvers

#[test]
fn a_stack_command_builds_the_stack_and_the_engine_checks_what_it_left() {
    let mut fx = Fx::new("[stack]\ncommand = [\"sh\", \"-c\", \"git cherry-pick lane1 lane2 && echo \\\"$GATETRAIN_BRANCH|$GATETRAIN_LANES\\\" > ../stack.ran\"]\n");
    fx.open("b1");
    fx.step("b1", "stacked");
    let subjects = g(&fx.repo, &["log", "--format=%s", "main..batch/b1/s1"]);
    assert!(subjects.contains("feat: lane1") && subjects.contains("feat: lane2"), "{subjects}");
    let ran = std::fs::read_to_string(fx.outside("stack.ran")).unwrap();
    assert_eq!(ran.trim(), "batch/b1/s1|lane1 lane2", "the plugin got the branch and the lanes");
}

#[test]
fn a_failing_or_incomplete_stack_command_fails_the_stack() {
    let mut fx = Fx::new("[stack]\ncommand = [\"sh\", \"-c\", \"exit 7\"]\n");
    fx.open("b1");
    assert!(fx.failed_text("b1").contains("stack command exited 7"));
    assert_eq!(fx.state("b1"), "open");
    // a plugin that leaves a lane out: the tip subject is missing from the stack (L33)
    let mut fx = Fx::new("[stack]\ncommand = [\"sh\", \"-c\", \"git cherry-pick lane1\"]\n");
    fx.open("b1");
    let m = fx.failed_text("b1");
    assert!(m.contains("lane2") && m.contains("L33"), "{m}");
    // a plugin that leaves uncommitted files
    let mut fx = Fx::new("[stack]\ncommand = [\"sh\", \"-c\", \"git cherry-pick lane1 lane2 && echo x > left.txt\"]\n");
    fx.open("b1");
    let m = fx.failed_text("b1");
    assert!(m.contains("uncommitted") && m.contains("left.txt"), "{m}");
}

#[test]
fn a_conflict_with_no_hub_and_no_resolver_stops_the_stack_and_leaves_the_checkout_clean() {
    let mut fx = Fx::conflicting("");
    fx.open("b1");
    let m = fx.failed_text("b1");
    assert!(m.contains("conflict applying") && m.contains("lane2"), "{m}");
    assert_eq!(g(&fx.repo, &["status", "--porcelain"]), "", "the pick was aborted");
}

#[test]
fn a_resolver_takes_a_side_and_a_hub_is_rebuilt_from_the_three_versions() {
    let mut fx = Fx::conflicting("[stack]\nresolvers = [{ glob = \"*.txt\", take = \"lane\" }]\n");
    fx.open("b1");
    fx.step("b1", "stacked");
    assert_eq!(g(&fx.repo, &["show", "batch/b1/s1:a.txt"]), "a\nfrom lane2", "the lane (theirs) won");
    // a hub: both sides kept by the repository's own rebuild command
    let mut fx = Fx::conflicting("[stack]\nhubs = [{ glob = \"a.*\", rebuild = [\"sh\", \"-c\", \"cat {ours} {theirs} | sort -u > {path}\"] }]\n");
    fx.open("b2");
    fx.step("b2", "stacked");
    assert_eq!(g(&fx.repo, &["show", "batch/b2/s1:a.txt"]), "a\nfrom lane1\nfrom lane2");
    assert_eq!(g(&fx.repo, &["status", "--porcelain"]), "");
}

#[test]
fn a_manual_resolver_a_failing_rebuild_and_leftover_markers_all_stop_the_stack() {
    let mut fx = Fx::conflicting("[stack]\nresolvers = [{ glob = \"a.txt\", take = \"manual\" }]\n");
    fx.open("b1");
    assert!(fx.failed_text("b1").contains("manual"));
    let mut fx = Fx::conflicting("[stack]\nhubs = [{ glob = \"a.txt\", rebuild = [\"sh\", \"-c\", \"exit 4\"] }]\n");
    fx.open("b1");
    assert!(fx.failed_text("b1").contains("hub rebuild for a.txt exited 4"));
    // the hub guard: a rebuild that writes conflict markers is refused
    let mut fx = Fx::conflicting("[stack]\nhubs = [{ glob = \"a.txt\", rebuild = [\"sh\", \"-c\", \"printf '<<<<<<< x\\\\nboth\\\\n>>>>>>> y\\\\n' > {path}\"] }]\n");
    fx.open("b1");
    assert!(fx.failed_text("b1").contains("conflict markers"), "markers");
    assert_eq!(g(&fx.repo, &["status", "--porcelain"]), "");
}

// ---- 2. settle before the chain: tested sha = merged sha

#[test]
fn settle_is_the_last_commit_before_the_chain() {
    let mut fx = Fx::new("");
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    // someone commits to the batch branch after settle: preflight refuses, naming the rule
    std::fs::write(fx.repo.join("late.txt"), "late\n").unwrap();
    g(&fx.repo, &["add", "--", "late.txt"]);
    g(&fx.repo, &["commit", "-q", "-m", "late"]);
    let (rule, because) = fx.refused("b1");
    assert_eq!(rule, "settle.tip-moved");
    assert!(because.contains("settle must be the last commit"), "{because}");
    assert_eq!(fx.state("b1"), "settled");
}

#[test]
fn the_chain_never_starts_on_a_dirty_tree() {
    let mut fx = Fx::new("");
    fx.open("b1");
    for s in ["stacked", "settled", "preflighted"] {
        fx.step("b1", s);
    }
    std::fs::write(fx.repo.join("generated-by-preflight.txt"), "x\n").unwrap();
    let (rule, because) = fx.refused("b1");
    assert_eq!(rule, "floor.clean-tree");
    assert!(because.contains("generated-by-preflight.txt"), "{because}");
    assert!(fx.lock.launches.lock().unwrap().is_empty(), "nothing was launched");
    // and a commit after the preflight stamp is the settle rule, before the stamp rule
    std::fs::remove_file(fx.repo.join("generated-by-preflight.txt")).unwrap();
    std::fs::write(fx.repo.join("late.txt"), "late\n").unwrap();
    g(&fx.repo, &["add", "--", "late.txt"]);
    g(&fx.repo, &["commit", "-q", "-m", "late"]);
    assert_eq!(fx.refused("b1").0, "settle.tip-moved");
}

// ---- settle failure paths

#[test]
fn a_failing_regen_command_fails_settle_and_the_batch_stays_stacked() {
    let mut fx = Fx::new("");
    fx.cfg.regen_command = vec!["sh".into(), "-c".into(), "echo boom >&2; exit 3".into()];
    fx.open("b1");
    fx.step("b1", "stacked");
    let m = fx.failed_text("b1");
    assert!(m.contains("regen command exited 3") && m.contains("boom"), "{m}");
    assert_eq!(fx.state("b1"), "stacked");
}

#[test]
fn a_generator_that_never_reaches_a_fixpoint_stops_after_max_passes() {
    let mut fx = Fx::new("");
    fx.cfg.regen_command = vec!["sh".into(), "-c".into(), "mkdir -p gen && echo x >> gen/out.txt".into()]; // grows on every pass
    fx.open("b1");
    fx.step("b1", "stacked");
    let m = fx.failed_text("b1");
    assert!(m.contains("no fixpoint after 3 regen passes"), "{m}");
    assert_eq!(fx.state("b1"), "stacked");
    let n = g(&fx.repo, &["log", "--format=%s", "main..HEAD"]).lines().filter(|s| s.starts_with("chore(batch): regenerate")).count();
    assert_eq!(n, 3, "the committed passes are kept, never deleted");
}

#[test]
fn a_generator_that_writes_outside_its_directories_fails_settle_naming_the_path() {
    let mut fx = Fx::new("");
    fx.cfg.regen_command = vec!["sh".into(), "-c".into(), "echo x > outside.txt".into()];
    fx.open("b1");
    fx.step("b1", "stacked");
    let m = fx.failed_text("b1");
    assert!(m.contains("floor.named-paths") && m.contains("outside.txt"), "{m}");
}

#[test]
fn a_stack_that_commits_conflict_markers_is_refused_before_settle_runs() {
    let mut fx = Fx::new("");
    g(&fx.repo, &["switch", "-q", "lane1"]);
    std::fs::write(fx.repo.join("notes.txt"), "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> lane\n").unwrap();
    g(&fx.repo, &["add", "--", "notes.txt"]);
    g(&fx.repo, &["commit", "-q", "-m", "chore: oops, markers"]);
    g(&fx.repo, &["switch", "-q", "main"]);
    fx.open("b1");
    fx.step("b1", "stacked");
    let (rule, because) = fx.refused("b1");
    assert_eq!(rule, "stack.conflict-markers");
    assert!(because.contains("notes.txt"), "{because}");
    assert!(!fx.repo.join("gen").exists(), "regen did not run");
}

// ---- 3. the cached-step guard

const CACHED: &str = "STEP_START a 2026-11-11T09:00:00Z\nSTEP_CACHED a\nSTEP_TIME a 0s\nSTEP_EXIT a 0\nGATES_DONE\n";

#[test]
fn a_proof_with_a_cached_step_and_no_verifier_is_refused() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    fx.write_log(CACHED);
    let (rule, because) = fx.refused("b1");
    assert_eq!(rule, "proof.cached-unverified");
    assert!(because.contains("step(s) a came from a step cache"), "{because}");
    assert_eq!(fx.state("b1"), "proving");
}

#[test]
fn a_cached_step_counts_only_when_the_verifier_vouches_for_it() {
    // the verifier sees the step name and the sha; it vouches for step a only
    let mut fx = Fx::new("cache_verify_command = [\"sh\", \"-c\", \"test \\\"$GATETRAIN_STEP\\\" = a && test -n \\\"$GATETRAIN_SHA\\\"\"]\n");
    fx.drive_to_proving("b1");
    fx.write_log(CACHED);
    fx.step("b1", "proven");
    let mut fx = Fx::new("cache_verify_command = [\"sh\", \"-c\", \"exit 1\"]\n");
    fx.drive_to_proving("b1");
    fx.write_log(CACHED);
    let (rule, because) = fx.refused("b1");
    assert_eq!(rule, "proof.cached-unverified");
    assert!(because.contains("not verified"), "{because}");
}

// ---- 4. the main checkout after a ref swap

#[test]
fn a_checked_out_protected_branch_is_fast_forwarded_with_its_working_tree() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    fx.write_log(GREEN);
    fx.step("b1", "proven");
    // main is checked out in a second working tree (the deploy checkout)
    let main_wt = fx.outside("main-wt");
    g(&fx.repo, &["worktree", "add", "-q", &fwd(&main_wt), "main"]);
    let tip = g(&fx.repo, &["rev-parse", "batch/b1/s1"]);
    fx.step("b1", "merged");
    assert_eq!(g(&fx.repo, &["rev-parse", "main"]), tip);
    assert_eq!(g(&main_wt, &["rev-parse", "HEAD"]), tip, "the checkout moved with the ref");
    assert!(main_wt.join("one.txt").exists() && main_wt.join("two.txt").exists(), "its working tree has the merged files");
    assert_eq!(g(&main_wt, &["status", "--porcelain"]), "", "and no phantom reverse changes");
}

#[test]
fn a_dirty_checked_out_protected_branch_refuses_the_merge() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    fx.write_log(GREEN);
    fx.step("b1", "proven");
    let main_wt = fx.outside("main-wt");
    g(&fx.repo, &["worktree", "add", "-q", &fwd(&main_wt), "main"]);
    std::fs::write(main_wt.join("a.txt"), "edited by hand\n").unwrap();
    let before = g(&fx.repo, &["rev-parse", "main"]);
    let (rule, because) = fx.refused("b1");
    assert_eq!(rule, "merge.checkout-dirty");
    assert!(because.contains("a.txt"), "{because}");
    assert_eq!(g(&fx.repo, &["rev-parse", "main"]), before, "the protected branch did not move");
    assert_eq!(fx.state("b1"), "proven");
}

// ---- 5. post-merge plugin, health wait and health timeout

fn deployed(extra: &str) -> Fx {
    let mut fx = Fx::new(extra);
    fx.drive_to_proving("b1");
    fx.write_log(GREEN);
    fx.step("b1", "proven");
    fx.step("b1", "merged");
    fx
}

#[test]
fn the_post_merge_command_runs_before_the_first_deploy_and_its_failure_stops_the_deploy() {
    let mut fx = deployed(
        "[postmerge]\ncommand = [\"sh\", \"-c\", \"echo $GATETRAIN_SHA > ../postmerge.ran\"]\n\
         [deploy.staging]\ncommand = [\"sh\", \"-c\", \"test -f ../postmerge.ran && echo deployed > ../deploy.ran\"]\nhealth = [\"sh\", \"-c\", \"true\"]\n",
    );
    fx.step("b1", "deploying");
    let tip = g(&fx.repo, &["rev-parse", "main"]);
    assert_eq!(std::fs::read_to_string(fx.outside("postmerge.ran")).unwrap().trim(), tip, "it got the merged sha");
    assert!(fx.outside("deploy.ran").exists(), "the deploy saw the post-merge result");
    // a failing post-merge: the deploy never runs
    let mut fx = deployed("[postmerge]\ncommand = [\"sh\", \"-c\", \"echo no deps >&2; exit 5\"]\n[deploy.staging]\ncommand = [\"sh\", \"-c\", \"echo x > ../deploy.ran\"]\nhealth = [\"sh\", \"-c\", \"true\"]\n");
    let m = fx.failed_text("b1");
    assert!(m.contains("post-merge command exited 5") && m.contains("no deps"), "{m}");
    assert!(!fx.outside("deploy.ran").exists());
    assert_eq!(fx.state("b1"), "merged");
}

#[test]
fn health_is_asked_about_the_merged_sha_and_the_batch_waits_until_it_answers() {
    let mut fx = deployed(
        "[deploy.staging]\ncommand = [\"sh\", \"-c\", \"true\"]\n\
         health = [\"sh\", \"-c\", \"echo $1 > ../health.asked; test -f ../ready\", \"sh\", \"{sha}\"]\n",
    );
    fx.step("b1", "deploying");
    let tip = g(&fx.repo, &["rev-parse", "main"]);
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Waiting(m) if m.contains("healthy")), "not healthy yet: wait");
    assert_eq!(fx.state("b1"), "deploying");
    assert_eq!(std::fs::read_to_string(fx.outside("health.asked")).unwrap().trim(), tip, "asked about the new build");
    std::fs::write(fx.outside("ready"), "").unwrap();
    fx.step("b1", "live");
}

#[test]
fn a_target_with_no_health_check_cannot_go_live() {
    let mut fx = deployed("[deploy.staging]\ncommand = [\"sh\", \"-c\", \"true\"]\n");
    fx.step("b1", "deploying");
    assert_eq!(fx.refused("b1").0, "deploy.health-missing");
    assert_eq!(fx.state("b1"), "deploying");
}

#[test]
fn health_that_stays_down_past_its_timeout_is_refused_once_and_a_late_recovery_still_goes_live() {
    let mut fx = deployed("[deploy.staging]\ncommand = [\"sh\", \"-c\", \"true\"]\nhealth = [\"sh\", \"-c\", \"test -f ../ready\"]\nhealth_timeout_min = 30\n");
    fx.step("b1", "deploying");
    // 10:00 deployed; at 10:00 the wait is fresh, at 11:00 it is over the 30 minutes
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Waiting(_)));
    let o = fx.adv_at("b1", &LATER).unwrap();
    match &o {
        Outcome::Refused { rule, because, .. } => {
            assert_eq!(rule, "deploy.health-timeout");
            assert!(because.contains("30 min"), "{because}");
        }
        x => panic!("{x:?}"),
    }
    assert_eq!(o.exit_code(), 3);
    let n = fx.decisions().len();
    fx.adv_at("b1", &LATER).unwrap();
    assert_eq!(fx.decisions().len(), n, "an identical refusal is logged once");
    assert_eq!(fx.state("b1"), "deploying");
    // the target finally answers: it goes live even after the deadline
    std::fs::write(fx.outside("ready"), "").unwrap();
    assert!(matches!(fx.adv_at("b1", &LATER).unwrap(), Outcome::Advanced { to, .. } if to == "live"));
}


// ============================================================ real seams, exit codes and generated-delta (audit items 2 and 4)

struct Rule(&'static str);
impl gatetrain_core::batch::Admission for Rule {
    fn admit(&self, _: &str, _: &str) -> gatetrain_core::Result<Option<String>> {
        Ok(Some("refused".into()))
    }
    fn admit_rule(&self, _: &str, _: &str) -> gatetrain_core::Result<Option<(String, String)>> {
        Ok(Some((self.0.to_string(), "the host seam said no".into())))
    }
}

#[test]
fn an_admission_refusal_leaves_with_the_exit_code_of_the_hold_that_would_refuse() {
    for (rule, code) in [("host.refuse", 75), ("host.refuse-memory", 76), ("host.refuse-disk", 77), ("host.refuse-cwd", 78), ("host.refuse-doctor", 79), ("host.admission", 3)] {
        let mut fx = Fx::new("");
        fx.open("b1");
        fx.step("b1", "stacked");
        fx.step("b1", "settled");
        let deny = Rule(rule);
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &deny, now: &NOW, state_dir: fx.state.clone(), actor: "engine".into(), crash_after_effect: None };
        let o = e.advance("b1").unwrap();
        assert!(matches!(&o, Outcome::Refused { rule: r, .. } if r == rule), "{o:?}");
        assert_eq!(o.exit_code(), code, "{rule}");
        assert_eq!(fx.state("b1"), "settled");
    }
}

fn status_fact(fx: &mut Fx, id: &str, path: &[&str]) -> serde_json::Value {
    let adm = AllowAll;
    let e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "engine".into(), crash_after_effect: None };
    let mut v = e.status(id).unwrap()["facts"].clone();
    for p in path {
        v = v[*p].clone();
    }
    v
}

fn proven_with_chain_output(fx: &mut Fx, files: &[(&str, &str)]) -> String {
    fx.drive_to_proving("b1");
    fx.write_log(GREEN);
    for (f, text) in files {
        let p = fx.repo.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
    g(&fx.repo, &["rev-parse", "batch/b1/s1"])
}

#[test]
fn files_the_chain_produced_under_the_generated_dirs_are_committed_by_name_when_the_merge_re_settles_them() {
    let mut fx = Fx::new("");
    let tested = proven_with_chain_output(&mut fx, &[("gen/chain-made.txt", "made by the chain\n"), ("gen/deeper/also-new.txt", "x\n")]);
    fx.step("b1", "proven");
    let delta = status_fact(&mut fx, "b1", &["proof", "generated_delta"]);
    assert_eq!(delta.as_array().unwrap().len(), 2, "{delta}");
    fx.step("b1", "merged");
    let main = g(&fx.repo, &["rev-parse", "main"]);
    assert_ne!(main, tested, "the merge carries the commit of the generated files");
    assert_eq!(g(&fx.repo, &["rev-parse", "main~1"]), tested, "and only that commit, on top of what was tested");
    assert_eq!(g(&fx.repo, &["show", "main:gen/chain-made.txt"]), "made by the chain");
    assert!(g(&fx.repo, &["log", "-1", "--format=%s"]).contains("generated-delta"));
    assert_eq!(g(&fx.repo, &["status", "--porcelain"]), "", "nothing is left over");
    assert_eq!(status_fact(&mut fx, "b1", &["merge_new"]).as_str().unwrap(), main, "deploy and health use the merged sha");
}

#[test]
fn a_change_outside_the_generated_dirs_after_the_chain_refuses_the_proof_and_is_never_committed() {
    let mut fx = Fx::new("");
    proven_with_chain_output(&mut fx, &[("gen/fine.txt", "ok\n"), ("notes/stray.txt", "oops\n")]);
    let (rule, because) = fx.refused("b1");
    assert_eq!(rule, "chain.tree-differs");
    assert!(because.contains("notes/stray.txt") && !because.contains("gen/fine.txt"), "{because}");
    assert_eq!(fx.state("b1"), "proving");
    assert!(g(&fx.repo, &["ls-files", "notes"]).is_empty(), "never committed");
}

#[test]
fn generated_files_that_the_generators_would_change_again_stop_the_merge() {
    let mut fx = Fx::new("");
    proven_with_chain_output(&mut fx, &[("gen/chain-made.txt", "made by the chain\n")]);
    fx.step("b1", "proven");
    fx.cfg.regen_command = vec!["sh".into(), "-c".into(), "mkdir -p gen && echo more >> gen/grow.txt".into()]; // never a fixpoint
    let m = fx.failed_text("b1");
    assert!(m.contains("not a fixpoint") && m.contains("gen/grow.txt"), "{m}");
    assert_eq!(fx.state("b1"), "proven");
    assert_ne!(g(&fx.repo, &["rev-parse", "main"]), g(&fx.repo, &["rev-parse", "HEAD"]), "main did not move");
}

#[test]
fn a_crash_after_the_merge_with_generated_files_resumes_as_done_with_the_merged_sha() {
    let mut fx = Fx::new("");
    proven_with_chain_output(&mut fx, &[("gen/chain-made.txt", "made by the chain\n")]);
    fx.step("b1", "proven");
    fx.crash = Some("T7".into());
    assert!(fx.adv("b1").is_err());
    fx.crash = None;
    fx.step("b1", "merged");
    let main = g(&fx.repo, &["rev-parse", "main"]);
    assert_eq!(status_fact(&mut fx, "b1", &["merge_new"]).as_str().unwrap(), main);
    assert_eq!(g(&fx.repo, &["show", "main:gen/chain-made.txt"]), "made by the chain");
}


#[test]
fn a_staged_rename_into_the_generated_dir_is_a_deletion_outside_it_and_is_refused() {
    // the reviewer's sequence: `git mv one.txt gen/one.txt` shows ONE staged rename whose destination is generated; the deletion of the
    // source is a change outside the generated dirs and must count
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    fx.write_log(GREEN);
    std::fs::create_dir_all(fx.repo.join("gen")).unwrap();
    g(&fx.repo, &["mv", "one.txt", "gen/one.txt"]);
    assert!(g(&fx.repo, &["status", "--porcelain"]).contains("R "), "git shows a staged rename");
    let (rule, because) = fx.refused("b1");
    assert_eq!(rule, "chain.tree-differs");
    assert!(because.contains("one.txt"), "{because}");
    assert_eq!(fx.state("b1"), "proving");
    // and the deletion was never committed
    assert_eq!(g(&fx.repo, &["rev-parse", "HEAD"]), g(&fx.repo, &["rev-parse", "batch/b1/s1"]));
}

// ---------------------------------------------------------------- governance: lane stamp (stack.lane-stamp) and cause-gated rerun

const RED_LOG: &str = "STEP_START a 2026-11-11T09:00:00Z\nSTEP_TIME a 1s\nSTEP_EXIT a 1\nRED_NOW a rc=1\nRED_SUMMARY: a\nGATES_DONE\n";

impl Fx {
    fn retry(&mut self, id: &str) -> Outcome {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "engine".into(), crash_after_effect: None };
        e.retry(id).unwrap()
    }

    fn note(&mut self, id: &str, class: &str, reason: &str) {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "human:me".into(), crash_after_effect: None };
        e.note_cause(id, class, reason).unwrap();
    }

    fn red(&mut self, id: &str) {
        self.drive_to_proving(id);
        self.write_log(RED_LOG);
        self.step(id, "red");
    }

    /// A new commit on the batch branch, with the preflight stamp the launch guard asks for.
    fn commit_and_stamp(&mut self, name: &str) -> String {
        std::fs::write(self.repo.join(name), "fix\n").unwrap();
        g(&self.repo, &["add", "--", name]);
        g(&self.repo, &["commit", "-q", "-m", &format!("fix: {name}")]);
        let tip = g(&self.repo, &["rev-parse", "HEAD"]);
        std::fs::create_dir_all(self.state.join("stamps")).unwrap();
        std::fs::write(self.state.join("stamps").join(format!("{tip}.stamp")), format!("{tip}\n")).unwrap();
        tip
    }

    fn refusals(&self, rule: &str) -> usize {
        self.decisions().iter().filter(|(k, r, o)| k == "refusal" && r.as_deref() == Some(rule) && o == "refused").count()
    }

    fn retry_to_proving(&mut self, id: &str) {
        let o = self.retry(id);
        assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "proving"), "{o:?}");
    }
}

fn stamp_cfg(dir: &Path) -> String {
    format!("[lanes]\nrequire_stamp = true\nstamp_dir = \"{}\"\n", fwd(dir))
}

fn write_lane_stamp(dir: &Path, sha: &str) {
    std::fs::write(dir.join(format!("stamp-{sha}.json")), format!("{{\"sha\": \"{sha}\", \"steps\": [], \"digest\": \"d\"}}\n")).unwrap();
}

#[test]
fn a_lane_without_a_stamp_is_refused_and_named() {
    let sd = tempfile::tempdir().unwrap();
    let mut fx = Fx::new(&stamp_cfg(sd.path()));
    fx.open("b1");
    let (t1, t2) = (g(&fx.repo, &["rev-parse", "lane1"]), g(&fx.repo, &["rev-parse", "lane2"]));
    write_lane_stamp(sd.path(), &t1); // lane1 is stamped, lane2 is not
    match fx.adv("b1").unwrap() {
        Outcome::Refused { rule, because, .. } => {
            assert_eq!(rule, "stack.lane-stamp");
            assert!(because.contains("lane2") && because.contains(&t2[..9]), "{because}");
            assert!(!because.contains("lane1"), "a stamped lane is not named: {because}");
        }
        x => panic!("{x:?}"),
    }
    assert_eq!(fx.state("b1"), "open");
    assert_eq!(fx.refusals("stack.lane-stamp"), 1, "recorded as a refusal in the decision log");
    assert!(g(&fx.repo, &["branch", "--list", "batch/b1/s1"]).is_empty(), "nothing was stacked");
    assert!(declog::verify(&fx.ledger).unwrap().is_ok());
}

#[test]
fn every_unstamped_lane_is_named() {
    let sd = tempfile::tempdir().unwrap();
    let mut fx = Fx::new(&stamp_cfg(sd.path()));
    fx.open("b1");
    match fx.adv("b1").unwrap() {
        Outcome::Refused { because, .. } => assert!(because.contains("lane1") && because.contains("lane2"), "{because}"),
        x => panic!("{x:?}"),
    }
}

#[test]
fn stamps_for_every_lane_let_the_stack_through() {
    let sd = tempfile::tempdir().unwrap();
    let mut fx = Fx::new(&stamp_cfg(sd.path()));
    fx.open("b1");
    for l in ["lane1", "lane2"] {
        write_lane_stamp(sd.path(), &g(&fx.repo, &["rev-parse", l]));
    }
    fx.step("b1", "stacked");
}

#[test]
fn a_stamp_for_another_sha_does_not_count() {
    let sd = tempfile::tempdir().unwrap();
    let mut fx = Fx::new(&stamp_cfg(sd.path()));
    fx.open("b1");
    let t1 = g(&fx.repo, &["rev-parse", "lane1"]);
    let t2 = g(&fx.repo, &["rev-parse", "lane2"]);
    write_lane_stamp(sd.path(), &t1);
    // lane2's file name carries the right sha but the body names another commit: not a stamp for this tip
    std::fs::write(sd.path().join(format!("stamp-{t2}.json")), "{\"sha\": \"deadbeef\", \"steps\": [], \"digest\": \"d\"}").unwrap();
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Refused { rule, .. } if rule == "stack.lane-stamp"));
}

#[test]
fn the_lane_stamp_setting_off_behaves_as_before() {
    let mut fx = Fx::new(""); // no [lanes] table: no stamps exist and the stack goes through
    fx.open("b1");
    fx.step("b1", "stacked");
    let sd = tempfile::tempdir().unwrap();
    let mut fx2 = Fx::new(&format!("[lanes]\nrequire_stamp = false\nstamp_dir = \"{}\"\n", fwd(sd.path())));
    fx2.open("b2");
    fx2.step("b2", "stacked");
}

#[test]
fn require_stamp_without_a_stamp_dir_is_a_config_error() {
    assert!(BatchConfig::parse("[lanes]\nrequire_stamp = true\n").is_err());
}

#[test]
fn a_retry_on_an_unmoved_tip_after_a_red_run_is_refused() {
    let mut fx = Fx::new("");
    fx.red("b1");
    let o = fx.retry("b1");
    assert_eq!(o.exit_code(), 3);
    match o {
        Outcome::Refused { rule, because, .. } => {
            assert_eq!(rule, "retry.causes-open");
            assert!(because.contains("b1-p1"), "names the red run: {because}");
        }
        x => panic!("{x:?}"),
    }
    assert_eq!(fx.state("b1"), "red");
    assert_eq!(fx.lock.launches.lock().unwrap().len(), 1, "no second chain was launched");
    assert_eq!(fx.refusals("retry.causes-open"), 1, "recorded in the decision log");
    fx.retry("b1");
    assert_eq!(fx.refusals("retry.causes-open"), 1, "an identical repeat is not logged twice");
}

#[test]
fn a_retry_is_allowed_after_a_new_commit() {
    let mut fx = Fx::new("");
    fx.red("b1");
    fx.commit_and_stamp("fix.txt");
    let o = fx.retry("b1");
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "proving"), "{o:?}");
    assert_eq!(fx.state("b1"), "proving");
    let l = fx.lock.launches.lock().unwrap();
    assert_eq!(l.len(), 2);
    assert_eq!(l[1].run_id, "b1-p2", "a new run id and a new log");
    assert_eq!(l[1].sha, g(&fx.repo, &["rev-parse", "HEAD"]), "the new tip is what runs");
}

#[test]
fn a_retry_in_a_state_other_than_red_is_idle() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    assert!(matches!(fx.retry("b1"), Outcome::Idle(_)));
}

#[test]
fn a_recorded_flake_or_infra_note_allows_the_retry_on_the_same_tip() {
    for class in ["flake", "infra"] {
        let mut fx = Fx::new("");
        fx.red("b1");
        assert!(matches!(fx.retry("b1"), Outcome::Refused { .. }));
        fx.note("b1", class, "the registry timed out, unrelated to the code");
        let o = fx.retry("b1");
        assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "proving"), "{class}: {o:?}");
        let l = fx.lock.launches.lock().unwrap();
        assert_eq!(l[1].sha, l[0].sha, "same sha: that is what a flake rerun means");
        // the note is in the decision log with its reason, as an override by a person
        let n: i64 = fx.ledger.conn.query_row("SELECT count(*) FROM decisions WHERE kind='override' AND rule='retry.cause-noted' AND actor='human:me' AND reason LIKE '%registry%'", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert!(declog::verify(&fx.ledger).unwrap().is_ok());
    }
}

#[test]
fn a_note_is_for_one_red_run_only_and_needs_a_reason_and_a_known_class() {
    let mut fx = Fx::new("");
    fx.red("b1");
    {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), crash_after_effect: None };
        assert!(e.note_cause("b1", "flake", "  ").is_err(), "no reason, no note");
        assert!(e.note_cause("b1", "bug", "it is fine").is_err(), "only flake or infra");
    }
    fx.note("b1", "flake", "network blip");
    fx.retry_to_proving("b1");
    // the second run is red as well: the old note does not cover it
    fx.write_log(RED_LOG);
    fx.step("b1", "red");
    assert!(matches!(fx.retry("b1"), Outcome::Refused { rule, .. } if rule == "retry.causes-open"));
}

#[test]
fn the_full_run_budget_refuses_run_n_plus_one() {
    let mut fx = Fx::new("max_full_runs_per_batch = 2\n");
    fx.red("b1"); // run 1
    fx.commit_and_stamp("f1.txt");
    fx.retry_to_proving("b1"); // run 2
    fx.write_log(RED_LOG);
    fx.step("b1", "red");
    fx.commit_and_stamp("f2.txt"); // the cause gate would pass; the budget does not
    match fx.retry("b1") {
        Outcome::Refused { rule, because, .. } => {
            assert_eq!(rule, "budget.full-runs");
            assert!(because.contains('2'), "{because}");
        }
        x => panic!("{x:?}"),
    }
    assert_eq!(fx.lock.launches.lock().unwrap().len(), 2);
    assert_eq!(fx.refusals("budget.full-runs"), 1);
}

#[test]
fn a_stopped_run_does_not_spend_the_full_run_budget() {
    let mut fx = Fx::new("max_full_runs_per_batch = 1\n");
    fx.drive_to_proving("b1");
    fx.lock.named.lock().unwrap().clear(); // the holder is gone: stopped, not a finished run
    fx.step("b1", "preflighted");
    fx.step("b1", "proving"); // the relaunch is still run 1 of 1
    fx.write_log(RED_LOG);
    fx.step("b1", "red");
    fx.commit_and_stamp("f1.txt");
    assert!(matches!(fx.retry("b1"), Outcome::Refused { rule, .. } if rule == "budget.full-runs"));
}

#[test]
fn the_budget_guards_the_launch_of_a_second_proof_from_preflighted() {
    // a budget of 0 means no full run at all: the very first launch is refused
    let mut fx = Fx::new("max_full_runs_per_batch = 0\n");
    fx.open("b1");
    for s in ["stacked", "settled", "preflighted"] {
        fx.step("b1", s);
    }
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Refused { rule, .. } if rule == "budget.full-runs"));
    assert_eq!(fx.state("b1"), "preflighted");
}

#[test]
fn no_budget_set_means_no_limit() {
    let mut fx = Fx::new("");
    fx.red("b1");
    for i in 0..4 {
        fx.commit_and_stamp(&format!("f{i}.txt"));
        fx.retry_to_proving("b1");
        fx.write_log(RED_LOG);
        fx.step("b1", "red");
    }
}

#[test]
fn a_green_retry_proves_and_merges_like_any_proof() {
    let mut fx = Fx::new("");
    fx.red("b1");
    fx.commit_and_stamp("fix.txt");
    fx.retry_to_proving("b1");
    fx.write_log(GREEN);
    fx.step("b1", "proven");
    fx.step("b1", "merged");
}

// ============================================================ E3: one evidence rule (engine-v2 section 7)

fn sha256_hex(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

const KEYED: &str = "STEP_START a 2026-11-11T09:00:00Z\nSTEP_CACHED a K1 from pre-1\nSTEP_TIME a 0s\nSTEP_EXIT a 0\nGATES_DONE\n";

impl Fx {
    fn root(&self) -> PathBuf {
        self.state.parent().unwrap().to_path_buf()
    }

    /// Stamp folder, steps file and proof host of the evidence rule; three would-hits of key K1 already seen in shadow.
    fn evidence(&mut self) {
        let stamps = self.root().join("stamps");
        std::fs::create_dir_all(&stamps).unwrap();
        let steps = self.root().join("steps.toml");
        std::fs::write(&steps, "[[step]]\nname = \"a\"\n").unwrap();
        self.cfg.steps_file = Some(fwd(&steps));
        self.cfg.evidence_stamp_dirs = vec![fwd(&stamps)];
        self.cfg.proof_host = Some("h".into());
        self.seed_shadow("K1", 3);
    }

    fn seed_shadow(&self, key: &str, n: usize) {
        for i in 0..n {
            let run = format!("shadow-{key}-{i}");
            let line = format!("STEP_CACHE_SHADOW a would-hit {key} rc=0\n");
            assert_eq!(gatetrain_core::evidence::record_shadow(&self.state, &run, &line).unwrap(), 1);
        }
    }

    fn plan_digest(&self) -> String {
        sha256_hex(&std::fs::read(self.cfg.steps_file.as_ref().unwrap()).unwrap())
    }

    fn good_stamp(&self, sha: &str) -> serde_json::Value {
        serde_json::json!({"v": 1, "kind": "preflight", "sha": sha, "host": "h", "host_class": "c1", "at": "2026-11-15T09:00:00+00:00",
            "digest": self.plan_digest(), "run_id": "pre-1", "steps": [{"name": "a", "rc": 0, "key": "K1"}]})
    }

    fn put_stamp(&self, file: &str, v: &serde_json::Value) {
        std::fs::write(self.root().join("stamps").join(file), serde_json::to_string(v).unwrap()).unwrap();
    }

    fn tip_now(&self, id: &str) -> String {
        g(&self.repo, &["rev-parse", &format!("batch/{id}/s1")])
    }

    /// proving, with the stamp `stamp` builds (given the tip) written first and the proof log `log` ready.
    fn proving_with(&mut self, id: &str, stamp: impl Fn(&Fx, &str) -> serde_json::Value, log: &str) {
        self.evidence();
        self.open(id);
        self.step(id, "stacked");
        self.step(id, "settled");
        let tip = self.tip_now(id);
        let v = stamp(self, &tip);
        self.put_stamp("stamp-pre-1.json", &v);
        self.step(id, "preflighted");
        self.step(id, "proving");
        self.write_log(log);
    }

    fn why(&mut self, id: &str) -> String {
        let (rule, because) = self.refused(id);
        assert_eq!(rule, "proof.cached-unverified", "{because}");
        assert_eq!(self.state(id), "proving");
        because
    }
}

#[test]
fn a_cached_step_with_a_matching_stamp_key_sha_host_and_plan_is_accepted_and_the_evidence_is_recorded() {
    let mut fx = Fx::new("");
    fx.proving_with("b1", |f, t| f.good_stamp(t), KEYED);
    fx.step("b1", "proven");
    let ev = status_fact(&mut fx, "b1", &["proof", "evidence"]);
    assert_eq!(ev[0]["step"], "a");
    assert_eq!(ev[0]["source_run"], "pre-1");
    assert_eq!(ev[0]["input_key"], "K1");
    assert_eq!(ev[0]["source_host"], "h");
    assert_eq!(ev[0]["verified_by"], "stamp");
}

#[test]
fn evidence_sha_a_result_from_another_commit_is_refused() {
    let mut fx = Fx::new("");
    fx.proving_with("b1", |f, _| f.good_stamp(&"0".repeat(40)), KEYED);
    let why = fx.why("b1");
    assert!(why.contains("evidence-sha"), "{why}");
}

#[test]
fn evidence_key_a_result_with_another_key_is_refused() {
    let mut fx = Fx::new("");
    fx.proving_with(
        "b1",
        |f, t| {
            let mut v = f.good_stamp(t);
            v["steps"][0]["key"] = "K2".into();
            v
        },
        KEYED,
    );
    let why = fx.why("b1");
    assert!(why.contains("evidence-key"), "{why}");
}

#[test]
fn evidence_class_another_host_counts_only_for_a_portable_step_of_the_same_class_and_an_accepted_role() {
    let other = |f: &Fx, t: &str, portable: bool, class: &str| {
        let mut v = f.good_stamp(t);
        v["host"] = "ws".into();
        v["host_class"] = class.into();
        v["steps"][0]["portable"] = portable.into();
        v
    };
    let setup = |fx: &mut Fx| {
        fx.cfg.proof_host_class = Some("c1".into());
        fx.cfg.accept_roles = vec!["proof".into(), "scout".into()];
        fx.cfg.host_roles.insert("ws".into(), "scout".into());
    };
    // not portable
    let mut fx = Fx::new("");
    setup(&mut fx);
    fx.proving_with("b1", |f, t| other(f, t, false, "c1"), KEYED);
    assert!(fx.why("b1").contains("evidence-class"));
    // another class
    let mut fx = Fx::new("");
    setup(&mut fx);
    fx.proving_with("b1", |f, t| other(f, t, true, "c2"), KEYED);
    assert!(fx.why("b1").contains("evidence-class"));
    // a role the policy does not list
    let mut fx = Fx::new("");
    setup(&mut fx);
    fx.cfg.accept_roles = vec!["proof".into()];
    fx.proving_with("b1", |f, t| other(f, t, true, "c1"), KEYED);
    assert!(fx.why("b1").contains("evidence-class"));
    // portable, same class, listed role: the same fixture without the violation passes
    let mut fx = Fx::new("");
    setup(&mut fx);
    fx.proving_with("b1", |f, t| other(f, t, true, "c1"), KEYED);
    fx.step("b1", "proven");
    assert_eq!(status_fact(&mut fx, "b1", &["proof", "evidence"])[0]["source_host"], "ws");
}

#[test]
fn evidence_stamp_a_source_run_of_another_steps_file_is_refused() {
    let mut fx = Fx::new("");
    fx.proving_with(
        "b1",
        |f, t| {
            let mut v = f.good_stamp(t);
            v["digest"] = "0".repeat(64).into();
            v
        },
        KEYED,
    );
    assert!(fx.why("b1").contains("evidence-stamp"));
}

#[test]
fn evidence_stamp_is_unknown_without_a_configured_steps_file_and_unknown_is_a_refusal() {
    let mut fx = Fx::new("");
    fx.proving_with("b1", |f, t| f.good_stamp(t), KEYED);
    fx.cfg.steps_file = None;
    assert!(fx.why("b1").contains("evidence-stamp"));
}

#[test]
fn evidence_partial_a_result_whose_need_the_source_run_never_ran_is_refused() {
    let mut fx = Fx::new("");
    fx.proving_with(
        "b1",
        |f, t| {
            let mut v = f.good_stamp(t);
            v["steps"][0]["needs"] = serde_json::json!(["pre"]);
            v
        },
        KEYED,
    );
    assert!(fx.why("b1").contains("evidence-partial"));
    // the same need green in the source run passes
    let mut fx = Fx::new("");
    fx.proving_with(
        "b1",
        |f, t| {
            let mut v = f.good_stamp(t);
            v["steps"][0]["needs"] = serde_json::json!(["pre"]);
            v["steps"].as_array_mut().unwrap().push(serde_json::json!({"name": "pre", "rc": 0, "key": "P"}));
            v
        },
        KEYED,
    );
    fx.step("b1", "proven");
}

#[test]
fn a_stamp_marked_partial_is_never_evidence() {
    let mut fx = Fx::new("");
    fx.proving_with(
        "b1",
        |f, t| {
            let mut v = f.good_stamp(t);
            v["partial"] = true.into();
            v
        },
        KEYED,
    );
    assert!(fx.why("b1").contains("evidence-partial"));
}

#[test]
fn evidence_stale_a_result_older_than_max_age_hours_is_refused() {
    let mut fx = Fx::new("");
    fx.cfg.max_age_hours = Some(24.0);
    fx.proving_with(
        "b1",
        |f, t| {
            let mut v = f.good_stamp(t);
            v["at"] = "2026-11-13T09:00:00+00:00".into();
            v
        },
        KEYED,
    );
    assert!(fx.why("b1").contains("evidence-stale"));
}

#[test]
fn evidence_undeclared_a_cached_step_with_no_key_is_refused_without_a_verifier() {
    let mut fx = Fx::new("");
    fx.proving_with("b1", |f, t| f.good_stamp(t), CACHED);
    let why = fx.why("b1");
    assert!(why.contains("evidence-undeclared") && why.contains("step(s) a came from a step cache"), "{why}");
}

#[test]
fn evidence_unproven_a_key_needs_shadow_would_hits_and_no_defect() {
    // three would-hits are not enough when four are asked for
    let mut fx = Fx::new("");
    fx.proving_with("b1", |f, t| f.good_stamp(t), KEYED);
    fx.cfg.shadow_min = 4;
    assert!(fx.why("b1").contains("evidence-unproven"));
    // a would-hit whose real run was red is a key-model defect: the key is off
    let mut fx = Fx::new("");
    fx.proving_with("b1", |f, t| f.good_stamp(t), KEYED);
    assert_eq!(gatetrain_core::evidence::record_shadow(&fx.state, "shadow-bad", "STEP_CACHE_SHADOW a would-hit K1 rc=1\n").unwrap(), 1);
    let why = fx.why("b1");
    assert!(why.contains("evidence-unproven") && why.contains("defect"), "{why}");
}

#[test]
fn shadow_lines_are_recorded_once_per_run_and_step() {
    let fx = Fx::new("");
    let text = "STEP_CACHE_SHADOW a would-hit K9 rc=0\nSTEP_CACHE_SHADOW b miss K8 rc=0\nSTEP_START x 2026-11-11T09:00:00Z\n";
    assert_eq!(gatetrain_core::evidence::record_shadow(&fx.state, "r1", text).unwrap(), 2);
    assert_eq!(gatetrain_core::evidence::record_shadow(&fx.state, "r1", text).unwrap(), 0);
}

#[test]
fn a_shadow_miss_counts_for_nothing() {
    let mut fx = Fx::new("");
    fx.proving_with("b1", |f, t| f.good_stamp(t), KEYED);
    fx.cfg.shadow_min = 4;
    gatetrain_core::evidence::record_shadow(&fx.state, "shadow-miss", "STEP_CACHE_SHADOW a miss K1 rc=0\n").unwrap();
    assert!(fx.why("b1").contains("evidence-unproven"));
}

#[test]
fn evidence_missing_a_keyed_hit_with_no_result_anywhere_is_refused() {
    let mut fx = Fx::new("");
    fx.proving_with(
        "b1",
        |f, t| {
            let mut v = f.good_stamp(t);
            v["steps"][0]["name"] = "other".into();
            v
        },
        KEYED,
    );
    assert!(fx.why("b1").contains("evidence-missing"));
}

#[test]
fn the_policy_never_refuses_every_cached_step_even_a_verified_one() {
    let mut fx = Fx::new("");
    fx.cfg.allow_cached_steps = "never".into();
    fx.proving_with("b1", |f, t| f.good_stamp(t), KEYED);
    assert!(fx.why("b1").contains("policy-never"));
}

#[test]
fn a_keyless_cached_step_is_still_vouched_for_by_the_verify_command() {
    // the adopter's own cache plug-in keeps working (the earlier tests pin its messages)
    let mut fx = Fx::new("cache_verify_command = [\"sh\", \"-c\", \"exit 0\"]\n");
    fx.proving_with("b1", |f, t| f.good_stamp(t), CACHED);
    fx.step("b1", "proven");
}

#[test]
fn the_proof_launch_writes_the_evidence_file_the_chain_is_given() {
    let mut fx = Fx::new("");
    fx.cfg.chain_command = vec!["fake-chain".into(), "--evidence".into(), "{evidence}".into()];
    fx.evidence();
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    let tip = fx.tip_now("b1");
    fx.put_stamp("stamp-pre-1.json", &fx.good_stamp(&tip));
    fx.put_stamp("stamp-old.json", &fx.good_stamp(&"1".repeat(40)));
    fx.step("b1", "preflighted");
    fx.step("b1", "proving");
    let req = fx.lock.launches.lock().unwrap()[0].clone();
    assert_eq!(req.command[1], "--evidence");
    let path = PathBuf::from(&req.command[2]);
    assert!(path.is_file(), "{path:?}");
    assert!(req.env.iter().any(|(k, v)| k == "GATETRAIN_EVIDENCE" && fwd(&PathBuf::from(v)) == fwd(&path)));
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(v["sha"], tip.as_str());
    let rows = v["results"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "only the result for the tip: {v}");
    assert_eq!(rows[0]["step"], "a");
    assert_eq!(rows[0]["key"], "K1");
    assert_eq!(rows[0]["run_id"], "pre-1");
}

#[test]
fn the_evidence_file_leaves_out_results_the_proof_could_not_accept() {
    let mut fx = Fx::new("");
    fx.cfg.chain_command = vec!["fake-chain".into(), "--evidence".into(), "{evidence}".into()];
    fx.evidence();
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    let tip = fx.tip_now("b1");
    // another host that is not portable, another steps file, a red row: none may be offered
    let mut a = fx.good_stamp(&tip);
    a["host"] = "ws".into();
    fx.put_stamp("stamp-1.json", &a);
    let mut b = fx.good_stamp(&tip);
    b["digest"] = "0".repeat(64).into();
    fx.put_stamp("stamp-2.json", &b);
    let mut c = fx.good_stamp(&tip);
    c["steps"][0]["rc"] = 1.into();
    fx.put_stamp("stamp-3.json", &c);
    fx.step("b1", "preflighted");
    fx.step("b1", "proving");
    let req = fx.lock.launches.lock().unwrap()[0].clone();
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&req.command[2]).unwrap()).unwrap();
    assert_eq!(v["results"].as_array().unwrap().len(), 0, "{v}");
}
