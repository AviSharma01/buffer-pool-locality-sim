//! Serialisable views of simulator state. `Snapshot` is the contract with the
//! Python/Manim visualisers: add fields freely, but renaming or removing one
//! breaks them.

use serde::Serialize;

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct FrameSnap {
    /// -1 for an empty frame.
    pub page_id: i64,
    /// -1 for an empty frame.
    pub table_id: i32,
    pub dirty: bool,
    /// Clock reference bit (always false for other policies).
    pub ref_bit: bool,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct MetricsSnap {
    /// Cumulative hits / (hits + misses).
    pub hit_rate: f64,
    /// Exponential moving average (alpha 0.02) of per-query cost.
    pub avg_cost: f64,
    /// Cost of the most recent query.
    pub query_cost: f64,
    /// Cumulative cost since the start of the run.
    pub total_cost: f64,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub dirty_evictions: u64,
    /// EMA of the mean slot distance between tables touched by one query.
    pub locality_score: f64,
}

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub tick: u64,
    /// True if a recluster happened on this tick.
    pub recluster: bool,
    pub policy: String,
    pub frames: Vec<FrameSnap>,
    pub metrics: MetricsSnap,
    /// Disk slot -> table id.
    pub disk_layout: Vec<u16>,
}

/// End-of-run summary, written as `summary.json`.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub policy: String,
    pub frames: usize,
    /// None when reclustering is disabled.
    pub recluster_every: Option<u64>,
    pub co_access_decay: f64,
    pub queries: u64,
    pub accesses: u64,
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
    pub evictions: u64,
    pub dirty_evictions: u64,
    pub total_cost: f64,
    pub mean_cost_per_query: f64,
    pub final_avg_cost_ema: f64,
    pub final_locality_ema: f64,
    pub reclusters: u64,
    pub final_layout: Vec<u16>,
}
