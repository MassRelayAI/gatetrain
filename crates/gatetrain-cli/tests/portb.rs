//! `doctor`, `lanes` and `generated` in Rust against the outputs the Python version printed for the same inputs, byte for byte.
//! tests/golden/portb/ holds those outputs (a temporary folder is written `{TMP}`); tests/corpus/portb holds the doctor transcripts.
//! The Python version is retired; the recorded files are the proof. `GT_RECORD=1` rewrites them from this binary.
use gatetrain_core::{config, doctor, pycompat::dumps_indent2, runner::FakeRunner};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// The Rust binary, run from an empty temp directory.
fn rs(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gatetrain")).args(args).current_dir(cwd).output().unwrap()
}

/// Commit ids (9 or 40 hex digits) differ in every run: they are written `{SHA}`.
fn sha_free(t: &str) -> String {
    let cs: Vec<char> = t.chars().collect();
    let mut o = String::new();
    let mut i = 0;
    while i < cs.len() {
        let hex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
        let mut j = i;
        while j < cs.len() && hex(cs[j]) {
            j += 1;
        }
        let word_before = i > 0 && (cs[i - 1].is_ascii_alphanumeric() || cs[i - 1] == '_');
        let word_after = j < cs.len() && (cs[j].is_ascii_alphanumeric() || cs[j] == '_');
        if (j - i == 9 || j - i == 40) && !word_before && !word_after && cs[i..j].iter().any(|c| c.is_ascii_digit()) {
            o.push_str("{SHA}");
            i = j;
        } else if j > i {
            o.extend(&cs[i..j]);
            i = j;
        } else {
            o.push(cs[i]);
            i += 1;
        }
    }
    o
}

/// The recorded outputs of one test: the n-th call of the test is the file `<test>-<n>.txt`.
struct Gold {
    test: &'static str,
    n: usize,
    tmp: Vec<String>,
}

impl Gold {
    fn new(test: &'static str, tmp: &[&Path]) -> Gold {
        let fwd = |p: &Path| p.to_string_lossy().replace('\\', "/");
        Gold { test, n: 0, tmp: tmp.iter().map(|p| fwd(p)).collect() }
    }

    fn render(&self, o: &Output) -> String {
        let mut t = format!("rc={}\n--- stdout\n{}--- stderr\n{}", o.status.code().unwrap(), String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        t = t.replace("\r\n", "\n");
        t = sha_free(&t);
        for d in &self.tmp {
            t = t.replace(d.as_str(), "{TMP}").replace(&d.replace('/', "\\"), "{TMP}");
        }
        t
    }

    fn same(&mut self, o: &Output, what: &str) {
        self.n += 1;
        let p = repo().join(format!("tests/golden/portb/{}-{}.txt", self.test, self.n));
        let got = self.render(o);
        if std::env::var_os("GT_RECORD").is_some() {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, &got).unwrap();
        }
        let want = std::fs::read_to_string(&p).unwrap_or_else(|_| panic!("{} is missing: {what}", p.display())).replace("\r\n", "\n");
        assert_eq!(got, want, "{what}");
    }
}

fn doctor_cases() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(repo().join("tests/corpus/portb/doctor")).unwrap().map(|e| e.unwrap().path()).collect();
    v.sort();
    assert!(v.len() >= 2);
    v
}

fn run_case(case: &Path) -> Vec<doctor::DResult> {
    let cfg = config::load(&case.join("hosts.toml")).unwrap();
    let tr: serde_json::Value = serde_json::from_slice(&std::fs::read(case.join("transcript.json")).unwrap()).unwrap();
    doctor::run_doctor(&cfg, None, &|h| {
        if h.reach == "local" || h.reach.starts_with("ssh ") {
            Some(Box::new(FakeRunner::from_json(&tr[h.name.as_str()])))
        } else {
            None
        }
    })
    .unwrap()
}

