//! `gatetrain lanes`: a pre-stack collision radar. Port of gatetrain/lanes.py; read-only advice, git through the [`Runner`].
//! Text and `--json` are byte-identical to the Python output (tests/corpus and crates/gatetrain-cli/tests/portb.rs).
//!
//! Known difference: `--resource-regex`, `--new-item-regex` and the paired-count patterns use the Rust `regex` syntax, which has
//! no look-around or back-references; the built-in patterns are written to give the same matches as the Python ones.
use crate::pycompat::{dumps_indent2, fnmatch, J};
use crate::runner::Runner;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

pub const DEFAULT_SHARED: [&str; 12] = [
    "package.json", "package-lock.json", "*.lock", "*.csv", "docs/*.md", "*routes*.json", "*registry*.json", "locales/*.json", "*/locales/*.json", "i18n/*.json", "*/i18n/*.json",
    "*.po",
];
const DEFAULT_PORT_RX: &str = r"\b(5[0-9]{4})\b";
const TEST_FILE: [&str; 3] = ["*test*", "*spec*", "tests/*"];
const NOT_CHECKS: [&str; 5] = ["docs/*", "*/docs/*", "_archive/*", "*/_archive/*", "*.md"];
const DEFAULT_NEW_ITEM: [&str; 4] = [
    r#"\b(?:app|router|server|fastify)\.(?:get|post|put|patch|delete)\(\s*['\"`]"#,
    r#"\.route\(\s*['\"`]"#,
    r"(?i)\bCREATE TABLE\b",
    r"#\[(?:get|post|put|patch|delete)\(",
];

fn rank(kind: &str) -> u8 {
    match kind {
        "SAME-LINES" | "DELETE-EDIT" => 0,
        "CODE-BOTH" => 1,
        _ => 2,
    }
}

fn action(kind: &str) -> &'static str {
    match kind {
        "SAME-LINES" => "certain conflict, merge by hand",
        "DELETE-EDIT" => "one lane deletes or renames it, the other edits it: decide which wins, by hand",
        "CODE-BOTH" => "a person reads both diffs",
        _ => "rebuild, never text-merge; check entries survive",
    }
}

#[derive(Debug)]
pub struct GitError(pub String);

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type R<T> = Result<T, GitError>;

pub struct Git<'a> {
    repo: &'a Path,
    runner: &'a dyn Runner,
}

impl<'a> Git<'a> {
    pub fn new(repo: &'a Path, runner: &'a dyn Runner) -> Self {
        Git { repo, runner }
    }
    fn run(&self, args: &[&str], ok: &[i32]) -> R<String> {
        let mut argv: Vec<String> = vec!["git".into(), "-c".into(), "core.quotepath=off".into()];
        argv.extend(args.iter().map(|s| s.to_string()));
        let o = self.runner.run_argv(self.repo, &argv).map_err(GitError)?;
        if !ok.contains(&o.code) {
            return Err(GitError(format!("git {}: {}", args.join(" "), o.stderr.trim())));
        }
        Ok(o.stdout)
    }
    fn out(&self, args: &[&str]) -> R<String> {
        Ok(self.run(args, &[0])?.trim().to_string())
    }
}

#[derive(Debug, Clone, Default)]
struct Hunk {
    old_start: i64,
    old_count: i64,
    removed: Vec<String>,
    added: Vec<String>,
}

type FileHunks = Vec<(String, Vec<Hunk>)>;

fn hunks_of<'a>(h: &'a FileHunks, f: &str) -> &'a [Hunk] {
    h.iter().find(|(k, _)| k == f).map(|(_, v)| v.as_slice()).unwrap_or(&[])
}

/// `{path: [hunk]}` in the order the files appear.
fn parse_diff(text: &str) -> FileHunks {
    let hdr = Regex::new(r"^@@ -([0-9]+)(?:,([0-9]+))? \+[0-9]+(?:,[0-9]+)? @@").unwrap();
    let mut files: FileHunks = vec![];
    let mut cur: Option<String> = None;
    let mut hunk_open = false;
    for line in text.split('\n') {
        if line.starts_with("diff --git ") {
            cur = None;
            hunk_open = false;
        } else if line.starts_with("--- ") && !hunk_open {
            cur = Some(line[4..].split('\t').next().unwrap_or("").to_string());
        } else if line.starts_with("+++ ") && !hunk_open {
            let mut p = Some(line[4..].split('\t').next().unwrap_or("").to_string());
            if p.as_deref() == Some("/dev/null") {
                p = cur.clone();
            }
            let p = p.map(|p| if p.starts_with("a/") || p.starts_with("b/") { p[2..].to_string() } else { p });
            cur = p;
            if let Some(c) = &cur {
                if !files.iter().any(|(k, _)| k == c) {
                    files.push((c.clone(), vec![]));
                }
            }
        } else if line.starts_with("@@") && cur.is_some() {
            if let Some(m) = hdr.captures(line) {
                let h = Hunk { old_start: m[1].parse().unwrap_or(0), old_count: m.get(2).map(|c| c.as_str().parse().unwrap_or(1)).unwrap_or(1), ..Default::default() };
                let c = cur.as_ref().unwrap();
                if let Some((_, v)) = files.iter_mut().find(|(k, _)| k == c) {
                    v.push(h);
                    hunk_open = true;
                }
            }
        } else if hunk_open && line.starts_with('-') {
            last_hunk(&mut files, &cur).removed.push(line[1..].to_string());
        } else if hunk_open && line.starts_with('+') {
            last_hunk(&mut files, &cur).added.push(line[1..].to_string());
        }
    }
    files
}

