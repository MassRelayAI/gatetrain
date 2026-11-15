//! The few Python behaviours the doctor, the lanes radar, check and tidy print through, so the Rust output is byte-identical:
//! `json.dumps(indent=2)`, `repr(str)`, `format(x, "g")`, `str.splitlines`, `shlex.quote`, `posixpath.normpath`, `fnmatch`.

#[derive(Debug, Clone, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn strs<S: AsRef<str>>(xs: &[S]) -> J {
        J::Arr(xs.iter().map(|x| J::s(x.as_ref())).collect())
    }
    pub fn opt(x: &Option<String>) -> J {
        x.as_ref().map_or(J::Null, |s| J::s(s.as_str()))
    }
    pub fn s(x: impl Into<String>) -> J {
        J::Str(x.into())
    }
    pub fn opt_int(x: Option<i64>) -> J {
        x.map(J::Int).unwrap_or(J::Null)
    }
    pub fn opt_str(x: &Option<String>) -> J {
        x.clone().map(J::Str).unwrap_or(J::Null)
    }
    pub fn opt_bool(x: Option<bool>) -> J {
        x.map(J::Bool).unwrap_or(J::Null)
    }
}

/// `json.dumps(v, indent=indent, sort_keys=sort_keys)`
pub fn dumps(v: &J, indent: Option<usize>, sort_keys: bool) -> String {
    let mut o = String::new();
    write(v, indent, sort_keys, 0, &mut o);
    o
}

fn write(v: &J, indent: Option<usize>, sort: bool, level: usize, o: &mut String) {
    match v {
        J::Null => o.push_str("null"),
        J::Bool(b) => o.push_str(if *b { "true" } else { "false" }),
        J::Int(i) => o.push_str(&i.to_string()),
        J::Float(f) => o.push_str(&float_repr(*f)),
        J::Str(s) => string(s, o),
        J::Arr(a) => {
            if a.is_empty() {
                o.push_str("[]");
                return;
            }
            o.push('[');
            for (i, x) in a.iter().enumerate() {
                sep(i, indent, level + 1, o);
                write(x, indent, sort, level + 1, o);
            }
            close(indent, level, o);
            o.push(']');
        }
        J::Obj(m) => {
            if m.is_empty() {
                o.push_str("{}");
                return;
            }
            let mut items: Vec<&(String, J)> = m.iter().collect();
            if sort {
                items.sort_by(|a, b| a.0.encode_utf16().cmp(b.0.encode_utf16()));
            }
            o.push('{');
            for (i, (k, x)) in items.into_iter().enumerate() {
                sep(i, indent, level + 1, o);
                string(k, o);
                o.push_str(": ");
                write(x, indent, sort, level + 1, o);
            }
            close(indent, level, o);
            o.push('}');
        }
    }
}

fn sep(i: usize, indent: Option<usize>, level: usize, o: &mut String) {
    match indent {
        Some(n) => {
            if i > 0 {
                o.push(',');
            }
            o.push('\n');
            o.push_str(&" ".repeat(n * level));
        }
        None => {
            if i > 0 {
                o.push_str(", ");
            }
        }
    }
}

fn close(indent: Option<usize>, level: usize, o: &mut String) {
    if let Some(n) = indent {
        o.push('\n');
        o.push_str(&" ".repeat(n * level));
    }
}

/// Python's float repr for the values the log holds (rounded GB figures).
pub fn float_repr(f: f64) -> String {
    let s = format!("{:?}", f);
    s.replace("e", "e+").replace("e+-", "e-")
}

fn string(s: &str, o: &mut String) {
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\u{8}' => o.push_str("\\b"),
            '\u{c}' => o.push_str("\\f"),
            ' '..='~' => o.push(c),
            c => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    o.push_str(&format!("\\u{:04x}", u));
                }
            }
        }
    }
    o.push('"');
}

/// `round(x, 2)` as Python does it (correctly rounded decimal string, parsed back).
pub fn round2(x: f64) -> f64 {
    fmt_f(x, 2).parse().unwrap_or(x)
}

/// `json.dumps(v, indent=2)` (ensure_ascii on, key order kept).
pub fn dumps_indent2(v: &J) -> String {
    dumps(v, Some(2), false)
}

/// Python `f"{x:.Nf}"`.
pub fn fmt_f(x: f64, decimals: usize) -> String {
    format!("{:.*}", decimals, x)
}

fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

/// Python `str.strip()`.
pub fn strip(s: &str) -> &str {
    s.trim_matches(is_py_space)
}

/// Python `str.rstrip()`.
pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(is_py_space)
}

/// Python `str.split()` with no argument.
pub fn split_ws(s: &str) -> Vec<&str> {
    s.split(is_py_space).filter(|x| !x.is_empty()).collect()
}

