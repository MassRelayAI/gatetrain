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
        Fx { state: t.path().join("state"), repo, counter, ledger, cfg: BatchConfig::parse(&toml).unwrap(), lock: FakeLock::default(), _t: t }
    }

    fn adv(&mut self, id: &str) -> gatetrain_core::Result<Outcome> {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "engine".into(), progress: None };
        e.advance(id)
    }

    fn state(&mut self, id: &str) -> String {
        let adm = AllowAll;
        let e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "engine".into(), progress: None };
        e.status(id).unwrap()["state"].as_str().unwrap().to_string()
    }

    fn open(&mut self, id: &str) {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "engine".into(), progress: None };
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
        "[deploy.staging]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"true\"]\nhealth = [\"sh\", \"-c\", \"true\"]\ntier = \"staging\"\n\
         [deploy.prod]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"exit 9\"]\ntier = \"production\"\n",
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
    gatetrain_core::batch::crash_seam::set(Some("T3".into()));
    assert!(fx.adv("b1").is_err(), "the simulated crash must surface");
    assert_eq!(fx.state("b1"), "settled", "no done was written");
    assert_eq!(std::fs::read_to_string(&fx.counter).unwrap().lines().count(), 1);
    gatetrain_core::batch::crash_seam::set(None);
    // the probe finds the stamp for exactly this sha: happened, the preflight command is not run again
    fx.step("b1", "preflighted");
    assert_eq!(std::fs::read_to_string(&fx.counter).unwrap().lines().count(), 1, "preflight must not run twice");
    // a crash after the launch: the lock names the run id, so the launch is adopted, never doubled
    gatetrain_core::batch::crash_seam::set(Some("T4".into()));
    assert!(fx.adv("b1").is_err());
    gatetrain_core::batch::crash_seam::set(None);
    fx.step("b1", "proving");
    assert_eq!(fx.lock.launches.lock().unwrap().len(), 1, "launch must not run twice");
}

#[test]
fn an_intent_whose_effect_did_not_happen_runs_again_as_a_new_attempt_with_a_new_run_id() {
    let mut fx = Fx::new("");
    fx.open("b1");
    for s in ["stacked", "settled", "preflighted"] {
        fx.step("b1", s);
    }
    // a crash right after the intent: the launch never happened (no lock names the run, no log was opened)
    gatetrain_core::batch::crash_seam::set(Some("before:T4".into()));
    assert!(fx.adv("b1").is_err());
    gatetrain_core::batch::crash_seam::set(None);
    assert!(fx.lock.launches.lock().unwrap().is_empty());
    fx.step("b1", "proving");
    let l = fx.lock.launches.lock().unwrap();
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].run_id, "b1-p2", "a resume never reuses a run id: the interrupted attempt keeps b1-p1");
    let outcome: String = fx.ledger.conn.query_row("SELECT json_extract(detail,'$.outcome') FROM batch_events WHERE batch='b1' AND transition='T4' AND phase='failed'", [], |r| r.get(0)).unwrap();
    assert_eq!(outcome, "interrupted");
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
        Outcome::Refused { rule, because, decision, .. } => {
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
    let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &deny, now: &NOW, state_dir: fx.state.clone(), actor: "engine".into(), progress: None };
    assert!(matches!(e.advance("b1").unwrap(), Outcome::Refused { rule, .. } if rule == "host.admission"));
}

#[test]
fn hold_and_release_round_trip_and_abandon_deletes_nothing() {
    let mut fx = Fx::new("");
    fx.open("b1");
    fx.step("b1", "stacked");
    let adm = AllowAll;
    {
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), progress: None };
        e.hold("b1", "waiting on a review").unwrap();
    }
    assert_eq!(fx.state("b1"), "held");
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Refused { rule, .. } if rule == "batch.held"));
    {
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), progress: None };
        e.release("b1").unwrap();
    }
    assert_eq!(fx.state("b1"), "stacked");
    {
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), progress: None };
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
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now, state_dir: self.state.clone(), actor: "engine".into(), progress: None };
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
         [deploy.staging]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"test -f ../postmerge.ran && echo deployed > ../deploy.ran\"]\nhealth = [\"sh\", \"-c\", \"true\"]\n",
    );
    fx.step("b1", "deploying");
    let tip = g(&fx.repo, &["rev-parse", "main"]);
    assert_eq!(std::fs::read_to_string(fx.outside("postmerge.ran")).unwrap().trim(), tip, "it got the merged sha");
    assert!(fx.outside("deploy.ran").exists(), "the deploy saw the post-merge result");
    // a failing post-merge: the deploy never runs
    let mut fx = deployed("[postmerge]\ncommand = [\"sh\", \"-c\", \"echo no deps >&2; exit 5\"]\n[deploy.staging]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"echo x > ../deploy.ran\"]\nhealth = [\"sh\", \"-c\", \"true\"]\n");
    let m = fx.failed_text("b1");
    assert!(m.contains("post-merge command exited 5") && m.contains("no deps"), "{m}");
    assert!(!fx.outside("deploy.ran").exists());
    assert_eq!(fx.state("b1"), "merged");
}

#[test]
fn health_is_asked_about_the_merged_sha_and_the_batch_waits_until_it_answers() {
    let mut fx = deployed(
        "[deploy.staging]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"true\"]\n\
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
    let mut fx = deployed("[deploy.staging]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"true\"]\n");
    fx.step("b1", "deploying");
    assert_eq!(fx.refused("b1").0, "deploy.health-missing");
    assert_eq!(fx.state("b1"), "deploying");
}

#[test]
fn health_that_stays_down_past_its_timeout_is_refused_once_and_a_late_recovery_still_goes_live() {
    let mut fx = deployed("[deploy.staging]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"true\"]\nhealth = [\"sh\", \"-c\", \"test -f ../ready\"]\nhealth_timeout_min = 30\n");
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
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &deny, now: &NOW, state_dir: fx.state.clone(), actor: "engine".into(), progress: None };
        let o = e.advance("b1").unwrap();
        assert!(matches!(&o, Outcome::Refused { rule: r, .. } if r == rule), "{o:?}");
        assert_eq!(o.exit_code(), code, "{rule}");
        assert_eq!(fx.state("b1"), "settled");
    }
}

fn status_fact(fx: &mut Fx, id: &str, path: &[&str]) -> serde_json::Value {
    let adm = AllowAll;
    let e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "engine".into(), progress: None };
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
    gatetrain_core::batch::crash_seam::set(Some("T7".into()));
    assert!(fx.adv("b1").is_err());
    gatetrain_core::batch::crash_seam::set(None);
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
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "engine".into(), progress: None };
        e.retry(id).unwrap()
    }

    fn note(&mut self, id: &str, class: &str, reason: &str) {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "human:me".into(), progress: None };
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
fn a_partial_stamp_is_no_lane_stamp() {
    let sd = tempfile::tempdir().unwrap();
    let mut fx = Fx::new(&stamp_cfg(sd.path()));
    fx.open("b1");
    for l in ["lane1", "lane2"] {
        let t = g(&fx.repo, &["rev-parse", l]);
        // a run of some of the steps (--only, or one step of a multi-host run) proves only those steps
        std::fs::write(sd.path().join(format!("stamp-{t}.json")), format!("{{\"sha\": \"{t}\", \"steps\": [], \"digest\": \"d\", \"partial\": true}}")).unwrap();
    }
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
        let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), progress: None };
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

// ============================================================ E5: the pre-flight as a launched run, and the pipelining (engine-v2 section 8)

impl Fx {
    /// `[preflight] chain_command`: the pre-flight is launched under the holder and observed (state `preflighting`).
    fn launched_preflight(&mut self) {
        let dir = self.root().join("pfstamps");
        std::fs::create_dir_all(&dir).unwrap();
        let steps = self.root().join("pf-steps.toml");
        std::fs::write(&steps, "[[step]]\nname = \"a\"\n").unwrap();
        self.cfg.preflight_chain_command = vec!["fake-pf".into(), "{sha}".into(), "{run}".into()];
        self.cfg.preflight_stamp_dir = Some(fwd(&dir));
        self.cfg.preflight_steps_file = Some(fwd(&steps));
    }

    fn pf_digest(&self) -> String {
        sha256_hex(&std::fs::read(self.cfg.preflight_steps_file.as_ref().unwrap()).unwrap())
    }

    fn pf_stamp_path(&self, sha: &str) -> PathBuf {
        PathBuf::from(self.cfg.preflight_stamp_dir.as_ref().unwrap()).join(format!("stamp-{sha}.json"))
    }

    fn pf_stamp(&self, sha: &str) {
        let v = serde_json::json!({"v": 1, "kind": "preflight", "mode": "preflight", "sha": sha, "digest": self.pf_digest(), "steps": [{"name": "a", "rc": 0}]});
        std::fs::write(self.pf_stamp_path(sha), serde_json::to_string(&v).unwrap()).unwrap();
    }

    fn pf_log(&self, mode: &str, body: &str) {
        let req = self.lock.launches.lock().unwrap().last().unwrap().clone();
        std::fs::write(&req.log, format!("branch b at {} on h\nCHAIN_MODE {mode}\n{body}", req.sha)).unwrap();
    }

