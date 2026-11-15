//! Chrome trace events of a run (port of gatetrain/analysis.py `trace_events`, printed as `json.dumps` prints them).
use crate::analysis::timed;
use crate::logfmt::Step;
use std::fmt::Write;

/// Python `json.dumps` string form: ensure_ascii, `\uXXXX` with surrogate pairs.
fn py_json_str(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            '\u{8}' => o.push_str("\\b"),
            '\u{c}' => o.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7f => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    let _ = write!(o, "\\u{:04x}", u);
                }
            }
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// The JSON list `json.dumps(trace_events(steps))` prints (default separators, insertion-ordered keys).
pub fn trace_json(steps: &[Step]) -> String {
    let mut t = timed(steps);
    if t.is_empty() {
        return "[]".into();
    }
    t.sort_by_key(|s| (s.start.unwrap().instant_us(), s.end().unwrap().instant_us()));
    let origin = t[0].start.unwrap();
    let mut lane_end: Vec<i64> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for s in t {
        let start = s.start.unwrap();
        let ts = (start.secs_since(&origin) * 1_000_000.0) as i64;
        let dur = ((s.duration.unwrap() * 1_000_000.0) as i64).max(1);
        let tid = match lane_end.iter().position(|e| *e <= ts) {
            Some(i) => {
                lane_end[i] = ts + dur;
                i
            }
            None => {
                lane_end.push(ts + dur);
                lane_end.len() - 1
            }
        };
        let red = matches!(s.exit_code, Some(x) if x != 0);
        let exit = s.exit_code.map_or("null".to_string(), |x| x.to_string());
        out.push(format!(
            "{{\"name\": {}, \"ph\": \"X\", \"ts\": {ts}, \"dur\": {dur}, \"pid\": 1, \"tid\": {tid}, \"cname\": \"{}\", \"args\": {{\"exit\": {exit}, \"cached\": {}, \"start\": {}}}}}",
            py_json_str(&s.name),
            if red { "terrible" } else { "good" },
            s.cached,
            py_json_str(&start.iso())
        ));
    }
    format!("[{}]", out.join(", "))
}