/// `repr()` of a str.
pub fn repr_str(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut o = String::new();
    o.push(q);
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if c == q => {
                o.push('\\');
                o.push(c)
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => o.push_str(&format!("\\x{:02x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push(q);
    o
}

/// `repr()` of a list of str, e.g. `['chain', 'preflight']`.
pub fn repr_list<S: AsRef<str>>(xs: &[S]) -> String {
    format!("[{}]", xs.iter().map(|x| repr_str(x.as_ref())).collect::<Vec<_>>().join(", "))
}

/// `format(x, "g")`: six significant digits, trailing zeros dropped, exponent form outside 1e-4 .. 1e6.
pub fn fmt_g(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    if !x.is_finite() {
        return if x.is_nan() { "nan".into() } else if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let e = format!("{:.5e}", x); // d.ddddde[-]X, correctly rounded
    let (mant, exp) = e.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    if !(-4..6).contains(&exp) {
        let m = if mant.contains('.') { mant.trim_end_matches('0').trim_end_matches('.') } else { mant };
        format!("{}e{}{:02}", m, if exp < 0 { '-' } else { '+' }, exp.abs())
    } else {
        let digits = (5 - exp).max(0) as usize;
        let s = format!("{:.*}", digits, x);
        if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
    }
}

/// `str.splitlines()` (the separators Python knows).
pub fn splitlines(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                out.push(std::mem::take(&mut cur));
            }
            '\n' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}' => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_digit())
}

/// `shlex.quote`.
pub fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// `shlex.join`.
pub fn shlex_join(argv: &[String]) -> String {
    argv.iter().map(|a| shlex_quote(a)).collect::<Vec<_>>().join(" ")
}

/// `posixpath.normpath`.
pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let initial = if path.starts_with('/') {
        if path.starts_with("//") && !path.starts_with("///") { 2 } else { 1 }
    } else {
        0
    };
    let mut comps: Vec<&str> = Vec::new();
    for c in path.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c != ".." || (initial == 0 && comps.is_empty()) || comps.last() == Some(&"..") {
            comps.push(c);
        } else if !comps.is_empty() {
            comps.pop();
        }
    }
    let r = format!("{}{}", "/".repeat(initial), comps.join("/"));
    if r.is_empty() { ".".into() } else { r }
}

/// `fnmatch.fnmatchcase` over chars.
pub fn fnmatchcase(name: &str, pat: &str) -> bool {
    let n: Vec<char> = name.chars().collect();
    let p: Vec<char> = pat.chars().collect();
    fm(&n, &p)
}

/// `fnmatch.fnmatch`: Python folds case and path separators on Windows only.
pub fn fnmatch(name: &str, pat: &str) -> bool {
    if cfg!(windows) {
        let f = |s: &str| s.replace('/', "\\").to_lowercase();
        fnmatchcase(&f(name), &f(pat))
    } else {
        fnmatchcase(name, pat)
    }
}

fn set_end(p: &[char], i: usize) -> Option<usize> {
    // i is just past '['; mirrors fnmatch.translate: optional '!', optional leading ']', then up to the next ']'
    let mut j = i;
    if j < p.len() && p[j] == '!' {
        j += 1;
    }
    if j < p.len() && p[j] == ']' {
        j += 1;
    }
    while j < p.len() && p[j] != ']' {
        j += 1;
    }
    if j >= p.len() { None } else { Some(j) }
}

fn in_set(stuff: &[char], c: char) -> bool {
    let (neg, s) = if stuff.first() == Some(&'!') { (true, &stuff[1..]) } else { (false, stuff) };
    let mut hit = false;
    let mut k = 0;
    while k < s.len() {
        if k + 2 < s.len() && s[k + 1] == '-' {
            if s[k] <= c && c <= s[k + 2] {
                hit = true;
            }
            k += 3;
        } else {
            if s[k] == c {
                hit = true;
            }
            k += 1;
        }
    }
    hit != neg
}

