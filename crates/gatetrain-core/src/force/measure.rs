//! `gatetrain measure`: per host, what holds disk (worktrees, cargo targets, docker images, containers, volumes and cache, VM disk images,
//! the recycle bin), each number with the method that produced it. Read-only. An unknown size is "unknown", never 0.
use crate::force::exec::Exec;
use crate::tidy::inventory::{human, parse_docker_size, Inventory};
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct MRow {
    pub class: String,
    pub item: String,
    pub bytes: Option<i64>,
    pub state: String,
    pub method: String,
}

#[derive(Debug, Clone, Default)]
pub struct Measure {
    pub host: String,
    pub rows: Vec<MRow>,
    /// (disk path, free GB)
    pub free: Vec<(String, Option<f64>)>,
}

fn row(class: &str, item: &str, bytes: Option<i64>, state: String, method: &str) -> MRow {
    MRow { class: class.into(), item: item.into(), bytes, state, method: method.into() }
}

fn tf(v: Option<bool>, yes: &str, no: &str) -> String {
    match v {
        Some(true) => yes.into(),
        Some(false) => no.into(),
        None => format!("{yes}/{no}: unknown"),
    }
}

/// The rows of a tidy inventory (its probes are the ones tidy trusts: links never followed, a probe that runs out of time is unknown).
pub fn from_inventory(inv: &Inventory, win: bool) -> Vec<MRow> {
    let size_m = if win { "PowerShell Get-ChildItem -Recurse sum (tidy size probe)" } else { "du -sk (tidy size probe)" };
    let mut v = vec![];
    for w in &inv.worktrees {
        let mut st = vec![tf(w.dirty, "dirty", "clean"), tf(w.merged, "merged", "unmerged")];
        if let Some(k) = &w.keep {
            st.push(format!("kept: {k}"));
        }
        v.push(row("worktree", &w.path, w.bytes, st.join(", "), size_m));
    }
    for t in &inv.targets {
        let mut st = vec![if t.cargo { "cargo target".to_string() } else { "folder".to_string() }];
        if let Some(i) = t.idle_min {
            st.push(format!("idle {:.0} min", i));
        }
        if let Some(k) = &t.keep {
            st.push(format!("kept: {k}"));
        }
        v.push(row("cargo-target", &t.path, t.bytes, st.join(", "), size_m));
    }
    for i in &inv.images {
        v.push(row("docker-image", &i.name, Some(i.bytes), if i.in_use { "in use" } else { "unused" }.into(), "docker images (decimal units)"));
    }
    for c in &inv.containers {
        v.push(row("docker-container", c, None, "exited, throwaway".into(), "docker ps -a (name only; sizes are in the docker totals)"));
    }
    for t in &inv.trash {
        let mut st = vec![t.age_days.map(|d| format!("{d:.1} days old")).unwrap_or_else(|| "age unknown".into())];
        if let Some(k) = &t.keep {
            st.push(format!("kept: {k}"));
        }
        v.push(row("trash", &t.path, t.bytes, st.join(", "), size_m));
    }
    v
}

/// Docker's own totals: images, containers, volumes, build cache.
pub fn docker_rows(e: &dyn Exec) -> Vec<MRow> {
    let o = e.run(&["docker".into(), "system".into(), "df".into(), "--format".into(), "{{.Type}}|{{.Size}}|{{.Reclaimable}}".into()]);
    let method = "docker system df (decimal units)";
    if !o.ok() || o.out.trim().is_empty() {
        return vec![row("docker-total", "docker", None, "unknown: docker did not answer".into(), method)];
    }
    let mut v = vec![];
    for l in o.out.lines() {
        let p: Vec<&str> = l.trim().split('|').collect();
        if p.len() < 2 {
            continue;
        }
        let class = match p[0].trim() {
            "Images" => "docker-image-total",
            "Containers" => "docker-container-total",
            "Local Volumes" => "docker-volume",
            "Build Cache" => "docker-cache",
            _ => continue,
        };
        let rec = p.get(2).map(|r| r.split_whitespace().next().unwrap_or("")).unwrap_or("");
        v.push(row(class, p[0].trim(), Some(parse_docker_size(p[1])), format!("reclaimable {rec}"), method));
    }
    v
}

