//! `gatetrain watch`: one read-only look at a repository's lanes and what to do next.
//!
//! It reads the repository named in the `[watch]` table of the config (never a path written in this code) and reports: live lanes
//! (branches under `lane_prefix` whose content is not on the main branch) with worktree, last commit age and dirty state; merged
//! lanes that still have a branch or a worktree (tidy's work); stale and idle lanes; lanes ready to stack; what is blocked; and the
//! suggested next assignments, from the lane ledger (a CSV) and the plan index (a CSV) when they are configured.
//!
//! It never writes to the repository, not even a timestamp: git runs with `--no-optional-locks` (no index refresh) and only
//! commands that create no object run. `merge-tree --write-tree` is not one of them: even with its objects sent to a throwaway
//! object directory, git "freshens" (re-dates) every object that already exists in the repository, which an earlier merge proof
//! leaves behind. So the content proof compares paths (every path the lane changed is the same on main) and the conflict probe is
//! the three-way `merge-tree <base> <main> <lane>`, which prints its result and stores nothing. The ledger and the index are read
//! as files.
use crate::runner::{run_cmd, RunOut};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct WatchCfg {
    pub repo: String,
    pub main_branch: String,
    pub lane_prefix: String,
    /// the lane ledger, relative to `repo` (columns by name: id, batch, branch, worktree, status)
    pub lanes_csv: Option<String>,
    /// the plan index, relative to `repo` (columns by name: tracker_id, kind, status, plan_file, last_change)
    pub plans_index: Option<String>,
    pub stale_days: f64,
    pub idle_hours: f64,
    pub ready_status: Vec<String>,
    pub blocked_status: Vec<String>,
    /// ledger statuses that mean the lane is finished (a row with one of these and no branch is not a problem)
    pub done_status: Vec<String>,
    /// plan-index statuses that make a plan a candidate for a new lane
    pub suggest_status: Vec<String>,
    /// plan-index statuses that mean the plan's lane is merged (a live branch of such a plan is merged in a way git cannot prove)
    pub merged_plan_status: Vec<String>,
    pub max_suggestions: usize,
    pub git_timeout_s: f64,
    /// paths the stacker rebuilds (globs: `*` matches any run of characters, `/` included; `?` one character). A lane whose
    /// merge into main conflicts only in such paths is "stackable", not "blocked".
    pub regenerable: Vec<String>,
}

impl Default for WatchCfg {
    fn default() -> Self {
        WatchCfg {
            repo: String::new(),
            main_branch: "main".into(),
            lane_prefix: "line/".into(),
            lanes_csv: None,
            plans_index: None,
            stale_days: 3.0,
            idle_hours: 24.0,
            ready_status: vec!["built".into(), "ready".into()],
            blocked_status: vec!["blocked".into(), "red".into()],
            done_status: vec!["merged".into(), "abandoned".into(), "dropped".into()],
            suggest_status: vec!["open".into()],
            merged_plan_status: vec!["merged".into()],
            max_suggestions: 8,
            git_timeout_s: 60.0,
            regenerable: vec![],
        }
    }
}

const KEYS: [&str; 15] = [
    "blocked_status", "done_status", "git_timeout_s", "idle_hours", "lane_prefix", "lanes_csv", "main_branch", "max_suggestions", "merged_plan_status", "plans_index",
    "ready_status", "regenerable", "repo", "stale_days", "suggest_status",
];