fn fm(n: &[char], p: &[char]) -> bool {
    if p.is_empty() {
        return n.is_empty();
    }
    match p[0] {
        '*' => (0..=n.len()).any(|k| fm(&n[k..], &p[1..])),
        '?' => !n.is_empty() && fm(&n[1..], &p[1..]),
        '[' => match set_end(p, 1) {
            Some(j) => !n.is_empty() && in_set(&p[1..j], n[0]) && fm(&n[1..], &p[j + 1..]),
            None => !n.is_empty() && n[0] == '[' && fm(&n[1..], &p[1..]),
        },
        c => !n.is_empty() && n[0] == c && fm(&n[1..], &p[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn g_matches_python() {
        for (x, w) in [(0.0, "0"), (50.0, "50"), (2.5, "2.5"), (120.0, "120"), (1e6, "1e+06"), (123456.0, "123456"), (0.0001, "0.0001"), (0.00001, "1e-05"), (1234567.0, "1.23457e+06"), (6.0, "6"), (0.5, "0.5")] {
            assert_eq!(fmt_g(x), w, "{x}");
        }
    }

    #[test]
    fn repr_and_json() {
        assert_eq!(repr_str("a'b"), "\"a'b\"");
        assert_eq!(repr_str("C:\\x"), "'C:\\\\x'");
        assert_eq!(repr_list(&["chain", "x"]), "['chain', 'x']");
        let v = J::Obj(vec![("a".into(), J::Arr(vec![])), ("b".into(), J::Arr(vec![J::Int(1), J::s("\u{e9}\u{1F600}")])), ("c".into(), J::Null)]);
        assert_eq!(dumps_indent2(&v), "{\n  \"a\": [],\n  \"b\": [\n    1,\n    \"\\u00e9\\ud83d\\ude00\"\n  ],\n  \"c\": null\n}");
    }

    #[test]
    fn paths_and_globs() {
        assert_eq!(normpath("a//b/../c/./d"), "a/c/d");
        assert_eq!(normpath("/../x"), "/x");
        assert_eq!(normpath(""), ".");
        assert_eq!(shlex_quote("a b'c"), "'a b'\"'\"'c'");
        assert!(fnmatchcase("docs/a/b.md", "docs/*.md"));
        assert!(fnmatchcase("x/locales/en.json", "*/locales/*.json"));
        assert!(!fnmatchcase("a.txt", "*.md"));
        assert!(fnmatchcase("a1", "a[0-9]") && !fnmatchcase("ab", "a[!b]") && fnmatchcase("a[", "a["));
    }

    #[test]
    fn lines() {
        assert_eq!(splitlines("a\r\nb\n\nc"), vec!["a", "b", "", "c"]);
        assert_eq!(splitlines("a\n"), vec!["a"]);
    }


    #[test]
    fn g_matches_python_tidy() {
        assert_eq!(fmt_g(50.0), "50");
        assert_eq!(fmt_g(0.5), "0.5");
        assert_eq!(fmt_g(60.0), "60");
        assert_eq!(fmt_g(1234567.0), "1.23457e+06");
        assert_eq!(fmt_g(0.0001), "0.0001");
        assert_eq!(fmt_g(0.00001), "1e-05");
        assert_eq!(fmt_g(7.0), "7");
        assert_eq!(fmt_g(100000.0), "100000");
        assert_eq!(fmt_g(999999.5), "1e+06");
    }

    #[test]
    fn f_ties_match_python_tidy() {
        // Python: f"{0.25:.1f}" == "0.2", f"{2.5:.0f}" == "2", f"{1.25:.1f}" == "1.2", f"{0.35:.1f}" == "0.3"
        assert_eq!(fmt_f(0.25, 1), "0.2");
        assert_eq!(fmt_f(2.5, 0), "2");
        assert_eq!(fmt_f(1.25, 1), "1.2");
        assert_eq!(fmt_f(3.5, 0), "4");
        assert_eq!(fmt_f(-0.3, 0), "-0");
    }

    #[test]
    fn quote_and_repr_tidy() {
        assert_eq!(shlex_quote("a b"), "'a b'");
        assert_eq!(shlex_quote("/x/y-z.1"), "/x/y-z.1");
        assert_eq!(shlex_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(repr_str("it's"), "\"it's\"");
        assert_eq!(repr_list(&["a", "b"]), "['a', 'b']");
    }

    #[test]
    fn fnmatch_cases_tidy() {
        assert!(fnmatchcase("gate-img:1", "gate-img:*"));
        assert!(!fnmatchcase("keepme:1", "gate-img:*"));
        assert!(fnmatchcase("a1", "a[0-9]"));
        assert!(!fnmatchcase("ab", "a[!b]"));
        assert!(fnmatchcase("ab", "a?"));
    }

    #[test]
    fn splitlines_like_python_tidy() {
        assert_eq!(splitlines("a\nb\r\nc\n"), vec!["a", "b", "c"]);
        assert_eq!(splitlines("a\n\nb"), vec!["a", "", "b"]);
        assert_eq!(splitlines(""), Vec::<String>::new());
    }


    #[test]
    fn matches_python_dumps_tidy_json() {
        let v = J::Obj(vec![("b".into(), J::Int(1)), ("a".into(), J::Arr(vec![J::s("é\"\n"), J::Null, J::Float(5.0)]))]);
        assert_eq!(dumps(&v, None, true), r#"{"a": ["\u00e9\"\n", null, 5.0], "b": 1}"#);
        assert_eq!(dumps(&v, Some(2), false), "{\n  \"b\": 1,\n  \"a\": [\n    \"\\u00e9\\\"\\n\",\n    null,\n    5.0\n  ]\n}");
        assert_eq!(dumps(&J::Arr(vec![]), Some(2), false), "[]");
        assert_eq!(dumps(&J::Obj(vec![]), Some(2), false), "{}");
        assert_eq!(dumps(&J::s("\u{7f}"), None, false), "\"\\u007f\"");
        assert_eq!(dumps(&J::s("😀"), None, false), "\"\\ud83d\\ude00\"");
    }
}
