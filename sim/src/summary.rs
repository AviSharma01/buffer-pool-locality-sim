//! End-of-run summary, written by the CLI as `summary.json`.

use serde::Serialize;

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub policy: String,
    pub frames: usize,
    pub queries: u64,
    pub accesses: u64,
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
    pub evictions: u64,
    pub dirty_evictions: u64,
    pub total_cost: f64,
    pub mean_cost_per_query: f64,
}
