//! The archive's secret scan: names, the content shapes of the project's commit hook (ported) and the common vendor shapes, and the diff reader.
//! Every value here is SYNTHETIC and assembled at run time from pieces, so no scanner (this repo's or anyone's) sees a credential in the source.
use gatetrain_core::force::secrets::{scan_diff, scan_text, secret_name, Finding};

fn j(parts: &[&str]) -> String {
    parts.concat()
}

/// The project's own env-key prefix, spelled backwards in the source so this public file never carries the name.
fn own() -> String {
    "YALERSSAM".chars().rev().collect()
}

fn alnum(n: usize) -> String {
    "aB3dE5gH7jK9mN2pQ4rS6tU8vW0xY1zA".chars().cycle().take(n).collect()
}

#[test]
fn secret_file_names_are_caught_whatever_their_prefix() {
    for n in [
        "stag.env", "prod.env", "foo.env", ".env", ".env.production", ".envrc", "a.env.local", "deploy/stag.env", "id_rsa", "id_ed25519", "id_rsa.pub", "cert.pem", "server.key", "bundle.p12",
        "store.pfx", "release.jks", "creds.age", "credentials.json", "credentials", "secrets.yaml", "secrets", ".npmrc", ".netrc", "token.txt", "SECRETS.TOML", "keys/Prod.ENV",
    ] {
        assert!(secret_name(n), "{n} must be a secret file name");
    }
    for n in ["README.md", "envelope.txt", "environment.md", "src/main.rs", "idle.rs", "keyboard.md", "monkey.txt", "docs/identity.md", "tokenizer.rs"] {
        assert!(!secret_name(n), "{n} is not a secret file name");
    }
}

#[test]
fn every_shape_of_the_hook_and_the_common_vendors_is_found() {
    let cases: Vec<(&str, String)> = vec![
        ("aws_key", j(&["AK", "IA", "ABCDEFGHIJKLMNOP"])),
        ("aws_key", j(&["AS", "IA", "ABCDEFGHIJKLMNOP"])),
        ("generic_secret", j(&["api", "_key = '", "abcdefgh12345678", "'"])),
        ("generic_secret", j(&["pass", "word: \"", "hunter2hunter2", "\""])),
        ("generic_secret", j(&["to", "ken = '", &alnum(12), "'"])),
        ("private_key", j(&["-----", "BEGIN RSA PRIVATE", " KEY-----"])),
        ("private_key", j(&["-----", "BEGIN OPENSSH PRIVATE", " KEY-----"])),
        ("jwt", j(&["ey", "JhbGciOiJIUzI1NiJ9", ".ey", "JzdWIiOiIxMjM0NTY3ODkwIn0", ".", "abcdefghij_klmnop"])),
        ("github_token", j(&["gh", "p_", &alnum(36)])),
        ("doku_client_id", j(&["BR", "N-0123-", "12345678"])),
        ("doku_client_id", j(&["MC", "H-0123-", "1234567890"])),
        ("doku_secret_key", j(&["S", "K-", &alnum(20)])),
        ("doku_secret_key", j(&["S", "K-", &alnum(8), "-", &alnum(12)])),
        ("pem_base64", j(&["LS0tLS1CRUdJTiBQUklWQVRFIEtFWS0tLS0t", "AAAA"])),
        ("pem_base64", j(&["x", "0tLS0tQkVHSU4gUlNBIFBSSVZBVEUgS0VZLS0t", "AAAA"])),
        ("env_key", j(&["APEX_MIDTRANS_SERVER", "_KEY=", &alnum(24)])),
        ("env_key", j(&["APEX_PADDLE_API", "_KEY = ", &alnum(24)])),
        ("env_key", j(&[&own(), "_ACME", "_KEY=", &alnum(24)])),
        ("env_key", j(&[&own(), "_DOKU_PROD", "_SECRET_KEY=", &alnum(24)])),
        ("env_key", j(&["CLOUDFLARE_API", "_TOKEN=", &alnum(30)])),
        ("env_key", j(&["TUNNEL", "_TOKEN=", &alnum(30)])),
        ("openai_key", j(&["s", "k-", &alnum(40)])),
        ("openai_key", j(&["s", "k-proj-", &alnum(40)])),
        ("stripe_key", j(&["s", "k_live_", &alnum(24)])),
        ("stripe_key", j(&["r", "k_test_", &alnum(24)])),
        ("google_api_key", j(&["AI", "za", &alnum(35)])),
        ("huggingface_token", j(&["h", "f_", &alnum(34)])),
        ("anthropic_key", j(&["s", "k-ant-api03-", &alnum(30)])),
        ("slack_token", j(&["xo", "xb-", "1234567890-", &alnum(12)])),
        ("url_credentials", j(&["postgres://app:", "pa55w0rd-", "x@db.internal/app"])),
    ];
    for (want, text) in cases {
        let got = scan_text(&text);
        assert!(got.contains(&want), "{want} not found in {} chars of text; got {got:?}", text.len());
    }
}

#[test]
fn placeholders_ordinary_words_and_public_keys_are_not_secrets() {
    let ph = format!("{}_ACME_KEY=${{ACME_KEY}}", own());
    for t in [
        "APEX_PADDLE_API_KEY=REPLACE_ME",
        ph.as_str(),
        "APEX_MIDTRANS_SERVER_KEY=<your-key>",
        "CLOUDFLARE_API_TOKEN=process.env.CF",
        "SK-1234-fix-login",
        "SK-BLUE-LARGE-01",
        "-----BEGIN PUBLIC KEY-----",
        "api_key = 'short'",
        "token = process.env.TOKEN",
        "see https://example.com/docs/page",
        "postgres://localhost:5432/app",
        "the sky is blue",
    ] {
        assert!(scan_text(t).is_empty(), "{t:?} is not a secret: {:?}", scan_text(t));
    }
}

#[test]
fn the_diff_reader_names_the_file_and_line_and_never_carries_the_value() {
    let secret = j(&["AK", "IA", "ABCDEFGHIJKLMNOP"]);
    let diff = format!("diff --git a/conf/app.txt b/conf/app.txt\nindex 1..2 100644\n--- a/conf/app.txt\n+++ b/conf/app.txt\n@@ -1,0 +5,3 @@\n+fine\n+key = {secret}\n+also fine\n@@ -9 +11 @@\n-old\n+{}\n", j(&["to", "ken = '", &alnum(12), "'"]));
    let f = scan_diff(&diff);
    assert_eq!(
        f,
        vec![Finding { file: "conf/app.txt".into(), line: 6, name: "aws_key" }, Finding { file: "conf/app.txt".into(), line: 11, name: "generic_secret" }]
    );
    assert!(!format!("{f:?}").contains(&secret));
    // a removed line, a file header and a context line are never additions
    assert!(scan_diff(&format!("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-{secret}\n")).is_empty());
    // a new file in a log: -p output carries commit lines, which are ignored
    let log = format!("0123456789012345678901234567890123456789\ndiff --git a/n.txt b/n.txt\nnew file mode 100644\n--- /dev/null\n+++ b/n.txt\n@@ -0,0 +1 @@\n+{secret}\n");
    assert_eq!(scan_diff(&log), vec![Finding { file: "n.txt".into(), line: 1, name: "aws_key" }]);
}
