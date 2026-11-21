//! A tiny program the tests run as a synthetic child process: `gt-testchild <mode> [args]`.
//! It stands in for the interpreter one-liners the tests used before: print, exit, sleep, show the environment or the
//! working directory, write a file of a given size, read a lock sidecar, hold memory. Not installed, not published.
use std::io::Write;
use std::process::Command;
use std::time::Duration;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| a.get(i).map(String::as_str).unwrap_or("");
    match arg(0) {
        "print" => println!("{}", arg(1)),
        "exit" => std::process::exit(arg(1).parse().unwrap_or(1)),
        "pass" => {}
        // `redprint RC LINE...`: print each LINE (the two characters `\e` become ESC, so a test can carry ANSI colour), exit RC: a step that
        // is red every time, with output a reader can judge
        "redprint" => {
            for l in a.iter().skip(2) {
                println!("{}", l.replace("\\e", "\u{1b}"));
            }
            std::process::exit(arg(1).parse().unwrap_or(1));
        }
        // `flaky MARKER RC LINE...`: the first run (MARKER absent) writes MARKER, prints each LINE and exits RC; every later run exits 0
        "flaky" => {
            if std::path::Path::new(arg(1)).exists() {
                return;
            }
            std::fs::write(arg(1), "ran once").unwrap();
            for l in a.iter().skip(3) {
                println!("{}", l.replace("\\e", "\u{1b}"));
            }
            std::process::exit(arg(2).parse().unwrap_or(1));
        }
        // `lines N`: print N numbered lines (one process, no shell)
        "lines" => {
            let out = std::io::stdout();
            let mut w = std::io::BufWriter::new(out.lock());
            for i in 0..arg(1).parse::<u64>().unwrap_or(0) {
                let _ = writeln!(w, "line {i}");
            }
        }
        // `marker FILE SECS`: sleep, then write FILE (proves the process was not killed)
        "marker" => {
            std::thread::sleep(Duration::from_secs_f64(arg(2).parse().unwrap_or(0.0)));
            std::fs::write(arg(1), "alive").unwrap();
        }
        // `gcmarker FILE SECS`: start a `marker` grandchild, then sleep as long as it does
        "gcmarker" => {
            let me = std::env::current_exe().unwrap();
            let _ = Command::new(me).args(["marker", arg(1), arg(2)]).spawn();
            std::thread::sleep(Duration::from_secs_f64(arg(2).parse().unwrap_or(0.0)));
        }
        // `fakessh TARGET -o OPT ... COMMAND`: stands in for ssh in the multi-host tests. COMMAND (POSIX-quoted, as the coordinator
        // sends it) runs here, on this machine; `cat -- FILE` is answered by reading FILE. `FAKESSH_DROP=<target>,...` makes every
        // `agent run` to those targets end like a dropped connection (exit 255, no output).
        "fakessh" => {
            let target = arg(1).to_string();
            let mut i = 2;
            while arg(i) == "-o" {
                i += 2;
            }
            let words = split_posix(arg(i));
            let drop = std::env::var("FAKESSH_DROP").unwrap_or_default();
            if words.iter().any(|w| w == "run") && drop.split(',').any(|t| t == target) {
                eprintln!("Connection to {target} closed by remote host.");
                std::process::exit(255);
            }
            if words.first().map(String::as_str) == Some("cat") {
                let mut so = std::io::stdout();
                for f in words.iter().skip(1).filter(|w| *w != "--") {
                    match std::fs::read(f) {
                        Ok(b) => {
                            let _ = so.write_all(&b);
                        }
                        Err(e) => {
                            eprintln!("cat: {f}: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                return;
            }
            let st = Command::new(&words[0]).args(&words[1..]).status().unwrap_or_else(|e| {
                eprintln!("fakessh: {}: {e}", words[0]);
                std::process::exit(127)
            });
            std::process::exit(st.code().unwrap_or(255));
        }
        "sleep" => std::thread::sleep(Duration::from_secs_f64(arg(1).parse().unwrap_or(0.0))),
        // `printsleep TEXT SECS`: print TEXT and flush, then sleep (output written before a timeout kill)
        "printsleep" => {
            println!("{}", arg(1));
            let _ = std::io::stdout().flush();
            std::thread::sleep(Duration::from_secs_f64(arg(2).parse().unwrap_or(0.0)));
        }
        // `exists FILE`: exit 0 when FILE exists relative to the working directory, else 1
        "exists" => std::process::exit(if std::path::Path::new(arg(1)).exists() { 0 } else { 1 }),
        // a grandchild that holds our stdout for as long as we do
        "grandchild" => {
            let me = std::env::current_exe().unwrap();
            let _ = Command::new(me).args(["sleep", arg(1)]).spawn();
            std::thread::sleep(Duration::from_secs_f64(arg(1).parse().unwrap_or(0.0)));
        }
        // `env P=GATETRAIN_PARALLEL C=GATETRAIN_CORES`: one `LABEL=value` word each, `unset` when the variable is absent
        "env" => {
            let words: Vec<String> = a[1..]
                .iter()
                .map(|x| {
                    let (label, var) = x.split_once('=').unwrap_or((x, x));
                    format!("{label}={}", std::env::var(var).unwrap_or_else(|_| "unset".into()))
                })
                .collect();
            println!("{}", words.join(" "));
        }
        "chain" => {
            println!("CHAIN {} {}", std::env::var("GATETRAIN_RUN_ID").unwrap_or_default(), std::env::var("GATETRAIN_PARALLEL").unwrap_or_else(|_| "unset".into()));
        }
        "cwd" => {
            let p = std::env::current_dir().unwrap().canonicalize().unwrap();
            println!("CWD={}", p.to_string_lossy().trim_start_matches(r"\\?\"));
        }
        // `write FILE MB`: make the folder and write MB mebibytes
        "write" => {
            let p = std::path::Path::new(arg(1));
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).unwrap();
            }
            std::fs::write(p, vec![b'x'; arg(2).parse::<usize>().unwrap_or(1) * 1_048_576]).unwrap();
        }
        // `sidecar LOCK`: print the lock's `.info` file the way the chain would read it
        "sidecar" => {
            let t = std::fs::read_to_string(format!("{}.info", arg(1))).unwrap();
            let v: serde_json::Value = serde_json::from_str(&t).unwrap();
            println!("SIDECAR={v}");
        }
        // `alloc MB SECS`: hold and touch MB of memory for SECS seconds
        "alloc" => {
            let mut v = vec![0u8; arg(1).parse::<usize>().unwrap_or(1) * 1_048_576];
            for i in (0..v.len()).step_by(4096) {
                v[i] = 1;
            }
            std::thread::sleep(Duration::from_secs_f64(arg(2).parse().unwrap_or(0.0)));
            std::hint::black_box(&v);
        }
        // `exitif VALUE WHEN CODE`: exit CODE when VALUE equals WHEN, else 0 (a shard that is red for one number only)
        "exitif" => std::process::exit(if arg(1) == arg(2) { arg(3).parse().unwrap_or(1) } else { 0 }),
        // `failprint CODE VALUE WHEN A B..`: print A, B.. one per line, then exit CODE when VALUE equals WHEN (else 0)
        "failprint" => {
            for x in a.iter().skip(4) {
                println!("{x}");
            }
            std::process::exit(if arg(2) == arg(3) { arg(1).parse().unwrap_or(1) } else { 0 });
        }
        // `args A B C`: one argument per line
        "args" => {
            for x in &a[1..] {
                println!("{x}");
            }
        }
        other => {
            let _ = writeln!(std::io::stderr(), "gt-testchild: unknown mode {other:?}");
            std::process::exit(64);
        }
    }
}

/// POSIX shell words: single quotes literal, double quotes and backslash escapes; enough for what `shlex_join` writes.
fn split_posix(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut have = false;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\'' => {
                have = true;
                for d in it.by_ref() {
                    if d == '\'' {
                        break;
                    }
                    cur.push(d);
                }
            }
            '"' => {
                have = true;
                while let Some(d) = it.next() {
                    match d {
                        '"' => break,
                        '\\' => {
                            if let Some(e) = it.next() {
                                cur.push(e);
                            }
                        }
                        _ => cur.push(d),
                    }
                }
            }
            '\\' => {
                have = true;
                if let Some(e) = it.next() {
                    cur.push(e);
                }
            }
            c if c.is_whitespace() => {
                if have {
                    out.push(std::mem::take(&mut cur));
                    have = false;
                }
            }
            c => {
                have = true;
                cur.push(c);
            }
        }
    }
    if have {
        out.push(cur);
    }
    out
}
