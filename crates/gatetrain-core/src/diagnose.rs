//! The red classifier (`gatetrain diagnose`). Byte parity with gatetrain/diagnose.py is required: the same failure-block
//! splitting, the same rule order, the same verdicts and summary lines. The rules are DATA: rules/red-classes.toml, shared
//! with Python and embedded here. A red no rule explains is `real`, never silently a flake (lessons L52-L58).
use crate::logfmt::{splitlines, ParsedLog, Step};
use regex::{Regex, RegexBuilder};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const SLOW_FACTOR: f64 = 3.0;
const TAIL: usize = 30;
const REAL_REMEDY: &str = "a real failure until shown otherwise: read the evidence, write a failing test, fix it on the lane";
const BUILTIN_RULES: &str = include_str!("../../../rules/red-classes.toml");

#[derive(Debug, thiserror::Error)]
#[error("rules: {0}")]
pub struct RuleError(pub String);

/// One row of the rule table (keys as documented in rules/red-classes.toml).
#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    pub cls: String,
    pub pattern: Regex,
    pub absent: Option<Regex>,
    pub also: Option<Regex>,
    pub slow: bool,
    pub shrink: bool,
    pub remedy: String,
    pub rerun: bool,
}

/// Per step name, the durations of earlier green runs (from the ledger); empty = judge against the log's own steps.
pub type History = HashMap<String, Vec<f64>>;

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub step: String,
    pub rc: Option<i64>,
    pub cls: String,
    pub evidence: String,
    pub remedy: String,
    pub rerun: bool,
    pub rule: String,
    pub block: String,
}

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn rx(rule: &str, key: &str, p: &str) -> Result<Regex, RuleError> {
    RegexBuilder::new(p).multi_line(true).build().map_err(|e| RuleError(format!("rule {rule:?} {key}: {e}")))
}

/// Rules from a JSON list (the `--rules` file) or the `rule` array of a TOML table, already read into a `Value`.
pub fn rules_from_value(v: &Value) -> Result<Vec<Rule>, RuleError> {
    let list = match v {
        Value::Array(a) => a,
        Value::Object(o) => o.get("rule").and_then(Value::as_array).ok_or_else(|| RuleError("no [[rule]] table".into()))?,
        _ => return Err(RuleError("a list of rules is expected".into())),
    };
    list.iter()
        .map(|r| {
            let get = |k: &str| r.get(k);
            let text = |k: &str| -> Result<String, RuleError> {
                get(k).and_then(Value::as_str).map(str::to_string).ok_or_else(|| RuleError(format!("a rule needs a string `{k}`")))
            };
            let id = get("id").and_then(Value::as_str).unwrap_or("").to_string();
            let opt = |k: &str| -> Result<Option<Regex>, RuleError> {
                match get(k).filter(|x| truthy(x)) {
                    Some(x) => Ok(Some(rx(&id, k, x.as_str().ok_or_else(|| RuleError(format!("`{k}` must be a string")))?)?)),
                    None => Ok(None),
                }
            };
            Ok(Rule {
                cls: text("cls")?,
                pattern: rx(&id, "pattern", &text("pattern")?)?,
                absent: opt("absent")?,
                also: opt("also")?,
                slow: get("when").and_then(Value::as_str) == Some("slow"),
                shrink: get("shrink").map(truthy).unwrap_or(false),
                remedy: text("remedy")?,
                rerun: get("rerun").map(truthy).ok_or_else(|| RuleError("a rule needs `rerun`".into()))?,
                id,
            })
        })
        .collect()
}

/// The shared table, rules/red-classes.toml, as embedded at build time.
pub fn builtin_rules() -> &'static [Rule] {
    static R: OnceLock<Vec<Rule>> = OnceLock::new();
    R.get_or_init(|| {
        let v: Value = toml::from_str(BUILTIN_RULES).expect("rules/red-classes.toml is valid TOML");
        rules_from_value(&v).expect("rules/red-classes.toml holds valid rules")
    })
}

/// Python `str.strip()` whitespace: Unicode white space plus the separators \x1c-\x1f.
fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}
fn py_strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}
fn py_rstrip(s: &str) -> &str {
    s.trim_end_matches(is_py_space)
}
fn py_lstrip(s: &str) -> &str {
    s.trim_start_matches(is_py_space)
}

/// Lines between a `=== <step>` banner and the next marker line, per step (a banner names its step).
pub fn step_output(text: &str) -> HashMap<String, Vec<String>> {
    static MARK: OnceLock<Regex> = OnceLock::new();
    let mark = MARK.get_or_init(|| Regex::new(r"^(?:STEP_|RED_|GATES_DONE|CHAIN_|branch )").unwrap());
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    let mut cur: Option<String> = None;
    for line in splitlines(text) {
        if let Some(rest) = line.strip_prefix("=== ") {
            let name = py_strip(rest).to_string();
            out.entry(name.clone()).or_default();
            cur = Some(name);
        } else if mark.is_match(line) {
            if !line.starts_with("STEP_START") {
                continue;
            }
            cur = None;
        } else if let Some(c) = &cur {
            out.get_mut(c).unwrap().push(py_rstrip(line).to_string());
        }
    }
    out
}

struct BlockRe {
    list_head: Regex,
    bullet: Regex,
    start: Regex,
    fail_list: Regex,
    end: Regex,
}

