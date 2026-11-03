//! Replaying the committed SQLite capture (traces/shop.trace.json). The
//! expected numbers match an independent JavaScript model of the same policies
//! from the design mockups, minus the two commit writes it counted as hits.

use bpsim::policy::{FrameState, Note, Why};
use bpsim::sim::{Event, Outcome};
use bpsim::{CapturedTrace, PolicyKind, SimConfig, Simulator};

fn shop() -> CapturedTrace {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../traces/shop.trace.json");
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!("{path}: {e} (regenerate with capture/capture.py, see CLAUDE.md)")
    });
    CapturedTrace::from_json(&text).unwrap()
}

fn run(policy: PolicyKind, frames: usize, record: bool) -> Simulator {
    let cfg = SimConfig {
        frames,
        policy,
        ..SimConfig::default()
    };
    let mut sim = shop().simulator(cfg);
    if record {
        sim.record_events();
    }
    while sim.step().is_some() {}
    sim
}

fn replay(policy: PolicyKind, frames: usize) -> bpsim::snapshot::RunSummary {
    run(policy, frames, false).summary()
}

/// The report (query 4) scans orders; queries 5..9 repeat the lookups.
const REPORT: usize = 4;

fn hits_after_report(policy: PolicyKind) -> usize {
    let sim = run(policy, 8, true);
    let after: Vec<&Event> = sim
        .events()
        .iter()
        .filter(|e| e.ctx.query > REPORT)
        .collect();
    assert_eq!(after.len(), 14);
    after.iter().filter(|e| e.outcome == Outcome::Hit).count()
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

    // The report's plan is `SCAN orders`: every orders page it reads (root 4
    // and leaves 66..=80) is a scan of a 16-page relation; page 1 is not.
    let report = trace.query_bounds(REPORT);
    let scans: Vec<_> = trace.accesses()[report]
        .iter()
        .map(|a| (a.page, a.scan))
        .collect();
    assert_eq!(scans[0], (1, None));
    assert!(
        scans[1..]
            .iter()
            .all(|&(p, s)| s == Some(16) && (p == 4 || (66..=80).contains(&p)))
    );
    assert!(trace.accesses().iter().filter(|a| a.scan.is_some()).count() == 16);
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
fn real_world_policies_with_8_frames_match_the_design_numbers() {
    assert_eq!(replay(PolicyKind::Pg, 8).hits, 18);
    assert_eq!(replay(PolicyKind::InnoDb, 8).hits, 20);
    // While the pool fills, the old sublist is 3/8 of the pages loaded so far,
    // so lookups promoted before the report stay young through it.
    assert_eq!(replay(PolicyKind::InnoDb, 16).hits, 20);
    // What the report cost the lookups that follow it (14 accesses): LRU lets
    // the scan flush them, the scan-aware designs keep most, OPT keeps all.
    let after: Vec<usize> = [
        PolicyKind::Lru,
        PolicyKind::Pg,
        PolicyKind::InnoDb,
        PolicyKind::Opt,
    ]
    .into_iter()
    .map(hits_after_report)
    .collect();
    assert_eq!(after, [7, 10, 12, 14]);
}

#[test]
fn events_explain_every_access() {
    for kind in PolicyKind::ALL {
        for frames in [1, 2, 3, 8, 16, 80] {
            let sim = run(kind, frames, true);
            let s = sim.summary();
            let plain = run(kind, frames, false).summary();
            assert_eq!(s, plain, "{kind}/{frames}: recording changed the run");

            let events = sim.events();
            assert_eq!(events.len() as u64, s.accesses, "{kind}/{frames}");
            let hits = events.iter().filter(|e| e.outcome == Outcome::Hit).count();
            assert_eq!(hits as u64, s.hits, "{kind}/{frames}");
            let mut dirty = 0;
            for (pos, e) in events.iter().enumerate() {
                let at = format!("{kind}/{frames} access {pos}: {e:?}");
                assert_eq!(e.ctx.pos, pos, "{at}");
                assert_eq!(e.frames.len(), frames, "{at}");
                assert_ne!(e.frames[e.frame], FrameState::Empty, "{at}");
                let evicts: Vec<_> = e
                    .notes
                    .iter()
                    .filter_map(|n| match *n {
                        Note::Evict { frame, why } => Some((frame, why)),
                        _ => None,
                    })
                    .collect();
                match e.outcome {
                    Outcome::Miss {
                        evicted: Some(ev), ..
                    } => {
                        assert_eq!(evicts.len(), 1, "{at}");
                        assert_eq!(evicts[0].0, e.frame, "{at}");
                        dirty += ev.dirty as u64;
                    }
                    _ => assert!(evicts.is_empty(), "{at}"),
                }
            }
            assert_eq!(dirty, s.dirty_evictions, "{kind}/{frames}");
        }
    }
}

#[test]
fn the_report_uses_a_ring_under_pg_and_stays_old_under_innodb() {
    let report = |kind| -> Vec<Event> {
        let sim = run(kind, 8, true);
        sim.events()
            .iter()
            .filter(|e| e.ctx.query == REPORT)
            .cloned()
            .collect()
    };
    let notes =
        |events: &[Event]| -> Vec<Note> { events.iter().flat_map(|e| e.notes.clone()).collect() };

    // 8 frames give a 2-frame ring: two frames join it, the other 14 scan
    // misses recycle them.
    let pg = notes(&report(PolicyKind::Pg));
    let joined = pg
        .iter()
        .filter(|n| matches!(n, Note::JoinedRing { .. }))
        .count();
    let reused = pg
        .iter()
        .filter(|n| {
            matches!(
                n,
                Note::Evict {
                    why: Why::RingReuse { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!((joined, reused), (2, 14));

    // Scan pages are read once each, so none is promoted to young.
    let inno = notes(&report(PolicyKind::InnoDb));
    assert!(!inno.iter().any(|n| matches!(n, Note::Promoted { .. })));
    assert_eq!(
        inno.iter()
            .filter(|n| matches!(n, Note::Inserted { .. }))
            .count(),
        16
    );
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