    fn get_settled(&mut self, id: &str) {
        self.open(id);
        self.step(id, "stacked");
        self.step(id, "settled");
    }

    fn launches(&self) -> usize {
        self.lock.launches.lock().unwrap().len()
    }
}

#[test]
fn the_preflight_is_launched_as_a_run_and_the_batch_is_preflighting_until_its_stamp_is_read() {
    let mut fx = Fx::new("");
    fx.launched_preflight();
    fx.get_settled("b1");
    fx.step("b1", "preflighting");
    assert!(!fx.counter.exists(), "the blocking [preflight] command did not run");
    let req = fx.lock.launches.lock().unwrap()[0].clone();
    assert_eq!(req.role, "light");
    assert_eq!(req.run_id, "b1-f1");
    assert_eq!(req.command[0], "fake-pf");
    assert_eq!(req.command[1], req.sha);
    assert_eq!(req.command[2], "b1-f1");
    assert!(req.env.iter().any(|(k, v)| k == "GATETRAIN_RUN_ID" && v == "b1-f1"));
    // a live run: wait, no transition
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Waiting(_)));
    assert_eq!(fx.state("b1"), "preflighting");
    // the run ends green with its stamp: preflighted, and the proof may start
    fx.pf_log("preflight", GREEN);
    fx.pf_stamp(&req.sha);
    fx.step("b1", "preflighted");
    fx.step("b1", "proving");
    let proof = fx.lock.launches.lock().unwrap()[1].clone();
    assert_eq!((proof.role.as_str(), proof.run_id.as_str()), ("proof", "b1-p1"));
}

#[test]
fn a_crash_after_the_preflight_launch_resumes_by_adopting_the_run() {
    let mut fx = Fx::new("");
    fx.launched_preflight();
    fx.get_settled("b1");
    gatetrain_core::batch::crash_seam::set(Some("T3".into()));
    assert!(fx.adv("b1").is_err());
    gatetrain_core::batch::crash_seam::set(None);
    assert_eq!(fx.launches(), 1);
    fx.step("b1", "preflighting");
    assert_eq!(fx.launches(), 1, "the run was adopted, not started again");
}

#[test]
fn a_red_preflight_is_a_state_of_its_own_and_is_not_relaunched() {
    let mut fx = Fx::new("");
    fx.launched_preflight();
    fx.get_settled("b1");
    fx.step("b1", "preflighting");
    fx.pf_log("preflight", RED_LOG);
    fx.step("b1", "preflight-red");
    let (rule, _) = fx.refused("b1");
    assert_eq!(rule, "preflight.red");
    assert_eq!(fx.launches(), 1);
    assert_eq!(fx.state("b1"), "preflight-red");
}

#[test]
fn a_preflight_that_stopped_without_gates_done_returns_get_settled_and_the_relaunch_is_a_new_run() {
    let mut fx = Fx::new("");
    fx.launched_preflight();
    fx.get_settled("b1");
    fx.step("b1", "preflighting");
    fx.lock.named.lock().unwrap().clear();
    fx.step("b1", "settled");
    fx.step("b1", "preflighting");
    assert_eq!(fx.lock.launches.lock().unwrap()[1].run_id, "b1-f2");
}

#[test]
fn a_preflight_log_that_is_not_in_preflight_mode_is_refused() {
    let mut fx = Fx::new("");
    fx.launched_preflight();
    fx.get_settled("b1");
    fx.step("b1", "preflighting");
    fx.pf_log("full", GREEN);
    let (rule, _) = fx.refused("b1");
    assert_eq!(rule, "floor.preflight-mode");
    assert_eq!(fx.state("b1"), "preflighting");
}

#[test]
fn a_green_preflight_without_a_stamp_for_the_tip_with_the_steps_file_digest_is_refused() {
    let mut fx = Fx::new("");
    fx.launched_preflight();
    fx.get_settled("b1");
    fx.step("b1", "preflighting");
    let sha = fx.lock.launches.lock().unwrap()[0].sha.clone();
    fx.pf_log("preflight", GREEN);
    // no stamp at all
    let (rule, because) = fx.refused("b1");
    assert_eq!(rule, "preflight.stamp-for-sha");
    assert!(because.contains("no chain stamp"), "{because}");
    // a stamp of another steps file
    let v = serde_json::json!({"sha": sha, "digest": "0".repeat(64), "steps": []});
    std::fs::write(fx.pf_stamp_path(&sha), v.to_string()).unwrap();
    let (rule, because) = fx.refused("b1");
    assert_eq!(rule, "preflight.stamp-for-sha");
    assert!(because.contains("steps file"), "{because}");
    // a stamp for another sha
    std::fs::remove_file(fx.pf_stamp_path(&sha)).unwrap();
    fx.pf_stamp(&"2".repeat(40));
    assert_eq!(fx.refused("b1").0, "preflight.stamp-for-sha");
    // a partial stamp of the right sha and steps file
    let v = serde_json::json!({"sha": sha, "digest": fx.pf_digest(), "steps": [{"name": "a", "rc": 0}], "partial": true});
    std::fs::write(fx.pf_stamp_path(&sha), v.to_string()).unwrap();
    let (rule, because) = fx.refused("b1");
    assert_eq!(rule, "preflight.stamp-for-sha");
    assert!(because.contains("partial"), "{because}");
    // the same fixture without the violation passes
    fx.pf_stamp(&sha);
    fx.step("b1", "preflighted");
}

#[test]
fn the_proof_launch_needs_the_chain_stamp_when_the_preflight_is_launched() {
    let mut fx = Fx::new("");
    fx.launched_preflight();
    fx.get_settled("b1");
    fx.step("b1", "preflighting");
    let sha = fx.lock.launches.lock().unwrap()[0].sha.clone();
    fx.pf_log("preflight", GREEN);
    fx.pf_stamp(&sha);
    fx.step("b1", "preflighted");
    std::fs::remove_file(fx.pf_stamp_path(&sha)).unwrap();
    let (rule, _) = fx.refused("b1");
    assert_eq!(rule, "preflight.stamp-for-sha");
    assert_eq!(fx.launches(), 1);
}

#[test]
fn a_preflight_run_on_a_proving_host_is_refused_with_the_hold_exit_code() {
    let o = Outcome::Refused { rule: "host.refuse-proof-running".into(), because: "x".into(), decision: 1, clears_by_itself: true };
    assert_eq!(o.exit_code(), 75);
}

#[test]
fn the_launched_preflight_is_admitted_as_light() {
    let mut fx = Fx::new("");
    fx.launched_preflight();
    fx.get_settled("b1");
    let deny = DenyRole("light", "the host is proving");
    let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &deny, now: &NOW, state_dir: fx.state.clone(), actor: "engine".into(), progress: None };
    assert!(matches!(e.advance("b1").unwrap(), Outcome::Refused { rule, .. } if rule == "host.admission"));
    assert_eq!(fx.launches(), 0);
}

// ---- pipelining

impl Fx {
    fn lane3(&self) {
        g(&self.repo, &["switch", "-q", "-c", "lane3", "main"]);
        std::fs::write(self.repo.join("three.txt"), "x\n").unwrap();
        g(&self.repo, &["add", "--", "three.txt"]);
        g(&self.repo, &["commit", "-q", "-m", "feat: lane3"]);
        g(&self.repo, &["switch", "-q", "main"]);
    }

    fn wt(&self, name: &str) -> PathBuf {
        let p = self.root().join(name);
        g(&self.repo, &["worktree", "add", "-q", "--detach", &fwd(&p), "main"]);
        p
    }

    fn open_on(&mut self, id: &str, base: &str, wd: &Path, lanes: &[&str]) -> gatetrain_core::Result<()> {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "engine".into(), progress: None };
        e.new_batch(id, base, wd, &lanes.iter().map(|l| l.to_string()).collect::<Vec<_>>())
    }

    fn restack(&mut self, id: &str) -> Outcome {
        let adm = AllowAll;
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm: &adm, now: &NOW, state_dir: self.state.clone(), actor: "engine".into(), progress: None };
        e.restack(id).unwrap()
    }

    /// b1 on the main checkout, b2 on b1's tip in its own worktree, both preflighted.
    fn two_preflighted(&mut self) -> PathBuf {
        // no regeneration: the second checkout would rewrite a file the first already committed (line-ending noise on Windows)
        self.cfg.regen_command = vec![];
        self.lane3();
        let wd2 = self.wt("wt2");
        self.open("b1");
        for s in ["stacked", "settled", "preflighted"] {
            self.step("b1", s);
        }
        self.open_on("b2", "b1", &wd2, &["lane3"]).unwrap();
        for s in ["stacked", "settled", "preflighted"] {
            self.step("b2", s);
        }
        wd2
    }

    fn prove_b1(&mut self) {
        self.step("b1", "proving");
        self.write_log(GREEN);
        self.step("b1", "proven");
    }
}

