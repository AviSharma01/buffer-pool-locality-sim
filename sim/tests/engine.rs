//! Behavioural tests for the engine: regression against v0.1 and policy
//! guarantees from the literature.

use bpsim::{PolicyKind, SimConfig, Simulator, StorageKind, Workload};

fn workload(file: &str) -> Workload {
    let path = format!("{}/workloads/{file}", env!("CARGO_MANIFEST_DIR"));
    Workload::from_toml(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn cfg(policy: PolicyKind, frames: usize) -> SimConfig {
    SimConfig {
        frames,
        policy,
        ..SimConfig::default()
    }
}

fn misses(w: &Workload, seed: u64, ticks: u64, policy: PolicyKind, frames: usize) -> u64 {
    Simulator::run(w, seed, ticks, cfg(policy, frames))
        .metrics()
        .misses
}

/// The engine must still cache exactly as v0.1 did for seed 42 (numbers from
/// the original README / snapshot 005000.json, tag v0.1). Costs are not
/// compared: v0.1 also charged for where reclustering had moved each table.
#[test]
fn reproduces_v0_1_seed_42() {
    let s = Simulator::run(
        &Workload::builtin_default(),
        42,
        5000,
        cfg(PolicyKind::Clock, 64),
    )
    .summary();
    assert_eq!(s.hit_rate, 0.2860001446515104);
    assert_eq!((s.evictions, s.dirty_evictions), (29552, 2060));
}

#[test]
fn same_seed_is_deterministic() {
    let w = workload("scan.toml");
    for kind in PolicyKind::ALL {
        let a = Simulator::run(&w, 7, 800, cfg(kind, 32));
        let b = Simulator::run(&w, 7, 800, cfg(kind, 32));
        assert_eq!(a.summary(), b.summary(), "{kind}");
    }
}

/// Belady's MIN is optimal: no policy may miss less on the same trace.
#[test]
fn opt_is_a_lower_bound() {
    for file in ["default.toml", "scan.toml"] {
        let w = workload(file);
        for seed in 1..=4 {
            for frames in [8, 64, 200] {
                let best = misses(&w, seed, 2500, PolicyKind::Opt, frames);
                for kind in PolicyKind::ALL {
                    let m = misses(&w, seed, 2500, kind, frames);
                    assert!(
                        best <= m,
                        "{file} seed {seed} frames {frames}: opt {best} > {kind} {m}"
                    );
                }
            }
        }
    }
}

/// LRU and OPT are stack algorithms: a bigger pool never misses more.
#[test]
fn stack_policies_never_get_worse_with_more_frames() {
    let w = Workload::builtin_default();
    for kind in [PolicyKind::Lru, PolicyKind::Opt] {
        let mut prev = u64::MAX;
        for frames in (8..=256).step_by(8) {
            let m = misses(&w, 5, 1500, kind, frames);
            assert!(m <= prev, "{kind}: {frames} frames missed {m} > {prev}");
            prev = m;
        }
    }
}

/// FIFO is not a stack algorithm: the classic string gives 9 misses with 3
/// frames but 10 with 4.
#[test]
fn belady_anomaly_under_fifo() {
    let w = workload("belady.toml");
    assert_eq!(misses(&w, 0, 1, PolicyKind::Fifo, 3), 9);
    assert_eq!(misses(&w, 0, 1, PolicyKind::Fifo, 4), 10);
    // LRU on the same string improves with more memory (10 -> 8).
    assert_eq!(misses(&w, 0, 1, PolicyKind::Lru, 3), 10);
    assert_eq!(misses(&w, 0, 1, PolicyKind::Lru, 4), 8);
}

/// PostgreSQL's scan ring and InnoDB's midpoint insertion exist to keep a
/// big scan from flushing the hot set; both should clearly beat LRU once the
/// scan phase of the scan workload starts.
#[test]
fn real_world_policies_resist_scans() {
    let w = workload("scan.toml");
    for seed in 1..=4 {
        let lru = misses(&w, seed, 5000, PolicyKind::Lru, 64);
        for kind in [PolicyKind::Pg, PolicyKind::InnoDb] {
            let m = misses(&w, seed, 5000, kind, 64);
            assert!(
                (m as f64) < 0.85 * lru as f64,
                "seed {seed}: {kind} {m} vs lru {lru}"
            );
        }
    }
}

#[test]
fn shipped_workloads_are_valid() {
    let dir = format!("{}/workloads", env!("CARGO_MANIFEST_DIR"));
    let mut n = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "toml") {
            let text = std::fs::read_to_string(&path).unwrap();
            Workload::from_toml(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            n += 1;
        }
    }
    assert!(n >= 3);
}

/// Storage only prices the run: hits, evictions and how each page came off
/// the device are the same on every profile, and a hard drive is slower.
#[test]
fn storage_never_changes_cache_behaviour() {
    for file in ["default.toml", "scan.toml"] {
        let w = workload(file);
        for kind in PolicyKind::ALL {
            let [ssd, hdd] = StorageKind::ALL.map(|storage| {
                let c = SimConfig {
                    storage,
                    ..cfg(kind, 64)
                };
                Simulator::run(&w, 9, 1500, c).summary()
            });
            assert_eq!(
                (ssd.hits, ssd.evictions, ssd.dirty_evictions, ssd.jump_reads),
                (hdd.hits, hdd.evictions, hdd.dirty_evictions, hdd.jump_reads),
                "{file} {kind}"
            );
            assert!(hdd.total_ms > ssd.total_ms, "{file} {kind}");
        }
    }
}

/// A scan reads consecutive pages, so most of its misses are next-page reads.
#[test]
fn scans_read_sequentially() {
    let s = Simulator::run(&workload("scan.toml"), 3, 5000, cfg(PolicyKind::Lru, 64)).summary();
    let w = Simulator::run(
        &Workload::builtin_default(),
        3,
        5000,
        cfg(PolicyKind::Lru, 64),
    )
    .summary();
    let share = |s: &bpsim::summary::RunSummary| s.next_page_reads as f64 / s.misses as f64;
    assert!(share(&s) > 0.5, "scan workload: {}", share(&s));
    assert!(
        share(&s) > 2.0 * share(&w),
        "scan {} vs default {}",
        share(&s),
        share(&w)
    );
}

#[test]
fn old_cost_sections_are_refused_with_a_pointer_to_storage() {
    let text = format!(
        "{}\n[cost]\nhit = 1.0\n",
        bpsim::workload::DEFAULT_WORKLOAD_TOML
    );
    let err = Workload::from_toml(&text).unwrap_err();
    assert!(err.contains("--storage ssd|hdd"), "{err}");
}

/// A random captured-style trace: queries of point lookups, short runs and
/// marked scans over `pages` pages, with writes, and some pages written
/// without being read first (new or reused pages).
fn random_trace(seed: u64) -> (bpsim::trace::Trace, u32) {
    use bpsim::trace::{Access, Trace};
    use rand::{Rng, SeedableRng, rngs::StdRng};
    let mut rng = StdRng::seed_from_u64(seed);
    let pages: u32 = rng.gen_range(4..=40);
    let queries = (0..rng.gen_range(1..=12))
        .map(|_| {
            let mut q = Vec::new();
            for _ in 0..rng.gen_range(1..=6) {
                let start = rng.gen_range(1..=pages);
                let (len, scan) = match rng.gen_range(0..3) {
                    0 => (1, None),
                    1 => (rng.gen_range(2..=5), None),
                    _ => (pages, Some(pages)),
                };
                for page in (start..start + len).map(|p| (p - 1) % pages + 1) {
                    let write = rng.gen_bool(0.2);
                    q.push(Access {
                        page,
                        write,
                        fresh: write && rng.gen_bool(0.25),
                        scan,
                    });
                }
            }
            q
        })
        .collect();
    (Trace::from_queries(1, pages + 1, queries), pages)
}

fn replay(
    trace: &bpsim::trace::Trace,
    policy: PolicyKind,
    frames: usize,
    storage: StorageKind,
) -> bpsim::sim::Metrics {
    let cfg = SimConfig {
        frames,
        policy,
        storage,
    };
    let mut sim = Simulator::new(trace.clone(), cfg);
    while sim.step().is_some() {}
    sim.metrics().clone()
}

/// Misses of a textbook LRU and of Belady's OPT, written as plainly as
/// possible, to compare the engine against.
fn reference_misses(pages: &[u32], frames: usize) -> (u64, u64) {
    let mut lru: Vec<u32> = Vec::new(); // least recent first
    let mut lru_misses = 0;
    for &p in pages {
        if let Some(i) = lru.iter().position(|&q| q == p) {
            lru.remove(i);
        } else {
            lru_misses += 1;
            if lru.len() == frames {
                lru.remove(0);
            }
        }
        lru.push(p);
    }
    let mut opt: Vec<u32> = Vec::new();
    let mut opt_misses = 0;
    for (i, &p) in pages.iter().enumerate() {
        if opt.contains(&p) {
            continue;
        }
        opt_misses += 1;
        if opt.len() == frames {
            let next_use = |q: u32| {
                pages[i + 1..]
                    .iter()
                    .position(|&r| r == q)
                    .unwrap_or(usize::MAX)
            };
            let furthest = (0..opt.len()).max_by_key(|&j| next_use(opt[j])).unwrap();
            opt.remove(furthest);
        }
        opt.push(p);
    }
    (lru_misses, opt_misses)
}

/// Guarantees that hold for any trace, checked on many random ones: the
/// counts add up, storage only prices what the pool decided, LRU and OPT
/// never get worse with more memory, and nothing beats OPT on misses.
#[test]
fn guarantees_hold_on_random_traces() {
    for seed in 0..300 {
        let (trace, pages) = random_trace(seed);
        let mut sizes: Vec<usize> = [1, 2, 3, 5, 8, 13]
            .into_iter()
            .filter(|&f| f <= pages as usize)
            .collect();
        sizes.push(pages as usize);
        let mut prev = [u64::MAX; 2];
        for &frames in &sizes {
            let opt = replay(&trace, PolicyKind::Opt, frames, StorageKind::Ssd).misses;
            let order: Vec<u32> = trace.accesses().iter().map(|a| a.page).collect();
            let lru = replay(&trace, PolicyKind::Lru, frames, StorageKind::Ssd).misses;
            assert_eq!(
                (lru, opt),
                reference_misses(&order, frames),
                "seed {seed}, {frames} frames"
            );
            for kind in PolicyKind::ALL {
                let what = format!("seed {seed}, {kind}, {frames} frames");
                let [ssd, hdd] = StorageKind::ALL.map(|s| replay(&trace, kind, frames, s));
                assert_eq!(
                    ssd.hits + ssd.misses,
                    trace.accesses().len() as u64,
                    "{what}"
                );
                assert_eq!(
                    ssd.next_page_reads + ssd.ahead_reads + ssd.jump_reads + ssd.fresh_pages,
                    ssd.misses,
                    "{what}"
                );
                assert_eq!(
                    (
                        ssd.hits,
                        ssd.next_page_reads,
                        ssd.ahead_reads,
                        ssd.jump_reads,
                        ssd.fresh_pages,
                        ssd.evictions,
                        ssd.dirty_evictions
                    ),
                    (
                        hdd.hits,
                        hdd.next_page_reads,
                        hdd.ahead_reads,
                        hdd.jump_reads,
                        hdd.fresh_pages,
                        hdd.evictions,
                        hdd.dirty_evictions
                    ),
                    "{what}"
                );
                assert!(
                    opt <= ssd.misses,
                    "{what}: OPT missed {opt} > {}",
                    ssd.misses
                );
                if let Some(i) = [PolicyKind::Lru, PolicyKind::Opt]
                    .iter()
                    .position(|&k| k == kind)
                {
                    assert!(
                        ssd.misses <= prev[i],
                        "{what}: {} > {} with fewer frames",
                        ssd.misses,
                        prev[i]
                    );
                    prev[i] = ssd.misses;
                }
            }
        }
    }
}
