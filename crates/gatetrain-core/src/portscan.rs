//! Find RANDOM port defaults in gate scripts: `N + ((process.pid * k + Date.now()) % M)`, `random.randint(a, b)`,
//! `$((N + RANDOM % M))`. Port of gatetrain/portscan.py. Returns (line, low, high) for each; the doctor compares the range
//! with the host's ephemeral range (L21, L52).
use crate::pycompat::splitlines;
use regex::Regex;
use std::sync::OnceLock;

struct Rx {
    hint: Regex,
    modr: Regex,
    sh: Regex,
    rand: Regex,
}

fn rx() -> &'static Rx {
    static R: OnceLock<Rx> = OnceLock::new();
    R.get_or_init(|| Rx {
        hint: Regex::new(r"(?i)pid|random|RANDOM|Date\.now|time\(|randint|randrange").unwrap(),
        // Python: (?<![\w.])([0-9]{4,5})\s*\+\s*[\(\$].*?%\s*([0-9]{2,5})\b ; the look-behind is checked by hand in `modr_search`
        modr: Regex::new(r"([0-9]{4,5})\s*\+\s*[\(\$].*?%\s*([0-9]{2,5})\b").unwrap(),
        sh: Regex::new(r"\$\(\(\s*([0-9]{4,5})\s*\+\s*\$?RANDOM\s*%\s*([0-9]{2,5})").unwrap(),
        rand: Regex::new(r"(?:randint|randrange|uniform)\(\s*([0-9]{4,5})\s*,\s*([0-9]{4,5})\s*\)").unwrap(),
    })
}

fn word_or_dot(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

/// Python's `_MOD.search(line)`: the leftmost match whose first digit is not preceded by a word character or a dot.
fn modr_search<'a>(line: &'a str) -> Option<regex::Captures<'a>> {
    let r = &rx().modr;
    let mut at = 0;
    while at <= line.len() {
        let caps = r.captures_at(line, at)?;
        let m = caps.get(1).unwrap();
        let prev = line[..m.start()].chars().next_back();
        if !prev.map(word_or_dot).unwrap_or(false) {
            return Some(caps);
        }
        // a longer digit run starting one char later may still satisfy the look-behind only if that char is a non-word: it never is (a digit)
        at = line[m.start()..].char_indices().nth(1).map(|(i, _)| m.start() + i).unwrap_or(line.len() + 1);
    }
    None
}

pub fn scan_random_ports(text: &str) -> Vec<(usize, i64, i64)> {
    let r = rx();
    let mut out = vec![];
    for (i, line) in splitlines(text).iter().enumerate() {
        let n = i + 1;
        let m = r.sh.captures(line).or_else(|| modr_search(line));
        if let Some(m) = m {
            if r.hint.is_match(line) {
                let lo: i64 = m[1].parse().unwrap();
                out.push((n, lo, lo + m[2].parse::<i64>().unwrap() - 1));
                continue;
            }
        }
        if let Some(m) = r.rand.captures(line) {
            out.push((n, m[1].parse().unwrap(), m[2].parse().unwrap()));
        }
    }
    out
}

/// Parts of [lo, hi] inside the ephemeral range that no reserved range covers; empty means safe.
pub fn uncovered(lo: i64, hi: i64, ephemeral: (i64, i64), reserved: &[(i64, i64)]) -> Vec<(i64, i64)> {
    let (a, b) = (lo.max(ephemeral.0), hi.min(ephemeral.1));
    if a > b {
        return vec![];
    }
    let mut gaps = vec![(a, b)];
    let mut rs = reserved.to_vec();
    rs.sort();
    for (ra, rb) in rs {
        let mut nxt = vec![];
        for (ga, gb) in gaps {
            if rb < ga || ra > gb {
                nxt.push((ga, gb));
                continue;
            }
            if ra > ga {
                nxt.push((ga, ra - 1));
            }
            if rb < gb {
                nxt.push((rb + 1, gb));
            }
        }
        gaps = nxt;
    }
    gaps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_three_shapes() {
        let t = "const port = 50000 + ((process.pid * 7 + Date.now()) % 3000);\nport=$((20000 + RANDOM % 5000))\nrandom.randint(30000, 31999)\nx = 1\n";
        assert_eq!(scan_random_ports(t), vec![(1, 50000, 52999), (2, 20000, 24999), (3, 30000, 31999)]);
    }

    #[test]
    fn lookbehind_rejects_a_glued_number() {
        assert!(scan_random_ports("a.50000 + (pid % 3000)").is_empty());
        assert!(scan_random_ports("v150000 + (pid % 3000)").is_empty());
    }

    #[test]
    fn uncovered_splits() {
        assert_eq!(uncovered(1, 10, (5, 20), &[(6, 7)]), vec![(5, 5), (8, 10)]);
        assert!(uncovered(1, 4, (5, 20), &[]).is_empty());
    }
}