#[test]
fn a_batch_may_be_stacked_only_on_a_batch_that_is_preflighted_proving_or_proven() {
    let mut fx = Fx::new("");
    fx.cfg.regen_command = vec![];
    fx.lane3();
    let wd2 = fx.wt("wt2");
    fx.open("b1");
    let e = fx.open_on("b2", "b1", &wd2, &["lane3"]).unwrap_err().to_string();
    assert!(e.contains("pipeline.base-state"), "{e}");
    for s in ["stacked", "settled", "preflighted"] {
        fx.step("b1", s);
    }
    fx.open_on("b2", "b1", &wd2, &["lane3"]).unwrap();
    let tip = g(&fx.repo, &["rev-parse", "batch/b1/s1"]);
    assert_eq!(status_fact(&mut fx, "b2", &["base_sha"]), tip.as_str());
    assert_eq!(status_fact(&mut fx, "b2", &["base_batch"]), "b1");
    // the lane was cut from main, below the base batch's tip: stacking it on that tip still works
    fx.step("b2", "stacked");
    assert!(g(&wd2, &["log", "--format=%s", &format!("{tip}..HEAD")]).contains("feat: lane3"));
}

#[test]
fn a_batch_on_an_unproven_base_does_not_prove_unless_the_policy_allows_it() {
    let mut fx = Fx::new("");
    fx.two_preflighted();
    fx.step("b1", "proving");
    let (rule, because) = fx.refused("b2");
    assert_eq!(rule, "pipeline.base-unproven");
    assert!(because.contains("b1"), "{because}");
    assert_eq!(fx.launches(), 1);
    fx.cfg.prove_ahead = true;
    fx.step("b2", "proving");
    assert_eq!(fx.launches(), 2);
}

#[test]
fn the_next_batch_proves_at_once_after_the_base_is_fast_forwarded() {
    let mut fx = Fx::new("");
    fx.two_preflighted();
    fx.prove_b1();
    fx.step("b1", "merged");
    fx.step("b2", "proving");
    let req = fx.lock.launches.lock().unwrap()[1].clone();
    assert_eq!(req.sha, g(&fx.repo, &["rev-parse", "batch/b2/s1"]));
    fx.write_log(GREEN);
    fx.step("b2", "proven");
    // b2 merges once b1's deploy settled (enable-readiness MAJOR 1: the newer sha is the last one deployed)
    assert_eq!(fx.refused("b2").0, "deploy.another-batch-deploying");
    fx.step("b1", "deploying");
    fx.step("b2", "merged");
    assert_eq!(g(&fx.repo, &["rev-parse", "main"]), g(&fx.repo, &["rev-parse", "batch/b2/s1"]));
}

#[test]
fn a_batch_never_merges_before_its_base_is_on_the_protected_branch() {
    let mut fx = Fx::new("");
    fx.two_preflighted();
    fx.prove_b1();
    fx.step("b2", "proving");
    fx.write_log(GREEN);
    fx.step("b2", "proven");
    // main would fast-forward to b2's tip and carry b1's commits with it: refused
    let (rule, because) = fx.refused("b2");
    assert_eq!(rule, "pipeline.base-not-merged");
    assert!(because.contains("b1"), "{because}");
    assert_ne!(g(&fx.repo, &["rev-parse", "main"]), g(&fx.repo, &["rev-parse", "batch/b2/s1"]));
    fx.step("b1", "merged");
    fx.step("b1", "deploying");
    fx.step("b2", "merged");
}

#[test]
fn when_the_base_tip_moves_the_next_batch_is_restacked_on_the_new_tip() {
    let mut fx = Fx::new("");
    fx.two_preflighted();
    fx.prove_b1();
    // a patch on the base batch moves its tip
    std::fs::write(fx.repo.join("patch.txt"), "p\n").unwrap();
    g(&fx.repo, &["add", "--", "patch.txt"]);
    g(&fx.repo, &["commit", "-q", "-m", "fix: patch"]);
    let new_tip = g(&fx.repo, &["rev-parse", "HEAD"]);
    let (rule, because) = fx.refused("b2");
    assert_eq!(rule, "pipeline.base-moved");
    assert!(because.contains("restack"), "{because}");
    let o = fx.restack("b2");
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "open"), "{o:?}");
    fx.step("b2", "stacked");
    assert_eq!(status_fact(&mut fx, "b2", &["base_sha"]), new_tip.as_str());
    assert_eq!(status_fact(&mut fx, "b2", &["branch"]), "batch/b2/s2");
    assert!(g(&fx.repo, &["log", "--format=%s", &format!("{new_tip}..batch/b2/s2")]).contains("feat: lane3"));
}

#[test]
fn a_proving_batch_is_not_restacked() {
    let mut fx = Fx::new("");
    fx.two_preflighted();
    fx.prove_b1();
    fx.step("b2", "proving");
    let o = fx.restack("b2");
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "pipeline.restack-state"), "{o:?}");
    assert_eq!(fx.state("b2"), "proving");
    // a batch that is not stacked on another is not restacked either
    let o = fx.restack("b1");
    assert!(matches!(&o, Outcome::Idle(_)), "{o:?}");
}

#[test]
fn the_depth_of_the_pipeline_is_limited() {
    let mut fx = Fx::new("");
    fx.two_preflighted();
    let wd3 = fx.wt("wt3");
    let e = fx.open_on("b3", "b2", &wd3, &["lane2"]).unwrap_err().to_string();
    assert!(e.contains("pipeline.max-ahead"), "{e}");
    fx.cfg.max_ahead = 2;
    fx.open_on("b3", "b2", &wd3, &["lane2"]).unwrap();
}

// ============================================================ T9 close (owned set) and the disk floor

use gatetrain_core::batch::Admission;
use gatetrain_core::tidy::close::FakeCloseEnv;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

/// An admission seam with a scripted disk verdict, a counted `tidy:auto` and a fake tidy environment for close.
struct CloseAdm {
    env: FakeCloseEnv,
    /// What `admit_rule` answers, in order; when empty it admits.
    script: RefCell<VecDeque<Option<(String, String)>>>,
    tidies: Cell<usize>,
    can_tidy: bool,
    tidy_fails: bool,
}

impl CloseAdm {
    fn new(env: FakeCloseEnv) -> CloseAdm {
        CloseAdm { env, script: RefCell::new(VecDeque::new()), tidies: Cell::new(0), can_tidy: true, tidy_fails: false }
    }
    fn floor(script: Vec<Option<(&str, &str)>>) -> CloseAdm {
        let a = CloseAdm::new(FakeCloseEnv::default());
        *a.script.borrow_mut() = script.into_iter().map(|s| s.map(|(r, w)| (r.to_string(), w.to_string()))).collect();
        a
    }
}

impl Admission for CloseAdm {
    fn admit(&self, _: &str, _: &str) -> gatetrain_core::Result<Option<String>> {
        Ok(None)
    }
    fn admit_rule(&self, _: &str, _: &str) -> gatetrain_core::Result<Option<(String, String)>> {
        Ok(self.script.borrow_mut().pop_front().flatten())
    }
    fn tidy_auto(&self, _: &str) -> gatetrain_core::Result<Option<String>> {
        if !self.can_tidy {
            return Ok(None);
        }
        if self.tidy_fails {
            return Err(gatetrain_core::Error::Batch("tidy --auto failed".into()));
        }
        self.tidies.set(self.tidies.get() + 1);
        Ok(Some("tidy --auto reclaimed 3 GB".into()))
    }
    fn close_env(&self) -> Option<&dyn gatetrain_core::tidy::close::CloseEnv> {
        Some(&self.env)
    }
}

impl Fx {
    fn adv_with(&mut self, id: &str, adm: &dyn Admission) -> Outcome {
        let mut e = Engine { ledger: &mut self.ledger, cfg: &self.cfg, lock: &self.lock, adm, now: &NOW, state_dir: self.state.clone(), actor: "engine".into(), progress: None };
        e.advance(id).unwrap()
    }

    fn rows(&self, rule: &str) -> Vec<(String, String)> {
        let mut st = self.ledger.conn.prepare("SELECT kind,outcome FROM decisions WHERE rule=? ORDER BY seq").unwrap();
        st.query_map([rule], |r| Ok((r.get(0)?, r.get(1)?))).unwrap().map(|x| x.unwrap()).collect()
    }
}

const TGT: &str = "/build/tgt-b1";
const TMP: &str = "/tmp/gt-b1-step";
const BR: &str = "line/l1";
const OWN: &str = "batch/b1/s1";