fn block_re() -> &'static BlockRe {
    static B: OnceLock<BlockRe> = OnceLock::new();
    B.get_or_init(|| BlockRe {
        list_head: Regex::new(r"^\s*FAIL\s+\d+\s+problems?(?:\(s\))?\s*:").unwrap(),
        bullet: Regex::new(r"^\s+(?:[-*•]|\d+[.)])\s+\S").unwrap(),
        // Python: FAIL(?:ED)?\s+(?!\d+\s+problem)\S ; the lookahead is the separate `fail_list` test below
        start: Regex::new(r"^\s*(?:FAIL(?:ED)?\s+\S|✖\s|not ok\s|---- .+ stdout ----\s*$|thread '.*' panicked at)").unwrap(),
        fail_list: Regex::new(r"^\s*FAIL(?:ED)?\s+\d+\s+problem").unwrap(),
        end: Regex::new(r"^\s*(?:⎯{3,}|Test Files\s|Tests\s|Duration\s|test result:|failures:|✔\s|✓\s|ok \d+)").unwrap(),
    })
}

fn is_start(ln: &str) -> bool {
    let b = block_re();
    b.start.is_match(ln) && !b.fail_list.is_match(ln)
}

/// Split a red step's output into FAILURE BLOCKS: vitest `FAIL  file > test`, node:test `✖ name` / `not ok`, cargo
/// `---- test stdout ----` / `panicked at`, and the items of a `FAIL  N problem(s):` list. Text outside a block can explain
/// nothing. Output with no recognisable block is one block: its last lines.
pub fn failure_blocks(lines: &[String]) -> Vec<Vec<String>> {
    let b = block_re();
    let mut blocks: Vec<Vec<String>> = Vec::new();
    let mut cur: Option<usize> = None; // index into blocks
    let mut in_list = false;
    for ln in lines {
        let ln = ln.as_str();
        if b.list_head.is_match(ln) {
            cur = None;
            in_list = true;
        } else if in_list && b.bullet.is_match(ln) {
            blocks.push(vec![ln.to_string()]);
            cur = Some(blocks.len() - 1);
        } else if let (true, Some(c), true) = (in_list, cur, (ln.starts_with(' ') || ln.starts_with('\t')) && !py_strip(ln).is_empty()) {
            blocks[c].push(ln.to_string());
        } else if is_start(ln)
            && !(cur.is_some_and(|c| py_lstrip(ln).starts_with("thread ") && blocks[c][0].starts_with("----")))
        {
            blocks.push(vec![ln.to_string()]);
            cur = Some(blocks.len() - 1);
            in_list = false;
        } else if b.end.is_match(ln) {
            cur = None;
            in_list = false;
        } else if let Some(c) = cur {
            blocks[c].push(ln.to_string());
        } else if in_list && py_strip(ln).is_empty() {
            in_list = false;
        }
    }
    if blocks.is_empty() {
        let nonblank: Vec<&String> = lines.iter().filter(|x| !py_strip(x).is_empty()).collect();
        let tail: Vec<String> = nonblank[nonblank.len().saturating_sub(TAIL)..].iter().map(|s| s.to_string()).collect();
        return if tail.is_empty() { vec![] } else { vec![tail] };
    }
    blocks
}

fn median(v: &[f64]) -> f64 {
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

fn is_slow(idx: usize, steps: &[Step], history: &History) -> bool {
    let step = &steps[idx];
    let mut rf: Vec<f64> = history.get(&step.name).cloned().unwrap_or_default();
    if rf.is_empty() {
        rf = steps
            .iter()
            .enumerate()
            .filter(|(i, s)| *i != idx && s.duration.is_some() && matches!(s.exit_code, Some(0) | None))
            .map(|(_, s)| s.duration.unwrap())
            .collect();
    }
    let m = if rf.is_empty() { 0.0 } else { median(&rf) };
    matches!(step.duration, Some(d) if d != 0.0 && m > 0.0 && d > SLOW_FACTOR * m)
}

fn line_of(text: &str, pos: usize) -> &str {
    let a = text[..pos].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let b = text[pos..].find('\n').map(|i| pos + i).unwrap_or(text.len());
    py_strip(&text[a..b])
}

/// Python `int(a) < int(b)` on ASCII digit runs of any length.
fn digits_less(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_start_matches('0'), b.trim_start_matches('0'));
    (a.len(), a) < (b.len(), b)
}

/// A rule's match inside one block: pattern, then absent, also and shrink, all within the block. The start of the match.
fn rule_match(body: &str, r: &Rule) -> Option<usize> {
    let caps = r.pattern.captures(body)?;
    if r.absent.as_ref().is_some_and(|a| a.is_match(body)) {
        return None;
    }
    if r.also.as_ref().is_some_and(|a| !a.is_match(body)) {
        return None;
    }
    if r.shrink {
        let (e, g) = (caps.get(1), caps.get(2));
        match (e, g) {
            (Some(e), Some(g)) if digits_less(g.as_str(), e.as_str()) => {}
            _ => return None,
        }
    }
    Some(caps.get(0).unwrap().start())
}

fn cap100(s: &str) -> String {
    s.chars().take(100).collect()
}