/// The `[watch]` table of a TOML file (other tables are ignored, so it can live in hosts.toml). Unknown keys are an error.
pub fn load_cfg(path: &Path) -> Result<WatchCfg, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let v: toml::Table = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
    let Some(w) = v.get("watch") else { return Err(format!("{}: no [watch] table (repo = \"...\" at least)", path.display())) };
    let w = w.as_table().ok_or("watch: must be a table")?;
    let mut bad: Vec<&String> = w.keys().filter(|k| !KEYS.contains(&k.as_str())).collect();
    bad.sort();
    if !bad.is_empty() {
        return Err(format!("watch: unknown key(s) {bad:?}; allowed {KEYS:?}"));
    }
    let mut c = WatchCfg::default();
    let s = |k: &str| -> Result<Option<String>, String> {
        match w.get(k) {
            None => Ok(None),
            Some(toml::Value::String(x)) if !x.is_empty() => Ok(Some(x.clone())),
            Some(_) => Err(format!("watch.{k}: must be a non-empty string")),
        }
    };
    let f = |k: &str, d: f64| -> Result<f64, String> {
        match w.get(k) {
            None => Ok(d),
            Some(toml::Value::Integer(i)) if *i >= 0 => Ok(*i as f64),
            Some(toml::Value::Float(x)) if *x >= 0.0 => Ok(*x),
            Some(_) => Err(format!("watch.{k}: must be a number >= 0")),
        }
    };
    let l = |k: &str, d: &[String]| -> Result<Vec<String>, String> {
        match w.get(k) {
            None => Ok(d.to_vec()),
            Some(toml::Value::Array(a)) => a.iter().map(|x| x.as_str().map(|s| s.to_lowercase()).ok_or(format!("watch.{k}: must be a list of strings"))).collect(),
            Some(_) => Err(format!("watch.{k}: must be a list of strings")),
        }
    };
    c.repo = s("repo")?.ok_or("watch.repo: required")?;
    if let Some(x) = s("main_branch")? {
        c.main_branch = x;
    }
    if let Some(x) = s("lane_prefix")? {
        c.lane_prefix = x;
    }
    c.lanes_csv = s("lanes_csv")?;
    c.plans_index = s("plans_index")?;
    c.stale_days = f("stale_days", c.stale_days)?;
    c.idle_hours = f("idle_hours", c.idle_hours)?;
    c.git_timeout_s = f("git_timeout_s", c.git_timeout_s)?;
    c.max_suggestions = f("max_suggestions", c.max_suggestions as f64)? as usize;
    c.ready_status = l("ready_status", &c.ready_status)?;
    c.blocked_status = l("blocked_status", &c.blocked_status)?;
    c.done_status = l("done_status", &c.done_status)?;
    c.suggest_status = l("suggest_status", &c.suggest_status)?;
    c.merged_plan_status = l("merged_plan_status", &c.merged_plan_status)?;
    c.regenerable = match w.get("regenerable") {
        None => vec![],
        Some(toml::Value::Array(a)) => a.iter().map(|x| x.as_str().map(str::to_string).ok_or("watch.regenerable: must be a list of strings".to_string())).collect::<Result<_, _>>()?,
        Some(_) => return Err("watch.regenerable: must be a list of strings".into()),
    };
    for b in [&c.main_branch, &c.lane_prefix] {
        if b.starts_with('-') || b.chars().any(|ch| ch.is_whitespace() || ch.is_control() || "~^:?*[\\".contains(ch)) {
            return Err(format!("watch: {b:?} is not a plain branch name"));
        }
    }
    Ok(c)
}

// ---------------------------------------------------------------- git, read-only

struct Git {
    repo: PathBuf,
    timeout: f64,
}

impl Git {
    fn cmd(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut c = Command::new("git");
        c.arg("--no-optional-locks").args(args).current_dir(cwd);
        c.env("GIT_OPTIONAL_LOCKS", "0").env("GIT_TERMINAL_PROMPT", "0");
        c
    }
    fn run_in(&self, cwd: &Path, args: &[&str]) -> Result<(i64, String), String> {
        match run_cmd(self.cmd(cwd, args), Some(self.timeout))? {
            RunOut::Done { rc, out } => Ok((rc, out)),
            RunOut::TimedOut => Err(format!("git {} timed out after {}s", args.join(" "), self.timeout)),
        }
    }
    fn run(&self, args: &[&str]) -> Result<(i64, String), String> {
        self.run_in(&self.repo, args)
    }
    fn ok(&self, args: &[&str]) -> Result<String, String> {
        let (rc, out) = self.run(args)?;
        if rc != 0 {
            return Err(format!("git {} failed (rc {rc})", args.join(" ")));
        }
        Ok(out)
    }
}

/// The paths `git diff --name-only -z` names (no rename detection: a rename is its two paths).
fn paths(out: &str) -> std::collections::HashSet<String> {
    out.split('\0').filter(|p| !p.is_empty()).map(String::from).collect()
}

// ---------------------------------------------------------------- the CSV files

