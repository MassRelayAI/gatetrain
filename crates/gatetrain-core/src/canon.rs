//! Canonical JSON, the subset of RFC 8785 the decision log needs (design 6.8):
//! UTF-8, object keys sorted by UTF-16 code unit (as RFC 8785 says; for ASCII keys this is byte order),
//! no whitespace, integers only, strings with the minimal escapes.
use crate::{Error, Result};
use serde_json::Value;

pub fn canonical(v: &Value) -> Result<String> {
    let mut out = String::new();
    write(v, &mut out)?;
    Ok(out)
}

fn write(v: &Value, out: &mut String) -> Result<()> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if n.is_f64() {
                return Err(Error::Canon(format!("floats are not allowed: {n}")));
            }
            out.push_str(&n.to_string());
        }
        // serde_json escapes only ", \, and control characters below 0x20 (short forms for \b \t \n \f \r): the RFC 8785 minimum.
        Value::String(s) => out.push_str(&serde_json::to_string(s)?),
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(x, out)?;
            }
            out.push(']');
        }
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k)?);
                out.push(':');
                write(&m[k], out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn c(v: Value) -> String {
        canonical(&v).unwrap()
    }

    #[test]
    fn golden_sorted_no_whitespace() {
        assert_eq!(c(json!({"b": 1, "a": [true, null, "x"], "c": {"z": 0, "y": -5}})), r#"{"a":[true,null,"x"],"b":1,"c":{"y":-5,"z":0}}"#);
    }

    #[test]
    fn golden_escapes_minimal() {
        // input: a " b \ c, LF, TAB, U+0001, U+007F, e-acute, slash
        let input = "a\"b\\c\n\t\u{1}\u{7f}é/";
        // short escapes for the named controls, \u0001 for the rest, DEL and non-ASCII and the slash written as they are
        let want = r#""a\"b\\c\n\t\u0001"#.to_string() + "\u{7f}é/\"";
        assert_eq!(c(json!(input)), want);
    }

    #[test]
    fn golden_key_order_is_utf16_not_utf8() {
        // U+FF5E (BMP, UTF-16 0xFF5E) sorts after U+10000 (surrogate 0xD800) in UTF-16 but before it in UTF-8/code points.
        let v = json!({"\u{FF5E}": 1, "\u{10000}": 2});
        assert_eq!(c(v), "{\"\u{10000}\":2,\"\u{FF5E}\":1}");
    }

    #[test]
    fn floats_are_refused() {
        assert!(canonical(&json!({"x": 1.5})).is_err());
        assert!(canonical(&json!(1.0)).is_err());
    }

    #[test]
    fn big_integers_survive() {
        assert_eq!(c(json!(18446744073709551615u64)), "18446744073709551615");
        assert_eq!(c(json!(i64::MIN)), "-9223372036854775808");
    }

    #[test]
    fn round_trip_is_stable() {
        let s = c(json!({"k": ["é", {"b": 2, "a": 1}], "n": null}));
        let again = c(serde_json::from_str(&s).unwrap());
        assert_eq!(s, again);
    }
}