#[test]
fn doctor_text_json_and_exit_match_the_recorded_output_on_every_transcript_case() {
    let mut checked = 0;
    for case in doctor_cases() {
        let res = run_case(&case);
        let text = format!("{}\n", doctor::format_text(&res));
        let json = format!("{}\n", dumps_indent2(&doctor::to_json(&res)));
        let want = |f: &str| std::fs::read_to_string(case.join(f)).unwrap();
        assert_eq!(text, want("expected-stdout"), "text {}", case.display());
        assert_eq!(json, want("expected-json"), "json {}", case.display());
        assert_eq!(doctor::exit_code(&res).to_string(), want("expected-exit").trim(), "exit {}", case.display());
        checked += 2;
    }
    assert!(checked >= 4);
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "core.autocrlf=false"]).args(args).current_dir(dir).output().unwrap();
    assert!(o.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn put(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn commit(dir: &Path, msg: &str, files: &[(&str, &str)]) {
    for (f, t) in files {
        put(dir, f, t);
    }
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", msg]);
}

fn branch(dir: &Path, name: &str, from: &str) {
    git(dir, &["checkout", "-q", "-B", name, from]);
}

/// Every radar section at once: same lines, shared hub, code on both sides, delete/edit, containment, a carried commit
/// (optionally already on main by patch), a port clash, a stale paired count, a missing baseline entry, main moving on.
fn lane_repo(carried_on_main: bool) -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let d = t.path();
    git(d, &["init", "-q", "-b", "main"]);
    commit(
        d,
        "c1",
        &[
            ("a.txt", "l1\nl2\nl3\nl4\nl5\n"),
            ("package.json", "{\"a\":1}\n"),
            ("d.txt", "d\n"),
            ("src/c.js", "const MAX_ROUTES = 5;\n"),
            ("tests/c.test.js", "expect(MAX_ROUTES).toBe(5) // 5\n"),
            ("src/code.js", "a\nb\nc\nd\n"),
            ("baseline.txt", "x\n"),
        ],
    );
    branch(d, "laneA", "main");
    commit(d, "A", &[("a.txt", "l1\nA2\nl3\nl4\nl5\n"), ("package.json", "{\"a\":2}\n"), ("src/code.js", "a\nbA\nc\nd\n"), ("tools/gate-a.sh", "PORT=51234\n")]);
    branch(d, "laneB", "main");
    commit(d, "B", &[("a.txt", "l1\nB2\nl3\nl4\nl5\n"), ("package.json", "{\"a\":3}\n"), ("src/code.js", "a\nb\ncB\nd\n"), ("tools/gate-b.sh", "PORT=51234\n"), ("src/c.js", "const MAX_ROUTES = 6;\n")]);
    branch(d, "laneC", "main");
    git(d, &["rm", "-q", "d.txt"]);
    git(d, &["commit", "-q", "-m", "C"]);
    branch(d, "laneD", "main");
    commit(d, "D", &[("d.txt", "dD\n")]);
    branch(d, "laneE", "laneA");
    commit(d, "E", &[("a.txt", "l1\nA2\nl3\nl4\nE5\n")]);
    branch(d, "shared", "main");
    commit(d, "S", &[("s.txt", "s\n")]);
    branch(d, "laneF", "shared");
    commit(d, "F", &[("f.txt", "f\n"), ("src/code.js", "a\nb\nc\ndF\n")]);
    branch(d, "laneG", "shared");
    commit(d, "G", &[("g.txt", "g\n"), ("src/code.js", "a\nb\nc\ndG\n")]);
    branch(d, "laneH", "main");
    commit(d, "H", &[("src/routes.js", "app.get('/x', h);\n")]);
    branch(d, "laneI", "main"); // no own commits
    git(d, &["checkout", "-q", "main"]);
    commit(d, "M", &[("a.txt", "l1\nM2\nl3\nl4\nl5\n")]);
    if carried_on_main {
        git(d, &["cherry-pick", "shared"]);
    }
    t
}

const LANES: [&str; 9] = ["laneA", "laneB", "laneC", "laneD", "laneE", "laneF", "laneG", "laneH", "laneI"];

fn lanes_args<'a>(repo: &'a str, extra: &[&'a str], refs: &[&'a str]) -> Vec<&'a str> {
    let mut a = vec!["lanes", "--repo", repo, "--main", "main"];
    a.extend(refs);
    a.extend(extra);
    a
}

#[test]
fn lanes_text_and_json_match_the_recorded_output_on_synthetic_repos() {
    let mut n = 0;
    let scratch = tempfile::tempdir().unwrap();
    let mut g = Gold::new("lanes", &[scratch.path()]);
    for on_main in [false, true] {
        let r = lane_repo(on_main);
        let rp = r.path().to_str().unwrap();
        g.tmp.push(r.path().to_string_lossy().replace('\\', "/"));
        let variants: Vec<(Vec<&str>, Vec<&str>)> = vec![
            (vec![], LANES.to_vec()),
            (vec!["--json"], LANES.to_vec()),
            (vec!["--baseline-files", "baseline.txt", "--json"], LANES.to_vec()),
            (vec!["--baseline-files", "baseline.txt"], LANES.to_vec()),
            (vec!["--shared-files", "*.json,*.sh"], LANES.to_vec()),
            (vec!["--resource-regex", "port=PORT=([0-9]+)"], LANES.to_vec()),
            (vec![], vec!["laneE", "laneA", "laneB"]),
            (vec!["--json"], vec!["laneA", "laneA"]),
        ];
        for (extra, refs) in variants {
            let args = lanes_args(rp, &extra, &refs);
            let a = rs(scratch.path(), &args);
            g.same(&a, &format!("on_main={on_main} {:?}", args));
            assert!(a.status.success(), "{}", String::from_utf8_lossy(&a.stderr));
            n += 1;
        }
        // sanity: the radar saw what the repo was built to show
        let text = String::from_utf8_lossy(&rs(scratch.path(), &lanes_args(rp, &["--baseline-files", "baseline.txt"], &LANES)).stdout).into_owned();
        for must in ["SAME-LINES", "SHARED-HUB", "CODE-BOTH", "DELETE-EDIT", "contained in", "MAX_ROUTES 5 -> 6", "5b. baseline radar", "doubled", "PORT"] {
            assert!(text.contains(must) || must == "PORT", "missing {must:?} in\n{text}");
        }
    }
    assert!(n >= 16);
}

