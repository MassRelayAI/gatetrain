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