/// A batch that is `live`, with the tidy rule on and the run's target folder recorded; its run named a container and a temp folder.
fn live_fx() -> Fx {
    let mut fx = Fx::new("[deploy.staging]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"true\"]\nhealth = [\"sh\", \"-c\", \"true\"]\ntier = \"staging\"\n");
    fx.cfg.close_tidy = true;
    fx.cfg.close_roots = vec!["/build".into(), "/tmp".into(), "/wt".into()];
    fx.cfg.chain_target_dirs = vec![SHARED.into()]; // the shared chain target sits under a root: only the shared-target rule keeps it
    // lanes the way the adopter names them
    g(&fx.repo, &["branch", "line/l1", "lane1"]);
    g(&fx.repo, &["branch", "line/l2", "lane2"]);
    fx.open_on("b1", "main", &fx.repo.clone(), &["line/l1", "line/l2"]).unwrap();
    for s in ["stacked", "settled", "preflighted", "proving"] {
        fx.step("b1", s);
    }
    fx.write_log(GREEN);
    for s in ["proven", "merged", "deploying", "live"] {
        fx.step("b1", s);
    }
    std::fs::create_dir_all(fx.state.join("targets")).unwrap();
    std::fs::write(fx.state.join("targets").join("b1-p1.txt"), format!("{TGT}\n")).unwrap();
    std::fs::create_dir_all(fx.state.join("containers")).unwrap();
    std::fs::write(fx.state.join("containers").join("b1-p1.txt"), "mr-gate-b1-pg\n").unwrap();
    std::fs::create_dir_all(fx.state.join("tmpdirs")).unwrap();
    std::fs::write(fx.state.join("tmpdirs").join("b1-p1.txt"), format!("{TMP}\n")).unwrap();
    fx
}

/// The full owned set of `live_fx` present in a fake environment: a lane worktree on lane1, the target, a container, a temp folder,
/// the archived `line/batch-b1` branch.
fn full_env() -> FakeCloseEnv {
    let mut e = FakeCloseEnv::with(&[("worktree", "/wt/lane1"), ("target", TGT), ("container", "mr-gate-b1-pg"), ("tmp", TMP), ("branch", BR), ("branch", OWN)]);
    e.wt_branch.insert("/wt/lane1".into(), "line/l1".into());
    e.tags.insert(BR.into());
    e.tags.insert(OWN.into());
    e
}

fn closes_with(code: &str, tweak: impl FnOnce(&mut FakeCloseEnv), untouched: &str) {
    let mut fx = live_fx();
    let mut env = full_env();
    tweak(&mut env);
    let adm = CloseAdm::new(env);
    let o = fx.adv_with("b1", &adm);
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == code), "{code}: {o:?}");
    assert_eq!(o.exit_code(), 3);
    assert_eq!(fx.state("b1"), "live", "{code}: the batch stays live");
    assert!(!fx.rows(code).is_empty(), "{code}: a decision row");
    assert!(!adm.env.disposed.borrow().iter().any(|(_, k)| k == untouched), "{code}: {untouched} must not be touched: {:?}", adm.env.disposed.borrow());
    // the next attempt is the same refusal, and the row is not written again
    let n = fx.rows(code).len();
    fx.adv_with("b1", &adm);
    assert_eq!(fx.rows(code).len(), n, "{code}: a retry does not repeat its row");
}

#[test]
fn a_clean_owned_set_is_tidied_and_the_batch_closes_and_check_shows_nothing() {
    let mut fx = live_fx();
    let adm = CloseAdm::new(full_env());
    let o = fx.adv_with("b1", &adm);
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "closed"), "{o:?}");
    assert_eq!(fx.state("b1"), "closed");
    assert!(adm.env.present.borrow().is_empty(), "check shows nothing: {:?}", adm.env.present.borrow());
    let kinds: Vec<String> = adm.env.disposed.borrow().iter().map(|(k, _)| k.clone()).collect();
    for k in ["worktree", "target", "container", "tmp", "branch"] {
        assert!(kinds.iter().any(|x| x == k), "{k} in {kinds:?}");
    }
    assert_eq!(fx.rows("tidy.moved").len(), 6);
}

#[test]
fn only_recorded_items_are_planned_never_a_stamp_or_an_out_folder() {
    let mut fx = live_fx();
    let env = full_env();
    env.present.borrow_mut().insert(("target".into(), "/build/other-batch-tgt".into()));
    env.present.borrow_mut().insert(("tmp".into(), "/out/b1/stamp-x.json".into()));
    let adm = CloseAdm::new(env);
    fx.adv_with("b1", &adm);
    let touched: Vec<String> = adm.env.disposed.borrow().iter().map(|(_, k)| k.clone()).collect();
    assert!(!touched.iter().any(|k| k.contains("other-batch") || k.contains("stamp")), "{touched:?}");
}

#[test]
fn a_protected_item_is_refused_and_nothing_inside_or_linking_into_it_goes() {
    closes_with("tidy.protected", |e| e.protect.push("/build".into()), TGT);
}

#[test]
fn a_reference_stack_container_is_refused() {
    closes_with("tidy.reference-stack", |e| e.reference_prefixes.push("mr-gate-".into()), "mr-gate-b1-pg");
}

#[test]
fn a_worktree_with_uncommitted_work_is_refused() {
    closes_with("tidy.uncommitted", |e| { e.dirty.insert("/wt/lane1".into()); }, "/wt/lane1");
}

#[test]
fn a_worktree_whose_lane_is_not_proven_on_main_is_a_live_lane() {
    closes_with("tidy.live-lane", |e| { e.unmerged.insert("line/l1".into()); }, "/wt/lane1");
}

#[test]
fn the_main_checkout_is_never_tidied_even_when_it_is_the_batch_workdir() {
    let mut fx = live_fx();
    fx.cfg.close_own_workdir = true;
    let wd = fx.repo.to_string_lossy().to_string(); // the spelling the engine recorded
    let mut env = full_env();
    env.present.borrow_mut().insert(("worktree".into(), wd.clone()));
    env.wt_branch.insert(wd.clone(), OWN.into());
    env.main_checkout = Some(wd.clone());
    let (o, adm) = run_closed(&mut fx, env);
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "tidy.main-checkout"), "{o:?}");
    assert!(!touched(&adm, &wd));
}

#[test]
fn a_batch_checkout_on_a_branch_the_batch_does_not_own_is_not_owned() {
    let mut fx = live_fx();
    fx.cfg.close_own_workdir = true;
    let wd = fx.repo.to_string_lossy().to_string(); // the spelling the engine recorded
    fx.cfg.close_roots.push(above(&wd));
    let mut env = full_env();
    env.present.borrow_mut().insert(("worktree".into(), wd.clone()));
    env.wt_branch.insert(wd.clone(), "someone-elses".into());
    let (o, adm) = run_closed(&mut fx, env);
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "tidy.not-owned"), "{o:?}");
    assert!(!touched(&adm, &wd));
}

#[test]
fn an_item_in_use_is_refused() {
    closes_with("tidy.in-use", |e| { e.in_use.insert(TGT.into(), "the host lock is held by run b9-p1".into()); }, TGT);
    closes_with("tidy.in-use", |e| { e.in_use.insert("mr-gate-b1-pg".into(), "container mr-gate-b1-pg is running".into()); }, "mr-gate-b1-pg");
}

#[test]
fn a_branch_without_a_local_archive_tag_is_refused() {
    closes_with("tidy.no-archive-tag", |e| { e.tags.clear(); }, BR);
}

#[test]
fn an_unreadable_item_is_refused_not_guessed() {
    closes_with("tidy.unreadable", |e| { e.unreadable.insert(TGT.into()); }, TGT);
}

#[test]
fn a_failed_move_leaves_the_batch_live_with_check_not_clean() {
    closes_with("tidy.check-not-clean", |e| { e.fail_dispose.insert(TMP.into()); }, "never-matches");
    // and it is tried again at the next advance: once the move works the batch closes
    let mut fx = live_fx();
    let mut env = full_env();
    env.fail_dispose.insert(TMP.into());
    let adm = CloseAdm::new(env);
    assert!(matches!(fx.adv_with("b1", &adm), Outcome::Refused { rule, .. } if rule == "tidy.check-not-clean"));
    assert_eq!(fx.state("b1"), "live");
    let healed = CloseAdm::new(FakeCloseEnv::with(&[("tmp", TMP)]));
    assert!(matches!(fx.adv_with("b1", &healed), Outcome::Advanced { to, .. } if to == "closed"));
}

#[test]
fn a_refusal_never_blocks_the_next_batch_and_a_person_may_close_with_residue() {
    let mut fx = live_fx();
    let adm = CloseAdm::new({
        let mut e = full_env();
        e.dirty.insert("/wt/lane1".into());
        e
    });
    assert!(matches!(fx.adv_with("b1", &adm), Outcome::Refused { .. }));
    // another batch opens and advances while b1 is still live
    let repo = fx.repo.clone();
    fx.open_on("b2", "main", &repo, &[]).unwrap();
    assert!(matches!(fx.adv_with("b2", &adm), Outcome::Advanced { to, .. } if to == "stacked"));
    // a reason is required; with one, the batch closes and the residue is on the record
    let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), progress: None };
    assert!(e.close_with_residue("b1", " ").is_err());
    let o = e.close_with_residue("b1", "the lane worktree is kept on purpose").unwrap();
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "closed"), "{o:?}");
    let rows = fx.rows("close.with-residue");
    assert_eq!(rows, vec![("override".to_string(), "recorded".to_string())]);
    assert_eq!(fx.state("b1"), "closed");
}

