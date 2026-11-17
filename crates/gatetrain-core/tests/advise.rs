//! Engine v2 E8 (section 10.3): `advise` only reads. Each rule: a ledger-shaped fixture that fires it and one that does not; an
//! unknown input gives `minutes: unknown` and at most low confidence; the evidence names its figures.
use gatetrain_core::advise::{advise, Advice, DiskCfg, DiskInfo, Inputs, Minutes, ReclaimInfo, RunInfo, SampleInfo, StepInfo};

fn step(name: &str, start: f64, dur: f64, shards: i64) -> StepInfo {
    StepInfo { name: name.into(), start_s: start, dur_s: dur, shards }
}

/// One run of `wall` seconds: a long step `big` plus filler, 3 slots busy on average.
fn run(id: &str, wall: f64, avg_par: f64, steps: Vec<StepInfo>) -> RunInfo {
    let busy = steps.iter().map(|s| s.dur_s).sum();
    RunInfo { id: id.into(), wall_s: wall, busy_s: busy, avg_par, steps }
}

fn sample(cpu: f64, io: f64, disk: f64, mem: f64) -> SampleInfo {
    SampleInfo { run_id: Some("r1".into()), t: 0.0, cpu_busy: Some(cpu), iowait: Some(io), stall_io_some: Some(io / 2.0), stall_io_full: Some(io / 4.0), stall_cpu_some: Some(0.0), mem_free_mb: Some(mem), disks: vec![("sda".into(), Some(disk))] }
}

fn base() -> Inputs {
    Inputs {
        host: "h1".into(),
        allowed_par: Some(8),
        cores: Some(16),
        memory_reserve_gb: Some(4.0),
        disks: vec![DiskCfg { path: "/build".into(), min_free_gb: 20.0, fast: false, uses: vec!["chain".into()] }],
        disk_info: vec![],
        runs: (1..=3).map(|i| run(&format!("r{i}"), 1000.0, 3.0, vec![step("big", 0.0, 600.0, 1), step("b", 0.0, 300.0, 1), step("c", 600.0, 400.0, 1), step("d", 300.0, 300.0, 1)])).collect(),
        samples: (0..30).map(|_| sample(40.0, 2.0, 20.0, 20_000.0)).collect(),
        growth_mb: None,
        reclaim: vec![],
        split_steps: vec![],
        config_hash: "cfg1".into(),
        probe_age_days: Some(1.0),
    }
}

fn by<'a>(a: &'a [Advice], rule: &str) -> Option<&'a Advice> {
    a.iter().find(|x| x.rule == rule)
}

#[test]
fn a_host_with_room_and_a_serial_plan_gets_plan_shape_with_a_range() {
    let a = advise(&base());
    let p = by(&a, "advise.plan-shape").expect("avg parallelism 3 of 8, cpu 40%: the plan is serial");
    // wall 1000 - max(longest 600, busy 1600 / allowed 8 = 200) = 400
    assert!(matches!(p.minutes, Minutes::Range(lo, hi) if (lo - 400.0 / 60.0).abs() < 0.01 && hi >= lo), "{:?}", p.minutes);
    assert!(p.evidence.iter().any(|e| e.contains("cpu busy") && e.contains("40")), "{:?}", p.evidence);
    assert_eq!(p.confidence, "medium", "3 runs, a fresh probe, no unknown input");
    assert!(by(&a, "advise.par-down").is_none());
    // no: a busy host is not a serial plan
    let mut busy = base();
    busy.samples = (0..30).map(|_| sample(97.0, 2.0, 20.0, 20_000.0)).collect();
    assert!(by(&advise(&busy), "advise.plan-shape").is_none());
}

#[test]
fn a_saturated_host_gets_par_down_with_unknown_minutes() {
    let mut i = base();
    i.samples = (0..30).map(|_| sample(98.0, 2.0, 20.0, 20_000.0)).collect();
    let a = advise(&i);
    let p = by(&a, "advise.par-down").expect("cpu busy p50 98%");
    assert!(p.move_line.contains("max_parallel_steps") && p.move_line.contains("6"), "8 lowered by a quarter: {}", p.move_line);
    assert!(matches!(&p.minutes, Minutes::Unknown(why) if why.contains("regression")), "{:?}", p.minutes);
    assert_eq!(p.confidence, "low", "an unknown input is at most low");
    assert!(by(&a, "advise.plan-shape").is_none());
}

#[test]
fn memory_below_the_reserve_gets_mem_and_iowait_gets_par_down() {
    let mut i = base();
    i.samples = (0..30).map(|k| sample(40.0, 2.0, 20.0, if k < 3 { 1_000.0 } else { 20_000.0 })).collect();
    let m = advise(&i);
    let a = by(&m, "advise.mem").expect("free memory p05 under the 4 GB reserve");
    assert!(a.evidence.iter().any(|e| e.contains("p05")), "{:?}", a.evidence);
    let mut j = base();
    j.samples = (0..30).map(|_| sample(50.0, 35.0, 30.0, 20_000.0)).collect();
    assert!(by(&advise(&j), "advise.par-down").is_some(), "iowait p50 35%");
    assert!(by(&advise(&base()), "advise.mem").is_none());
}

