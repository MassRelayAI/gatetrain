//! `gatetrain privacy-check`: fail when any file of a repository holds a word on the deny-list.
//!
//! The deny-list (`.privacy-denylist`, one sha256 hex per line) holds HASHES of lowercase words, never the words. Every tracked file
//! (and every untracked file git does not ignore) is read, lower-cased and split into words (runs of `[a-z0-9]`); each word is hashed
//! and compared. File paths are checked the same way. A hit names the file and line, never the word.
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

pub const DENYLIST: &str = ".privacy-denylist";

/// sha256 hex of a word.
pub fn sha(word: &str) -> String {
    Sha256::digest(word.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn is_hex64(l: &str) -> bool {
    l.len() == 64 && l.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The hashes of `root/.privacy-denylist` (comments and blank lines skipped; any other line must be 64 lowercase hex).
pub fn load_denylist(root: &Path) -> Result<HashSet<String>, String> {
    let path = root.join(DENYLIST);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut out = HashSet::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        if !is_hex64(line) {
            return Err(format!("{DENYLIST}: not a sha256 hex line"));
        }
        out.insert(line.to_string());
    }
    Ok(out)
}

fn files(root: &Path) -> Result<Vec<String>, String> {
    let o = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !o.status.success() {
        return Err(format!("git ls-files failed in {}: {}", root.display(), String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).split('\0').filter(|p| !p.is_empty()).map(str::to_string).collect())
}

/// True when any word of `text` (runs of `[a-z0-9]` after lower-casing) is on the deny-list.
pub fn scan_text(text: &str, deny: &HashSet<String>) -> bool {
    let low = text.to_lowercase();
    low.split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit())).filter(|w| !w.is_empty()).any(|w| deny.contains(&sha(w)))
}

/// `file`, `file:line` or `file (path)` of every hit; no word is ever returned.
pub fn check(root: &Path, deny: &HashSet<String>) -> Result<Vec<String>, String> {
    let mut hits = vec![];
    for rel in files(root)? {
        if rel == DENYLIST {
            continue;
        }
        if scan_text(&rel, deny) {
            hits.push(format!("{rel} (path)"));
            continue;
        }
        let p = root.join(&rel);
        if !p.is_file() {
            continue;
        }
        let Ok(data) = std::fs::read(&p) else { continue };
        let text = String::from_utf8_lossy(&data);
        if !scan_text(&text, deny) {
            continue;
        }
        for (n, line) in text.lines().enumerate() {
            if scan_text(line, deny) {
                hits.push(format!("{rel}:{}", n + 1));
            }
        }
    }
    Ok(hits)
}

/// The command: prints to stdout, returns the exit code (0 clean, 1 hit, 2 cannot run).
pub fn run(root: &Path, hash: Option<&str>) -> i32 {
    if let Some(w) = hash {
        println!("{}", sha(&w.trim().to_lowercase()));
        return 0;
    }
    let hits = match load_denylist(root).and_then(|d| check(root, &d)) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("privacy: {e}");
            return 2;
        }
    };
    for h in &hits {
        println!("privacy: deny-listed word in {h}");
    }
    if hits.is_empty() {
        println!("privacy: clean");
        0
    } else {
        println!("privacy: {} hit(s); replace with a neutral name (builder-1, host-a, /srv/repos)", hits.len());
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A throwaway git repository holding `files`, with a deny-list of `words` (hashes only).
    fn repo_with(files: &[(&str, &str)], words: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().to_path_buf();
        assert!(Command::new("git").args(["init", "-q"]).arg(&root).status().unwrap().success());
        std::fs::write(root.join(DENYLIST), words.iter().map(|w| sha(w) + "\n").collect::<String>()).unwrap();
        for (rel, text) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        (d, root)
    }

    fn hits(root: &Path) -> Vec<String> {
        let mut h = check(root, &load_denylist(root).unwrap()).unwrap();
        h.sort();
        h
    }

    #[test]
    fn the_repository_is_clean() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert_eq!(hits(&root), Vec::<String>::new());
    }

    #[test]
    fn the_denylist_holds_hashes_only() {
        let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(DENYLIST)).unwrap();
        let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
        assert!(lines.len() >= 3);
        assert!(lines.iter().all(|l| is_hex64(l)), "a line is not a sha256 hex");
    }

    #[test]
    fn a_word_in_a_file_is_found_with_its_line_and_never_printed() {
        let (_d, root) = repo_with(&[("a/b.txt", "fine\nhost secretname here\n")], &["secretname"]);
        let h = hits(&root);
        assert_eq!(h, vec!["a/b.txt:2"]);
        assert!(!h.concat().contains("secretname"));
    }

    #[test]
    fn case_and_punctuation_do_not_hide_a_word() {
        let (_d, root) = repo_with(&[("x.log", "ssh SecretName-01\n"), ("y.toml", "name = \"SECRETNAME\"\n"), ("z.txt", "secretnames and xsecretname\n")], &["secretname"]);
        // words are runs of [a-z0-9]: a hyphen splits, case is folded, a longer word is a different word
        assert_eq!(hits(&root), vec!["x.log:1", "y.toml:1"]);
    }

    #[test]
    fn a_deny_listed_word_in_a_path_is_found() {
        let (_d, root) = repo_with(&[("dir-secretname/x.txt", "clean")], &["secretname"]);
        assert_eq!(hits(&root), vec!["dir-secretname/x.txt (path)"]);
    }

    #[test]
    fn a_clean_tree_passes() {
        let (_d, root) = repo_with(&[("a.txt", "builder-1 host-a /srv/repos")], &["secretname"]);
        assert_eq!(hits(&root), Vec::<String>::new());
    }

    #[test]
    fn a_bad_denylist_line_and_a_missing_denylist_are_errors() {
        let (_d, root) = repo_with(&[], &[]);
        std::fs::write(root.join(DENYLIST), "not-a-hash\n").unwrap();
        assert!(load_denylist(&root).unwrap_err().contains("not a sha256 hex line"));
        std::fs::remove_file(root.join(DENYLIST)).unwrap();
        assert!(load_denylist(&root).is_err());
        assert_eq!(run(&root, None), 2);
    }

    #[test]
    fn the_exit_code_is_one_on_a_hit_and_zero_when_clean() {
        let (_d, root) = repo_with(&[("a.txt", "secretname")], &["secretname"]);
        assert_eq!(run(&root, None), 1);
        let (_d2, clean) = repo_with(&[("a.txt", "fine")], &["secretname"]);
        assert_eq!(run(&clean, None), 0);
        assert_eq!(sha("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }
}