/// The most specific line of a failure block: the message after a Rust `panicked at file:l:c:` line, then an `Error:` or
/// `AssertionError` line, an `error returned from database:` line, a `FAILED —` line, any other line that names an error,
/// and only last a `test result: FAILED` summary. `None` when the block has no such line.
fn best_evidence(blk: &[String]) -> Option<String> {
    static PANIC: OnceLock<Regex> = OnceLock::new();
    static ERRWORD: OnceLock<Regex> = OnceLock::new();
    let panic = PANIC.get_or_init(|| Regex::new(r"panicked at \S+:\d+:\d+:?\s*$").unwrap());
    let errword = ERRWORD.get_or_init(|| Regex::new(r"Error|assert|panick|expected|FAIL|fail").unwrap());
    let lines: Vec<&str> = blk.iter().map(|x| py_strip(x)).collect();
    let is_summary = |x: &str| x.starts_with("test result:");
    if let Some(i) = lines.iter().position(|x| panic.is_match(x)) {
        if let Some(m) = lines[i + 1..].iter().find(|x| !x.is_empty()) {
            return Some(m.to_string());
        }
    }
    let pick = |f: &dyn Fn(&str) -> bool| lines.iter().find(|x| !is_summary(x) && f(x)).map(|x| x.to_string());
    pick(&|x| x.starts_with("Error:") || x.contains("AssertionError"))
        .or_else(|| pick(&|x| x.contains("error returned from database:")))
        .or_else(|| pick(&|x| x.contains("FAILED —")))
        .or_else(|| pick(&|x| errword.is_match(x)))
        .or_else(|| lines.iter().find(|x| errword.is_match(x)).map(|x| x.to_string()))
}

