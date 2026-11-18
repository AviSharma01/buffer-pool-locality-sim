//! Replaying the committed SQLite captures. For traces/shop.trace.json the
//! expected numbers match an independent JavaScript model of the same policies
//! from the design mockups, minus the two commit writes it counted as hits.
//! traces/checkout.trace.json is mostly writes and grows the file. The growth
//! and *-vacuum traces are the same queries on two layouts of one database: as
//! it grew, and rebuilt with VACUUM. Their numbers below were recounted by an
//! independent LRU model in Python.

use bpsim::policy::{FrameState, Note, Why};
use bpsim::sim::{Event, Outcome};
use bpsim::{CapturedTrace, PolicyKind, SimConfig, Simulator, StorageKind};

fn load(name: &str) -> CapturedTrace {
    let path = format!("{}/../traces/{name}.trace.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("{path}: {e} (regenerate with capture/capture.py, see CLAUDE.md)")
    });
    CapturedTrace::from_json(&text).unwrap()
}

fn shop() -> CapturedTrace {
    load("shop")
}

fn summary(
    name: &str,
    policy: PolicyKind,
    frames: usize,
    storage: StorageKind,
) -> bpsim::summary::RunSummary {
    let cfg = SimConfig {
        frames,
        policy,
        storage,
    };
    let mut sim = load(name).simulator(cfg);
    while sim.step().is_some() {}
    sim.summary()
}