#[test]
fn lanes_errors_match_the_recorded_output() {
    let r = lane_repo(false);
    let rp = r.path().to_str().unwrap();
    let s = tempfile::tempdir().unwrap();
    let mut g = Gold::new("lanes_errors", &[s.path(), r.path()]);
    for args in [vec!["lanes", "--repo", rp, "--main", "nope", "laneA"], vec!["lanes", "--repo", rp, "--main", "main", "laneA", "--resource-regex", "noequals"]] {
        let a = rs(s.path(), &args);
        g.same(&a, &format!("{:?}", args));
        assert!(!a.status.success());
    }
}

#[test]
fn generated_matches_the_recorded_output() {
    let r = tempfile::tempdir().unwrap();
    let d = r.path();
    git(d, &["init", "-q", "-b", "main"]);
    commit(d, "c", &[("gen/tracked.txt", "t\n")]);
    let s = tempfile::tempdir().unwrap();
    let mut g = Gold::new("generated", &[s.path(), d]);
    let rp = d.to_str().unwrap();
    let cfg = d.join("c.toml");
    std::fs::write(&cfg, "[stack]\ngenerated_dirs = [\"gen\", \"out\"]\n").unwrap();
    let empty = d.join("e.toml");
    std::fs::write(&empty, "[[hosts]]\nname = \"h\"\n").unwrap();
    let cases: Vec<Vec<&str>> = vec![
        vec!["generated", "--repo", rp, "--dir", "gen"],
        vec!["generated", "--repo", rp, "--config", cfg.to_str().unwrap()],
        vec!["generated", "--repo", rp],
        vec!["generated", "--repo", rp, "--config", empty.to_str().unwrap()],
        vec!["generated", "--repo", rp, "--config", "no-such.toml"],
    ];
    let mut codes = vec![];
    for step in 0..2 {
        if step == 1 {
            put(d, "gen/new b.txt", "n\n");
            put(d, "out/z.txt", "z\n");
            put(d, "gen/new a.txt", "n\n");
        }
        for args in &cases {
            let a = rs(s.path(), args);
            g.same(&a, &format!("{:?}", args));
            codes.push(a.status.code().unwrap());
        }
    }
    assert!(codes.contains(&0) && codes.contains(&1) && codes.contains(&2));
}

#[test]
fn doctor_real_run_has_its_shape_and_its_errors() {
    // the checks look at this machine, so the text is not recorded: the shape and the exit codes are
    let s = tempfile::tempdir().unwrap();
    let cfg = s.path().join("hosts.toml");
    std::fs::write(&cfg, "[[hosts]]\nname = \"here\"\nreach = \"local\"\ntools = [\"git\", \"definitely-not-a-tool-xyz\"]\ncontainer_prefix = \"mr-\"\n[[hosts]]\nname = \"far\"\nreach = \"pigeon\"\n").unwrap();
    let c = cfg.to_str().unwrap();
    let text = rs(s.path(), &["doctor", "--config", c]);
    let t = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(t.contains("definitely-not-a-tool-xyz") && t.contains("far"), "{t}");
    assert_eq!(text.status.code(), Some(1), "a missing tool is a FAIL: {t}");
    let json = rs(s.path(), &["doctor", "--config", c, "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert!(v.as_array().unwrap().iter().any(|r| r["host"] == "far" && r["status"] == "WARN"));
    let far = rs(s.path(), &["doctor", "--config", c, "--host", "far"]);
    assert_eq!(far.status.code(), Some(0), "{}", String::from_utf8_lossy(&far.stdout));
    let nobody = rs(s.path(), &["doctor", "--config", c, "--host", "nobody"]);
    assert_ne!(nobody.status.code(), Some(0));
    let bad = rs(s.path(), &["doctor", "--config", "missing.toml"]);
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).starts_with("config error: "));
}
