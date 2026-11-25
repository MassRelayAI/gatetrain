//! The archive's secret scan. `tidy --force` pushes an archive branch to a remote: nothing that looks like a credential may go in it.
//! The content patterns are the ones of the project's own commit hook (`commit-secret-scan.mjs`: generic credential shapes, DOKU
//! shapes, base64 private-key headers, our own environment keys whose value is not a placeholder) plus the common vendor shapes (OpenAI,
//! Stripe, Google, Hugging Face, Anthropic, Slack, `scheme://user:pass@` URLs). They were derived from that hook and public documentation;
//! no real credential file was read to write them. A finding names the file, the line and the KIND of secret, never the value.
use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub file: String,
    pub line: u64,
    pub name: &'static str,
}

/// File names that hold secrets whatever is inside: `*.env`, `.env*`, `*.env.*`, `*.pem`, `*.key`, `id_*`, `*.p12`, `credentials*`, `secrets*` and the
/// usual others. Fail closed: `.env.example` is a hit too (rename it, or archive it by hand).
pub fn secret_name(path: &str) -> bool {
    let b = path.replace('\\', "/").rsplit('/').next().unwrap_or("").to_lowercase();
    b.starts_with(".env")
        || b.ends_with(".env")
        || b.contains(".env.")
        || [".pem", ".key", ".p12", ".pfx", ".jks", ".age", ".kdbx", ".secret", ".secrets"].iter().any(|x| b.ends_with(x))
        || ["id_", "credentials", "secrets"].iter().any(|x| b.starts_with(x))
        || b == ".npmrc"
        || b == ".netrc"
        || b == "token"
        || b.starts_with("token.")
        || b.starts_with("token_")
}