#[test]
fn without_the_close_tidy_setting_close_is_immediate_as_before() {
    let mut fx = live_fx();
    fx.cfg.close_tidy = false;
    fx.step("b1", "closed");
}

// ---- the disk floor (T3, T4)

#[test]
fn below_the_floor_with_policy_the_engine_tidies_once_measures_again_and_admits() {
    let mut fx = Fx::new("");
    fx.cfg.tidy_auto = true;
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    let adm = CloseAdm::floor(vec![Some(("host.refuse-disk", "free 5 GB - predicted 8 GB < min_free_gb 10")), None]);
    let o = fx.adv_with("b1", &adm);
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "preflighted"), "{o:?}");
    assert_eq!(adm.tidies.get(), 1);
    assert_eq!(fx.rows("tidy.auto-disk"), vec![("tidy".to_string(), "recovered".to_string())]);
    assert!(fx.rows("host.refuse-disk").is_empty());
}

#[test]
fn still_below_after_the_tidy_is_one_tidy_row_and_one_refusal_row() {
    let mut fx = Fx::new("");
    fx.cfg.tidy_auto = true;
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    let adm = CloseAdm::floor(vec![Some(("host.refuse-disk", "free 5 GB < floor")), Some(("host.refuse-disk", "free 6 GB < floor"))]);
    let o = fx.adv_with("b1", &adm);
    assert!(matches!(&o, Outcome::Refused { rule, because, .. } if rule == "host.refuse-disk" && because.contains("tidy:auto")), "{o:?}");
    assert_eq!(o.exit_code(), 77);
    assert_eq!(adm.tidies.get(), 1, "tidied once, not in a loop");
    assert_eq!(fx.rows("tidy.auto-disk"), vec![("tidy".to_string(), "still-below".to_string())]);
    assert_eq!(fx.rows("host.refuse-disk").len(), 1);
    assert_eq!(fx.state("b1"), "settled");
}

#[test]
fn without_the_policy_the_engine_does_not_tidy_it_only_refuses() {
    let mut fx = Fx::new("");
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    let adm = CloseAdm::floor(vec![Some(("host.refuse-disk", "free 5 GB < floor"))]);
    assert!(matches!(fx.adv_with("b1", &adm), Outcome::Refused { rule, .. } if rule == "host.refuse-disk"));
    assert_eq!(adm.tidies.get(), 0);
    assert!(fx.rows("tidy.auto-disk").is_empty());
}

#[test]
fn another_refusal_than_disk_never_triggers_a_tidy() {
    let mut fx = Fx::new("");
    fx.cfg.tidy_auto = true;
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    let adm = CloseAdm::floor(vec![Some(("host.refuse-memory", "no"))]);
    assert!(matches!(fx.adv_with("b1", &adm), Outcome::Refused { rule, .. } if rule == "host.refuse-memory"));
    assert_eq!(adm.tidies.get(), 0);
}

#[test]
fn still_below_on_two_batches_in_a_row_is_a_founder_escalation() {
    let mut fx = Fx::new("");
    fx.cfg.tidy_auto = true;
    for id in ["b1", "b2"] {
        fx.open(id);
        fx.step(id, "stacked");
        fx.step(id, "settled");
        let adm = CloseAdm::floor(vec![Some(("host.refuse-disk", "low")), Some(("host.refuse-disk", "still low"))]);
        assert!(matches!(fx.adv_with(id, &adm), Outcome::Refused { .. }));
        let esc = fx.ledger.conn.query_row("SELECT COUNT(*) FROM decisions WHERE rule='host.disk-escalation' AND batch=?", [id], |r| r.get::<_, i64>(0)).unwrap();
        assert_eq!(esc, if id == "b2" { 1 } else { 0 }, "{id}");
    }
}

// ---- the legacy stamp bridge

#[test]
fn the_legacy_stamp_command_runs_after_a_green_launched_preflight_with_the_sha_and_the_results_path() {
    let mut fx = Fx::new("");
    fx.launched_preflight();
    let out = fx.root().join("legacy.out");
    fx.cfg.legacy_stamp_command = vec!["sh".into(), "-c".into(), format!("echo \"$0 $1\" > '{}'", fwd(&out)), "{sha}".into(), "{results}".into()];
    fx.get_settled("b1");
    fx.step("b1", "preflighting");
    let sha = fx.lock.launches.lock().unwrap()[0].sha.clone();
    fx.pf_log("preflight", GREEN);
    fx.pf_stamp(&sha);
    fx.step("b1", "preflighted");
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.starts_with(&sha) && text.trim().ends_with(&format!("stamp-{sha}.json")), "{text}");
}

#[test]
fn a_failing_legacy_stamp_command_keeps_the_batch_preflighting_and_is_tried_again() {
    let mut fx = Fx::new("");
    fx.launched_preflight();
    fx.cfg.legacy_stamp_command = vec!["sh".into(), "-c".into(), "exit 4".into()];
    fx.get_settled("b1");
    fx.step("b1", "preflighting");
    let sha = fx.lock.launches.lock().unwrap()[0].sha.clone();
    fx.pf_log("preflight", GREEN);
    fx.pf_stamp(&sha);
    let (rule, because) = fx.refused("b1");
    assert_eq!(rule, "preflight.legacy-stamp");
    assert!(because.contains("exited 4"), "{because}");
    assert_eq!(fx.state("b1"), "preflighting");
    fx.cfg.legacy_stamp_command = vec!["sh".into(), "-c".into(), "exit 0".into()];
    fx.step("b1", "preflighted");
}

// ============================================================ review fixes (B1-B4, M1-M5): every one red first

const SHARED: &str = "/build/shared-chain";

fn run_closed(fx: &mut Fx, env: FakeCloseEnv) -> (Outcome, CloseAdm) {
    let adm = CloseAdm::new(env);
    let o = fx.adv_with("b1", &adm);
    (o, adm)
}

fn touched(adm: &CloseAdm, key: &str) -> bool {
    adm.env.disposed.borrow().iter().any(|(_, k)| k == key)
}

#[test]
fn b2_a_branch_is_never_deleted_while_a_worktree_is_on_it() {
    // the lane worktree is refused as dirty: the branch under it must stay, or git's own guard is bypassed
    closes_with("tidy.uncommitted", |e| { e.dirty.insert("/wt/lane1".into()); }, BR);
}

#[test]
fn b3_a_path_inside_the_main_checkout_is_refused() {
    closes_with("tidy.main-checkout", |e| e.main_checkout = Some("/build".into()), TGT);
}

#[test]
fn b3_a_folder_that_contains_a_registered_worktree_is_refused() {
    closes_with(
        "tidy.not-owned",
        |e| {
            e.present.borrow_mut().insert(("worktree".into(), "/tmp/gt-b1-step/inner".into()));
            e.wt_branch.insert("/tmp/gt-b1-step/inner".into(), "someone-elses".into());
        },
        TMP,
    );
}

#[test]
fn b3_roots_are_mandatory_when_close_tidy_is_on() {
    assert!(BatchConfig::parse("[close]\ntidy = true\n").is_err());
    assert!(BatchConfig::parse("[close]\ntidy = true\nroots = []\n").is_err());
    assert!(BatchConfig::parse("[close]\ntidy = true\nroots = [\"/build\"]\n").is_ok());
    assert!(BatchConfig::parse("[close]\ntidy = false\n").is_ok());
}

#[test]
fn b3_a_path_outside_every_root_is_not_owned() {
    let mut fx = live_fx();
    fx.cfg.close_roots = vec!["/elsewhere".into()];
    let (o, adm) = run_closed(&mut fx, full_env());
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "tidy.not-owned"), "{o:?}");
    assert!(!touched(&adm, TGT) && !touched(&adm, TMP), "{:?}", adm.env.disposed.borrow());
    assert_eq!(fx.state("b1"), "live");
}

#[test]
fn b4a_the_shared_chain_target_is_never_moved_only_the_runs_own_recorded_folder() {
    let mut fx = live_fx();
    let env = full_env();
    env.present.borrow_mut().insert(("target".into(), SHARED.into()));
    let (o, adm) = run_closed(&mut fx, env);
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "closed"), "{o:?}");
    assert!(!touched(&adm, SHARED), "the shared [chain] target_dirs folder is not the batch's: {:?}", adm.env.disposed.borrow());
    assert!(touched(&adm, TGT), "the run's own recorded target folder is");
}

#[test]
fn b4b_a_line_batch_branch_is_not_derived_by_name_and_the_recorded_stack_branch_is_owned() {
    let mut fx = live_fx();
    let env = full_env();
    env.present.borrow_mut().insert(("branch".into(), "line/batch-b1".into()));
    let mut env = env;
    env.tags.insert("line/batch-b1".into());
    let (o, adm) = run_closed(&mut fx, env);
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "closed"), "{o:?}");
    assert!(!touched(&adm, "line/batch-b1"), "never derived by name");
    assert!(touched(&adm, OWN), "the branch the engine recorded (batch/b1/s1) is the batch's");
}

