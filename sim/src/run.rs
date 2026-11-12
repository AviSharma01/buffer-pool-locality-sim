//! The run file (format `bpsim-run`, version 1): one captured trace replayed
//! through one or more policies with every access explained. The CLI writes it
//! as `run.json` and builds it into `viewer.html`.
//!
//! Shape: the trace's description (objects, pages, queries, rows, notes), the
//! pool size and cost model, then per policy its summary and one
//! [`Event`](crate::sim::Event) per access, each with every frame after it.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::capture::{CapturedTrace, ObjectInfo, PageInfo, PageRows};
use crate::sim::{Event, Simulator};
use crate::snapshot::RunSummary;
use crate::workload::CostModel;

pub const FORMAT: &str = "bpsim-run";
pub const VERSION: u32 = 1;

#[derive(Serialize)]
pub struct RunFile<'a> {
    pub format: &'static str,
    pub version: u32,
    pub scenario: &'a str,
    pub sqlite_version: &'a str,
    pub page_size: u32,
    pub page_count: u32,
    pub frames: usize,
    pub cost: &'a CostModel,
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
            cost: first.cost_model(),
            objects: &trace.objects,
            pages: &trace.pages,
            queries,
            rows: &trace.rows,
            notes: &trace.notes,
            runs,
        }
    }
}