type Pats = Vec<(&'static str, Regex)>;

fn re(p: &str) -> Regex {
    Regex::new(p).unwrap()
}

fn shapes() -> &'static Pats {
    static P: OnceLock<Pats> = OnceLock::new();
    P.get_or_init(|| {
        vec![
            ("aws_key", re(r"(?:AKIA|ASIA)[0-9A-Z]{16}")),
            ("generic_secret", re(r#"(?i)(?:api[-_]?key|secret|passw(?:or)?d|token)\s*[=:]\s*['"][^'"]{8,}"#)),
            ("private_key", re(r"-{5}BEGIN (?:(?:RSA|EC|DSA|ENCRYPTED|OPENSSH) )?PRIVATE KEY-{5}")),
            ("jwt", re(r"eyJ[\w-]{10,}\.eyJ[\w-]{10,}\.[\w-]{10,}")),
            ("github_token", re(r"gh[opsru]_\w{36,}")),
            ("doku_client_id", re(r"\b(?:BRN|MCH)-[0-9]{4}-[0-9]{8,16}\b")),
            ("doku_secret_key", re(r"(?:\b|_)SK-(?:[A-Za-z0-9]{16,}|[A-Za-z0-9]{8,}-[A-Za-z0-9]{8,})")),
            (
                "pem_base64",
                re(r"[A-Za-z0-9+/]{0,3}(?:0tLS[0]tQkVHSU4gRFNBIFBSSVZBVEUgS0VZLS0t|0tLS[0]tQkVHSU4gRU5DUllQVEVEIFBSSVZBVEUgS0VZLS0t|0tLS[0]tQkVHSU4gRUMgUFJJVkFURSBLRVktLS0t|0tLS[0]tQkVHSU4gT1BFTlNTSCBQUklWQVRFIEtFWS0tLS0t|0tLS[0]tQkVHSU4gUFJJVkFURSBLRVktLS0t|0tLS[0]tQkVHSU4gUlNBIFBSSVZBVEUgS0VZLS0t|LS0t[L]S1CRUdJTiBEU0EgUFJJVkFURSBLRVktLS0t|LS0t[L]S1CRUdJTiBFQyBQUklWQVRFIEtFWS0tLS0t|LS0t[L]S1CRUdJTiBFTkNSWVBURUQgUFJJVkFURSBLRVktLS0t|LS0t[L]S1CRUdJTiBPUEVOU1NIIFBSSVZBVEUgS0VZLS0t|LS0t[L]S1CRUdJTiBQUklWQVRFIEtFWS0tLS0t|LS0t[L]S1CRUdJTiBSU0EgUFJJVkFURSBLRVktLS0t|tLS0[t]LUJFR0lOIE9QRU5TU0ggUFJJVkFURSBLRVktLS0t|tLS0[t]LUJFR0lOIERTQSBQUklWQVRFIEtFWS0tLS0t|tLS0[t]LUJFR0lOIEVDIFBSSVZBVEUgS0VZLS0t|tLS0[t]LUJFR0lOIEVOQ1JZUFRFRCBQUklWQVRFIEtFWS0tLS0t|tLS0[t]LUJFR0lOIFBSSVZBVEUgS0VZLS0t|tLS0[t]LUJFR0lOIFJTQSBQUklWQVRFIEtFWS0tLS0t)[A-Za-z0-9+/=]*"),
            ),
            ("openai_key", re(r"\bsk-(?:proj-)?[A-Za-z0-9_-]{32,}")),
            ("anthropic_key", re(r"sk-ant-[A-Za-z0-9_-]{20,}")),
            ("stripe_key", re(r"\b[sr]k_(?:live|test)_[A-Za-z0-9]{16,}")),
            ("google_api_key", re(r"AIza[0-9A-Za-z_-]{35}")),
            ("huggingface_token", re(r"\bhf_[A-Za-z0-9]{30,}")),
            ("slack_token", re(r"xox[baprs]-[A-Za-z0-9-]{10,}")),
            ("url_credentials", re(r#"[A-Za-z][A-Za-z0-9+.-]*://[^\s/:@'"]+:[^\s/@'"]{3,}@"#)),
        ]
    })
}

/// Environment keys: the value (group 1) must not be an obvious placeholder.
fn env_keys() -> &'static Vec<Regex> {
    static P: OnceLock<Vec<Regex>> = OnceLock::new();
    P.get_or_init(|| {
        // the project's own prefix is spelled backwards here, so this public source never carries the name
        let own: String = "YALERSSAM".chars().rev().collect();
        let own_keys = format!(r"{own}_[A-Z0-9_]*_KEY\s*=\s*(\S+)");
        let own_doku = format!(r"{own}_DOKU_(?:PROD|SANDBOX)_[A-Z0-9_]*\s*=\s*(\S+)");
        [
            r"APEX_MIDTRANS_SERVER_KEY\s*=\s*(\S+)",
            r"APEX_PADDLE_API_KEY\s*=\s*(\S+)",
            r"APEX_[A-Z0-9_]*(?:KEY|SECRET|TOKEN|PASSWORD)\s*=\s*(\S+)",
            own_keys.as_str(),
            own_doku.as_str(),
            r"(?:CLOUDFLARE_[A-Z0-9_]*(?:TOKEN|KEY)|CF_API_(?:TOKEN|KEY)|TUNNEL_TOKEN)\s*=\s*(\S+)",
        ]
        .iter()
        .map(|p| re(p))
        .collect()
    })
}

fn is_placeholder(value: &str) -> bool {
    let v = value.trim_matches(|c| c == '"' || c == '\'' || c == ';').trim();
    if v.is_empty() {
        return true;
    }
    if (v.starts_with("${") && v.ends_with('}')) || (v.starts_with('<') && v.ends_with('>') && !v[1..v.len() - 1].contains(['<', '>'])) || v.starts_with("process.env.") {
        return true;
    }
    let u = v.to_uppercase().replace(['_', '-'], "");
    ["REPLACEME", "CHANGEME", "YOURKEY", "YOURKEYHERE", "PLACEHOLDER", "EXAMPLE", "TODO", "TBD", "FIXME"].contains(&u.as_str()) || (u.len() >= 3 && u.chars().all(|c| c == 'X'))
}

/// The kinds of secret one line of text holds (each kind once).
pub fn scan_text(text: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = vec![];
    for line in text.lines() {
        for (name, r) in shapes() {
            if !out.contains(name) && r.is_match(line) {
                out.push(name);
            }
        }
        if !out.contains(&"env_key") && env_keys().iter().any(|r| r.captures(line).map(|c| !is_placeholder(c.get(1).map_or("", |m| m.as_str()))).unwrap_or(false)) {
            out.push("env_key");
        }
    }
    out
}

/// The additions of a `git diff -U0` or `git log -p -U0` text, scanned line by line: the file, the new-file line number and the kind. Removed and
/// context lines are not additions; a file header is not an addition.
pub fn scan_diff(diff: &str) -> Vec<Finding> {
    static HUNK: OnceLock<Regex> = OnceLock::new();
    let hunk = HUNK.get_or_init(|| re(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@"));
    let mut out: Vec<Finding> = vec![];
    let (mut file, mut line_no) = (String::new(), 0u64);
    for l in diff.lines() {
        if let Some(f) = l.strip_prefix("+++ ") {
            file = f.strip_prefix("b/").unwrap_or(f).trim().to_string();
            continue;
        }
        if let Some(c) = hunk.captures(l) {
            line_no = c[1].parse::<u64>().unwrap_or(1).saturating_sub(1);
            continue;
        }
        if l.starts_with('+') && !l.starts_with("+++") {
            line_no += 1;
            for name in scan_text(&l[1..]) {
                let f = Finding { file: file.clone(), line: line_no, name };
                if !out.contains(&f) {
                    out.push(f);
                }
            }
        }
    }
    out
}

/// "file:line (kind), ..." for a message; never a value.
pub fn describe(f: &[Finding]) -> String {
    f.iter().take(8).map(|x| format!("{}:{} ({})", x.file, x.line, x.name)).collect::<Vec<_>>().join(", ") + if f.len() > 8 { ", ..." } else { "" }
}
