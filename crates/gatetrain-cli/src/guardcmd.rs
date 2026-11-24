//! `gatetrain guard` (docs/admission.md): the admission guard on its own, for launchers that are not `gatetrain chain`.
//! `wait` blocks until a new step may start (exit 0; it never fails a run), `monitor` prints a GUARD_MONITOR line on its interval,
//! `status` prints one check, `env` prints the opt-in `[guard.caps]` as shell assignments. The same flags go on `gatetrain chain`.
use clap::{Args, Subcommand};
use gatetrain_core::admission::{self, Gate, Guard, Overrides, Spec};
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

/// The guard's flags: a hosts file with a `[guard]` table, and overrides of it.
#[derive(Args, Clone, Default)]
pub struct GuardFlags {
    /// a hosts file whose `[guard]` table sets every threshold; re-read before every check (an edit applies at the next step)
    #[arg(long, value_name = "HOSTS_TOML")]
    pub guard_config: Option<PathBuf>,
    /// hold while free space at PATH is below GB (repeatable; overrides the same path's `[[guard.disk]]`)
    #[arg(long, value_name = "PATH=GB", value_parser = admission::parse_disk_floor)]
    pub min_free_disk: Vec<admission::DiskFloor>,
    /// hold while /proc/pressure/memory `full avg10` is above this (percent)
    #[arg(long)]
    pub max_psi_mem: Option<f64>,
    /// hold while /proc/pressure/io `full avg10` is above this (percent)
    #[arg(long)]
    pub max_psi_io: Option<f64>,
    /// hold while /proc/pressure/cpu `some avg10` is above this (percent)
    #[arg(long)]
    pub max_psi_cpu: Option<f64>,
    /// hold while the 1-minute load average is above this
    #[arg(long)]
    pub max_load: Option<f64>,
    /// a protected service's URL (repeatable); hold while it is slow, 5xx or silent --probe-fails times in a row
    #[arg(long)]
    pub probe: Vec<String>,
    /// a probe slower than this many milliseconds is bad (default 2000)
    #[arg(long)]
    pub probe_max_ms: Option<u64>,
    /// this many bad probes in a row hold (default 3)
    #[arg(long)]
    pub probe_fails: Option<u32>,
    /// seconds between checks while holding (default 5)
    #[arg(long)]
    pub guard_every: Option<f64>,
    /// seconds of continuous holding after which the run goes on at one step at a time (default 1800)
    #[arg(long)]
    pub max_hold: Option<f64>,
    /// seconds between GUARD_MONITOR lines (default 300)
    #[arg(long)]
    pub monitor_every: Option<f64>,
    /// seconds between two probes of one service (default 30)
    #[arg(long)]
    pub probe_every: Option<f64>,
    /// read pressure and load under this folder instead of /proc (tests)
    #[arg(long, hide = true)]
    pub guard_proc: Option<PathBuf>,
}

impl GuardFlags {
    pub fn spec(&self) -> Spec {
        Spec {
            config: self.guard_config.clone(),
            overrides: Overrides {
                max_psi_mem_full: self.max_psi_mem,
                max_psi_io_full: self.max_psi_io,
                max_psi_cpu_some: self.max_psi_cpu,
                max_load: self.max_load,
                min_free_disk: self.min_free_disk.clone(),
                probe: self.probe.clone(),
                probe_max_ms: self.probe_max_ms,
                probe_fails: self.probe_fails,
                every_s: self.guard_every,
                max_hold_s: self.max_hold,
                monitor_every_s: self.monitor_every,
                probe_every_s: self.probe_every,
            },
            proc_root: self.guard_proc.clone(),
        }
    }
}

