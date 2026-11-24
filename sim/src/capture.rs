//! Traces captured from SQLite by `capture/capture.py` (format `bpsim-trace`,
//! version 2). The engine replays `queries`; the other fields describe the
//! database for the viewer and are passed through to the run file.
//!
//! The captured database file is modelled as a single "table" whose page ids
//! are SQLite's own page numbers, so distance on disk is the real distance
//! between pages in the file.
//!
//! SQLite logs a write when a statement commits, after the statement already
//! read and changed that page in memory. Replaying the write as its own request
//! would count it as an extra cache hit, or with a tiny pool as a disk read that
//! never happened. So each write is folded into the same query's latest read of
//! that page ("read, then modify", like synthetic workloads); a write to a page
//! the query never read stays a write access.
//!
//! Each query lists the tables and indexes it reads end to end (`scans`,
//! worked out by the capture from the statement's bytecode). Its accesses to
//! those objects' pages are marked as a scan of a relation that many pages
//! long.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use crate::sim::{SimConfig, Simulator};
use crate::trace::{Access, Trace};

pub const FORMAT: &str = "bpsim-trace";
pub const VERSION: u32 = 2;
/// The most pages a trace may describe: 40 GB of 4 KB pages, far beyond what
/// a capture produces, but small enough that replaying it cannot exhaust
/// memory before its first access.
pub const MAX_PAGES: u32 = 10_000_000;

#[derive(Debug, Clone, Deserialize)]
pub struct CapturedTrace {
    pub format: String,
    pub version: u32,
    pub scenario: String,
    pub sqlite_version: String,
    pub page_size: u32,
    pub page_count: u32,
    pub objects: Vec<ObjectInfo>,
    /// Which table or index owns each page.
    pub pages: Vec<PageInfo>,
    pub queries: Vec<CapturedQuery>,
    /// Decoded contents of the pages the queries touch.
    pub rows: BTreeMap<u32, PageRows>,
    pub notes: Vec<String>,
    /// Pages SQLite's own page cache read from the file when the same queries
    /// ran with `cache_size` set to `pages` (capture.py --sqlite-cache).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sqlite_cache: Vec<SqliteCacheReads>,
}

/// SQLite's cache is LRU, so the engine's LRU with read-ahead off should read
/// about as many pages. Not exactly: SQLite pins the pages a query is using,
/// and checks page 1 with a 16-byte header read instead of reading it.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct SqliteCacheReads {
    pub pages: usize,
    pub reads: u64,
}

/// A table or index.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ObjectInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub table: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PageInfo {
    pub page: u32,
    /// None for a free page: on the freelist, owned by no table or index.
    pub object: Option<String>,
    /// internal, leaf, overflow or free.
    pub kind: String,
}