fn last_hunk<'a>(files: &'a mut FileHunks, cur: &Option<String>) -> &'a mut Hunk {
    let c = cur.as_ref().unwrap();
    files.iter_mut().find(|(k, _)| k == c).unwrap().1.last_mut().unwrap()
}

fn span(h: &Hunk) -> (i64, i64) {
    let (a, n) = (h.old_start, h.old_count);
    if n == 0 { (2 * a + 1, 2 * a + 1) } else { (2 * a, 2 * (a + n - 1)) }
}

fn matches<S: AsRef<str>>(path: &str, pats: &[S]) -> bool {
    let base = path.rsplit('/').next().unwrap_or(path);
    pats.iter().any(|p| fnmatch(path, p.as_ref()) || fnmatch(base, p.as_ref()))
}

struct DiffInfo {
    hunks: FileHunks,
    deleted: HashSet<String>,
    touched: BTreeSet<String>,
}

#[derive(Clone)]
struct LaneState {
    lane: String,
    tip: String,
    merge_base: String,
    own_commits: usize,
    set: HashSet<String>,
    parents: HashMap<String, Vec<String>>,
    base: String,
    contained_in: Option<String>,
    carries: Vec<usize>,
}

struct Carried {
    tip: String,
    commits: usize,
    lanes: BTreeSet<String>,
    common: HashSet<String>,
    on_main: bool,
}

pub enum Pass {
    Plain { base: String, lanes: Vec<String> },
    CarryTip { base: String, carry_tip: String, carried_by: Vec<String> },
    Carried { base: String, lanes: Vec<String> },
}

pub struct Hot {
    kind: &'static str,
    file: String,
    lanes: [String; 2],
    detail: Option<String>,
    action: &'static str,
}

pub struct Report {
    main: String,
    lanes: Vec<LaneState>,
    contained: Vec<(String, String)>,
    carried: Vec<(String, usize, Vec<String>, bool)>,
    passes: Vec<Vec<Pass>>,
    hot: Vec<Hot>,
    doubled: Vec<(String, i64, [String; 2])>,
    clashes: Vec<(String, String, Vec<String>, Vec<String>)>,
    paired: Vec<Paired>,
    base_warn: Vec<(String, Vec<String>, usize)>,
    groups: Vec<Vec<String>>,
    edges: Vec<(String, Vec<String>)>,
    summary: [usize; 5],
    summary_line: String,
}

struct Paired {
    lane: String,
    name: String,
    old: String,
    new: String,
    file: String,
    stale_in: String,
    line: i64,
}

pub struct Options {
    pub shared: Option<Vec<String>>,
    pub resources: Option<Vec<(String, String)>>,
    pub baseline: Option<Vec<String>>,
    pub new_item: Option<Vec<String>>,
}

fn diffinfo(g: &Git, base: &str, tip: &str) -> R<DiffInfo> {
    let hunks = parse_diff(&g.run(&["diff", "-U0", "--no-color", "--no-renames", "--no-ext-diff", base, tip], &[0])?);
    let mut deleted = HashSet::new();
    let mut touched: BTreeSet<String> = hunks.iter().map(|(k, _)| k.clone()).collect();
    for ln in crate::pycompat::splitlines(&g.run(&["diff", "--name-status", "-M", "--no-ext-diff", base, tip], &[0])?) {
        let parts: Vec<&str> = ln.split('\t').collect();
        if parts[0].starts_with('D') {
            if parts.len() > 1 {
                deleted.insert(parts[1].to_string());
                touched.insert(parts[1].to_string());
            }
        } else if parts[0].starts_with('R') {
            if parts.len() > 1 {
                deleted.insert(parts[1].to_string());
            }
            for p in parts.iter().skip(1).take(2) {
                touched.insert(p.to_string());
            }
        } else if parts.len() > 1 {
            touched.insert(parts[parts.len() - 1].to_string());
        }
    }
    Ok(DiffInfo { hunks, deleted, touched })
}