/// The key two reds share when they have one cause: the evidence with counts, times, ids, hex and the head of every absolute
/// path removed, whitespace collapsed, quoted SQL and error text kept.
pub fn cause_key(evidence: &str) -> String {
    static PATH: OnceLock<Regex> = OnceLock::new();
    static HEX: OnceLock<Regex> = OnceLock::new();
    static NUM: OnceLock<Regex> = OnceLock::new();
    let path = PATH.get_or_init(|| Regex::new(r#"(^|[\s"'(=])((?:[A-Za-z]:)?[/\\][\w.\-/\\]+)"#).unwrap());
    let hex = HEX.get_or_init(|| Regex::new(r"\b(?:0x[0-9a-fA-F]+|[0-9a-f]{7,})\b").unwrap());
    let num = NUM.get_or_init(|| Regex::new(r"\b\d+(?:\.\d+)?(?:ms|us|ns|s|m|h|KB|MB|GB|B)?\b").unwrap());
    let s = path.replace_all(evidence, |c: &regex::Captures| {
        let segs: Vec<&str> = c[2].split(['/', '\\']).filter(|x| !x.is_empty()).collect();
        format!("{}{}", &c[1], segs[segs.len().saturating_sub(2)..].join("/"))
    });
    let s = hex.replace_all(&s, "#");
    let s = num.replace_all(&s, "#");
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Red steps that share a cause key: (evidence of the first, step names in order), most steps first, two or more steps only.
/// Also the number of distinct causes among all red blocks.
pub fn shared_causes(findings: &[Finding]) -> (Vec<(String, Vec<String>)>, usize) {
    let mut order: Vec<String> = Vec::new();
    let mut by: HashMap<String, (String, Vec<String>)> = HashMap::new();
    for f in findings {
        let k = cause_key(&f.evidence);
        let e = by.entry(k.clone()).or_insert_with(|| {
            order.push(k.clone());
            (f.evidence.clone(), Vec::new())
        });
        if !e.1.contains(&f.step) {
            e.1.push(f.step.clone());
        }
    }
    let total = order.len();
    let mut v: Vec<(String, Vec<String>)> = order.into_iter().map(|k| by.remove(&k).unwrap()).filter(|(_, s)| s.len() >= 2).collect();
    v.sort_by_key(|a| std::cmp::Reverse(a.1.len())); // stable: ties keep first-seen order
    (v, total)
}

fn mk(step: &Step, cls: &str, evidence: String, remedy: &str, rerun: bool, rule: &str, block: String) -> Finding {
    Finding { step: step.name.clone(), rc: step.exit_code, cls: cls.into(), evidence, remedy: remedy.into(), rerun, rule: rule.into(), block }
}

/// One finding per failure block. A rule applies inside ONE block. A step whose every block is a timeout, with no assertion in
/// any block, on a slow step, is one `flake-load` finding. A block no rule explains is `real`.
pub fn classify(idx: usize, lines: &[String], steps: &[Step], history: &History, rules: &[Rule]) -> Vec<Finding> {
    let step = &steps[idx];
    // a third-party advisory (cargo-deny, cargo-audit) is its own class: it is not this batch's defect, and a person decides
    // what to do with it. It is reported with its ids and never ignored or retried by gatetrain.
    let mut advisory: Option<Finding> = None;
    if let Some(r) = rules.iter().find(|r| r.id == "advisory") {
        let hit = lines.iter().find(|l| r.pattern.is_match(l));
        if let Some(first) = hit {
            static ID: OnceLock<Regex> = OnceLock::new();
            let id = ID.get_or_init(|| Regex::new(r"\b(?:RUSTSEC-\d{4}-\d{4}|GHSA(?:-[0-9a-z]{4}){3})\b").unwrap());
            let mut ids: Vec<String> = Vec::new();
            for l in lines {
                for m in id.find_iter(l) {
                    if !ids.iter().any(|x| x == m.as_str()) {
                        ids.push(m.as_str().to_string());
                    }
                }
            }
            let ev = if ids.is_empty() { py_strip(first).to_string() } else { ids.join(", ") };
            advisory = Some(mk(step, &r.cls, ev, &r.remedy, r.rerun, &r.id, cap100(py_strip(first))));
            if !lines.iter().any(|l| is_start(l)) {
                return vec![advisory.unwrap()];
            }
        }
    }
    // the advisory row has been applied above; the block loop must not match it again
    let others: Vec<Rule> = rules.iter().filter(|r| r.id != "advisory").cloned().collect();
    let mut found = classify_blocks(idx, lines, steps, history, &others);
    if let Some(a) = advisory {
        found.insert(0, a);
    }
    found
}

fn classify_blocks(idx: usize, lines: &[String], steps: &[Step], history: &History, rules: &[Rule]) -> Vec<Finding> {
    let step = &steps[idx];
    let mut blocks = failure_blocks(lines);
    let mut seen: Vec<Vec<String>> = Vec::new();
    blocks.retain(|b| !seen.contains(b) && { seen.push(b.clone()); true });
    let mut out: Vec<Option<Finding>> = Vec::new();
    let mut timeouts: Vec<(usize, &Rule, usize, String)> = Vec::new(); // (block index, rule, match start, body)
    for (bi, blk) in blocks.iter().enumerate() {
        let body = blk.join("\n");
        let hit = rules.iter().find_map(|r| rule_match(&body, r).map(|s| (r, s)));
        let title = cap100(blk.iter().map(|x| py_strip(x)).find(|x| !x.is_empty()).unwrap_or(""));
        match hit {
            Some((r, s)) if r.slow => {
                timeouts.push((bi, r, s, body));
                out.push(None);
            }
            Some((r, s)) => out.push(Some(mk(step, &r.cls, line_of(&body, s).to_string(), &r.remedy, r.rerun, &r.id, title))),
            None => {
                let ev = best_evidence(blk).unwrap_or_else(|| title.clone());
                let ev = if ev.is_empty() { "(no output captured)".to_string() } else { ev };
                out.push(Some(mk(step, "real", ev, REAL_REMEDY, false, "default", title)));
            }
        }
    }
    if !timeouts.is_empty() {
        let every = timeouts.len() == blocks.len();
        if every && is_slow(idx, steps, history) {
            let (_, r, s, body) = &timeouts[0];
            return vec![mk(step, &r.cls, line_of(body, *s).to_string(), &r.remedy, r.rerun, &r.id, format!("{} timeout block(s)", timeouts.len()))];
        }
        for (k, o) in out.iter_mut().enumerate() {
            if o.is_none() {
                let blk = &blocks[k];
                let ev = blk.iter().find(|x| x.to_lowercase().contains("timed out")).map(|x| py_strip(x)).unwrap_or_else(|| py_strip(&blk[0]));
                let why = if !every {
                    "a timeout next to a failed assertion is not a load flake"
                } else {
                    "a timeout with no load evidence (the step is not far above its median)"
                };
                *o = Some(mk(step, "real", ev.to_string(), &format!("{REAL_REMEDY}; {why}"), false, "default", cap100(py_strip(&blk[0]))));
            }
        }
    }
    let v: Vec<Finding> = out.into_iter().flatten().collect();
    if v.is_empty() {
        vec![mk(step, "real", "(no output captured for this step)".into(), REAL_REMEDY, false, "default", String::new())]
    } else {
        v
    }
}

pub fn diagnose_log(text: &str, p: &ParsedLog, history: &History, rules: &[Rule]) -> Vec<Finding> {
    diagnose_log_in(text, p, history, rules, None)
}

/// The folder a `chain` run named in its `CHAIN_OUT` line, if the log has one.
pub fn chain_out_of(text: &str) -> Option<PathBuf> {
    splitlines(text).into_iter().find_map(|l| l.strip_prefix("CHAIN_OUT ").map(|d| PathBuf::from(py_strip(d)))).filter(|d| !d.as_os_str().is_empty())
}

/// At most this much of the end of a `.full` file is read: the evidence of a red is at its end.
const FULL_TAIL_BYTES: u64 = 4 * 1024 * 1024;

fn read_tail_lines(path: &Path) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let cut = len > FULL_TAIL_BYTES;
    if cut && f.seek(SeekFrom::Start(len - FULL_TAIL_BYTES)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.take(FULL_TAIL_BYTES).read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = splitlines(&text).into_iter().map(|l| py_rstrip(l).to_string()).collect();
    if cut && !lines.is_empty() {
        lines.remove(0); // a partial first line
    }
    lines
}

/// The output of a red step of a `chain` run, from its `.full` files in `dir` (engine v2 sections 5 and 6): `<step>.full`, or for a
/// sharded step the files of its red shards (named by its `STEP_SHARD <step> i/n EXIT rc` lines; none red = the `shard_check` file).
pub fn full_output(dir: &Path, step: &str, text: &str) -> Vec<String> {
    let head = format!("STEP_SHARD {step} ");
    let mut shards: Vec<(String, i64)> = Vec::new(); // (file, rc)
    for l in splitlines(text) {
        let Some(rest) = l.strip_prefix(head.as_str()) else { continue };
        let mut w = rest.split_whitespace();
        if let (Some(frac), Some("EXIT"), Some(rc)) = (w.next(), w.next(), w.next().and_then(|r| r.parse::<i64>().ok())) {
            if let Some((i, n)) = frac.split_once('/') {
                shards.push((format!("{step}.{i}of{n}.full"), rc));
            }
        }
    }
    if shards.is_empty() {
        return read_tail_lines(&dir.join(format!("{step}.full")));
    }
    let red: Vec<&(String, i64)> = shards.iter().filter(|(_, rc)| *rc != 0).collect();
    if red.is_empty() {
        return read_tail_lines(&dir.join(format!("{step}.check.full")));
    }
    red.iter().flat_map(|(f, _)| read_tail_lines(&dir.join(f))).collect()
}

/// Like [`diagnose_log`]; a red step with no `=== <step>` output in the log is read from its `.full` file in `out`, or else in the folder
/// the log's own `CHAIN_OUT` line names.
pub fn diagnose_log_in(text: &str, p: &ParsedLog, history: &History, rules: &[Rule], out: Option<&Path>) -> Vec<Finding> {
    let outs = step_output(text);
    let dir: Option<PathBuf> = out.map(Path::to_path_buf).or_else(|| chain_out_of(text)).filter(|d| d.is_dir());
    let mut res = Vec::new();
    for (i, s) in p.steps.iter().enumerate() {
        if !matches!(s.exit_code, None | Some(0)) {
            let banner = outs.get(&s.name).filter(|v| !v.is_empty());
            let lines = match (banner, &dir) {
                (Some(b), _) => b.clone(),
                (None, Some(d)) => full_output(d, &s.name, text),
                (None, None) => Vec::new(),
            };
            res.extend(classify(i, &lines, &p.steps, history, rules));
        }
    }
    res
}

/// Max number of timed steps running at once (analysis.concurrency, first value).
fn max_concurrency(steps: &[Step]) -> i64 {
    let mut ev: Vec<(i64, i64)> = Vec::new();
    for s in steps {
        if let (Some(st), Some(en), Some(d)) = (s.start, s.end(), s.duration) {
            if d > 0.0 {
                ev.push((st.instant(), 1));
                ev.push((en.instant(), -1));
            }
        }
    }
    ev.sort();
    let (mut cur, mut best) = (0, 0);
    for (_, d) in ev {
        cur += d;
        best = best.max(cur);
    }
    best
}

/// (value, source): the CHAIN_PARALLEL marker when the log has one, else the highest concurrency seen.
pub fn recorded_parallelism(p: &ParsedLog) -> Option<(i64, &'static str)> {
    if let Some(v) = p.parallel {
        return Some((v, "recorded"));
    }
    let mc = max_concurrency(&p.steps);
    (mc != 0).then_some((mc, "observed"))
}

/// A run whose parallelism is below the host's configured one is slow for a reason that is not the code.
pub fn parallelism_note(p: &ParsedLog, configured: Option<i64>) -> Option<String> {
    let configured = configured.filter(|c| *c != 0)?;
    let (got, how) = recorded_parallelism(p)?;
    if got >= configured {
        return None;
    }
    Some(format!(
        "note: {how} parallelism {got} is below the configured {configured}: this run is slower than the host can do (expect about {:.1}x the wall time on its parallel stretch)",
        configured as f64 / got as f64
    ))
}

fn group(cls: &str) -> usize {
    if cls == "real" {
        0
    } else if cls.starts_with("baseline") || cls == "generated-uncommitted" {
        2
    } else {
        1
    }
}

pub fn format_findings(findings: &[Finding]) -> String {
    if findings.is_empty() {
        return "no red steps".into();
    }
    let mut order: Vec<&str> = Vec::new();
    let mut by: HashMap<&str, Vec<&Finding>> = HashMap::new();
    for f in findings {
        if !by.contains_key(f.step.as_str()) {
            order.push(&f.step);
        }
        by.entry(&f.step).or_default().push(f);
    }
    let mut l: Vec<String> = Vec::new();
    let (shared, total) = shared_causes(findings);
    if !shared.is_empty() {
        l.push("causes:".into());
        for (ev, steps) in &shared {
            let head = steps.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
            let more = if steps.len() > 3 { format!(", +{} more", steps.len() - 3) } else { String::new() };
            l.push(format!("  {} steps: {ev} ({head}{more})", steps.len()));
        }
        let plural = if total == 1 { "cause" } else { "causes" };
        l.push(format!("{} red steps from {total} {plural} (1 cause explains {})", order.len(), shared[0].1.len()));
    }
    for name in &order {
        let fs = &by[name];
        let verdict = if fs.iter().any(|f| f.cls == "real") {
            "real".to_string()
        } else {
            fs.iter().map(|f| f.cls.clone()).collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>().join(", ")
        };
        let rc = fs[0].rc.map(|r| r.to_string()).unwrap_or_else(|| "None".into());
        l.push(format!("{name}: {verdict} (rc={rc}, {} block(s))", fs.len()));
        for f in fs {
            l.push(if f.block.is_empty() { format!("  [{}]", f.cls) } else { format!("  [{}] {}", f.cls, f.block) });
            l.push(format!("    evidence: {}", f.evidence));
            l.push(format!("    remedy: {}", f.remedy));
            l.push(format!(
                "    full rerun needed: {}",
                if f.cls == "advisory" {
                    "no (a rerun will not change a published advisory; a person decides, nothing is ignored automatically)"
                } else if f.rerun {
                    "yes (no lane defect found; a green run under the zero-red merge rule is still required)"
                } else {
                    "after the fix, the lane's defect comes first"
                }
            ));
        }
    }
    let mut c = [0usize; 3];
    for f in findings {
        c[group(&f.cls)] += 1;
    }
    let mut sv = [0usize; 3];
    for name in &order {
        let fs = &by[name];
        sv[if fs.iter().any(|f| f.cls == "real") { 0 } else { group(&fs[0].cls) }] += 1;
    }
    l.push(format!("{} red steps: {} real step(s), {} flake or infrastructure, {} baseline", order.len(), sv[0], sv[1], sv[2]));
    l.push(format!("{} blocks: {} real, {} flake or infrastructure, {} baseline", findings.len(), c[0], c[1], c[2]));
    l.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logfmt::parse_log;

    fn mklog(steps: &[(&str, u32, u32, i32, &[&str])]) -> String {
        let mut out = vec!["branch b at 0a1b2c3 on h".to_string(), "CHAIN_MODE full".to_string()];
        for (name, st, dur, rc, lines) in steps {
            out.push(format!("STEP_START {name} 2026-11-11T{:02}:{:02}:{:02}+07:00", 9 + st / 3600, st % 3600 / 60, st % 60));
            out.push(format!("=== {name}"));
            out.extend(lines.iter().map(|s| s.to_string()));
            out.push(format!("STEP_TIME {name} {dur}s"));
            out.push(format!("STEP_EXIT {name} {rc}"));
        }
        out.push("GATES_DONE".into());
        out.join("\n") + "\n"
    }

    const DENY: [&str; 9] = [
        "error[vulnerability]: Vulnerability detected",
        "   ┌─ Cargo.lock:12:1",
        "   = ID: RUSTSEC-2026-0001",
        "error[unmaintained]: crate is unmaintained",
        "   = ID: RUSTSEC-2026-0007",
        "error[vulnerability]: Vulnerability detected",
        "   = ID: RUSTSEC-2026-0001",
        "advisories FAILED, bans ok, licenses ok, sources ok",
        "error: 3 advisories found",
    ];

    #[test]
    fn a_third_party_advisory_is_its_own_class_with_its_ids_and_nothing_is_ignored() {
        let f = diag(&mklog(&[("deny", 0, 5, 1, &DENY)]));
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!((f[0].cls.as_str(), f[0].rule.as_str(), f[0].rerun), ("advisory", "advisory", false));
        assert_eq!(f[0].evidence, "RUSTSEC-2026-0001, RUSTSEC-2026-0007", "each id once, in order of appearance");
        assert!(f[0].remedy.contains("a person decides") && f[0].remedy.contains("never"), "{}", f[0].remedy);
        let text = format_findings(&f);
        assert!(text.contains("[advisory]") && text.contains("RUSTSEC-2026-0007"), "{text}");
        assert!(!text.contains("real step(s)") || text.contains("0 real"), "{text}");
        // not an advisory: an ordinary error line is still `real`
        let g = diag(&mklog(&[("deny", 0, 5, 1, &["error: could not compile", "error[E0308]: mismatched types"])]));
        assert!(g.iter().all(|x| x.cls != "advisory"), "{g:?}");
    }

    #[test]
    fn an_advisory_next_to_a_real_failure_reports_both() {
        let mut lines: Vec<&str> = vec!["---- t stdout ----", "assertion failed: left == right"];
        lines.extend(DENY);
        let f = diag(&mklog(&[("both", 0, 5, 1, &lines)]));
        assert!(f.iter().any(|x| x.cls == "advisory") && f.iter().any(|x| x.cls == "real"), "{f:?}");
    }

    fn diag(text: &str) -> Vec<Finding> {
        let p = parse_log(text).unwrap();
        diagnose_log(text, &p, &History::new(), builtin_rules())
    }

    const GOLDEN: [&str; 3] = ["FAIL  src/g.test.ts > golden > stored", "AssertionError: expected { a: 1 } to deeply equal { a: 2 }", " ❯ src/g.test.ts:14:20"];
    const SHRINK: [&str; 2] = ["FAIL  src/f.test.ts > frozen > only shrinks", "AssertionError: these routes now reach gates - delete their FROZEN entries: expected [ 'x' ] to deeply equal []"];

    #[test]
    fn builtin_table_loads_its_rules_in_order() {
        let ids: Vec<&str> = builtin_rules().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids[0], "out-of-memory", "the infra-memory row comes first, as the extra file used to be tried first");
        assert_eq!(ids[1], "port-collision");
        assert_eq!(ids.len(), 16);
        assert_eq!(ids[7], "load-flake");
        // the seven environment classes (audit rows 41, 66, 68, 69, 70, 96, 106) sit behind the original eight
        assert_eq!(ids[15], "advisory", "the advisory row is applied by its own pre-pass, so it sits last");
        assert_eq!(&ids[8..15], ["build-folder-missing", "service-never-came-up", "cache-server-died", "packages-not-built", "windows-linker", "generated-stale", "browser-timeout"]);
    }

    #[test]
    fn an_unmatched_assertion_makes_the_step_real_and_zero_real_never_prints() {
        let mut lines: Vec<&str> = GOLDEN.to_vec();
        lines.extend(SHRINK);
        lines.push("      Tests  2 failed | 9 passed (11)");
        let text = mklog(&[("s0", 0, 20, 0, &[]), ("unit", 20, 20, 1, &lines)]);
        let fs = diag(&text);
        assert_eq!(fs.iter().map(|f| f.cls.as_str()).collect::<Vec<_>>(), ["real", "baseline-shrink"]);
        let t = format_findings(&fs);
        assert!(t.contains("unit: real (rc=1, 2 block(s))"), "{t}");
        assert!(t.contains("1 real step(s)") && t.contains("2 blocks: 1 real, 0 flake or infrastructure, 1 baseline"), "{t}");
        assert!(!t.contains(" 0 real"));
    }

    #[test]
    fn shrink_wording_widened() {
        let t = mklog(&[("unit", 0, 20, 1, &SHRINK)]);
        assert_eq!(diag(&t)[0].cls, "baseline-shrink");
    }

    #[test]
    fn timeout_next_to_an_assertion_is_real() {
        let mut lines = vec!["FAIL  src/db.test.ts [ src/db.test.ts ]", "Error: Hook timed out in 30000ms."];
        lines.extend(GOLDEN);
        let t = mklog(&[("a", 0, 10, 0, &[]), ("b", 10, 10, 0, &[]), ("unit", 20, 400, 1, &lines)]);
        let fs = diag(&t);
        assert!(fs.iter().all(|f| f.cls == "real") && fs.len() == 2, "{fs:?}");
    }

    #[test]
    fn timeout_only_on_a_slow_step_is_one_flake() {
        let lines = ["FAIL  src/db.test.ts [ src/db.test.ts ]", "Error: Hook timed out in 30000ms.", "      Tests  0 failed | 9 passed (9)"];
        let t = mklog(&[("a", 0, 10, 0, &[]), ("b", 10, 10, 0, &[]), ("unit", 20, 400, 1, &lines)]);
        let fs = diag(&t);
        assert_eq!(fs.len(), 1);
        assert_eq!(fs[0].cls, "flake-load");
        assert_eq!(fs[0].block, "1 timeout block(s)");
    }

    #[test]
    fn blocks_split_per_runner() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(failure_blocks(&l(&["FAIL  2 problem(s):", "  - a", "    more", "  - b"])).len(), 2);
        assert_eq!(failure_blocks(&l(&["ok 1 - fine", "not ok 2 - x", "  ---", "ok 3"])).len(), 1);
        assert_eq!(failure_blocks(&l(&["just noise", "", "more noise"])), vec![l(&["just noise", "more noise"])]);
    }

    #[test]
    fn out_of_memory_lines_are_infra_memory_and_a_rerun_fixes_them() {
        for lines in [
            "Killed",
            "error: process exited with code 137",
            "FATAL ERROR: JavaScript heap out of memory",
            "Cannot allocate memory (OOM killer)",
            "Windows stopped because the system is running low on memory",
        ] {
            let t = mklog(&[("a", 0, 10, 0, &[]), ("unit", 10, 20, 137, &[lines])]);
            let fs = diag(&t);
            assert_eq!(fs.len(), 1, "{lines}: {fs:?}");
            assert_eq!(fs[0].cls, "infra-memory", "{lines}");
            assert!(fs[0].rerun, "{lines}");
        }
    }

    #[test]
    fn the_word_killed_inside_a_sentence_is_not_memory() {
        let t = mklog(&[("unit", 0, 20, 1, &["the test killed the mock server as designed", "AssertionError: x"])]);
        assert_eq!(diag(&t)[0].cls, "real");
    }

    #[test]
    fn every_rule_row_has_what_diagnose_needs_and_the_table_is_the_one_file() {
        assert!(builtin_rules().iter().any(|r| r.cls == "infra-memory"));
        assert!(builtin_rules().iter().all(|r| !r.cls.is_empty() && !r.remedy.is_empty()));
        assert!(!std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../rules/red-classes-extra.json").exists());
    }

    #[test]
    fn rules_file_rejects_a_bad_row() {
        let v: Value = serde_json::json!([{"cls": "x", "pattern": "(", "remedy": "r", "rerun": true}]);
        assert!(rules_from_value(&v).is_err());
        let v: Value = serde_json::json!([{"cls": "x", "pattern": "p", "remedy": "r"}]);
        assert!(rules_from_value(&v).is_err());
    }

    fn panic_step<'a>(thread: &'a str, pid: &'a str, msg: &'a str, time: &'a str) -> Vec<String> {
        vec![
            format!("---- {thread} stdout ----"),
            format!("thread '{thread}' ({pid}) panicked at crates/mr-control/tests/rw4b_db.rs:104:33:"),
            msg.to_string(),
            "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace".to_string(),
            format!("test result: FAILED. 0 passed; 6 failed; 0 ignored; finished in {time}s"),
        ]
    }

    fn mk_panics(msgs: &[&str]) -> String {
        let owned: Vec<(String, Vec<String>)> = msgs
            .iter()
            .enumerate()
            .map(|(i, m)| (format!("s{i}"), panic_step(&format!("t_{i}"), &format!("{}", 1474191 + i * 7), m, &format!("0.{i}4"))))
            .collect();
        let rows: Vec<(&str, u32, u32, i32, Vec<&str>)> =
            owned.iter().enumerate().map(|(i, (n, l))| (n.as_str(), i as u32 * 10, 5, 101, l.iter().map(|s| s.as_str()).collect())).collect();
        let refs: Vec<(&str, u32, u32, i32, &[&str])> = rows.iter().map(|(n, a, b, c, l)| (*n, *a, *b, *c, l.as_slice())).collect();
        mklog(&refs)
    }

    const PG: &str = "applying 0000_baseline.sql: error returned from database: syntax error at or near \"(\"";

    #[test]
    fn three_steps_with_one_panic_message_form_one_cause() {
        let t = format_findings(&diag(&mk_panics(&[PG, PG, PG])));
        assert!(t.starts_with("causes:\n"), "{t}");
        assert!(t.contains(&format!("3 steps: {PG}")), "{t}");
        assert!(t.contains("3 red steps from 1 cause (1 cause explains 3)"), "{t}");
    }

    #[test]
    fn two_different_messages_form_two_causes() {
        let a = "left: 1 right: 2 mismatch alpha";
        let t = format_findings(&diag(&mk_panics(&[PG, PG, a, a])));
        assert!(t.contains("2 steps: ") && t.contains(a) && t.contains(PG), "{t}");
        assert!(t.contains("4 red steps from 2 causes (1 cause explains 2)"), "{t}");
    }

    #[test]
    fn a_panic_blocks_evidence_is_the_message_not_the_summary() {
        let fs = diag(&mk_panics(&[PG]));
        assert_eq!(fs[0].evidence, PG, "{fs:?}");
    }

    #[test]
    fn duplicate_blocks_inside_one_step_collapse() {
        let one: Vec<String> = vec!["FAIL  src/a.test.ts > x".into(), "AssertionError: boom".into()];
        let mut lines: Vec<&str> = one.iter().map(|s| s.as_str()).collect();
        lines.extend(one.iter().map(|s| s.as_str()));
        let fs = diag(&mklog(&[("unit", 0, 5, 1, &lines)]));
        assert_eq!(fs.len(), 1, "{fs:?}");
    }

    #[test]
    fn one_red_step_prints_no_causes_section() {
        let t = format_findings(&diag(&mk_panics(&[PG])));
        assert!(!t.contains("causes:") && !t.contains("from 1 cause"), "{t}");
        assert!(t.starts_with("s0: real (rc=101, 1 block(s))"), "{t}");
    }
}

// ---- retry qualification (GT-1, D446) ----

/// One live row of a quarantine file: a test of a step that may be re-run once, with who decided (a finding id) and when it lapses.
#[derive(Debug, Clone, PartialEq)]
pub struct QuarantineRow {
    pub test: String,
    pub step: String,
    pub finding: String,
    pub added: String,
    pub expires: String,
}

/// Days since 1970-01-01 of a `YYYY-MM-DD` date (None when it is not a real calendar date).
fn days_of(date: &str) -> Option<i64> {
    let mut it = date.trim().split('-');
    let (y, m, d): (i64, i64, i64) = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) || date.trim().len() != 10 {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// Split one CSV line (double quotes may hold commas; `""` is a quote).
fn csv_fields(line: &str) -> Vec<String> {
    let (mut out, mut cur, mut quoted) = (Vec::new(), String::new(), false);
    let mut it = line.chars().peekable();
    while let Some(c) = it.next() {
        match (c, quoted) {
            ('"', true) if it.peek() == Some(&'"') => {
                cur.push('"');
                it.next();
            }
            ('"', _) => quoted = !quoted,
            (',', false) => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|f| f.trim().to_string()).collect()
}

/// The rows of a quarantine file (`test,step,finding,added,expires`; the header line is required) that are live on `today` (`YYYY-MM-DD`).
/// A row is dropped, never honoured, when it is malformed, lacks a finding id, has an expiry before `today`, or lives more than 14 days
/// (`expires` minus `added`). The dropped rows are returned as `(line number, why)` so the caller can say so.
pub fn live_quarantine(text: &str, today: &str) -> (Vec<QuarantineRow>, Vec<(usize, String)>) {
    let (mut live, mut dropped) = (Vec::new(), Vec::new());
    let now = days_of(today);
    for (n, line) in text.lines().enumerate() {
        if n == 0 || line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let f = csv_fields(line);
        if f.len() != 5 || f[..3].iter().any(String::is_empty) {
            dropped.push((n + 1, "expected test,step,finding,added,expires with the first three filled".into()));
            continue;
        }
        let (Some(a), Some(e)) = (days_of(&f[3]), days_of(&f[4])) else {
            dropped.push((n + 1, "added and expires must be YYYY-MM-DD".into()));
            continue;
        };
        if e - a > 14 || e < a {
            dropped.push((n + 1, "expires must be 0 to 14 days after added".into()));
        } else if now.is_some_and(|t| e < t) {
            dropped.push((n + 1, format!("expired on {}", f[4])));
        } else {
            live.push(QuarantineRow { test: f[0].clone(), step: f[1].clone(), finding: f[2].clone(), added: f[3].clone(), expires: f[4].clone() });
        }
    }
    (live, dropped)
}

/// Why a red step may be re-run once: the finding class (or `quarantine`), the rule or finding id behind it, and the test it names.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryCause {
    pub cls: String,
    /// `diagnose:<rule id>` or `quarantine:<finding id>`
    pub via: String,
    pub test: String,
}

/// D446: a red step is a flake candidate ONLY when every failure block of its output is classed `flake-*` or `infra-*` by the rule table,
/// or names the test of a live quarantine row for this step. Anything else (an unmatched block is `real`, an advisory, a baseline
/// class) disqualifies the whole step: a mixed red is not retried. `secs` and `history` feed the `slow` rule exactly as `diagnose` does.
pub fn retry_cause(step: &str, rc: i64, secs: f64, output: &[String], history: &History, rules: &[Rule], quarantine: &[QuarantineRow]) -> Option<RetryCause> {
    let steps = vec![Step { name: step.to_string(), duration: Some(secs), exit_code: Some(rc), ..Default::default() }];
    let findings = classify(0, output, &steps, history, rules);
    let mut first: Option<RetryCause> = None;
    for f in &findings {
        let hit = if f.cls.starts_with("flake-") || f.cls.starts_with("infra-") {
            RetryCause { cls: f.cls.clone(), via: format!("diagnose:{}", f.rule), test: f.block.clone() }
        } else {
            let q = quarantine.iter().find(|q| q.step == step && (f.block.contains(&q.test) || f.evidence.contains(&q.test)))?;
            RetryCause { cls: "quarantine".into(), via: format!("quarantine:{}", q.finding), test: q.test.clone() }
        };
        first.get_or_insert(hit);
    }
    first
}