/// One page's cells as a table: b-tree rows, index entries or routing entries.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PageRows {
    pub object: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CapturedQuery {
    pub title: String,
    pub sql: String,
    pub plan: Vec<String>,
    /// Tables and indexes this query reads end to end.
    pub scans: Vec<String>,
    pub io: Vec<(IoOp, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum IoOp {
    #[serde(rename = "r")]
    Read,
    #[serde(rename = "w")]
    Write,
}

impl CapturedTrace {
    pub fn from_json(text: &str) -> Result<Self, String> {
        let t: Self = serde_json::from_str(text).map_err(|e| format!("invalid trace JSON: {e}"))?;
        t.validate()?;
        Ok(t)
    }

    fn validate(&self) -> Result<(), String> {
        if self.format != FORMAT {
            return Err(format!("expected format {FORMAT:?}, got {:?}", self.format));
        }
        if self.version == 1 {
            return Err(
                "trace version 1 predates scan detection; capture it again with \
                 capture/capture.py"
                    .into(),
            );
        }
        if self.version != VERSION {
            return Err(format!(
                "unsupported trace version {} (this build reads version {VERSION})",
                self.version
            ));
        }
        if self.scenario.is_empty()
            || !self
                .scenario
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!(
                "scenario name must match [A-Za-z0-9_-]+, got {:?}",
                self.scenario
            ));
        }
        if self.page_count == 0 || self.page_count > MAX_PAGES {
            return Err(format!(
                "page_count must be between 1 and {MAX_PAGES}, got {}",
                self.page_count
            ));
        }
        let mut seen = std::collections::HashSet::new();
        for p in &self.pages {
            if p.page == 0 || p.page > self.page_count {
                return Err(format!(
                    "the page map lists page {}, outside 1..={}",
                    p.page, self.page_count
                ));
            }
            if !seen.insert(p.page) {
                return Err(format!("the page map lists page {} twice", p.page));
            }
        }
        if self.queries.is_empty() {
            return Err("trace has no queries".into());
        }
        for q in &self.queries {
            if let Some(&(_, page)) = q.io.iter().find(|&&(_, p)| p == 0 || p > self.page_count) {
                return Err(format!(
                    "query {:?} touches page {page}, outside 1..={}",
                    q.title, self.page_count
                ));
            }
        }
        Ok(())
    }

    /// One query per tick, with writes folded into reads and scans marked
    /// (see the module docs).
    pub fn to_trace(&self) -> Trace {
        let owner: HashMap<u32, &str> = self
            .pages
            .iter()
            .filter_map(|p| Some((p.page, p.object.as_deref()?)))
            .collect();
        let mut size: HashMap<&str, u32> = HashMap::new();
        for object in self.pages.iter().filter_map(|p| p.object.as_deref()) {
            *size.entry(object).or_default() += 1;
        }
        let queries = self
            .queries
            .iter()
            .map(|q| {
                let scan_of = |page: u32| {
                    let object = *owner.get(&page)?;
                    q.scans.iter().any(|s| s == object).then(|| size[object])
                };
                fold_writes(q, scan_of)
            })
            .collect();
        let mut ids: HashMap<&str, u32> = HashMap::new();
        let mut owners = vec![None; self.page_count as usize + 1];
        for p in &self.pages {
            if let (Some(object), Some(slot)) =
                (p.object.as_deref(), owners.get_mut(p.page as usize))
            {
                let next = ids.len() as u32;
                *slot = Some(*ids.entry(object).or_insert(next));
            }
        }
        // Page 0 does not exist in SQLite; reserving it keeps ids equal to page numbers.
        Trace::from_queries(1, self.page_count + 1, queries).with_owners(owners)
    }

    /// A simulator ready to replay this trace.
    pub fn simulator(&self, cfg: SimConfig) -> Simulator {
        Simulator::new(self.to_trace(), cfg)
    }
}

