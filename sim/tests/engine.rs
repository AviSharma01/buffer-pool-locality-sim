//! Behavioural tests for the engine: regression against v0.1, policy
//! guarantees from the literature, and the paired-comparison invariant.

use bpsim::{PolicyKind, SimConfig, Simulator, Workload};

fn workload(file: &str) -> Workload {
    let path = format!("{}/workloads/{file}", env!("CARGO_MANIFEST_DIR"));
    Workload::from_toml(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn cfg(policy: PolicyKind, frames: usize, recluster: bool) -> SimConfig {
    SimConfig {
        frames,
        policy,
        recluster_every: recluster.then_some(500),
        ..SimConfig::default()
    }
}

fn misses(w: &Workload, seed: u64, ticks: u64, policy: PolicyKind, frames: usize) -> u64 {
    Simulator::run(w, seed, ticks, cfg(policy, frames, false))
        .metrics()
        .misses
}

/// The refactored engine must reproduce the v0.1 seed-42 results bit for bit
/// (numbers from the original README / snapshot 005000.json).
#[test]
fn reproduces_v0_1_seed_42() {
    let w = Workload::builtin_default();

    let on = Simulator::run(&w, 42, 5000, cfg(PolicyKind::Clock, 64, true));
    let s = on.summary();
    assert_eq!(s.final_avg_cost_ema, 82.32561358114384);
    assert_eq!(s.final_locality_ema, 1.15550556352084);
    assert_eq!(s.hit_rate, 0.2860001446515104);
    assert_eq!((s.evictions, s.dirty_evictions), (29552, 2060));
    assert_eq!(s.final_layout, vec![3, 4, 2, 0, 1, 5]);

    let off = Simulator::run(&w, 42, 5000, cfg(PolicyKind::Clock, 64, false));
    let s = off.summary();
    assert_eq!(s.final_avg_cost_ema, 87.43163167490162);
    assert_eq!(s.final_locality_ema, 2.0167927875805627);
    assert_eq!(s.final_layout, vec![4, 2, 1, 0, 5, 3]);
}

#[test]
fn same_seed_is_deterministic() {
    let w = workload("drift.toml");
    for kind in PolicyKind::ALL {
        let a = Simulator::run(&w, 7, 800, cfg(kind, 32, true));
        let b = Simulator::run(&w, 7, 800, cfg(kind, 32, true));
        assert_eq!(a.summary(), b.summary(), "{kind}");
        assert_eq!(a.snapshot(), b.snapshot(), "{kind}");
    }
}

/// Reclustering changes only where pages live on disk, never which pages are
/// cached, so hits/misses/evictions must be identical with and without it.
/// This is what makes the A/B comparison paired.
#[test]
fn reclustering_never_changes_cache_behaviour() {
    let w = Workload::builtin_default();
    for kind in PolicyKind::ALL {
        let on = Simulator::run(&w, 11, 1500, cfg(kind, 64, true)).summary();
        let off = Simulator::run(&w, 11, 1500, cfg(kind, 64, false)).summary();
        assert_eq!(
            (on.hits, on.misses, on.evictions),
            (off.hits, off.misses, off.evictions),
            "{kind}"
        );
    }
}

/// Belady's MIN is optimal: no policy may miss less on the same trace.
#[test]
fn opt_is_a_lower_bound() {
    for file in ["default.toml", "scan.toml", "drift.toml"] {
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

/// 2Q exists to survive sequential scans; it should clearly beat LRU once the
/// scan phase of the scan workload starts.
#[test]
fn two_q_resists_scans() {
    let w = workload("scan.toml");
    let lru = misses(&w, 1, 5000, PolicyKind::Lru, 64);
    let two_q = misses(&w, 1, 5000, PolicyKind::TwoQ, 64);
    assert!(
        (two_q as f64) < 0.85 * lru as f64,
        "2q {two_q} vs lru {lru}"
    );
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
    assert!(n >= 4);
}
