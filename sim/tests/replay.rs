//! Replaying the committed SQLite capture (traces/shop.trace.json). The
//! expected numbers match an independent JavaScript model of the same policies
//! from the design mockups, minus the two commit writes it counted as hits.

use bpsim::{CapturedTrace, PolicyKind, SimConfig};

fn shop() -> CapturedTrace {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../traces/shop.trace.json");
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!("{path}: {e} (regenerate with capture/capture.py, see CLAUDE.md)")
    });
    CapturedTrace::from_json(&text).unwrap()
}

fn replay(policy: PolicyKind, frames: usize) -> bpsim::snapshot::RunSummary {
    let cfg = SimConfig {
        frames,
        policy,
        ..SimConfig::default()
    };
    let mut sim = shop().simulator(cfg);
    while sim.step().is_some() {}
    sim.summary()
}

#[test]
fn shop_trace_has_the_captured_shape() {
    let t = shop();
    assert_eq!(t.scenario, "shop");
    assert_eq!(t.queries.len(), 9);
    // 45 reads; the update's two commit writes fold into its reads of pages 1 and 5.
    let trace = t.to_trace();
    assert_eq!(trace.accesses().len(), 45);
    let written: Vec<u32> = trace
        .accesses()
        .iter()
        .filter(|a| a.write)
        .map(|a| a.page)
        .collect();
    assert_eq!(written, [1, 5]);
}

#[test]
fn lru_with_8_frames_matches_the_design_numbers() {
    let s = replay(PolicyKind::Lru, 8);
    assert_eq!((s.hits, s.misses), (15, 30));
    // The update dirtied pages 1 and 5; the report later pushes both out.
    assert_eq!(s.dirty_evictions, 2);
    assert_eq!(s.reclusters, 0);
}

#[test]
fn optimal_with_8_frames_matches_the_design_numbers() {
    let s = replay(PolicyKind::Opt, 8);
    assert_eq!((s.hits, s.misses), (22, 23));
}

#[test]
fn no_policy_beats_optimal_on_the_real_trace() {
    for frames in [2, 4, 8, 16, 32] {
        let best = replay(PolicyKind::Opt, frames).misses;
        for kind in PolicyKind::ALL {
            let s = replay(kind, frames);
            assert_eq!(s.accesses, 45, "{kind} at {frames} frames");
            let m = s.misses;
            assert!(best <= m, "frames {frames}: opt {best} > {kind} {m}");
        }
    }
}