/// RFC 4180: quoted fields, doubled quotes, newlines inside quotes. Rows as maps by the header's column names.
pub fn parse_csv(text: &str) -> Vec<HashMap<String, String>> {
    let mut rows: Vec<Vec<String>> = vec![];
    let (mut row, mut field, mut q) = (vec![], String::new(), false);
    let mut it = text.chars().peekable();
    while let Some(ch) = it.next() {
        if q {
            if ch == '"' {
                if it.peek() == Some(&'"') {
                    field.push('"');
                    it.next();
                } else {
                    q = false;
                }
            } else {
                field.push(ch);
            }
        } else {
            match ch {
                '"' => q = true,
                ',' => row.push(std::mem::take(&mut field)),
                '\r' => {}
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                c => field.push(c),
            }
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    let mut it = rows.into_iter();
    let Some(head) = it.next() else { return vec![] };
    let head: Vec<String> = head.into_iter().map(|h| h.trim_start_matches('\u{feff}').trim().to_string()).collect();
    it.filter(|r| r.iter().any(|f| !f.trim().is_empty())).map(|r| head.iter().cloned().zip(r.into_iter().chain(std::iter::repeat(String::new()))).collect()).collect()
}

fn read_csv(repo: &Path, rel: &Option<String>, notes: &mut Vec<String>) -> Vec<HashMap<String, String>> {
    let Some(rel) = rel else { return vec![] };
    match std::fs::read_to_string(repo.join(rel)) {
        Ok(t) => parse_csv(&t),
        Err(e) => {
            notes.push(format!("cannot read {rel}: {e}"));
            vec![]
        }
    }
}

// ---------------------------------------------------------------- the report

#[derive(Debug, Clone, PartialEq)]
pub enum Merged {
    /// the branch tip is an ancestor of main
    Ancestor,
    /// every commit has an equal patch on main (`git cherry`)
    Patch,
    /// every path the branch changed since its merge base is the same on main
    Content,
}

#[derive(Debug, Clone)]
pub struct Lane {
    pub branch: String,
    pub age_s: Option<i64>,
    pub worktree: Option<String>,
    pub dirty: Option<bool>,
    pub merged: Option<Merged>,
    /// merging into main would conflict (None = not probed or unknown)
    pub conflicts: Option<bool>,
    /// the paths that conflict (empty when none, or not probed)
    pub conflict_paths: Vec<String>,
    pub ahead: Option<usize>,
    pub ledger: Option<String>,
    pub batch: Option<String>,
    /// the plan-index row this lane carries: (tracker id, status)
    pub plan: Option<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct Suggestion {
    pub id: String,
    pub plan: String,
    pub kind: String,
    pub last_change: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub repo: String,
    pub main: String,
    pub main_sha: String,
    pub lanes: Vec<Lane>,
    /// ledger rows that are not done and whose branch does not exist: (id, branch, status)
    pub ghosts: Vec<(String, String, String)>,
    pub suggestions: Vec<Suggestion>,
    pub open_plans: usize,
    pub notes: Vec<String>,
}

fn key(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase()
}

/// `2026-11-14-agent-rules.md` -> `agentrules`
fn plan_slug(p: &str) -> String {
    let f = p.split(';').next().unwrap_or("").trim();
    let f = f.rsplit(['/', '\\']).next().unwrap_or(f).trim_end_matches(".md");
    let parts: Vec<&str> = f.splitn(4, '-').collect();
    let rest = if parts.len() == 4 && parts[..3].iter().all(|x| x.chars().all(|c| c.is_ascii_digit())) { parts[3] } else { f };
    key(rest)
}

pub fn gather(cfg: &WatchCfg, now: i64) -> Result<Report, String> {
    let git = Git { repo: PathBuf::from(&cfg.repo), timeout: cfg.git_timeout_s };
    if !matches!(git.run(&["rev-parse", "--git-dir"]), Ok((0, _))) {
        return Err(format!("{} is not a git repository", cfg.repo));
    }
    gather_with(cfg, &git, now)
}

fn gather_with(cfg: &WatchCfg, git: &Git, now: i64) -> Result<Report, String> {
    let main_ref = format!("refs/heads/{}", cfg.main_branch);
    let mut rep = Report { repo: cfg.repo.clone(), main: cfg.main_branch.clone(), ..Default::default() };
    rep.main_sha = git.ok(&["rev-parse", "--verify", &main_ref])?.trim().to_string();
    // worktrees: branch -> path
    let mut wt_of: HashMap<String, String> = HashMap::new();
    let mut cur: Option<String> = None;
    for l in git.ok(&["worktree", "list", "--porcelain"])?.lines() {
        if let Some(p) = l.strip_prefix("worktree ") {
            cur = Some(p.to_string());
        } else if let (Some(b), Some(p)) = (l.strip_prefix("branch refs/heads/"), cur.as_ref()) {
            wt_of.insert(b.to_string(), p.clone());
        }
    }
    // the ledger: the last row per branch wins
    let ledger = read_csv(&git.repo, &cfg.lanes_csv, &mut rep.notes);
    let mut row_of: HashMap<String, &HashMap<String, String>> = HashMap::new();
    for r in &ledger {
        if let Some(b) = r.get("branch").filter(|b| !b.trim().is_empty()) {
            row_of.insert(b.trim().to_string(), r);
        }
    }
    // branches under the prefix
    let refs = git.ok(&["for-each-ref", "--format=%(refname:short)%09%(committerdate:unix)", &format!("refs/heads/{}", cfg.lane_prefix)])?;
    let mut names: Vec<String> = vec![];
    for l in refs.lines() {
        let mut p = l.split('\t');
        let (Some(b), Some(t)) = (p.next(), p.next()) else { continue };
        if b == cfg.main_branch {
            continue;
        }
        names.push(b.to_string());
        let r = row_of.get(b);
        rep.lanes.push(Lane {
            branch: b.to_string(),
            age_s: t.trim().parse::<i64>().ok().map(|t| (now - t).max(0)),
            worktree: wt_of.get(b).cloned(),
            dirty: None,
            merged: None,
            conflicts: None,
            conflict_paths: vec![],
            ahead: None,
            ledger: r.and_then(|r| r.get("status")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            batch: r.and_then(|r| r.get("batch")).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()),
            plan: None,
        });
    }
    for lane in rep.lanes.iter_mut() {
        let br = format!("refs/heads/{}", lane.branch);
        lane.merged = match git.run(&["merge-base", "--is-ancestor", &br, &main_ref]) {
            Ok((0, _)) => Some(Merged::Ancestor),
            Ok((1, _)) => None,
            Ok((rc, _)) => {
                rep.notes.push(format!("{}: merge-base failed (rc {rc})", lane.branch));
                None
            }
            Err(e) => {
                rep.notes.push(format!("{}: {e}", lane.branch));
                None
            }
        };
        if lane.merged.is_none() {
            if let Ok((0, out)) = git.run(&["cherry", &main_ref, &br]) {
                let plus = out.lines().filter(|l| l.starts_with('+')).count();
                lane.ahead = Some(plus);
                if plus == 0 && !out.trim().is_empty() {
                    lane.merged = Some(Merged::Patch);
                }
            }
        }
        if lane.merged.is_none() {
            let base = match git.run(&["merge-base", &main_ref, &br]) {
                Ok((0, out)) => Some(out.trim().to_string()),
                _ => {
                    rep.notes.push(format!("{}: no merge base with {}", lane.branch, cfg.main_branch));
                    None
                }
            };
            if let Some(base) = base {
                // content: every path the lane changed is the same on main (then merging it changes nothing)
                let changed = git.run(&["diff", "--name-only", "-z", "--no-renames", &base, &br]);
                let differs = git.run(&["diff", "--name-only", "-z", "--no-renames", &main_ref, &br]);
                if let (Ok((0, c)), Ok((0, d))) = (&changed, &differs) {
                    let (c, d) = (paths(c), paths(d));
                    if !c.is_empty() && c.is_disjoint(&d) {
                        lane.merged = Some(Merged::Content);
                    }
                } else {
                    rep.notes.push(format!("{}: git diff failed; merge state by content unknown", lane.branch));
                }
                // conflicts: the three-way merge-tree prints the merge (conflict markers included) and stores nothing
                if lane.merged.is_none() {
                    match git.run(&["merge-tree", &base, &main_ref, &br]) {
                        Ok((0, out)) => {
                            lane.conflict_paths = conflict_paths(&out);
                            lane.conflicts = Some(out.lines().any(|l| l.starts_with("+<<<<<<< ")));
                        }
                        Ok((rc, _)) => rep.notes.push(format!("{}: merge-tree failed (rc {rc})", lane.branch)),
                        Err(e) => rep.notes.push(format!("{}: {e}", lane.branch)),
                    }
                }
            }
        }
        if let Some(w) = lane.worktree.clone() {
            lane.dirty = match git.run_in(Path::new(&w), &["status", "--porcelain", "--untracked-files=normal"]) {
                Ok((0, out)) => Some(!out.trim().is_empty()),
                Ok(_) => None,
                Err(e) => {
                    rep.notes.push(format!("{w}: {e}"));
                    None
                }
            };
        }
    }
    // ledger rows that are not done and have no branch
    let done: Vec<String> = cfg.done_status.clone();
    for r in &ledger {
        let b = r.get("branch").map(|s| s.trim()).unwrap_or("");
        let st = r.get("status").map(|s| s.trim().to_lowercase()).unwrap_or_default();
        // a stacked row is carried by its batch branch; a done row needs nothing
        if b.is_empty() || !b.starts_with(&cfg.lane_prefix) || done.contains(&st) || st == "stacked" || st.is_empty() || names.iter().any(|n| n == b) {
            continue;
        }
        rep.ghosts.push((r.get("id").cloned().unwrap_or_default(), b.to_string(), st));
    }
    // plans that no lane carries
    let lane_keys: Vec<String> = rep
        .lanes
        .iter()
        .filter(|l| l.merged.is_none())
        .map(|l| key(l.branch.strip_prefix(&cfg.lane_prefix).unwrap_or(&l.branch)))
        .chain(ledger.iter().filter(|r| !done.contains(&r.get("status").map(|s| s.trim().to_lowercase()).unwrap_or_default())).filter_map(|r| r.get("id").map(|i| key(i))))
        .filter(|k| !k.is_empty())
        .collect();
    let plans = read_csv(&git.repo, &cfg.plans_index, &mut rep.notes);
    // the plan each lane carries: its branch name (without the prefix) or its ledger id is the plan's tracker id or file slug
    let ids: HashMap<String, String> = ledger.iter().filter_map(|r| Some((r.get("branch")?.trim().to_string(), key(r.get("id")?)))).collect();
    for lane in rep.lanes.iter_mut() {
        let lk = [key(lane.branch.strip_prefix(&cfg.lane_prefix).unwrap_or(&lane.branch)), ids.get(&lane.branch).cloned().unwrap_or_default()];
        lane.plan = plans
            .iter()
            .find(|p| {
                let (ik, sk) = (key(p.get("tracker_id").map(String::as_str).unwrap_or("")), plan_slug(p.get("plan_file").map(String::as_str).unwrap_or("")));
                lk.iter().any(|k| !k.is_empty() && (*k == ik || *k == sk))
            })
            .map(|p| (p.get("tracker_id").cloned().unwrap_or_default(), p.get("status").map(|s| s.trim().to_lowercase()).unwrap_or_default()));
    }
    let mut open: Vec<Suggestion> = vec![];
    for p in &plans {
        let st = p.get("status").map(|s| s.trim().to_lowercase()).unwrap_or_default();
        let plan = p.get("plan_file").map(|s| s.trim().to_string()).unwrap_or_default();
        if !cfg.suggest_status.contains(&st) || plan.is_empty() {
            continue;
        }
        let id = p.get("tracker_id").map(|s| s.trim().to_string()).unwrap_or_default();
        let (ik, sk) = (key(&id), plan_slug(&plan));
        if lane_keys.iter().any(|k| *k == ik || *k == sk || (ik.len() >= 3 && k.contains(&ik))) {
            continue;
        }
        open.push(Suggestion { id, plan, kind: p.get("kind").map(|s| s.trim().to_string()).unwrap_or_default(), last_change: p.get("last_change").map(|s| s.trim().to_string()).unwrap_or_default() });
    }
    // a written plan before an open one, then the most recently changed
    open.sort_by(|a, b| (b.kind == "written").cmp(&(a.kind == "written")).then(b.last_change.cmp(&a.last_change)).then(a.id.cmp(&b.id)));
    rep.open_plans = open.len();
    open.truncate(cfg.max_suggestions);
    rep.suggestions = open;
    Ok(rep)
}

// ---------------------------------------------------------------- judging and text

pub fn age(s: Option<i64>) -> String {
    match s {
        None => "?".into(),
        Some(s) if s < 3600 => format!("{}m", s / 60),
        Some(s) if s < 2 * 86400 => format!("{}h", s / 3600),
        Some(s) => format!("{}d", s / 86400),
    }
}

/// The paths that conflict in the output of the three-way `git merge-tree <base> <main> <lane>`: a section ("changed in both",
/// "added in both", ...) names its path on its `  our`/`  their`/`  base` lines and conflicts when a hunk holds `+<<<<<<< `.
pub fn conflict_paths(out: &str) -> Vec<String> {
    let mut paths: Vec<String> = vec![];
    let (mut cur, mut hit): (Option<String>, bool) = (None, false);
    let mut flush = |cur: &mut Option<String>, hit: &mut bool| {
        if let (Some(p), true) = (cur.take(), *hit) {
            if !paths.contains(&p) {
                paths.push(p);
            }
        }
        *hit = false;
    };
    for l in out.lines() {
        if !l.starts_with([' ', '@', '+', '-']) {
            flush(&mut cur, &mut hit);
        } else if cur.is_none() && (l.starts_with("  our ") || l.starts_with("  their ") || l.starts_with("  base ")) {
            // `  our    100644 <sha> <path>`
            cur = l.split_whitespace().nth(3).map(str::to_string);
        } else if l.starts_with("+<<<<<<< ") {
            hit = true;
        }
    }
    flush(&mut cur, &mut hit);
    paths
}

/// `*` matches any run of characters (`/` included, as in a shell `case`), `?` one character; everything else is literal.
pub fn glob_match(pat: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (pat.chars().collect(), s.chars().collect());
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && p[pi] != '*' && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// What one live lane needs:("unproven" | "blocked" | "ready" | "stale" | "idle" | "active" | "stacked", why).
pub fn judge(l: &Lane, cfg: &WatchCfg) -> (&'static str, String) {
    let st = l.ledger.clone().unwrap_or_default().to_lowercase();
    // the ledger or the plan index calls it merged, git cannot show it: rebased, squashed with changes, or resolved inside a batch
    if cfg.done_status.contains(&st) {
        return ("unproven", format!("ledger says {st}; git cannot prove it: check by hand, then drop the branch"));
    }
    if let Some((id, ps)) = l.plan.as_ref().filter(|(_, ps)| cfg.merged_plan_status.contains(ps)) {
        return ("unproven", format!("plan {id} is {ps}; git cannot prove it: check by hand, then drop the branch"));
    }
    if l.conflicts == Some(true) {
        if !cfg.regenerable.is_empty() {
            let (gen, real): (Vec<&String>, Vec<&String>) = l.conflict_paths.iter().partition(|p| cfg.regenerable.iter().any(|g| glob_match(g, p)));
            if real.is_empty() && !gen.is_empty() {
                return ("stackable", format!("conflicts only in generated files ({}): the stacker rebuilds them", gen.len()));
            }
            if !real.is_empty() {
                let shown: Vec<&str> = real.iter().take(3).map(|s| s.as_str()).collect();
                let more = if real.len() > 3 { format!(" (+{} more)", real.len() - 3) } else { String::new() };
                return ("blocked", format!("conflicts with main in {}{}: rebase it", shown.join(", "), more));
            }
        }
        return ("blocked", "conflicts with main: rebase it".into());
    }
    if cfg.blocked_status.contains(&st) {
        return ("blocked", format!("ledger says {st}"));
    }
    if cfg.ready_status.contains(&st) {
        if l.dirty == Some(true) {
            return ("blocked", format!("ledger says {st} but the worktree has uncommitted work"));
        }
        if l.conflicts.is_none() {
            return ("blocked", format!("ledger says {st} but its merge into main could not be probed"));
        }
        return ("ready", format!("ledger says {st}, clean, merges into main"));
    }
    if st == "stacked" {
        return ("stacked", format!("in batch {}", l.batch.clone().unwrap_or_else(|| "?".into())));
    }
    let a = l.age_s.unwrap_or(0) as f64;
    let dirty = if l.dirty == Some(true) { ", uncommitted work" } else { "" };
    if a > cfg.stale_days * 86400.0 {
        return ("stale", format!("no commit for {}{dirty}", age(l.age_s)));
    }
    if a > cfg.idle_hours * 3600.0 {
        return ("idle", format!("no commit for {}{dirty}", age(l.age_s)));
    }
    ("active", format!("last commit {} ago{dirty}", age(l.age_s)))
}

fn branches<'a>(v: &[&(&'a Lane, &'static str, String)]) -> Vec<&'a str> {
    v.iter().map(|j| j.0.branch.as_str()).collect()
}

fn table(rows: Vec<Vec<String>>, head: &[&str]) -> Vec<String> {
    crate::tidy::report::table(rows, head).lines().map(|l| format!("  {l}")).collect()
}

fn yn(b: Option<bool>) -> String {
    match b {
        None => "-".into(),
        Some(true) => "yes".into(),
        Some(false) => "no".into(),
    }
}

pub fn format(rep: &Report, cfg: &WatchCfg) -> String {
    let (merged, live): (Vec<&Lane>, Vec<&Lane>) = rep.lanes.iter().partition(|l| l.merged.is_some());
    let judged: Vec<(&Lane, &'static str, String)> = live.iter().map(|l| {
        let (k, w) = judge(l, cfg);
        (*l, k, w)
    }).collect();
    let of = |k: &str| judged.iter().filter(|j| j.1 == k).collect::<Vec<_>>();
    let (ready, stackable, blocked, stale, idle, stacked, unproven) = (of("ready"), of("stackable"), of("blocked"), of("stale"), of("idle"), of("stacked"), of("unproven"));
    let list = |v: &[&str], n: usize| -> String {
        let mut s = v.iter().take(n).copied().collect::<Vec<_>>().join(" ");
        if v.len() > n {
            s += &format!(" (+{} more)", v.len() - n);
        }
        s
    };

    let mut o: Vec<String> = vec![];
    o.push(format!("== watch {} ({} at {})", rep.repo, rep.main, rep.main_sha.chars().take(10).collect::<String>()));
    o.push(format!(
        "lanes: {} live ({} ready to stack, {} stackable, {} blocked, {} stacked, {} stale, {} idle, {} merged by the ledger only), {} merged awaiting tidy, {} ledger row(s) without a branch",
        live.len(),
        ready.len(),
        stackable.len(),
        blocked.len(),
        stacked.len(),
        stale.len(),
        idle.len(),
        unproven.len(),
        merged.len(),
        rep.ghosts.len()
    ));
    // next, most useful first
    let mut next: Vec<String> = vec![];
    if !ready.is_empty() {
        next.push(format!("stack the ready lanes: {}", ready.iter().map(|j| j.0.branch.as_str()).collect::<Vec<_>>().join(" ")));
    }
    if !stackable.is_empty() {
        next.push(format!("ready for the stacker (conflicts only in generated files, which it rebuilds) {} lane(s): {}", stackable.len(), list(&branches(&stackable), 6)));
    }
    let (clash, other): (Vec<&(&Lane, &'static str, String)>, Vec<_>) = blocked.iter().copied().partition(|j| j.0.conflicts == Some(true));
    for j in &other {
        next.push(format!("unblock {}: {}", j.0.branch, j.2));
    }
    if !clash.is_empty() {
        next.push(format!("rebase {} lane(s) that conflict with main: {}", clash.len(), list(&branches(&clash), 6)));
    }
    if !unproven.is_empty() {
        next.push(format!("check by hand {} lane(s) the ledger calls merged (git cannot prove it), then drop them: {}", unproven.len(), list(&branches(&unproven), 6)));
    }
    let wts = merged.iter().filter(|l| l.worktree.is_some()).count();
    if !merged.is_empty() {
        // tidy removes only what it can prove (an ancestor, or a merge that changes nothing); a patch-only match it keeps
        let patch: Vec<&str> = merged.iter().filter(|l| l.merged == Some(Merged::Patch)).map(|l| l.branch.as_str()).collect();
        next.push(format!(
            "tidy: {} merged lane(s) ({} with a worktree) - `gatetrain tidy` (dry run) shows the plan{}",
            merged.len(),
            wts,
            if patch.is_empty() { String::new() } else { format!("; {} merged by patch only, which tidy keeps (check by hand): {}", patch.len(), list(&patch, 6)) }
        ));
    }
    let never: Vec<&(String, String, String)> = rep.ghosts.iter().filter(|g| g.2 == "dispatched").collect();
    if !never.is_empty() {
        let ids: Vec<&str> = never.iter().map(|g| g.0.as_str()).collect();
        next.push(format!("start or drop {} dispatched lane(s) that have no branch: {}", never.len(), list(&ids, 6)));
    }
    for s in &rep.suggestions {
        next.push(format!("assign {} ({}{})", s.id, s.plan, if s.last_change.is_empty() { String::new() } else { format!(", changed {}", s.last_change) }));
    }
    if rep.open_plans > rep.suggestions.len() {
        next.push(format!("... {} more open plan(s) with no lane", rep.open_plans - rep.suggestions.len()));
    }
    if !stale.is_empty() {
        next.push(format!("review {} stale lane(s): finish, park or drop", stale.len()));
    }
    o.push("next:".into());
    if next.is_empty() {
        o.push("  nothing to do".into());
    }
    o.extend(next.iter().enumerate().map(|(i, n)| format!("  {}. {n}", i + 1)));
    if !live.is_empty() {
        o.push("live lanes".into());
        o.extend(table(
            judged
                .iter()
                .map(|(l, k, w)| {
                    vec![
                        l.branch.clone(),
                        l.worktree.clone().unwrap_or_else(|| "-".into()),
                        age(l.age_s),
                        yn(l.dirty),
                        l.ahead.map(|a| a.to_string()).unwrap_or_else(|| "-".into()),
                        l.ledger.clone().unwrap_or_else(|| "-".into()),
                        k.to_string(),
                        w.clone(),
                    ]
                })
                .collect(),
            &["branch", "worktree", "age", "dirty", "ahead", "ledger", "state", "why"],
        ));
    }
    if !merged.is_empty() {
        o.push("merged, awaiting tidy".into());
        o.extend(table(
            merged
                .iter()
                .map(|l| {
                    vec![
                        l.branch.clone(),
                        match l.merged {
                            Some(Merged::Ancestor) => "ancestor",
                            Some(Merged::Patch) => "by patch",
                            _ => "by content",
                        }
                        .into(),
                        l.worktree.clone().unwrap_or_else(|| "-".into()),
                        yn(l.dirty),
                        age(l.age_s),
                    ]
                })
                .collect(),
            &["branch", "merged", "worktree", "dirty", "age"],
        ));
    }
    if !rep.ghosts.is_empty() {
        o.push("ledger rows whose branch does not exist".into());
        o.extend(table(rep.ghosts.iter().map(|(i, b, s)| vec![i.clone(), b.clone(), s.clone()]).collect(), &["id", "branch", "status"]));
    }
    o.extend(rep.notes.iter().map(|n| format!("note: {n}")));
    o.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_match_like_a_shell_case() {
        assert!(glob_match("tools/db/statement-census.json", "tools/db/statement-census.json"));
        assert!(glob_match("*.xlsx", "docs/a/b.xlsx"));
        assert!(glob_match("apps/web/src/i18n/locales/*.json", "apps/web/src/i18n/locales/en.json"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("tools/db/*.json", "tools/db/x.txt"));
        assert!(!glob_match("a", "ab"));
    }

    #[test]
    fn conflict_paths_reads_the_sections_that_hold_a_marker() {
        let out = "changed in both\n  base   100644 aaa gen/a.json\n  our    100644 bbb gen/a.json\n  their  100644 ccc gen/a.json\n@@ -1 +1,5 @@\n+<<<<<<< .our\n+x\n@@ -3 +3 @@\n+<<<<<<< .our\nadded in remote\n  their  100644 ddd b.txt\n@@ -0,0 +1 @@\n+fine\nchanged in both\n  base   100644 aaa src/c.rs\n  our    100644 bbb src/c.rs\n  their  100644 ccc src/c.rs\n@@ -1 +1,2 @@\n+<<<<<<< .our\n";
        assert_eq!(conflict_paths(out), vec!["gen/a.json".to_string(), "src/c.rs".to_string()]);
    }
}