/// `(?<!\d)o(?!\d)` found anywhere in `text`.
fn has_number(text: &str, o: &str) -> bool {
    text.match_indices(o).any(|(i, m)| {
        let before = text[..i].chars().next_back().map(|c| c.is_ascii_digit()).unwrap_or(false);
        let after = text[i + m.len()..].chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false);
        !before && !after
    })
}

pub fn analyse(repo: &Path, main: &str, refs: &[String], opt: &Options, runner: &dyn Runner) -> R<Report> {
    let g = Git::new(repo, runner);
    let shared: Vec<String> = match &opt.shared {
        Some(s) if !s.is_empty() => s.clone(),
        _ => DEFAULT_SHARED.iter().map(|s| s.to_string()).collect(),
    };
    let resources: Vec<(String, String)> = match &opt.resources {
        Some(r) if !r.is_empty() => r.clone(),
        _ => vec![("port".into(), DEFAULT_PORT_RX.into())],
    };
    let mainsha = g.out(&["rev-parse", "--verify", &format!("{main}^{{commit}}")])?;
    let order: Vec<String> = refs.to_vec();
    let mut l: HashMap<String, LaneState> = HashMap::new();
    for r in refs {
        let tip = g.out(&["rev-parse", "--verify", &format!("{r}^{{commit}}")])?;
        let base = g.out(&["merge-base", &mainsha, &tip])?;
        let mut set = HashSet::new();
        let mut parents = HashMap::new();
        let mut own = 0;
        for ln in g.out(&["rev-list", "--parents", &format!("{mainsha}..{tip}")])?.lines() {
            let p: Vec<&str> = ln.split_whitespace().collect();
            if p.is_empty() {
                continue;
            }
            own += 1;
            set.insert(p[0].to_string());
            parents.insert(p[0].to_string(), p[1..].iter().map(|s| s.to_string()).collect::<Vec<_>>());
        }
        l.insert(r.clone(), LaneState { lane: r.clone(), tip, merge_base: base.clone(), own_commits: own, set, parents, base, contained_in: None, carries: vec![] });
    }
    // containment
    let mut contained = vec![];
    for a in &order {
        for b in &order {
            if a == b || l[a].contained_in.is_some() || l[b].contained_in.is_some() {
                continue;
            }
            let (ta, tb) = (l[a].tip.clone(), l[b].tip.clone());
            let pos = |x: &String| order.iter().position(|y| y == x).unwrap();
            let same = ta == tb && pos(a) > pos(b);
            if same || (ta != tb && l[b].set.contains(&ta)) {
                l.get_mut(a).unwrap().contained_in = Some(b.clone());
                contained.push((a.clone(), b.clone()));
            }
        }
    }
    let live: Vec<String> = order.iter().filter(|r| l[*r].contained_in.is_none()).cloned().collect();
    // carried commits
    let mut carried: Vec<Carried> = vec![];
    for (i, a) in live.iter().enumerate() {
        for b in &live[i + 1..] {
            let common: HashSet<String> = l[a].set.intersection(&l[b].set).cloned().collect();
            if common.is_empty() {
                continue;
            }
            let mut ps: HashSet<&String> = HashSet::new();
            for c in &common {
                if let Some(p) = l[a].parents.get(c) {
                    ps.extend(p.iter());
                }
            }
            let mut tips: Vec<&String> = common.iter().filter(|c| !ps.contains(c)).collect();
            tips.sort();
            let Some(tip) = tips.first().map(|t| (*t).clone()) else { continue };
            let idx = match carried.iter().position(|e| e.tip == tip) {
                Some(i) => i,
                None => {
                    carried.push(Carried { tip: tip.clone(), commits: common.len(), lanes: BTreeSet::new(), common: HashSet::new(), on_main: false });
                    carried.len() - 1
                }
            };
            carried[idx].common.extend(common.iter().cloned());
            carried[idx].lanes.insert(a.clone());
            carried[idx].lanes.insert(b.clone());
        }
    }
    for e in &carried {
        for ln in &e.lanes {
            let st = l.get_mut(ln).unwrap();
            if st.carries.is_empty() || e.commits > *st.carries.iter().max().unwrap() {
                st.base = e.tip.clone();
            }
            st.carries.push(e.commits);
        }
    }
    for e in carried.iter_mut() {
        let out = g.run(&["cherry", &mainsha, &e.tip], &[0])?;
        let marks: HashMap<String, String> = out
            .lines()
            .filter_map(|ln| {
                let w: Vec<&str> = ln.split_whitespace().collect();
                if w.len() == 2 { Some((w[1].to_string(), w[0].to_string())) } else { None }
            })
            .collect();
        e.on_main = !e.common.is_empty() && e.common.iter().all(|c| marks.get(c).map(|m| m == "-").unwrap_or(false));
    }
    let by_order = |s: &BTreeSet<String>| -> Vec<String> {
        let mut v: Vec<String> = s.iter().cloned().collect();
        v.sort_by_key(|x| order.iter().position(|y| y == x).unwrap_or(usize::MAX));
        v
    };
    let carried_out: Vec<(String, usize, Vec<String>, bool)> = carried.iter().map(|e| (e.tip.clone(), e.commits, by_order(&e.lanes), e.on_main)).collect();
    // passes
    let mut groups: Vec<(String, Vec<String>)> = vec![];
    for r in &live {
        let b = l[r].base.clone();
        match groups.iter_mut().find(|(k, _)| *k == b) {
            Some(g) => g.1.push(r.clone()),
            None => groups.push((b, vec![r.clone()])),
        }
    }
    let is_carried = |b: &str| carried.iter().any(|e| e.tip == b);
    let mut passes: Vec<Vec<Pass>> = vec![];
    let mut p1: Vec<Pass> = groups.iter().filter(|(b, _)| !is_carried(b)).map(|(b, ls)| Pass::Plain { base: b.clone(), lanes: ls.clone() }).collect();
    for e in &carried {
        if !e.on_main {
            p1.push(Pass::CarryTip { base: g.out(&["merge-base", &mainsha, &e.tip])?, carry_tip: e.tip.clone(), carried_by: by_order(&e.lanes) });
        }
    }
    let p2: Vec<Pass> = groups.iter().filter(|(b, _)| is_carried(b)).map(|(b, ls)| Pass::Carried { base: b.clone(), lanes: ls.clone() }).collect();
    if !p1.is_empty() {
        passes.push(p1);
    }
    if !p2.is_empty() {
        passes.push(p2);
    }
    // own diffs, carried ranges and main since each merge-base, all as entities
    let mut d: HashMap<String, DiffInfo> = HashMap::new();
    for r in &live {
        d.insert(r.clone(), diffinfo(&g, &l[r].base, &l[r].tip)?);
    }
    let mut owners: HashMap<String, Vec<String>> = HashMap::new();
    let mut pairs: Vec<(String, String)> = vec![];
    for (i, a) in live.iter().enumerate() {
        for b in &live[i + 1..] {
            pairs.push((a.clone(), b.clone()));
        }
    }
    for e in &carried {
        if e.on_main {
            continue;
        }
        let name = format!("carried {}", &e.tip[..9.min(e.tip.len())]);
        let mb = g.out(&["merge-base", &mainsha, &e.tip])?;
        d.insert(name.clone(), diffinfo(&g, &mb, &e.tip)?);
        owners.insert(name.clone(), by_order(&e.lanes));
        for r in &live {
            if !e.lanes.contains(r) {
                pairs.push((name.clone(), r.clone()));
            }
        }
        pairs.push((name.clone(), "main".into()));
        let key = format!("main@{name}");
        if let std::collections::hash_map::Entry::Vacant(v) = d.entry(key) {
            v.insert(diffinfo(&g, &mb, &mainsha)?);
        }
    }
    for r in &live {
        d.insert(format!("main@{r}"), diffinfo(&g, &l[r].merge_base, &mainsha)?);
        pairs.push((r.clone(), "main".into()));
    }

    let mut hot: Vec<Hot> = vec![];
    let mut doubled: Vec<(String, i64, [String; 2])> = vec![];
    for (a, b) in &pairs {
        let da_ = &d[a];
        let db_ = if b == "main" { &d[&format!("main@{a}")] } else { &d[b] };
        for f in da_.touched.intersection(&db_.touched) {
            let (mut kind, mut detail): (Option<&'static str>, Option<String>) = (None, None);
            let (da, db) = (da_.deleted.contains(f), db_.deleted.contains(f));
            if matches(f, &shared) {
                kind = Some("SHARED-HUB");
            } else if da != db {
                kind = Some("DELETE-EDIT");
                detail = Some(format!("{} deletes or renames it", if da { a } else { b }));
            } else {
                let mut ov: Vec<(&Hunk, &Hunk)> = vec![];
                for x in hunks_of(&da_.hunks, f) {
                    for y in hunks_of(&db_.hunks, f) {
                        if span(x).0.max(span(y).0) <= span(x).1.min(span(y).1) && (&x.added, &x.removed) != (&y.added, &y.removed) {
                            ov.push((x, y));
                        }
                    }
                }
                if !ov.is_empty() {
                    kind = Some("SAME-LINES");
                    let set: BTreeSet<String> = ov
                        .iter()
                        .map(|(x, _)| if x.old_count > 1 { format!("{}-{}", x.old_start, x.old_start + x.old_count - 1) } else { x.old_start.to_string() })
                        .collect();
                    let mut v: Vec<String> = set.into_iter().collect();
                    v.sort_by_key(|t| t.split('-').next().unwrap().parse::<i64>().unwrap_or(0));
                    detail = Some(format!("lines {}", v.join(", ")));
                    for (x, y) in &ov {
                        if x.old_count <= 1 && y.old_count <= 1 && x.added.len() <= 1 && y.added.len() <= 1 && x.old_start == y.old_start {
                            doubled.push((f.clone(), x.old_start, [a.clone(), b.clone()]));
                        }
                    }
                } else if !da {
                    kind = Some("CODE-BOTH");
                }
            }
            if let Some(k) = kind {
                hot.push(Hot { kind: k, file: f.clone(), lanes: [a.clone(), b.clone()], detail, action: action(k) });
            }
        }
    }
    hot.sort_by(|x, y| (rank(x.kind), &x.file, &x.lanes).cmp(&(rank(y.kind), &y.file, &y.lanes)));

    // resource clashes and paired counts
    let custom = !(resources.len() == 1 && resources[0].0 == "port" && resources[0].1 == DEFAULT_PORT_RX);
    let mut pats: Vec<(String, Regex)> = vec![];
    for (n, rx) in &resources {
        pats.push((n.clone(), Regex::new(rx).map_err(|e| GitError(format!("bad regex for {n}: {e}")))?));
    }
    let mut found: BTreeMap<(String, String), BTreeMap<String, BTreeSet<String>>> = BTreeMap::new();
    for r in &live {
        for (f, hs) in &d[r].hunks {
            for (name, rx) in &pats {
                if !custom && !f.contains("gate") {
                    continue;
                }
                for h in hs {
                    for line in &h.added {
                        for m in rx.captures_iter(line) {
                            let v = if rx.captures_len() > 1 { m.get(1).map(|x| x.as_str()).unwrap_or("") } else { m.get(0).unwrap().as_str() };
                            found.entry((name.clone(), v.to_string())).or_default().entry(r.clone()).or_default().insert(f.clone());
                        }
                    }
                }
            }
        }
    }
    let clashes: Vec<(String, String, Vec<String>, Vec<String>)> = found
        .iter()
        .filter(|(_, dd)| dd.len() > 1)
        .map(|((n, v), dd)| (n.clone(), v.clone(), live.iter().filter(|x| dd.contains_key(*x)).cloned().collect(), dd.values().flatten().cloned().collect::<BTreeSet<_>>().into_iter().collect()))
        .collect();

    let paired_line = Regex::new(r"EXPECTED|expected exactly|const [A-Z_]+ = [0-9]+").unwrap();
    let digits = Regex::new(r"\d+").unwrap();
    let const_name = Regex::new(r"const ([A-Z_]+) =").unwrap();
    let any_name = Regex::new(r"\b([A-Z][A-Z0-9_]{2,})\b").unwrap();
    let mut paired = vec![];
    for r in &live {
        let tip = &l[r].tip;
        for (f, hs) in &d[r].hunks {
            for h in hs {
                for (old, new) in h.removed.iter().zip(h.added.iter()) {
                    if !paired_line.is_match(new) {
                        continue;
                    }
                    let on: Vec<&str> = digits.find_iter(old).map(|m| m.as_str()).collect();
                    let nn: Vec<&str> = digits.find_iter(new).map(|m| m.as_str()).collect();
                    if on.len() != nn.len() || on == nn {
                        continue;
                    }
                    let Some(nm) = const_name.captures(new).or_else(|| any_name.captures(new)) else { continue };
                    let name = nm[1].to_string();
                    let (o, n_) = on.iter().zip(nn.iter()).find(|(x, y)| x != y).map(|(x, y)| (x.to_string(), y.to_string())).unwrap();
                    let out = g.run(&["grep", "-n", "-I", "-F", &name, tip, "--"], &[0, 1])?;
                    let mut seen: HashSet<String> = HashSet::new();
                    for gl in out.lines() {
                        let parts: Vec<&str> = gl.splitn(4, ':').collect();
                        if parts.len() < 4 || parts[1] == f || seen.contains(parts[1]) || !matches(parts[1], &TEST_FILE) || matches(parts[1], &NOT_CHECKS) {
                            continue;
                        }
                        if has_number(parts[3], &o) {
                            seen.insert(parts[1].to_string());
                            paired.push(Paired { lane: r.clone(), name: name.clone(), old: o.clone(), new: n_.clone(), file: f.clone(), stale_in: parts[1].to_string(), line: parts[2].parse().unwrap_or(0) });
                        }
                    }
                }
            }
        }
    }
    // baseline radar: a lane adds routes or tables and touches none of the configured shrink-only / must-join list files
    let mut base_warn = vec![];
    if let Some(bl) = opt.baseline.as_ref().filter(|b| !b.is_empty()) {
        let pats: Vec<String> = match &opt.new_item {
            Some(n) if !n.is_empty() => n.clone(),
            _ => DEFAULT_NEW_ITEM.iter().map(|s| s.to_string()).collect(),
        };
        let mut rxs = vec![];
        for p in &pats {
            rxs.push(Regex::new(p).map_err(|e| GitError(format!("bad regex {p}: {e}")))?);
        }
        for r in &live {
            let mut adds: Vec<String> = vec![];
            for (f, hs) in &d[r].hunks {
                if matches(f, bl) || matches(f, &NOT_CHECKS) {
                    continue;
                }
                for h in hs {
                    for ln in &h.added {
                        if rxs.iter().any(|x| x.is_match(ln)) {
                            adds.push(ln.trim().to_string());
                        }
                    }
                }
            }
            if !adds.is_empty() && !d[r].touched.iter().any(|f| matches(f, bl)) {
                let n = adds.len();
                adds.truncate(5);
                base_warn.push((r.clone(), adds, n));
            }
        }
    }
    // plan: greedy colouring of the conflict graph
    let mut edges: Vec<(String, BTreeSet<String>)> = live.iter().map(|r| (r.clone(), BTreeSet::new())).collect();
    let ei = |edges: &Vec<(String, BTreeSet<String>)>, r: &str| edges.iter().position(|(k, _)| k == r).unwrap();
    for h in &hot {
        if h.kind != "SHARED-HUB" && !h.lanes.iter().any(|x| x == "main") {
            let own = |x: &String| owners.get(x).cloned().unwrap_or_else(|| vec![x.clone()]);
            for a in own(&h.lanes[0]) {
                for b in own(&h.lanes[1]) {
                    let (ia, ib) = (ei(&edges, &a), ei(&edges, &b));
                    edges[ia].1.insert(b.clone());
                    edges[ib].1.insert(a.clone());
                }
            }
        }
    }
    let mut colour: HashMap<String, usize> = HashMap::new();
    let mut ord: Vec<usize> = (0..live.len()).collect();
    ord.sort_by_key(|&i| (std::cmp::Reverse(edges[i].1.len()), i));
    for i in ord {
        let used: HashSet<usize> = edges[i].1.iter().filter_map(|n| colour.get(n).copied()).collect();
        let mut c = 0;
        while used.contains(&c) {
            c += 1;
        }
        colour.insert(live[i].clone(), c);
    }
    let ngroups = colour.values().max().map(|m| m + 1).unwrap_or(0);
    let groups_out: Vec<Vec<String>> = (0..ngroups).map(|c| live.iter().filter(|r| colour[*r] == c).cloned().collect()).collect();
    let conflicts = hot.iter().filter(|h| rank(h.kind) == 0).count();
    let hubs = hot.iter().filter(|h| h.kind == "SHARED-HUB").count();
    let summary = [live.len(), ngroups, conflicts, hubs, clashes.len()];
    let summary_line = format!("{} lanes: {} independent groups; {} certain conflicts; {} shared hubs; {} resource clashes", live.len(), ngroups, conflicts, hubs, clashes.len());
    let lanes_out: Vec<LaneState> = order.iter().map(|r| l[r].clone()).collect();
    Ok(Report {
        main: main.to_string(),
        lanes: lanes_out,
        contained,
        carried: carried_out,
        passes,
        hot,
        doubled,
        clashes,
        paired,
        base_warn,
        groups: groups_out,
        edges: edges.into_iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k, v.into_iter().collect())).collect(),
        summary,
        summary_line,
    })
}