#[test]
fn b4c_the_batch_checkout_is_left_alone_unless_own_workdir_is_set() {
    let mut fx = live_fx();
    let wd = fwd(&fx.repo);
    fx.cfg.close_roots.push(above(&wd)); // inside a root, so only the own_workdir rule keeps it out
    let mut env = full_env();
    env.present.borrow_mut().insert(("worktree".into(), wd.clone()));
    env.wt_branch.insert(wd.clone(), OWN.into());
    let (_, adm) = run_closed(&mut fx, env);
    assert!(!touched(&adm, &wd), "own_workdir = false: {:?}", adm.env.disposed.borrow());
    // with the setting the checkout is part of the owned set (and its branch may then go)
    let mut fx = live_fx();
    fx.cfg.close_own_workdir = true;
    let wd = fwd(&fx.repo);
    fx.cfg.close_roots.push(above(&wd));
    let mut env = full_env();
    env.present.borrow_mut().insert(("worktree".into(), wd.clone()));
    env.wt_branch.insert(wd.clone(), OWN.into());
    let (o, adm) = run_closed(&mut fx, env);
    assert!(touched(&adm, &wd), "{:?}", adm.env.disposed.borrow());
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "closed"), "{o:?}");
}

#[test]
fn m1_a_reference_stack_container_is_protected_even_with_no_prefix_configured() {
    let mut fx = live_fx();
    std::fs::write(fx.state.join("containers").join("b1-p1.txt"), "mr-ref-keep\n").unwrap();
    let env = FakeCloseEnv::with(&[("container", "mr-ref-keep")]);
    let (o, adm) = run_closed(&mut fx, env);
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "tidy.reference-stack"), "{o:?}");
    assert!(!touched(&adm, "mr-ref-keep"));
}

#[test]
fn m2_the_checks_are_made_again_at_the_moment_of_action() {
    // clean at plan time, dirty a moment later: the second look refuses
    closes_with("tidy.uncommitted", |e| { e.dirty_on_call.insert("/wt/lane1".into(), 2); }, "/wt/lane1");
}

#[test]
fn m3_a_locked_worktree_is_refused() {
    closes_with("tidy.in-use", |e| { e.locked.insert("/wt/lane1".into()); }, "/wt/lane1");
}

// ---- guards that survived mutation: one dedicated test each

#[test]
fn an_unknown_dirty_state_is_unreadable_not_clean() {
    closes_with("tidy.unreadable", |e| { e.dirty_unknown.insert("/wt/lane1".into()); }, "/wt/lane1");
}

#[test]
fn a_branch_that_is_not_proven_on_main_is_a_live_lane() {
    closes_with("tidy.live-lane", |e| { e.unmerged.insert(OWN.into()); }, OWN);
}

#[test]
fn a_branch_whose_merge_state_is_unknown_is_unreadable() {
    closes_with("tidy.unreadable", |e| { e.merge_unknown.insert(OWN.into()); }, OWN);
}

#[test]
fn an_item_that_is_still_there_after_a_successful_dispose_is_check_not_clean() {
    closes_with("tidy.check-not-clean", |e| { e.ghost.insert(TMP.into()); }, "never-matches");
}

#[test]
fn a_failed_dispose_is_residue_and_a_check_not_clean_row() {
    let mut fx = live_fx();
    let mut env = full_env();
    env.fail_dispose.insert(TGT.into());
    let (o, _) = run_closed(&mut fx, env);
    assert!(matches!(&o, Outcome::Refused { rule, because, .. } if rule == "tidy.check-not-clean" && because.contains("left over")), "{o:?}");
    assert!(!fx.rows("tidy.check-not-clean").is_empty());
    assert_eq!(fx.state("b1"), "live");
}

// ---- tidy:auto, once per batch (M4), and an escalation row only once

#[test]
fn m4_tidy_auto_runs_at_most_once_per_batch() {
    let mut fx = Fx::new("");
    fx.cfg.tidy_auto = true;
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    let adm = CloseAdm::floor(vec![Some(("host.refuse-disk", "low")), Some(("host.refuse-disk", "still low")), Some(("host.refuse-disk", "low again"))]);
    assert!(matches!(fx.adv_with("b1", &adm), Outcome::Refused { .. }));
    assert!(matches!(fx.adv_with("b1", &adm), Outcome::Refused { rule, .. } if rule == "host.refuse-disk"));
    assert_eq!(adm.tidies.get(), 1, "the second attempt of the same batch does not tidy again");
    assert_eq!(fx.rows("tidy.auto-disk").len(), 1);
}

#[test]
fn m4_a_failing_tidy_is_a_refusal_row_not_an_engine_error() {
    let mut fx = Fx::new("");
    fx.cfg.tidy_auto = true;
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    let mut adm = CloseAdm::floor(vec![Some(("host.refuse-disk", "low"))]);
    adm.tidy_fails = true;
    let o = fx.adv_with("b1", &adm);
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "host.refuse-disk"), "{o:?}");
    assert_eq!(fx.rows("tidy.auto-disk"), vec![("tidy".to_string(), "failed".to_string())]);
    assert_eq!(fx.state("b1"), "settled");
}

#[test]
fn the_escalation_row_is_written_once_per_batch() {
    let mut fx = Fx::new("");
    fx.cfg.tidy_auto = true;
    for id in ["b1", "b2"] {
        fx.open(id);
        fx.step(id, "stacked");
        fx.step(id, "settled");
    }
    let mk = || CloseAdm::floor(vec![Some(("host.refuse-disk", "low")), Some(("host.refuse-disk", "still low")), Some(("host.refuse-disk", "low")), Some(("host.refuse-disk", "still low"))]);
    fx.adv_with("b1", &mk());
    let adm = mk();
    fx.adv_with("b2", &adm);
    fx.adv_with("b2", &adm);
    let esc = fx.ledger.conn.query_row("SELECT COUNT(*) FROM decisions WHERE rule='host.disk-escalation' AND batch='b2'", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(esc, 1);
}

/// The folder that holds `p`: a `[close]` root owns what is strictly below it, never itself.
fn above(p: &str) -> String {
    std::path::Path::new(p).parent().unwrap().to_string_lossy().replace('\\', "/")
}

// ============================================================ review round 2 (BL2, MA1-MA3, minors 2, 4, 6): red first

#[test]
fn minor6_a_record_that_names_a_root_itself_never_owns_the_whole_root() {
    let mut fx = live_fx();
    fx.cfg.chain_target_dirs = vec![]; // /build holds the shared target: without this the shared-target rule would refuse it first
    std::fs::write(fx.state.join("targets").join("b1-p1.txt"), "/build\n").unwrap();
    let env = FakeCloseEnv::with(&[("target", "/build")]);
    let (o, adm) = run_closed(&mut fx, env);
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "tidy.not-owned"), "{o:?}");
    assert!(!touched(&adm, "/build"), "{:?}", adm.env.disposed.borrow());
}

#[test]
fn minor6_a_file_system_root_owns_nothing() {
    let mut fx = live_fx();
    fx.cfg.close_roots = vec!["/".into()];
    let (o, adm) = run_closed(&mut fx, full_env());
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "tidy.not-owned"), "{o:?}");
    assert!(!touched(&adm, TGT) && !touched(&adm, TMP), "{:?}", adm.env.disposed.borrow());
    assert!(BatchConfig::parse("[close]\ntidy = true\nroots = [\"/\"]\n").is_err(), "a file-system root is refused when the config is read");
    assert!(BatchConfig::parse("[close]\ntidy = true\nroots = [\"C:/\"]\n").is_err());
    assert!(BatchConfig::parse("[close]\ntidy = true\nroots = [\"/build\", \"D:\\\\\"]\n").is_err());
}

#[test]
fn minor6_a_root_that_holds_the_main_checkout_owns_nothing() {
    let mut fx = live_fx();
    let mut env = full_env();
    env.main_checkout = Some("/build/main".into()); // main sits inside the /build root: that root can own nothing
    let (o, adm) = run_closed(&mut fx, env);
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "tidy.not-owned"), "{o:?}");
    assert!(!touched(&adm, TGT), "{:?}", adm.env.disposed.borrow());
    assert!(touched(&adm, TMP), "a path under another, sound root still goes");
}

fn engine<'a>(fx: &'a mut Fx, adm: &'a CloseAdm) -> Engine<'a> {
    Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), progress: None }
}

#[test]
fn ma1_close_with_residue_tidies_nothing_when_close_tidy_is_off() {
    let mut fx = live_fx();
    fx.cfg.close_tidy = false;
    let adm = CloseAdm::new(full_env());
    let o = engine(&mut fx, &adm).close_with_residue("b1", "closing by hand").unwrap();
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "closed"), "{o:?}");
    assert!(adm.env.disposed.borrow().is_empty(), "[close] tidy = false and close --with-residue removed {:?}", adm.env.disposed.borrow());
    assert!(fx.rows("tidy.moved").is_empty());
}