#[derive(Subcommand)]
pub enum GuardCmd {
    /// Block until a new step may start: returns at once on a quiet host; while held, prints GUARD_HOLD and waits; after max_hold_s it returns (exit 0 always, 2 on a config error)
    Wait {
        #[command(flatten)]
        g: GuardFlags,
        /// a name printed on the hold lines (for example the gate about to start)
        #[arg(long)]
        label: Option<String>,
        /// a JSON file the probe counts, the last probe time and the hold start are carried in from one `wait` to the next
        /// (without it every wait starts fresh: a slow service then never reaches `fails` in a row, and each wait probes once)
        #[arg(long)]
        state: Option<PathBuf>,
    },
    /// Print a GUARD_MONITOR line every monitor_every_s (and GUARD_HOLD / GUARD_RELEASE as they happen) until killed
    Monitor {
        #[command(flatten)]
        g: GuardFlags,
        /// one check and exit
        #[arg(long)]
        once: bool,
    },
    /// One check: the settings, the monitor line, the decision (exit 0 admit, 1 hold, 2 config error)
    Status {
        #[command(flatten)]
        g: GuardFlags,
    },
    /// Print `[guard.caps]` (the opt-in limits a launcher applies) as shell assignments: GUARD_SCOPE, GUARD_CPU_QUOTA, ...
    Env {
        #[command(flatten)]
        g: GuardFlags,
    },
}

fn out(l: &str) {
    let mut so = std::io::stdout().lock();
    let _ = writeln!(so, "{l}");
    let _ = so.flush();
}

fn guard(g: &GuardFlags) -> Result<Guard, ExitCode> {
    Guard::new(g.spec()).map_err(|e| {
        eprintln!("guard: {e}");
        ExitCode::from(2)
    })
}

pub fn run(cmd: GuardCmd) -> ExitCode {
    match cmd {
        GuardCmd::Env { g } => match guard(&g) {
            Ok(gd) => {
                print!("{}", admission::caps_env(&gd.settings));
                ExitCode::SUCCESS
            }
            Err(c) => c,
        },
        GuardCmd::Status { g } => {
            let mut gd = match guard(&g) {
                Ok(x) => x,
                Err(c) => return c,
            };
            out(&gd.describe());
            let gate = gd.check(Instant::now(), &mut |l| out(l));
            out(&format!("GUARD_DECISION {}", if gate == Gate::Open { "admit" } else { "hold" }));
            ExitCode::from(u8::from(gate != Gate::Open))
        }
        GuardCmd::Monitor { g, once } => {
            let mut gd = match guard(&g) {
                Ok(x) => x,
                Err(c) => return c,
            };
            out(&gd.describe());
            loop {
                gd.check(Instant::now(), &mut |l| out(l));
                if once {
                    return ExitCode::SUCCESS;
                }
                std::thread::sleep(gd.wait());
            }
        }
        GuardCmd::Wait { g, label, state } => {
            let mut gd = match guard(&g) {
                Ok(x) => x,
                Err(c) => return c,
            };
            gd.monitor = false;
            if let Some(v) = state.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|t| serde_json::from_str(&t).ok()) {
                gd.load_state(&v, Instant::now());
            }
            let save = |gd: &Guard| {
                if let Some(p) = &state {
                    let tmp = p.with_extension(format!("tmp{}", std::process::id()));
                    if std::fs::write(&tmp, gd.state_json(Instant::now()).to_string()).is_ok() {
                        let _ = std::fs::rename(&tmp, p);
                    }
                }
            };
            let tag = label.map(|l| format!("{} ", l.replace(char::is_whitespace, "_"))).unwrap_or_default();
            let t0 = Instant::now();
            loop {
                let gate = gd.check(Instant::now(), &mut |l| out(&format!("{tag}{l}")));
                save(&gd);
                match gate {
                    Gate::Open => return ExitCode::SUCCESS,
                    Gate::Par1 => {
                        out(&format!("{tag}GUARD_WAIT_END after {:.1}s (max_hold_s reached)", t0.elapsed().as_secs_f64()));
                        return ExitCode::SUCCESS;
                    }
                    Gate::Hold => std::thread::sleep(gd.wait()),
                }
            }
        }
    }
}
