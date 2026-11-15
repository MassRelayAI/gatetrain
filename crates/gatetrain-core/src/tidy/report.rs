//! The text of `check` (`tidy.format_check`) and the small tables it prints.
use super::inventory::{human, human_f, Inventory, Item};
use crate::pycompat::{fmt_f, fmt_g, rstrip};

/// `12.0 MB`, or `12.0 MB + 2 item(s) of unknown size`.
pub fn total_str(items: &[Item]) -> String {
    let unk = items.iter().filter(|i| i.bytes.is_none()).count();
    let known = human_f(items.iter().filter_map(|i| i.bytes).sum::<i64>() as f64);
    if unk > 0 {
        format!("{known} + {unk} item(s) of unknown size")
    } else {
        known
    }
}

/// Columns padded to the widest cell; trailing spaces trimmed (`tidy.table`).
pub fn table(rows: Vec<Vec<String>>, head: &[&str]) -> String {
    let mut all: Vec<Vec<String>> = vec![head.iter().map(|s| s.to_string()).collect()];
    all.extend(rows);
    let w: Vec<usize> = (0..head.len()).map(|i| all.iter().map(|r| r[i].chars().count()).max().unwrap()).collect();
    all.iter()
        .map(|r| {
            let line = (0..head.len()).map(|i| format!("{}{}", r[i], " ".repeat(w[i] - r[i].chars().count()))).collect::<Vec<_>>().join("  ");
            rstrip(&line).to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn yn(v: Option<bool>) -> String {
    match v {
        None => "-".into(),
        Some(true) => "yes".into(),
        Some(false) => "no".into(),
    }
}

fn s(x: impl ToString) -> String {
    x.to_string()
}

pub fn format_check(inv: &Inventory) -> String {
    let mut l: Vec<String> = vec![format!("== {}", inv.host)];
    if !inv.disks.is_empty() {
        l.push("disks".into());
        l.push(table(
            inv.disks
                .iter()
                .map(|(p, f, m)| {
                    vec![
                        p.clone(),
                        match f {
                            None => "unreadable".into(),
                            Some(f) => format!("{} GB", fmt_f(*f, 0)),
                        },
                        format!("{} GB", fmt_g(*m)),
                        if f.map(|f| f < *m).unwrap_or(false) { "LOW" } else { "ok" }.into(),
                    ]
                })
                .collect(),
            &["path", "free", "min", "state"],
        ));
    }
    if !inv.targets.is_empty() {
        l.push("build targets".into());
        l.push(table(
            inv.targets
                .iter()
                .map(|t| {
                    vec![
                        t.path.clone(),
                        human(t.bytes),
                        match t.idle_min {
                            None => "-".into(),
                            Some(i) => format!("{}h", fmt_f(i / 60.0, 0)),
                        },
                        t.lane.clone().filter(|x| !x.is_empty()).unwrap_or_else(|| "-".into()),
                        yn(t.merged),
                        yn(t.tag),
                        if t.over {
                            "OVER".into()
                        } else if t.budget_gb.map(|b| b != 0.0).unwrap_or(false) && t.bytes.is_none() {
                            "size unknown".into()
                        } else {
                            "-".into()
                        },
                        t.keep.clone().filter(|x| !x.is_empty()).unwrap_or_else(|| "tidy".into()),
                    ]
                })
                .collect(),
            &["path", "size", "idle", "lane", "merged", "tag", "budget", "plan"],
        ));
    }
    if !inv.worktrees.is_empty() {
        l.push("worktrees".into());
        l.push(table(
            inv.worktrees
                .iter()
                .map(|w| {
                    vec![w.path.clone(), w.branch.clone().filter(|x| !x.is_empty()).unwrap_or_else(|| "-".into()), yn(w.merged), yn(w.dirty), s(w.links), w.keep.clone().unwrap_or_else(|| "tidy".into())]
                })
                .collect(),
            &["path", "branch", "merged", "dirty", "links", "plan"],
        ));
    }
    if !inv.containers.is_empty() {
        l.push(format!("exited containers: {}", inv.containers.join(", ")));
    }
    if !inv.images.is_empty() {
        l.push("images".into());
        l.push(table(inv.images.iter().map(|i| vec![i.name.clone(), human(Some(i.bytes)), if i.in_use { "in use" } else { "unused" }.into()]).collect(), &["image", "size", "state"]));
    }
    if let Some((size, rec)) = inv.cache {
        l.push(format!("build cache: {} ({} reclaimable)", human(Some(size)), human(Some(rec))));
    }
    if !inv.trash.is_empty() {
        l.push("recycle bin".into());
        l.push(table(
            inv.trash
                .iter()
                .map(|t| vec![t.path.clone(), human(t.bytes), match t.age_days { None => "-".into(), Some(a) => format!("{}d", fmt_f(a, 0)) }])
                .collect(),
            &["entry", "size", "age"],
        ));
    }
    if !inv.profiles.is_empty() {
        l.push("browser profiles in the wrong place (never auto-removed)".into());
        l.extend(inv.profiles.iter().map(|p| format!("  {p}")));
    }
    if !inv.items.is_empty() {
        l.push("would tidy".into());
        l.push(table(
            inv.items
                .iter()
                .map(|i| {
                    vec![
                        i.cls.clone(),
                        i.kind.clone(),
                        i.key.clone(),
                        match i.bytes {
                            None => "?".into(),
                            Some(b) if b != 0 => human(Some(b)),
                            _ => "-".into(),
                        },
                        if i.attended_only { "attended" } else { "" }.into(),
                        i.why.clone(),
                    ]
                })
                .collect(),
            &["class", "action", "item", "bytes", "mode", "why"],
        ));
        l.push(format!("total reclaimable: {}", total_str(&inv.items)));
    } else {
        l.push("would tidy: nothing".into());
    }
    l.extend(inv.breaches.iter().map(|b| format!("THRESHOLD: {b}")));
    l.join("\n")
}