impl Report {
    pub fn to_json(&self) -> J {
        let ss = |v: &[String]| J::strs(v);
        let obj = |v: Vec<(&str, J)>| J::Obj(v.into_iter().map(|(k, x)| (k.to_string(), x)).collect());
        obj(vec![
            ("main", J::s(&self.main)),
            (
                "lanes",
                J::Arr(
                    self.lanes
                        .iter()
                        .map(|x| {
                            obj(vec![
                                ("lane", J::s(&x.lane)),
                                ("tip", J::s(&x.tip)),
                                ("merge_base", J::s(&x.merge_base)),
                                ("own_commits", J::Int(x.own_commits as i64)),
                                ("base", J::s(&x.base)),
                                ("contained_in", J::opt(&x.contained_in)),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("contained", J::Arr(self.contained.iter().map(|(a, b)| obj(vec![("lane", J::s(a)), ("in", J::s(b))])).collect())),
            (
                "carried",
                J::Arr(self.carried.iter().map(|(t, c, ls, om)| obj(vec![("tip", J::s(t)), ("commits", J::Int(*c as i64)), ("lanes", ss(ls)), ("on_main", J::Bool(*om))])).collect()),
            ),
            (
                "passes",
                J::Arr(
                    self.passes
                        .iter()
                        .map(|p| {
                            J::Arr(
                                p.iter()
                                    .map(|x| match x {
                                        Pass::Plain { base, lanes } => obj(vec![("base", J::s(base)), ("lanes", ss(lanes))]),
                                        Pass::CarryTip { base, carry_tip, carried_by } => {
                                            obj(vec![("base", J::s(base)), ("lanes", J::Arr(vec![])), ("carry_tip", J::s(carry_tip)), ("carried_by", ss(carried_by))])
                                        }
                                        Pass::Carried { base, lanes } => obj(vec![("base", J::s(base)), ("lanes", ss(lanes)), ("carried", J::Bool(true))]),
                                    })
                                    .collect(),
                            )
                        })
                        .collect(),
                ),
            ),
            (
                "hotspots",
                J::Arr(
                    self.hot
                        .iter()
                        .map(|h| obj(vec![("kind", J::s(h.kind)), ("file", J::s(&h.file)), ("lanes", ss(&h.lanes)), ("detail", J::opt(&h.detail)), ("action", J::s(h.action))]))
                        .collect(),
                ),
            ),
            (
                "doubled_line_risk",
                J::Arr(
                    self.doubled
                        .iter()
                        .map(|(f, n, ls)| {
                            obj(vec![("file", J::s(f)), ("line", J::Int(*n)), ("lanes", ss(ls)), ("action", J::s("likely doubled line after stacking: keep one, check it appears once"))])
                        })
                        .collect(),
                ),
            ),
            (
                "resource_clashes",
                J::Arr(
                    self.clashes
                        .iter()
                        .map(|(n, v, ls, fs)| {
                            obj(vec![("name", J::s(n)), ("value", J::s(v)), ("lanes", ss(ls)), ("files", ss(fs)), ("action", J::s("give each lane its own value before the gates run in parallel"))])
                        })
                        .collect(),
                ),
            ),
            (
                "paired_counts",
                J::Arr(
                    self.paired
                        .iter()
                        .map(|p| {
                            obj(vec![
                                ("lane", J::s(&p.lane)),
                                ("name", J::s(&p.name)),
                                ("old", J::s(&p.old)),
                                ("new", J::s(&p.new)),
                                ("file", J::s(&p.file)),
                                ("stale_in", J::s(&p.stale_in)),
                                ("line", J::Int(p.line)),
                                ("heuristic", J::Bool(true)),
                                ("action", J::s(format!("count changed here but its check still says {}: update {}:{}", p.old, p.stale_in, p.line))),
                            ])
                        })
                        .collect(),
                ),
            ),
            (
                "baseline_warnings",
                J::Arr(
                    self.base_warn
                        .iter()
                        .map(|(r, adds, n)| {
                            obj(vec![
                                ("lane", J::s(r)),
                                ("adds", ss(adds)),
                                ("count", J::Int(*n as i64)),
                                ("action", J::s(BASELINE_ACTION)),
                            ])
                        })
                        .collect(),
                ),
            ),
            ("groups", J::Arr(self.groups.iter().map(|g| ss(g)).collect())),
            ("conflict_edges", J::Obj(self.edges.iter().map(|(k, v)| (k.clone(), ss(v))).collect())),
            (
                "summary",
                obj(vec![
                    ("lanes", J::Int(self.summary[0] as i64)),
                    ("independent_groups", J::Int(self.summary[1] as i64)),
                    ("certain_conflicts", J::Int(self.summary[2] as i64)),
                    ("shared_hubs", J::Int(self.summary[3] as i64)),
                    ("resource_clashes", J::Int(self.summary[4] as i64)),
                ]),
            ),
            ("summary_line", J::s(&self.summary_line)),
        ])
    }

    pub fn json(&self) -> String {
        dumps_indent2(&self.to_json())
    }

    pub fn render(&self) -> String {
        let s = |x: &str| x.chars().take(9).collect::<String>();
        let mut o: Vec<String> = vec![format!("lanes against {}", self.main), String::new(), "1. bases and containment".into()];
        for l in &self.lanes {
            if let Some(c) = &l.contained_in {
                o.push(format!("   {}: contained in {}: list only {}", l.lane, c, c));
            } else {
                o.push(format!(
                    "   {}: merge-base {}, {} own commits{}{}",
                    l.lane,
                    s(&l.merge_base),
                    l.own_commits,
                    if l.base != l.merge_base { format!(", base for stacking {}", s(&l.base)) } else { String::new() },
                    if l.own_commits == 0 { "  (no own commits: nothing to stack)" } else { "" }
                ));
            }
        }
        for (tip, commits, lanes, on_main) in &self.carried {
            if *on_main {
                o.push(format!(
                    "   carried commits up to {} are already on main (by patch) ({}): use {} as lane base and do NOT re-apply them",
                    s(tip),
                    lanes.join(" and "),
                    s(tip)
                ));
            } else {
                o.push(format!("   both carry {} commits up to {} ({}): stack with that tip as lane base", commits, s(tip), lanes.join(" and ")));
            }
        }
        for (i, p) in self.passes.iter().enumerate() {
            let i = i + 1;
            for x in p {
                match x {
                    Pass::CarryTip { base, carry_tip, carried_by } => o.push(format!("   pass {i}: stack the carried commits up to {} (base {}) on main, once, for {}", s(carry_tip), s(base), carried_by.join(" and "))),
                    Pass::Carried { base, lanes } => o.push(format!("   pass {i}: base {} (carried tip, after pass 1): {}", s(base), lanes.join(", "))),
                    Pass::Plain { base, lanes } => o.push(format!("   pass {i}: base {} (on main): {}", s(base), lanes.join(", "))),
                }
            }
        }
        o.push(String::new());
        o.push("2. collision hotspots".into());
        if self.hot.is_empty() {
            o.push("   none".into());
        }
        let mut cb: Vec<(String, Vec<String>)> = vec![];
        for h in self.hot.iter().filter(|h| h.kind == "CODE-BOTH") {
            let i = match cb.iter().position(|(f, _)| *f == h.file) {
                Some(i) => i,
                None => {
                    cb.push((h.file.clone(), vec![]));
                    cb.len() - 1
                }
            };
            for x in &h.lanes {
                if !cb[i].1.contains(x) {
                    cb[i].1.push(x.clone());
                }
            }
        }
        let mut shown: HashSet<&str> = HashSet::new();
        for h in &self.hot {
            if h.kind == "CODE-BOTH" {
                if shown.insert(&h.file) {
                    let ls = &cb.iter().find(|(f, _)| *f == h.file).unwrap().1;
                    o.push(format!("   CODE-BOTH   {}: lanes {} -> {}", h.file, ls.join(", "), h.action));
                }
                continue;
            }
            o.push(format!("   {:<11} {}  [{}]{} -> {}", h.kind, h.file, h.lanes.join(" x "), h.detail.as_ref().map(|d| format!(" {d}")).unwrap_or_default(), h.action));
        }
        o.push(String::new());
        o.push("3. doubled-line risk".into());
        if self.doubled.is_empty() {
            o.push("   none".into());
        }
        for (f, n, ls) in &self.doubled {
            o.push(format!("   {}:{} [{}] -> likely doubled line after stacking: keep one, check it appears once", f, n, ls.join(" x ")));
        }
        o.push(String::new());
        o.push("4. resource clashes".into());
        if self.clashes.is_empty() {
            o.push("   none".into());
        }
        for (n, v, ls, fs) in &self.clashes {
            o.push(format!("   {}={} introduced by {} ({}) -> give each lane its own value before the gates run in parallel", n, v, ls.join(" and "), fs.join(", ")));
        }
        o.push(String::new());
        o.push("5. paired counts (heuristic)".into());
        if self.paired.is_empty() {
            o.push("   none".into());
        }
        for p in &self.paired {
            o.push(format!(
                "   {}: {} {} -> {} in {}; count changed here but its check still says {} in {}:{} -> update it",
                p.lane, p.name, p.old, p.new, p.file, p.old, p.stale_in, p.line
            ));
        }
        if !self.base_warn.is_empty() {
            o.push(String::new());
            o.push("5b. baseline radar (heuristic)".into());
            for (r, adds, n) in &self.base_warn {
                o.push(format!("   {}: adds {} route/table line(s), e.g. {} -> {}", r, n, adds[0], BASELINE_ACTION));
            }
        }
        o.push(String::new());
        o.push("6. parallel plan".into());
        for (i, gp) in self.groups.iter().enumerate() {
            o.push(format!("   group {}: {}{}", i + 1, gp.join(", "), if i == 0 { "" } else { "  (after the groups above; conflicts with them)" }));
        }
        o.push(self.summary_line.clone());
        o.join("\n")
    }
}

const BASELINE_ACTION: &str = "this lane adds a route or table but touches no baseline file: add it to the list it must join, with a reason, in the same commit (a red `baseline-missing` otherwise)";

/// `--resource-regex NAME=REGEX` items; `Err` is the message of the Python `SystemExit`.
pub fn parse_resource(items: &[String]) -> Result<Option<Vec<(String, String)>>, String> {
    let mut out: Vec<(String, String)> = vec![];
    for it in items {
        let Some((n, rx)) = it.split_once('=') else { return Err(format!("--resource-regex wants NAME=REGEX, got {}", crate::pycompat::repr_str(it))) };
        if let Err(e) = Regex::new(rx) {
            return Err(format!("bad regex for {n}: {e}"));
        }
        match out.iter_mut().find(|(k, _)| k == n) {
            Some(e) => e.1 = rx.to_string(),
            None => out.push((n.to_string(), rx.to_string())),
        }
    }
    Ok(if out.is_empty() { None } else { Some(out) })
}

/// Split a `--shared-files` / `--baseline-files` comma list.
pub fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}
