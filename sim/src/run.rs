//! The run file (format `bpsim-run`, version 5): one captured trace replayed
//! through one or more policies with every access explained. The CLI writes it
//! as `run.json` and builds it into `viewer.html`.
//!
//! Shape: the trace's description (objects, pages, queries, rows, notes), the
//! pool size and storage profiles, then per policy its summary and one
//! [`Event`](crate::sim::Event) per access, each with every frame after it.
//! A miss that read its page has `read` (next, ahead with `skipped`, or
//! jump: where it was relative to the last page read) and, if it read ahead,
//! `ahead` (pages loaded in the same request) and `ahead_evicted`; a hit on a
//! page read ahead and not used before has `read_ahead_hit`. A miss on a page SQLite
//! wrote without reading has none (summary `fresh_pages`). Pages on the
//! freelist have `object: null`. Every storage profile is included, so a
//! viewer can price the same events for any of them.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::capture::{CapturedTrace, ObjectInfo, PageInfo, PageRows};
use crate::sim::{Event, Simulator};
use crate::storage::{Profile, StorageKind};
use crate::summary::RunSummary;

pub const FORMAT: &str = "bpsim-run";
pub const VERSION: u32 = 5;

#[derive(Serialize)]
pub struct RunFile<'a> {
    pub format: &'static str,
    pub version: u32,
    pub scenario: &'a str,
    pub sqlite_version: &'a str,
    pub page_size: u32,
    pub page_count: u32,
    pub frames: usize,
    /// The profile the summaries were priced with; the viewer opens on it.
    pub storage: StorageKind,
    /// Every profile, so the viewer can price the same events either way.
    pub profiles: BTreeMap<&'static str, Profile>,
    pub objects: &'a [ObjectInfo],
    pub pages: &'a [PageInfo],
    pub queries: Vec<QueryView<'a>>,
    pub rows: &'a BTreeMap<u32, PageRows>,
    pub notes: &'a [String],
    pub runs: Vec<PolicyRun<'a>>,
}

#[derive(Serialize)]
pub struct QueryView<'a> {
    pub title: &'a str,
    pub sql: &'a str,
    pub plan: &'a [String],
    /// Index range `[start, end)` of the query's accesses in each run's events.
    pub accesses: [usize; 2],
}

#[derive(Serialize)]
pub struct PolicyRun<'a> {
    pub policy: &'static str,
    pub summary: RunSummary,
    pub events: &'a [Event],
}

impl<'a> RunFile<'a> {
    /// `sims` replayed `trace` to the end with events recorded, all with the
    /// same pool size.
    pub fn new(trace: &'a CapturedTrace, sims: &'a [Simulator]) -> Self {
        let first = sims.first().expect("at least one policy");
        let frames = first.summary().frames;
        let runs = sims
            .iter()
            .map(|sim| {
                let summary = sim.summary();
                assert!(sim.is_done(), "replay finished");
                assert_eq!(summary.frames, frames, "same pool size");
                assert_eq!(summary.storage, first.summary().storage, "same storage");
                assert_eq!(
                    sim.events().len() as u64,
                    summary.accesses,
                    "events recorded"
                );
                PolicyRun {
                    policy: sim.policy_kind().name(),
                    summary,
                    events: sim.events(),
                }
            })
            .collect();
        let queries = trace
            .queries
            .iter()
            .enumerate()
            .map(|(q, query)| {
                let bounds = first.trace().query_bounds(q);
                QueryView {
                    title: &query.title,
                    sql: &query.sql,
                    plan: &query.plan,
                    accesses: [bounds.start, bounds.end],
                }
            })
            .collect();
        Self {
            format: FORMAT,
            version: VERSION,
            scenario: &trace.scenario,
            sqlite_version: &trace.sqlite_version,
            page_size: trace.page_size,
            page_count: trace.page_count,
            frames,
            storage: first.summary().storage,
            profiles: StorageKind::ALL
                .into_iter()
                .map(|k| (k.name(), k.profile()))
                .collect(),
            objects: &trace.objects,
            pages: &trace.pages,
            queries,
            rows: &trace.rows,
            notes: &trace.notes,
            runs,
        }
    }
}