fn fold_writes(q: &CapturedQuery, scan_of: impl Fn(u32) -> Option<u32>) -> Vec<Access> {
    let mut accesses: Vec<Access> = Vec::with_capacity(q.io.len());
    for &(op, page) in &q.io {
        let earlier = accesses.iter_mut().rev().find(|a| a.page == page);
        match (op, earlier) {
            (IoOp::Write, Some(read)) => read.write = true,
            (op, _) => accesses.push(Access {
                page,
                write: op == IoOp::Write,
                fresh: op == IoOp::Write,
                scan: scan_of(page),
            }),
        }
    }
    accesses
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace_json(format: &str, version: u32, io: &str) -> String {
        trace_with_scans(format, version, io, r#"["t"]"#)
    }

    /// The plan names an alias, as SQLite's does for `FROM t AS x`: only
    /// `scans` decides what is read end to end.
    fn trace_with_scans(format: &str, version: u32, io: &str, scans: &str) -> String {
        format!(
            r#"{{"format":"{format}","version":{version},"scenario":"tiny","sqlite_version":"3.54.1",
               "page_size":4096,"page_count":3,"rows":{{}},"notes":[],"objects":[],
               "pages":[{{"page":1,"object":"sqlite_schema","kind":"leaf"}},
                        {{"page":2,"object":"t","kind":"leaf"}},
                        {{"page":3,"object":"t","kind":"leaf"}}],
               "queries":[{{"title":"q","sql":"SELECT 1;","plan":["SCAN x"],"scans":{scans},
                            "io":{io}}}]}}"#
        )
    }

    #[test]
    fn sqlite_cache_reads_are_optional() {
        let plain = trace_json(FORMAT, VERSION, "[]");
        assert!(
            CapturedTrace::from_json(&plain)
                .unwrap()
                .sqlite_cache
                .is_empty()
        );
        let with = plain.replacen(
            r#""notes":[]"#,
            r#""notes":[],"sqlite_cache":[{"pages":64,"reads":900},{"pages":128,"reads":700}]"#,
            1,
        );
        let t = CapturedTrace::from_json(&with).unwrap();
        assert_eq!(
            t.sqlite_cache,
            [
                SqliteCacheReads {
                    pages: 64,
                    reads: 900
                },
                SqliteCacheReads {
                    pages: 128,
                    reads: 700
                }
            ]
        );
    }

    fn accesses(io: &str) -> Vec<(u32, bool)> {
        let t = CapturedTrace::from_json(&trace_json(FORMAT, VERSION, io)).unwrap();
        let trace = t.to_trace();
        assert_eq!(trace.num_queries(), 1);
        assert_eq!(trace.table_of(3), 0);
        trace.accesses().iter().map(|a| (a.page, a.write)).collect()
    }

    #[test]
    fn scan_marks_only_the_scanned_objects_pages() {
        let io = r#"[["r",1],["r",2],["r",3]]"#;
        let scans = |list: &str| -> Vec<Option<u32>> {
            let t = CapturedTrace::from_json(&trace_with_scans(FORMAT, VERSION, io, list)).unwrap();
            t.to_trace().accesses().iter().map(|a| a.scan).collect()
        };
        assert_eq!(scans(r#"["t"]"#), [None, Some(2), Some(2)]);
        assert_eq!(scans("[]"), [None, None, None], "the plan text is not read");
    }

    #[test]
    fn version_1_traces_are_refused_with_a_way_forward() {
        let err = CapturedTrace::from_json(&trace_json(FORMAT, 1, "[]")).unwrap_err();
        assert!(err.contains("capture it again"), "{err}");
    }

    #[test]
    fn writes_fold_into_the_query_read_of_the_same_page() {
        assert_eq!(
            accesses(r#"[["r",1],["r",3],["w",3]]"#),
            [(1, false), (3, true)]
        );
    }

    #[test]
    fn a_write_without_a_read_stays_a_write_access() {
        assert_eq!(accesses(r#"[["r",1],["w",2]]"#), [(1, false), (2, true)]);
        let t =
            CapturedTrace::from_json(&trace_json(FORMAT, VERSION, r#"[["r",1],["w",2],["w",1]]"#))
                .unwrap();
        let fresh: Vec<_> = t.to_trace().accesses().iter().map(|a| a.fresh).collect();
        assert_eq!(fresh, [false, true]);
    }

    /// Pages on the freelist belong to no table: they load, and a scan's
    /// size counts only the pages its table owns.
    #[test]
    fn free_pages_belong_to_no_object() {
        let json = trace_json(FORMAT, VERSION, r#"[["r",1],["r",2],["r",3]]"#).replace(
            r#"{"page":3,"object":"t","kind":"leaf"}"#,
            r#"{"page":3,"object":null,"kind":"free"}"#,
        );
        let t = CapturedTrace::from_json(&json).unwrap();
        assert_eq!(t.pages[2].object, None);
        let scans: Vec<_> = t.to_trace().accesses().iter().map(|a| a.scan).collect();
        assert_eq!(scans, [None, Some(1), None]);
    }

    #[test]
    fn a_folded_write_causes_no_extra_disk_read_in_a_tiny_pool() {
        // With two frames, page 1 is evicted before the commit writes it.
        let t = CapturedTrace::from_json(&trace_json(
            FORMAT,
            VERSION,
            r#"[["r",1],["r",2],["r",3],["w",1]]"#,
        ))
        .unwrap();
        let mut sim = t.simulator(SimConfig {
            frames: 2,
            policy: crate::PolicyKind::Lru,
            ..SimConfig::default()
        });
        while sim.step().is_some() {}
        let s = sim.summary();
        assert_eq!((s.accesses, s.misses), (3, 3));
    }

    #[test]
    fn rejects_bad_traces() {
        let cases = [
            (trace_json("other", 1, "[]"), "expected format"),
            (trace_json(FORMAT, 3, "[]"), "unsupported trace version"),
            (trace_json(FORMAT, VERSION, r#"[["r",4]]"#), "outside 1..=3"),
            (trace_json(FORMAT, VERSION, r#"[["r",0]]"#), "outside 1..=3"),
            (
                trace_json(FORMAT, VERSION, "[]")
                    .replace(r#""page_count":3"#, r#""page_count":4000000000"#),
                "page_count must be between",
            ),
            (
                trace_json(FORMAT, VERSION, "[]").replace(r#"{"page":3,"#, r#"{"page":2,"#),
                "lists page 2 twice",
            ),
            (
                trace_json(FORMAT, VERSION, "[]").replace(r#"{"page":3,"#, r#"{"page":9,"#),
                "lists page 9, outside",
            ),
            (
                trace_json(FORMAT, VERSION, r#"[["x",1]]"#),
                "unknown variant",
            ),
        ];
        for (json, expect) in cases {
            let err = CapturedTrace::from_json(&json).unwrap_err();
            assert!(err.contains(expect), "{expect}: {err}");
        }
    }
}