/// VM disk images by what they OCCUPY (allocated bytes), with the logical size beside it: a sparse `ext4.vhdx` is far smaller on disk.
pub fn vm_rows(e: &dyn Exec, win: bool, paths: &[String]) -> Vec<MRow> {
    let gb = |b: i64| format!("{:.1} GB", b as f64 / 1_073_741_824.0);
    let mut v = vec![];
    for p in paths {
        if win {
            let m = "GetCompressedFileSizeW (bytes allocated on disk; logical size from the file length)";
            let q = p.replace('\'', "''");
            let script = format!(
                "$p='{q}'; if(-not (Test-Path -LiteralPath $p)){{'missing'}} else {{ Add-Type -MemberDefinition '[DllImport(\"kernel32.dll\")] public static extern uint GetCompressedFileSizeW(string f, out uint hi);' -Name N -Namespace K; $hi=[uint32]0; $lo=[K.N]::GetCompressedFileSizeW($p,[ref]$hi); $l=(Get-Item -LiteralPath $p).Length; \"$l|$([uint64]$hi*4294967296+$lo)\" }}"
            );
            let o = e.run(&["powershell".into(), "-NoProfile".into(), "-Command".into(), script]);
            v.push(parse_vm(p, &o.out, o.ok(), m, &gb));
        } else {
            let m = "du -sk (allocated blocks; logical size from stat)";
            let l = e.run(&["stat".into(), "-c".into(), "%s".into(), p.clone()]);
            let d = e.run(&["du".into(), "-sk".into(), p.clone()]);
            let out = match (l.out.trim().parse::<i64>(), d.out.split_whitespace().next().and_then(|x| x.parse::<i64>().ok())) {
                (Ok(l), Some(k)) if l >= 0 && d.ok() => format!("{l}|{}", k * 1024),
                _ if l.ok() || d.ok() => "bad".into(),
                _ => String::new(),
            };
            v.push(parse_vm(p, &out, !out.is_empty(), m, &gb));
        }
    }
    v
}

fn parse_vm(p: &str, out: &str, ran: bool, method: &str, gb: &dyn Fn(i64) -> String) -> MRow {
    let t = out.trim();
    if !ran || t.is_empty() {
        return row("vm-disk", p, None, "unknown: the probe did not answer".into(), method);
    }
    if t == "missing" {
        return row("vm-disk", p, None, "missing".into(), method);
    }
    match t.split_once('|').and_then(|(a, b)| Some((a.trim().parse::<i64>().ok()?, b.trim().parse::<i64>().ok()?))) {
        Some((logical, disk)) => row("vm-disk", p, Some(disk), format!("logical {}, on disk {}", gb(logical), gb(disk)), method),
        None => row("vm-disk", p, None, "unknown: the probe answered something unreadable".into(), method),
    }
}

pub fn table(m: &Measure) -> String {
    let mut s = format!("== {}\n{:<18} {:>10}  {:<44} {:<}\n", m.host, "class", "size", "item", "state  [method]");
    for r in &m.rows {
        let size = r.bytes.map(|b| human(Some(b))).unwrap_or_else(|| "unknown".into());
        s.push_str(&format!("{:<18} {:>10}  {:<44} {}  [method: {}]\n", r.class, size, r.item, r.state, r.method));
    }
    // docker totals overlap the per-image rows, so the total counts the folders and the VM disks only
    let counted: i64 = m.rows.iter().filter(|r| matches!(r.class.as_str(), "worktree" | "cargo-target" | "trash" | "vm-disk")).filter_map(|r| r.bytes).sum();
    let unk = m.rows.iter().filter(|r| r.bytes.is_none()).count();
    s.push_str(&format!("total (worktrees, targets, trash, VM disks): {}; {unk} row(s) unknown\n", human(Some(counted))));
    for (p, f) in &m.free {
        s.push_str(&format!("free {p}: {}\n", f.map(|x| format!("{x:.1} GB")).unwrap_or_else(|| "unknown".into())));
    }
    s
}

pub fn to_json(m: &Measure) -> Value {
    json!({
        "host": m.host,
        "rows": m.rows.iter().map(|r| json!({"class": r.class, "item": r.item, "bytes": r.bytes, "state": r.state, "method": r.method})).collect::<Vec<_>>(),
        "free_gb": m.free.iter().map(|(p, f)| json!({"path": p, "free_gb": f})).collect::<Vec<_>>(),
    })
}
