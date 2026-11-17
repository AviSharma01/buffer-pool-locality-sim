//! End-of-run summary, written by the CLI as `summary.json`.

use serde::Serialize;

use crate::storage::StorageKind;

#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct RunSummary {
    pub policy: String,
    pub frames: usize,
    /// The profile the times below are for.
    pub storage: StorageKind,
    pub queries: u64,
    pub accesses: u64,
    pub hits: u64,
    pub misses: u64,
    pub hit_rate: f64,
    pub sequential_reads: u64,
    pub random_reads: u64,
    pub evictions: u64,
    /// Modified pages written back before their frame was reused.
    pub dirty_evictions: u64,
    pub total_ms: f64,
    pub mean_ms_per_query: f64,
}
