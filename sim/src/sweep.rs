//! A sweep: every policy at a range of pool sizes, for curves of disk reads
//! and time against memory.
//!
//! Each point is two runs. Disk reads come from a run with read-ahead off, so
//! they show what the policy alone keeps in memory and Optimal is a true
//! floor. Time comes from a run with the read-ahead asked for, since that is
//! what a real system pays; one run prices every kind of storage.

use serde::Serialize;

use crate::PolicyKind;
use crate::capture::SqliteCacheReads;
use crate::readahead::ReadAhead;
use crate::sim::{SimConfig, Simulator};
use crate::storage::StorageKind;
use crate::trace::Trace;

/// The smallest pool a default sweep tries.
const SMALLEST: usize = 8;

/// Pool sizes for a database of `pages` pages: powers of two from 8 and the
/// halfway points between them (8, 12, 16, 24, ...), then the whole database.
pub fn default_sizes(pages: usize) -> Vec<usize> {
    let mut sizes = Vec::new();
    let mut size = SMALLEST;
    while size < pages {
        sizes.push(size);
        let halfway = size + size / 2;
        if halfway < pages {
            sizes.push(halfway);
        }
        size *= 2;
    }
    sizes.push(pages);
    sizes
}

/// One policy at one pool size.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Point {
    pub policy: &'static str,
    pub frames: usize,
    /// With read-ahead off.
    pub hits: u64,
    pub misses: u64,
    /// Pages read from disk with read-ahead off: the misses, less the pages
    /// SQLite created without reading them.
    pub disk_reads: u64,
    /// With the read-ahead asked for: requests the queries waited on, and the
    /// pages they brought in (read-ahead included).
    pub read_requests: u64,
    pub pages_read: u64,
    pub ssd_ms_per_query: f64,
    pub hdd_ms_per_query: f64,
}

/// A sweep as the CLI writes it (`sweep.json`) and the sweep page reads it.
#[derive(Debug, Serialize)]
pub struct SweepFile<'a> {
    pub format: &'static str,
    pub version: u32,
    pub name: &'a str,
    /// "trace" or "workload".
    pub input: &'static str,
    /// Pages in the database: a pool this big holds all of it.
    pub pages: usize,
    /// Bytes per page, for memory sizes; workloads have none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,
    pub queries: usize,
    pub accesses: usize,
    /// For the time curves; disk reads are always without read-ahead.
    pub read_ahead: ReadAhead,
    /// The command that made the sweep, to make it again.
    pub command: String,
    pub policies: Vec<&'static str>,
    pub sizes: Vec<usize>,
    /// Every policy at every size, policy by policy.
    pub points: Vec<Point>,
    pub sqlite_cache: &'a [SqliteCacheReads],
}

pub const FORMAT: &str = "bpsim-sweep";
pub const VERSION: u32 = 1;

/// Replay `trace` with `policy` in a pool of `frames`, twice: read-ahead off
/// for disk reads, `read_ahead` for time.
pub fn point(trace: &Trace, policy: PolicyKind, frames: usize, read_ahead: ReadAhead) -> Point {
    let run = |read_ahead| {
        let cfg = SimConfig {
            frames,
            policy,
            storage: StorageKind::Ssd,
            read_ahead,
        };
        let mut sim = Simulator::new(trace.clone(), cfg);
        while sim.step().is_some() {}
        sim
    };
    let off = run(ReadAhead::Off);
    let with_read_ahead = (read_ahead != ReadAhead::Off).then(|| run(read_ahead));
    let timed = with_read_ahead.as_ref().unwrap_or(&off);
    let (m, t) = (off.metrics(), timed.metrics());
    let queries = timed.tick().max(1) as f64;
    Point {
        policy: policy.name(),
        frames,
        hits: m.hits,
        misses: m.misses,
        disk_reads: m.misses - m.fresh_pages,
        read_requests: t.next_page_reads + t.ahead_reads + t.jump_reads,
        pages_read: t.misses - t.fresh_pages + t.pages_read_ahead,
        ssd_ms_per_query: t.total_ms(StorageKind::Ssd) / queries,
        hdd_ms_per_query: t.total_ms(StorageKind::Hdd) / queries,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_double_with_halfway_points_and_end_at_the_whole_database() {
        assert_eq!(default_sizes(80), [8, 12, 16, 24, 32, 48, 64, 80]);
        assert_eq!(default_sizes(100), [8, 12, 16, 24, 32, 48, 64, 96, 100]);
        assert_eq!(default_sizes(9), [8, 9]);
        assert_eq!(default_sizes(8), [8]);
        assert_eq!(default_sizes(5), [5]);
    }
}
