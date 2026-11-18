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
    /// Reads of the page right after the last page read.
    pub next_page_reads: u64,
    /// Reads further on, skipping pages; priced by how many were skipped.
    pub ahead_reads: u64,
    /// Reads backwards, or the first read: a full random request.
    pub jump_reads: u64,
    /// Misses on pages SQLite wrote without reading them first (a new page,
    /// or a free page reused): the frame is filled with no disk read.
    /// misses = next_page_reads + ahead_reads + jump_reads + fresh_pages.
    pub fresh_pages: u64,
    pub evictions: u64,
    /// Modified pages written back before their frame was reused.
    pub dirty_evictions: u64,
    /// Modified pages still in memory when the run ends. A checkpoint would
    /// write them; that time is not counted.
    pub dirty_at_end: u64,
    pub total_ms: f64,
    pub mean_ms_per_query: f64,
}