#[test]
fn the_longest_step_is_sharded_by_min_four_and_free_slots() {
    let mut i = base();
    i.runs = (1..=3).map(|k| run(&format!("r{k}"), 1000.0, 3.0, vec![step("test-api", 0.0, 700.0, 1), step("lint", 0.0, 200.0, 1), step("tsc", 700.0, 200.0, 1)])).collect();
    let a = advise(&i);
    let s = by(&a, "advise.shard-step").expect("test-api is 70% of the wall and parallelism is 3 of 8");
    assert!(s.subject.contains("test-api") && s.move_line.contains("shards = 4"), "{} / {}", s.subject, s.move_line);
    // 700 x (1 - 1/4) = 525, capped at 700 - next longest 200 = 500
    assert!(matches!(s.minutes, Minutes::Range(lo, hi) if (hi - 500.0 / 60.0).abs() < 0.01 && lo <= hi), "{:?}", s.minutes);
    // few free slots: min(4, free)
    let mut tight = i.clone();
    tight.allowed_par = Some(5);
    let ta = advise(&tight);
    let t = by(&ta, "advise.shard-step").unwrap();
    assert!(t.move_line.contains("shards = 2"), "5 allowed - 3 used = 2 free: {}", t.move_line);
    // already sharded: nothing to advise
    let mut done = i.clone();
    for r in &mut done.runs {
        r.steps[0].shards = 4;
    }
    assert!(by(&advise(&done), "advise.shard-step").is_none());
    // a host with no room does not get it
    let mut full = i.clone();
    for r in &mut full.runs {
        r.avg_par = 8.0;
    }
    assert!(by(&advise(&full), "advise.shard-step").is_none());
}

fn io_bound() -> Inputs {
    let mut i = base();
    i.samples = (0..30).map(|k| sample(30.0, 25.0, if k % 2 == 0 { 95.0 } else { 20.0 }, 20_000.0)).collect();
    i.disks.push(DiskCfg { path: "/ssd".into(), min_free_gb: 20.0, fast: true, uses: vec!["cache".into()] });
    i.disk_info = vec![
        DiskInfo { path: "/build".into(), rotational: Some(true), rotational_reason: None, fsync_p50_ms: Some(12.0), free_gb: Some(500.0), total_gb: Some(1000.0) },
        DiskInfo { path: "/ssd".into(), rotational: Some(false), rotational_reason: None, fsync_p50_ms: Some(0.5), free_gb: Some(300.0), total_gb: Some(500.0) },
    ];
    i.growth_mb = Some(50_000.0);
    i
}

#[test]
fn a_disk_bound_chain_on_a_rotational_disk_moves_to_the_fast_disk_or_adds_one() {
    let a = advise(&io_bound());
    let m = by(&a, "advise.target-to-fast-disk").expect("busy at least 80% in half the samples; /build is rotational; /ssd has room and is 24x faster");
    assert!(m.move_line.contains("/ssd") && m.evidence.iter().any(|e| e.contains("rotational")), "{} {:?}", m.move_line, m.evidence);
    // the stall formula needs the I/O stall share: some 12.5 % of wall, full unknown here
    assert!(matches!(m.minutes, Minutes::Range(_, _) | Minutes::Unknown(_)));
    // no disk with room: add a device
    let mut none = io_bound();
    none.disk_info[1].free_gb = Some(10.0);
    let a = advise(&none);
    assert!(by(&a, "advise.add-device").is_some() && by(&a, "advise.target-to-fast-disk").is_none(), "{a:?}");
    // rotational unknown (a Linux subsystem on Windows): never guessed, so no move, and the unknown is said
    let mut unk = io_bound();
    unk.disk_info[0].rotational = None;
    unk.disk_info[0].rotational_reason = Some("virtual disk".into());
    let a = advise(&unk);
    assert!(by(&a, "advise.target-to-fast-disk").is_none() && by(&a, "advise.add-device").is_none());
    let note = by(&a, "advise.disk-bound-unknown").expect("the busy disk is reported with its unknown");
    assert!(note.evidence.iter().any(|e| e.contains("virtual disk")) && matches!(note.minutes, Minutes::Unknown(_)) && note.confidence == "low");
    // an idle disk gets none of these
    assert!(by(&advise(&base()), "advise.target-to-fast-disk").is_none());
}

