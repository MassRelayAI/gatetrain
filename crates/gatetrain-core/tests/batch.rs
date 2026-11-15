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