fn checkout(policy: PolicyKind, frames: usize) -> bpsim::summary::RunSummary {
    summary("checkout", policy, frames, StorageKind::Ssd)
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

fn replay(policy: PolicyKind, frames: usize) -> bpsim::summary::RunSummary {
    run(policy, frames, false).summary()
}

const TRACES: [&str; 5] = ["shop", "checkout", "growth", "growth-vacuum", "shop-vacuum"];

/// LRU with 8 frames: next-page, further-on and jump reads, and total time on
/// SSD and HDD. From the independent model in the 3.4b check (the reviewer's
/// policy model with the pricing rules written from the plan), which agreed
/// with the engine on 708 runs.
struct Pinned {
    name: &'static str,
    reads: (u64, u64, u64),
    fresh: u64,
    ssd_ms: f64,
    hdd_ms: f64,
}
const LRU_8: [Pinned; 5] = [
    Pinned {
        name: "shop",
        reads: (18, 7, 5),
        fresh: 0,
        ssd_ms: 1.3647,
        hdd_ms: 99.4415,
    },
    // The restock splits a full page into page 32, which SQLite writes
    // without reading: that miss fills a frame with no disk read.
    Pinned {
        name: "checkout",
        reads: (7, 12, 13),
        fresh: 1,
        ssd_ms: 1.6432,
        hdd_ms: 397.3618,
    },
    Pinned {
        name: "growth",
        reads: (12, 77, 6),
        fresh: 0,
        ssd_ms: 4.2373,
        hdd_ms: 100.6803,
    },
    Pinned {
        name: "growth-vacuum",
        reads: (84, 5, 6),
        fresh: 0,
        ssd_ms: 4.2373,
        hdd_ms: 98.3403,
    },
    Pinned {
        name: "shop-vacuum",
        reads: (16, 7, 7),
        fresh: 0,
        ssd_ms: 1.3647,
        hdd_ms: 124.7415,
    },
];

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
                assert_ne!(e.frames[e.frame].state, FrameState::Empty, "{at}");
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

#[test]
fn checkout_trace_writes_a_page_it_never_read() {
    let t = load("checkout");
    assert_eq!((t.queries.len(), t.page_count), (11, 32));
    let trace = t.to_trace();
    // 50 reads, plus the page the restock's split created: written, never read.
    assert_eq!(trace.accesses().len(), 51);
    let new_page: Vec<_> = trace.accesses().iter().filter(|a| a.page == 32).collect();
    assert_eq!(new_page.len(), 1);
    assert!(new_page[0].write && new_page[0].fresh);
    assert_eq!(trace.accesses().iter().filter(|a| a.fresh).count(), 1);
    assert_eq!(trace.accesses().iter().filter(|a| a.write).count(), 32);
}

#[test]
fn checkout_with_8_frames_separates_every_kind_of_policy() {
    let hits: Vec<u64> = PolicyKind::ALL
        .into_iter()
        .map(|kind| checkout(kind, 8).hits)
        .collect();
    // fifo, lru, clock, pg, innodb, opt
    assert_eq!(hits, [16, 18, 16, 22, 20, 28]);
    // Most pages get modified, so most evictions have to write back first.
    assert_eq!(checkout(PolicyKind::Lru, 8).dirty_evictions, 17);
    assert_eq!(checkout(PolicyKind::Opt, 8).dirty_evictions, 13);
}

#[test]
fn no_policy_beats_optimal_on_checkout() {
    for frames in [2, 4, 8, 16, 32] {
        let best = checkout(PolicyKind::Opt, frames).misses;
        for kind in PolicyKind::ALL {
            let s = checkout(kind, frames);
            assert_eq!(s.accesses, 51, "{kind} at {frames} frames");
            assert!(
                best <= s.misses,
                "frames {frames}: opt {best} > {kind} {}",
                s.misses
            );
        }
    }
}

/// Storage prices what the pool decided; it must never change the decisions.
#[test]
fn storage_never_changes_what_is_cached() {
    for name in TRACES {
        for frames in [1, 2, 4, 8, 16, 32] {
            for kind in PolicyKind::ALL {
                let [ssd, hdd] = StorageKind::ALL.map(|s| summary(name, kind, frames, s));
                let what = format!("{name} {kind} at {frames} frames");
                assert_eq!(
                    (ssd.hits, ssd.evictions, ssd.dirty_evictions),
                    (hdd.hits, hdd.evictions, hdd.dirty_evictions),
                    "{what}"
                );
                let reads = |s: &bpsim::summary::RunSummary| {
                    (
                        s.next_page_reads,
                        s.ahead_reads,
                        s.jump_reads,
                        s.fresh_pages,
                    )
                };
                assert_eq!(reads(&ssd), reads(&hdd), "{what}");
                let (next, ahead, jump, fresh) = reads(&ssd);
                assert_eq!(next + ahead + jump + fresh, ssd.misses, "{what}");
                assert!(hdd.total_ms > ssd.total_ms, "{what}");
            }
        }
    }
}

#[test]
fn reads_and_times_with_8_frames_match_the_independent_model() {
    for p in LRU_8 {
        let [ssd, hdd] = StorageKind::ALL.map(|s| summary(p.name, PolicyKind::Lru, 8, s));
        assert_eq!(
            (ssd.next_page_reads, ssd.ahead_reads, ssd.jump_reads),
            p.reads,
            "{}",
            p.name
        );
        assert_eq!(ssd.fresh_pages, p.fresh, "{}", p.name);
        assert!(
            (ssd.total_ms - p.ssd_ms).abs() < 1e-9,
            "{}: {}",
            p.name,
            ssd.total_ms
        );
        assert!(
            (hdd.total_ms - p.hdd_ms).abs() < 1e-9,
            "{}: {}",
            p.name,
            hdd.total_ms
        );
    }
}

/// Orders and their event log grew side by side, so orders' pages are 2 to 4
/// apart. In 3.3 only the very next page was cheap, so this layout looked 7x
/// slower on a hard drive; that was an artefact. Reading a few pages further
/// on costs a hard drive only the time for the skipped pages to pass under
/// the head, and an SSD nothing extra, so VACUUM barely changes the time here:
/// it pays off once pages can be read in fewer, larger requests (read-ahead).
#[test]
fn vacuum_barely_changes_the_time_of_finely_interleaved_tables() {
    for kind in PolicyKind::ALL {
        let [g, v] = ["growth", "growth-vacuum"].map(|n| summary(n, kind, 8, StorageKind::Ssd));
        assert_eq!(g.hits, v.hits, "{kind}");
        assert!(
            (g.total_ms - v.total_ms).abs() < 1e-9,
            "{kind}: an SSD has no head"
        );
        let [g, v] = ["growth", "growth-vacuum"].map(|n| summary(n, kind, 8, StorageKind::Hdd));
        let ratio = g.total_ms / v.total_ms;
        assert!((1.0..1.05).contains(&ratio), "{kind}: {ratio}");
        assert!(v.next_page_reads > 5 * g.next_page_reads, "{kind}");
    }
}

/// VACUUM can make things slower. Shop's scan was already in one run, and its
/// lookups read a user's index entry and row, which were written side by side
/// as the table grew. VACUUM separates them, so on a hard drive two cheap
/// reads become jumps.
#[test]
fn vacuum_can_cost_reads_when_rows_were_already_near_each_other() {
    let lru = |name| summary(name, PolicyKind::Lru, 8, StorageKind::Hdd);
    let (grown, vacuumed) = (lru("shop"), lru("shop-vacuum"));
    assert_eq!(grown.hits, vacuumed.hits);
    assert_eq!(vacuumed.jump_reads, grown.jump_reads + 2);
    for kind in PolicyKind::ALL {
        let [s, v] = ["shop", "shop-vacuum"].map(|n| summary(n, kind, 8, StorageKind::Hdd));
        assert!(v.total_ms > 1.1 * s.total_ms, "{kind}");
    }
    let find_by_email = |name| {
        let q = &load(name).queries[2];
        assert_eq!(q.title, "Find user by email");
        q.io.iter().map(|&(_, page)| page).collect::<Vec<_>>()
    };
    // Index leaf, then the cached users root, then the user's leaf right after it.
    assert_eq!(find_by_email("shop"), [1, 3, 29, 2, 30]);
    assert_eq!(find_by_email("shop-vacuum"), [1, 4, 52, 2, 21]);
}

/// Price a recorded run under any profile, the way the viewer does.
fn priced_with(name: &str, policy: PolicyKind, frames: usize, p: bpsim::storage::Profile) -> f64 {
    let cfg = SimConfig {
        frames,
        policy,
        ..SimConfig::default()
    };
    let mut sim = load(name).simulator(cfg);
    sim.record_events();
    while sim.step().is_some() {}
    sim.events()
        .iter()
        .map(|e| match (e.outcome, e.read) {
            (Outcome::Hit, _) => p.hit_ms,
            (Outcome::Miss { evicted }, read) => {
                read.map_or(0.0, |r| p.read_ms(r, 1))
                    + if evicted.is_some_and(|v| v.dirty) {
                        p.write_ms()
                    } else {
                        0.0
                    }
            }
        })
        .sum()
}

/// The hard drive's 0.2 ms overhead for reading further on is the one
/// assumed figure. The lessons drawn from it must hold from 0.05 to 2 ms:
/// growth and growth-vacuum stay within 5%, and shop-vacuum stays slower.
#[test]
fn the_vacuum_lessons_hold_for_any_reasonable_drive_overhead() {
    for near in [0.05, 0.1, 0.2, 0.5, 1.0, 2.0] {
        let p = bpsim::storage::Profile {
            near_request_ms: near,
            ..StorageKind::Hdd.profile()
        };
        for kind in PolicyKind::ALL {
            let [g, v, s, sv] = ["growth", "growth-vacuum", "shop", "shop-vacuum"]
                .map(|n| priced_with(n, kind, 8, p));
            assert!(
                (1.0..1.05).contains(&(g / v)),
                "{near} ms, {kind}: growth {g} vs {v}"
            );
            assert!(sv > 1.1 * s, "{near} ms, {kind}: shop-vacuum {sv} vs {s}");
        }
    }
    // At the profile's own figure, pricing events matches the engine's total.
    let hdd = StorageKind::Hdd.profile();
    let engine = summary("checkout", PolicyKind::Pg, 8, StorageKind::Hdd).total_ms;
    assert!((priced_with("checkout", PolicyKind::Pg, 8, hdd) - engine).abs() < 1e-9);
}