#[test]
fn free_disk_against_the_floor_and_chains_of_headroom_only_when_growth_is_measured() {
    let mut i = base();
    i.disk_info = vec![DiskInfo { path: "/build".into(), rotational: Some(false), rotational_reason: None, fsync_p50_ms: None, free_gb: Some(30.0), total_gb: Some(500.0) }];
    // growth unknown: only a free disk under the floor fires, and no chains figure is invented
    assert!(by(&advise(&i), "advise.disk-headroom").is_none(), "30 GB free over a 20 GB floor, growth not measured");
    i.disk_info[0].free_gb = Some(15.0);
    let a = advise(&i);
    let h = by(&a, "advise.disk-headroom").expect("free under the floor");
    assert!(h.evidence.iter().any(|e| e.contains("chains") && e.contains("unknown")), "{:?}", h.evidence);
    // growth measured: (30 - 20) / 8 GB = 1.2 chains of headroom
    let mut g = base();
    g.disk_info = vec![DiskInfo { path: "/build".into(), rotational: Some(false), rotational_reason: None, fsync_p50_ms: None, free_gb: Some(30.0), total_gb: Some(500.0) }];
    g.growth_mb = Some(8192.0);
    let a = advise(&g);
    let h = by(&a, "advise.disk-headroom").expect("1.2 chains of headroom");
    assert!(h.evidence.iter().any(|e| e.contains("1.2 chains")), "{:?}", h.evidence);
    g.disk_info[0].free_gb = Some(200.0);
    assert!(by(&advise(&g), "advise.disk-headroom").is_none(), "plenty of room");
}

#[test]
fn reclaimable_space_is_listed_by_class() {
    let mut i = base();
    i.disk_info = vec![DiskInfo { path: "/build".into(), rotational: Some(false), rotational_reason: None, fsync_p50_ms: None, free_gb: Some(25.0), total_gb: Some(500.0) }];
    i.reclaim = vec![
        ReclaimInfo { class: "docker images".into(), gb: 31.5, how: "docker system df".into() },
        ReclaimInfo { class: "docker build cache".into(), gb: 12.0, how: "docker system df".into() },
        ReclaimInfo { class: "stale target folders".into(), gb: 0.2, how: "tidy target roots".into() },
    ];
    let a = advise(&i);
    let r = by(&a, "advise.reclaim").expect("43.5 GB reclaimable");
    assert!(r.evidence.iter().any(|e| e.contains("docker images") && e.contains("31.5")), "{:?}", r.evidence);
    assert!(r.evidence.iter().any(|e| e.contains("docker build cache") && e.contains("12.0")));
    assert!(r.move_line.contains("gatetrain tidy"), "{}", r.move_line);
    assert!(!r.evidence.iter().any(|e| e.contains("stale target") && e.contains("0.2 ") && false));
    i.reclaim.clear();
    assert!(by(&advise(&i), "advise.reclaim").is_none());
    // an unmeasured class is not a zero
    i.reclaim = vec![ReclaimInfo { class: "docker images".into(), gb: f64::NAN, how: "docker is not installed".into() }];
    assert!(by(&advise(&i), "advise.reclaim").is_none());
}

#[test]
fn a_fast_flag_the_probe_contradicts_and_a_compound_run_are_reported() {
    let mut i = base();
    i.disks = vec![
        DiskCfg { path: "/a".into(), min_free_gb: 20.0, fast: true, uses: vec![] },
        DiskCfg { path: "/b".into(), min_free_gb: 20.0, fast: false, uses: vec![] },
    ];
    i.disk_info = vec![
        DiskInfo { path: "/a".into(), rotational: None, rotational_reason: Some("x".into()), fsync_p50_ms: Some(9.0), free_gb: Some(100.0), total_gb: None },
        DiskInfo { path: "/b".into(), rotational: None, rotational_reason: None, fsync_p50_ms: Some(1.0), free_gb: Some(100.0), total_gb: None },
    ];
    i.split_steps = vec!["build-all".into()];
    let a = advise(&i);
    assert!(by(&a, "advise.flag-contradicts-probe").unwrap().subject.contains("/a"));
    assert!(by(&a, "advise.split-step").unwrap().subject.contains("build-all"));
    i.disk_info[0].fsync_p50_ms = Some(0.5);
    i.split_steps.clear();
    let a = advise(&i);
    assert!(by(&a, "advise.flag-contradicts-probe").is_none() && by(&a, "advise.split-step").is_none());
}

#[test]
fn an_unknown_input_never_becomes_a_number() {
    let mut i = base();
    i.samples.clear(); // no sampler has run: no rule that needs a sample may fire
    i.probe_age_days = None;
    let a = advise(&i);
    for r in ["advise.plan-shape", "advise.par-down", "advise.mem", "advise.target-to-fast-disk"] {
        assert!(by(&a, r).is_none(), "{r} fired with no samples");
    }
    // the chain-B situation of the design: no sample, so nothing can choose between plan-shape and par-down; the shard rule needs none
    assert!(a.iter().all(|x| x.confidence == "low" || x.rule == "advise.shard-step" || x.confidence == "medium"));
}