#[test]
fn ma1_close_with_residue_writes_a_row_for_everything_it_removed_and_refused() {
    let mut fx = live_fx();
    let mut env = full_env();
    env.dirty.insert("/wt/lane1".into()); // the lane worktree is refused, so its branch stays with it
    let adm = CloseAdm::new(env);
    let o = engine(&mut fx, &adm).close_with_residue("b1", "the lane worktree is kept on purpose").unwrap();
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "closed"), "{o:?}");
    let gone: Vec<String> = adm.env.disposed.borrow().iter().map(|(_, k)| k.clone()).collect();
    assert_eq!(gone.len(), 4, "{gone:?}");
    assert_eq!(fx.rows("tidy.moved").len(), gone.len(), "one tidy.moved row per removed item");
    assert_eq!(fx.rows("tidy.uncommitted"), vec![("refusal".to_string(), "refused".to_string())]);
    assert_eq!(fx.rows("tidy.in-use"), vec![("refusal".to_string(), "refused".to_string())]);
}

#[test]
fn ma2_a_recorded_target_that_is_contains_or_sits_inside_a_shared_chain_target_is_not_owned() {
    let mut fx = live_fx();
    fx.cfg.chain_target_dirs = vec!["/build/chain/shared".into()];
    let recorded = ["/build/chain", "/build/chain/shared", "/build/chain/shared/debug"];
    std::fs::write(fx.state.join("targets").join("b1-p1.txt"), format!("{TGT}
{}
", recorded.join("
"))).unwrap();
    let env = full_env();
    for r in recorded {
        env.present.borrow_mut().insert(("target".into(), r.into()));
    }
    let (o, adm) = run_closed(&mut fx, env);
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "tidy.not-owned"), "{o:?}");
    for r in recorded {
        assert!(!touched(&adm, r), "{r} overlaps the shared [chain] target_dirs: {:?}", adm.env.disposed.borrow());
    }
    assert_eq!(fx.rows("tidy.not-owned").len(), 3);
    assert!(touched(&adm, TGT), "the run's own target still goes");
}

#[test]
fn ma3_a_floor_that_cannot_be_judged_never_tidies() {
    let mut fx = Fx::new("");
    fx.cfg.tidy_auto = true;
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    let adm = CloseAdm::floor(vec![Some(("host.disk-unknown", "host h is reached over ssh: its free space is unknown"))]);
    let o = fx.adv_with("b1", &adm);
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "host.disk-unknown"), "{o:?}");
    assert_eq!(o.exit_code(), 77, "still a disk refusal of admission");
    assert_eq!(adm.tidies.get(), 0, "nothing says the disk is low");
    assert!(fx.rows("tidy.auto-disk").is_empty());
}

#[test]
fn ma3_unknown_after_a_tidy_is_not_still_below_and_never_escalates() {
    let mut fx = Fx::new("");
    fx.cfg.tidy_auto = true;
    for id in ["b1", "b2"] {
        fx.open(id);
        fx.step(id, "stacked");
        fx.step(id, "settled");
        let adm = CloseAdm::floor(vec![Some(("host.refuse-disk", "free 5 GB < floor")), Some(("host.disk-unknown", "free space could not be read"))]);
        let o = fx.adv_with(id, &adm);
        assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "host.disk-unknown"), "{id}: {o:?}");
    }
    let outcomes: Vec<String> = fx.rows("tidy.auto-disk").into_iter().map(|(_, o)| o).collect();
    assert!(!outcomes.iter().any(|o| o == "still-below"), "a disk that could not be measured is not measured below its floor: {outcomes:?}");
    assert!(fx.rows("host.disk-escalation").is_empty(), "no founder escalation without a measured shortfall");
}

#[test]
fn minor4_a_branch_that_moves_between_the_plan_and_the_action_is_kept() {
    // judged at one sha, at another a moment later: the item is refused, never deleted at the new sha
    closes_with("tidy.unreadable", |e| { e.sha_moves.insert(OWN.into(), "sha-NEW-COMMIT".into()); }, OWN);
}

#[test]
fn minor2_an_escalation_already_on_record_for_the_batch_is_not_written_again() {
    let mut fx = Fx::new("");
    fx.cfg.tidy_auto = true;
    for id in ["b1", "b2"] {
        fx.open(id);
        fx.step(id, "stacked");
        fx.step(id, "settled");
    }
    let mk = || CloseAdm::floor(vec![Some(("host.refuse-disk", "low")), Some(("host.refuse-disk", "still low"))]);
    fx.adv_with("b1", &mk());
    // b2 already carries an escalation (written by a person, or before its tidy row): the engine does not repeat it
    let e = gatetrain_core::declog::NewEntry { at: "2026-11-17T00:00:00Z".into(), actor: "human:me".into(), kind: "escalation".into(), batch: Some("b2".into()), rule: Some("host.disk-escalation".into()), outcome: "founder".into(), reason: "raised by hand".into(), ..Default::default() };
    gatetrain_core::declog::append(&mut fx.ledger, &e).unwrap();
    fx.adv_with("b2", &mk());
    assert_eq!(fx.rows("tidy.auto-disk").len(), 2, "both batches tidied once");
    assert_eq!(fx.rows("host.disk-escalation").len(), 1, "one escalation for b2, not two");
}

#[test]
fn ma7_a_prefix_root_needs_a_folder_and_a_non_empty_name_prefix() {
    assert!(BatchConfig::parse("[close]\ntidy = true\nroots = [\"prefix:/repos/app-wt-\"]\n").is_ok());
    assert!(BatchConfig::parse("[close]\ntidy = true\nroots = [\"prefix:/repos/\"]\n").is_err(), "an empty name prefix owns every sibling");
    assert!(BatchConfig::parse("[close]\ntidy = true\nroots = [\"prefix:/app-wt-\"]\n").is_err(), "a prefix in a file-system root");
    assert!(BatchConfig::parse("[close]\ntidy = true\nroots = [\"prefix:app-wt-\"]\n").is_err(), "no folder");
}

#[test]
fn minor1_a_folder_that_sits_elsewhere_at_the_action_than_at_the_plan_is_kept() {
    closes_with("tidy.unreadable", |e| { e.located_moves.insert(TGT.into(), "/build/elsewhere".into()); }, TGT);
}

// ============================================================ the failure-model rebuild (S1..S8)

use gatetrain_core::batch::crash_seam;

#[test]
fn s1_a_held_batch_with_a_pending_t7_intent_merges_nothing_and_its_resume_re_runs_the_guards() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    fx.write_log(GREEN);
    fx.step("b1", "proven");
    let main0 = g(&fx.repo, &["rev-parse", "main"]);
    // the intent is written, the merge never ran
    crash_seam::set(Some("before:T7".into()));
    assert!(fx.adv("b1").is_err());
    crash_seam::set(None);
    let adm = AllowAll;
    let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), progress: None };
    e.hold("b1", "looking").unwrap();
    for _ in 0..2 {
        assert!(matches!(fx.adv("b1").unwrap(), Outcome::Refused { rule, .. } if rule == "batch.held"));
    }
    assert_eq!(g(&fx.repo, &["rev-parse", "main"]), main0, "nothing merged while held");
    assert_eq!(fx.state("b1"), "held");
    // released, but the batch branch moved meanwhile: the resume runs the T7 guards again and refuses instead of merging
    let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), progress: None };
    e.release("b1").unwrap();
    std::fs::write(fx.repo.join("late.txt"), "late\n").unwrap();
    g(&fx.repo, &["add", "--", "late.txt"]);
    g(&fx.repo, &["commit", "-q", "-m", "late"]);
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Refused { rule, .. } if rule == "merge.tested-equals-merged"));
    assert_eq!(g(&fx.repo, &["rev-parse", "main"]), main0, "the resume did not merge an untested tip");
}

#[test]
fn s1_an_abandoned_batch_with_a_pending_intent_is_never_resumed() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    fx.write_log(GREEN);
    fx.step("b1", "proven");
    let main0 = g(&fx.repo, &["rev-parse", "main"]);
    crash_seam::set(Some("before:T7".into()));
    assert!(fx.adv("b1").is_err());
    crash_seam::set(None);
    let adm = AllowAll;
    let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), progress: None };
    e.abandon("b1", "given up").unwrap();
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Idle(_)));
    assert_eq!(fx.state("b1"), "abandoned");
    assert_eq!(g(&fx.repo, &["rev-parse", "main"]), main0);
}

fn outcome_of(fx: &Fx, tr: &str) -> String {
    fx.ledger.conn.query_row("SELECT json_extract(detail,'$.outcome') FROM batch_events WHERE transition=? AND phase='failed' ORDER BY seq DESC LIMIT 1", [tr], |r| r.get(0)).unwrap()
}

#[test]
fn s3_a_failed_attempt_says_whether_it_left_the_world_changed() {
    // a stack command that fails after the stack branch was created: the branch is left (dirty)
    let mut fx = Fx::new("[stack]\ncommand = [\"sh\", \"-c\", \"exit 3\"]\n");
    fx.open("b1");
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Failed(_)));
    assert_eq!(outcome_of(&fx, "T1"), "failed-dirty");
    // a blocking pre-flight that fails writes no stamp and changes nothing (clean)
    let mut fx = Fx::new("");
    fx.cfg.preflight_command = vec!["sh".into(), "-c".into(), "exit 4".into()];
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Failed(_)));
    assert_eq!(outcome_of(&fx, "T3"), "failed-clean");
}

