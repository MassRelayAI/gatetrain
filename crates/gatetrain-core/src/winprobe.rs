//! The two small probes a Windows host answers (free disk space, real path of a folder), written for the PowerShell every Windows
//! host has. No Python is needed on any host. The command is built here, run by the host's runner, and parsed by the caller.

/// A PowerShell single-quoted literal.
fn ps(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// A command line that prints the free bytes of the volume holding `path`; it fails (non-zero) when the path does not exist.
pub fn free_bytes_script(path: &str) -> String {
    let p = ps(&path.replace('/', "\\"));
    format!("(New-Object System.IO.DriveInfo ([System.IO.Path]::GetPathRoot((Resolve-Path -LiteralPath {p} -ErrorAction Stop).ProviderPath))).AvailableFreeSpace")
}

/// The same as one shell command line.
pub fn free_bytes_cmd(path: &str) -> String {
    format!("powershell -NoProfile -Command \"{}\"", free_bytes_script(path))
}

/// The script (one `-Command` argument) that prints the real path of `path`: every component that is a symbolic link or a junction
/// is replaced by its target, the way `realpath` does; a missing tail is kept as written.
pub fn realpath_script(path: &str) -> String {
    let p = ps(&path.replace('/', "\\"));
    format!(
        "function GtReal($p){{$p=[System.IO.Path]::GetFullPath($p);$r=[System.IO.Path]::GetPathRoot($p);$c=$r;\
foreach($s in ($p.Substring($r.Length) -split '\\\\' | Where-Object {{$_}})){{$c=Join-Path $c $s;\
for($h=0;$h -lt 20;$h++){{$i=Get-Item -LiteralPath $c -Force -ErrorAction SilentlyContinue;\
if(-not($i -and $i.LinkType -and $i.Target)){{break}};$t=@($i.Target)[0];\
if(-not [System.IO.Path]::IsPathRooted($t)){{$t=Join-Path (Split-Path $c) $t}};$c=GtReal $t}}}};$c}};GtReal {p}"
    )
}

/// `\\?\C:\x` -> `C:\x`, `\\?\UNC\h\s` -> `\\h\s`.
pub fn strip_verbatim(s: &str) -> String {
    if let Some(r) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{r}")
    } else {
        s.strip_prefix(r"\\?\").unwrap_or(s).to_string()
    }
}

/// The real path of `path` on this machine, in-process: the nearest existing ancestor is resolved by the OS (links and junctions
/// followed, `std::fs::canonicalize`) and the missing tail is kept as written. None when no ancestor exists.
pub fn local_realpath(path: &str) -> Option<String> {
    let p = std::path::Path::new(path);
    let mut tail: Vec<&std::ffi::OsStr> = vec![];
    let mut cur = Some(p);
    while let Some(c) = cur {
        if let Ok(mut real) = std::fs::canonicalize(c) {
            for t in tail.iter().rev() {
                real.push(t);
            }
            return Some(strip_verbatim(&real.to_string_lossy()));
        }
        tail.push(c.file_name()?);
        cur = c.parent().filter(|x| !x.as_os_str().is_empty());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_realpath_keeps_a_missing_tail_and_strips_the_verbatim_prefix() {
        let d = tempfile::tempdir().unwrap();
        let want = strip_verbatim(&std::fs::canonicalize(d.path()).unwrap().to_string_lossy());
        let got = local_realpath(&d.path().join("a").join("b").to_string_lossy()).unwrap();
        assert!(got.starts_with(&want) && !got.starts_with(r"\\?\"), "{got} / {want}");
        assert!(got.ends_with("b") && got.contains("a"));
        assert_eq!(strip_verbatim(r"\\?\C:\x"), r"C:\x");
        assert_eq!(strip_verbatim(r"\\?\UNC\h\s"), r"\\h\s");
    }

    #[test]
    fn the_commands_name_no_python_and_quote_the_path() {
        let c = free_bytes_cmd("D:/it's here");
        assert!(c.starts_with("powershell -NoProfile -Command") && c.contains(r"'D:\it''s here'") && c.contains("AvailableFreeSpace"), "{c}");
        let s = realpath_script("C:/a b");
        assert!(s.ends_with(r";GtReal 'C:\a b'"), "{s}");
        assert!(!c.to_lowercase().contains("python") && !s.to_lowercase().contains("python"));
    }

    #[cfg(windows)]
    mod real {
        use super::super::*;
        use std::process::Command;

        fn out(args: &[&str]) -> (bool, String) {
            let o = Command::new(args[0]).args(&args[1..]).output().unwrap();
            (o.status.success(), String::from_utf8_lossy(&o.stdout).trim().to_string())
        }

        #[test]
        fn free_space_of_a_real_folder_is_a_positive_number_and_a_missing_one_fails() {
            let d = tempfile::tempdir().unwrap();
            let (ok, s) = out(&["powershell", "-NoProfile", "-Command", &free_bytes_script(&d.path().to_string_lossy())]);
            assert!(ok && s.parse::<u64>().unwrap() > 0, "{s}");
            let (ok, _) = out(&["powershell", "-NoProfile", "-Command", &free_bytes_script(&d.path().join("nope").to_string_lossy())]);
            assert!(!ok);
        }

        #[test]
        fn a_junction_in_the_middle_of_a_path_is_resolved_and_a_missing_tail_is_kept() {
            let d = tempfile::tempdir().unwrap();
            let target = d.path().join("target");
            std::fs::create_dir_all(target.join("sub")).unwrap();
            let link = d.path().join("link");
            let st = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&link).arg(&target).output().unwrap();
            assert!(st.status.success(), "{}", String::from_utf8_lossy(&st.stdout));
            let want = std::fs::canonicalize(&target).unwrap().to_string_lossy().trim_start_matches(r"\\?\").to_string();
            let (ok, got) = out(&["powershell", "-NoProfile", "-Command", &realpath_script(&link.join("sub").join("missing").to_string_lossy())]);
            assert!(ok, "{got}");
            assert_eq!(got.to_lowercase(), format!("{want}\\sub\\missing").to_lowercase());
            let _ = Command::new("cmd").args(["/C", "rmdir"]).arg(&link).output();
        }
    }
}