#[test]
fn s3_a_launch_whose_log_was_opened_but_whose_run_no_lock_names_is_interrupted_not_launched_again() {
    let mut fx = Fx::new("");
    fx.open("b1");
    for s in ["stacked", "settled", "preflighted"] {
        fx.step("b1", s);
    }
    crash_seam::set(Some("T4".into()));
    assert!(fx.adv("b1").is_err());
    crash_seam::set(None);
    // the holder was spawned and opened its log, but no lock names the run: it may still start, so it is not settled
    fx.lock.named.lock().unwrap().clear();
    let req = fx.lock.launches.lock().unwrap()[0].clone();
    std::fs::write(&req.log, "").unwrap();
    match fx.adv("b1").unwrap() {
        Outcome::Refused { rule, because, .. } => {
            assert_eq!(rule, "batch.failed-needs-person");
            assert!(because.contains("interrupted"), "{because}");
        }
        o => panic!("{o:?}"),
    }
    assert_eq!(fx.lock.launches.lock().unwrap().len(), 1, "never a second launch of a run that may be starting");
    assert_eq!(outcome_of(&fx, "T4"), "interrupted");
    assert_eq!(fx.state("b1"), "preflighted");
}

#[test]
fn s5_a_launched_preflight_whose_holder_names_the_run_late_is_waited_for() {
    let mut fx = Fx::new("");
    fx.launched_preflight();
    *fx.lock.late_polls.lock().unwrap() = 3;
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    fx.step("b1", "preflighting");
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Waiting(_)), "the holder named the run: still running, not stopped");
    assert_eq!(fx.lock.launches.lock().unwrap().len(), 1);
    assert_eq!(fx.state("b1"), "preflighting");
}

#[test]
fn s5_a_launched_preflight_whose_holder_never_names_the_run_fails_clean_once() {
    let mut fx = Fx::new("");
    fx.launched_preflight();
    fx.cfg.launch_ack_secs = 0.3;
    *fx.lock.late_polls.lock().unwrap() = usize::MAX;
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.step("b1", "settled");
    match fx.adv("b1").unwrap() {
        Outcome::Failed(m) => assert!(m.contains("did not name run b1-f1 within 0.3 s"), "{m}"),
        o => panic!("{o:?}"),
    }
    assert!(matches!(fx.adv("b1").unwrap(), Outcome::Refused { rule, .. } if rule == "batch.failed-needs-person"));
    assert_eq!(fx.lock.launches.lock().unwrap().len(), 1);
    assert_eq!(outcome_of(&fx, "T3"), "failed-clean");
}

#[test]
fn s6_a_hung_regen_fails_at_the_plugin_timeout() {
    let mut fx = Fx::new("");
    fx.cfg.regen_command = vec!["sh".into(), "-c".into(), "sleep 30".into()];
    fx.open("b1");
    fx.step("b1", "stacked");
    fx.cfg.plugin_timeout_min = 0.1; // set only now, with room for a loaded host
    let t0 = std::time::Instant::now();
    match fx.adv("b1").unwrap() {
        Outcome::Failed(m) => assert!(m.contains("regen timed out after 0.1 min"), "{m}"),
        o => panic!("{o:?}"),
    }
    assert!(t0.elapsed() < std::time::Duration::from_secs(20));
}

// ---- review round 4: a T7 that failed after the protected checkout already moved
fn install_post_merge_hook(fx: &Fx) {
    let hooks = std::path::PathBuf::from(g(&fx.repo, &["rev-parse", "--git-common-dir"]));
    let hooks = if hooks.is_absolute() { hooks } else { fx.repo.join(hooks) }.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(hooks.join("post-merge"), "#!/bin/sh\necho made-by-hook > hook-made.txt\n").unwrap();
    // git runs a hook only when it is executable (on Windows every file is)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(hooks.join("post-merge"), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn s7_rev4_t7_failed_after_the_checkout_moved_with_generated_delta() {
    let mut fx = Fx::new("");
    proven_with_chain_output(&mut fx, &[("gen/chain-made.txt", "made by the chain\n")]);
    fx.step("b1", "proven");
    let main_wt = fx.outside("main-wt");
    g(&fx.repo, &["worktree", "add", "-q", &fwd(&main_wt), "main"]);
    install_post_merge_hook(&fx);
    let m = fx.failed_text("b1");
    assert!(m.contains("is not at") || m.contains("clean tree"), "{m}");
    let head = g(&fx.repo, &["rev-parse", "HEAD"]);
    let main = g(&fx.repo, &["rev-parse", "main"]);
    assert_eq!(main, head, "the merge already happened");
    std::fs::remove_file(main_wt.join("hook-made.txt")).unwrap();
    let o = fx.adv("b1").unwrap();
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "merged"), "the advance after it finishes the merge that happened: {o:?}");
    assert_eq!(status_fact(&mut fx, "b1", &["merge_new"]), serde_json::json!(head));
}

#[test]
fn s7_rev4_t7_failed_after_the_checkout_moved_plain() {
    let mut fx = Fx::new("");
    fx.drive_to_proving("b1");
    fx.write_log(GREEN);
    fx.step("b1", "proven");
    let main_wt = fx.outside("main-wt");
    g(&fx.repo, &["worktree", "add", "-q", &fwd(&main_wt), "main"]);
    install_post_merge_hook(&fx);
    fx.failed_text("b1");
    std::fs::remove_file(main_wt.join("hook-made.txt")).unwrap();
    let o = fx.adv("b1").unwrap();
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "merged"), "{o:?}");
}

#[test]
fn s7_a_regen_failure_after_the_generated_files_commit_is_recoverable() {
    let mut fx = Fx::new("");
    proven_with_chain_output(&mut fx, &[("gen/chain-made.txt", "made by the chain\n")]);
    fx.step("b1", "proven");
    let regen = fx.cfg.regen_command.clone();
    fx.cfg.regen_command = vec!["sh".into(), "-c".into(), "mkdir -p gen && echo more >> gen/grow.txt".into()]; // never a fixpoint
    let m = fx.failed_text("b1");
    assert!(m.contains("not a fixpoint"), "{m}");
    let tested = status_fact(&mut fx, "b1", &["tested_sha"]);
    assert_ne!(serde_json::json!(g(&fx.repo, &["rev-parse", "HEAD"])), tested, "the generated-files commit moved the batch tip");
    // a person fixes the generator and removes what the broken one wrote, then retries
    fx.cfg.regen_command = regen;
    std::fs::remove_file(fx.repo.join("gen/grow.txt")).unwrap();
    let adm = AllowAll;
    let mut e = Engine { ledger: &mut fx.ledger, cfg: &fx.cfg, lock: &fx.lock, adm: &adm, now: &NOW, state_dir: fx.state.clone(), actor: "human:me".into(), progress: None };
    e.retry_failed("b1", "the generator is fixed").unwrap();
    let o = fx.adv("b1").unwrap();
    assert!(matches!(&o, Outcome::Advanced { to, .. } if to == "merged"), "not a permanent merge.tested-equals-merged: {o:?}");
    assert_eq!(g(&fx.repo, &["rev-parse", "main"]), g(&fx.repo, &["rev-parse", "HEAD"]));
    // a tip that moved by anything but generated files is still refused
}

#[test]
fn s7_a_tip_that_moved_beyond_the_generated_files_is_still_refused() {
    let mut fx = Fx::new("");
    proven_with_chain_output(&mut fx, &[("gen/chain-made.txt", "made by the chain\n")]);
    fx.step("b1", "proven");
    std::fs::remove_file(fx.repo.join("gen/chain-made.txt")).unwrap();
    std::fs::write(fx.repo.join("late.txt"), "late\n").unwrap();
    g(&fx.repo, &["add", "--", "late.txt"]);
    g(&fx.repo, &["commit", "-q", "-m", "late"]);
    let (rule, _) = fx.refused("b1");
    assert_eq!(rule, "merge.tested-equals-merged");
}

// ============================================================ before tick may be enabled (final review, round 5)

/// review round 5: a timed-out health call returned `Waiting` before the age check, so a check that hangs on every call waited for ever
#[test]
fn rev5_a_health_check_that_always_times_out_is_refused_past_the_health_timeout() {
    let mut fx = deployed("[deploy.staging]\nidempotent = true\ncommand = [\"sh\", \"-c\", \"true\"]\nhealth = [\"sh\", \"-c\", \"sleep 30\"]\nhealth_timeout_min = 30\n");
    // the limit is short only for the health call (git calls have their own, `git_min`), with room for a loaded host
    fx.cfg.plugin_timeout_min = 0.1;
    fx.step("b1", "deploying");
    let o = fx.adv_at("b1", &LATER).unwrap();
    assert!(matches!(&o, Outcome::Refused { rule, .. } if rule == "deploy.health-timeout"), "an hour past a 30 min health timeout, a hung health check: {o:?}");
}
